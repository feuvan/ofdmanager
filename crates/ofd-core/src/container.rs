//! OFD container access. An `.ofd` file is a ZIP archive; this module reads
//! entries by name as raw bytes. It performs no XML parsing and holds no model
//! state, so it stays trivially portable to wasm (bytes in, bytes out).

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

use crate::error::{OfdError, Result};

const CENTRAL_DIRECTORY_HEADER: &[u8; 4] = b"PK\x01\x02";
const CENTRAL_DIRECTORY_END: &[u8; 4] = b"PK\x05\x06";
const ZIP64_CENTRAL_DIRECTORY_END: &[u8; 4] = b"PK\x06\x06";
const ZIP64_CENTRAL_DIRECTORY_LOCATOR: &[u8; 4] = b"PK\x06\x07";
const ARCHIVE_EXTRA_DATA: &[u8; 4] = b"PK\x06\x08";
const CENTRAL_DIRECTORY_SIGNATURE: &[u8; 4] = b"PK\x05\x05";

const CENTRAL_DIRECTORY_HEADER_LEN: usize = 46;
const CENTRAL_DIRECTORY_END_LEN: usize = 22;
const ZIP64_CENTRAL_DIRECTORY_END_MIN_LEN: usize = 56;
const ZIP64_CENTRAL_DIRECTORY_LOCATOR_LEN: usize = 20;

/// Resource limits applied before and while reading an OFD ZIP container.
///
/// The defaults are intentionally finite so a normal [`crate::open`] call is
/// safe for untrusted input. Hosts that intentionally handle larger documents
/// can opt in through [`Container::open_with_limits`].
#[derive(Debug, Clone, Copy)]
pub struct ContainerLimits {
    /// Maximum size of the compressed OFD archive itself.
    pub max_archive_bytes: u64,
    /// Maximum number of entries in the central directory.
    pub max_entries: usize,
    /// Maximum uncompressed size of one entry.
    pub max_entry_bytes: u64,
    /// Maximum declared archive total and cumulative bytes returned by reads.
    pub max_total_uncompressed_bytes: u64,
    /// Maximum declared uncompressed-to-compressed ratio for one entry.
    pub max_compression_ratio: u64,
}

impl Default for ContainerLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 256 * 1024 * 1024,
            max_entries: 32_768,
            max_entry_bytes: 64 * 1024 * 1024,
            max_total_uncompressed_bytes: 256 * 1024 * 1024,
            max_compression_ratio: 10_000,
        }
    }
}

/// A read-only view over the entries of an OFD ZIP container.
pub struct Container {
    archive: zip::ZipArchive<Cursor<Vec<u8>>>,
    limits: ContainerLimits,
    total_read_bytes: u64,
    normalized_names: HashMap<String, Option<String>>,
    compatibility_paths: Vec<(String, String)>,
    compatibility_path_keys: HashSet<(String, String)>,
}

impl Container {
    /// Open a container from the full file bytes.
    pub fn open(bytes: Vec<u8>) -> Result<Self> {
        Self::open_with_limits(bytes, ContainerLimits::default())
    }

    /// Open a container with caller-selected resource limits.
    pub fn open_with_limits(bytes: Vec<u8>, limits: ContainerLimits) -> Result<Self> {
        if bytes.len() as u64 > limits.max_archive_bytes {
            return Err(OfdError::ResourceLimit(format!(
                "archive is {} bytes; limit is {}",
                bytes.len(),
                limits.max_archive_bytes
            )));
        }

        // `zip` stores entries in an IndexMap keyed by its decoded filename.
        // Inspect the raw central directory first: otherwise duplicate names
        // disappear before callers can reject them, and a ZIP64 entry count can
        // trigger a large allocation before `archive.len()` is available.
        let raw_entry_count = inspect_central_directory(&bytes, limits.max_entries)?;
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
        if archive.len() != raw_entry_count {
            return Err(OfdError::Malformed(format!(
                "ZIP central directory declares {raw_entry_count} entries but only {} distinct names are visible",
                archive.len()
            )));
        }

        let mut total = 0u64;
        let mut normalized_names: HashMap<String, Option<String>> = HashMap::new();
        for i in 0..archive.len() {
            let file = archive.by_index(i)?;
            let name = file.name().to_string();
            let key = normalized_name(&name);
            normalized_names
                .entry(key)
                .and_modify(|entry| {
                    if entry.as_deref() != Some(name.as_str()) {
                        *entry = None;
                    }
                })
                .or_insert_with(|| Some(name));

            let size = file.size();
            if size > limits.max_entry_bytes {
                return Err(OfdError::ResourceLimit(format!(
                    "entry {:?} declares {size} uncompressed bytes; per-entry limit is {}",
                    file.name(),
                    limits.max_entry_bytes
                )));
            }
            total = total.checked_add(size).ok_or_else(|| {
                OfdError::ResourceLimit("archive uncompressed size overflow".into())
            })?;
            if total > limits.max_total_uncompressed_bytes {
                return Err(OfdError::ResourceLimit(format!(
                    "archive declares {total} uncompressed bytes; total limit is {}",
                    limits.max_total_uncompressed_bytes
                )));
            }
            let compressed = file.compressed_size();
            if compressed > 0 && size > compressed.saturating_mul(limits.max_compression_ratio) {
                return Err(OfdError::ResourceLimit(format!(
                    "entry {:?} exceeds compression-ratio limit {}:1",
                    file.name(),
                    limits.max_compression_ratio
                )));
            }
        }

        Ok(Self {
            archive,
            limits,
            total_read_bytes: 0,
            normalized_names,
            compatibility_paths: Vec::new(),
            compatibility_path_keys: HashSet::new(),
        })
    }

    /// Read a single entry by its archive path (e.g. `"OFD.xml"`).
    ///
    /// OFD paths in XML are often absolute (`/Doc_0/Document.xml`); callers
    /// should strip the leading slash before calling, or use
    /// [`Container::read_normalized`].
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>> {
        let mut file = self.archive.by_name(name).map_err(|error| match error {
            zip::result::ZipError::FileNotFound => OfdError::MissingEntry(name.to_string()),
            other => OfdError::Container(other),
        })?;
        let declared = file.size();
        if declared > self.limits.max_entry_bytes {
            return Err(OfdError::ResourceLimit(format!(
                "entry {name:?} exceeds the {} byte limit",
                self.limits.max_entry_bytes
            )));
        }
        let remaining = self
            .limits
            .max_total_uncompressed_bytes
            .saturating_sub(self.total_read_bytes);
        if declared > remaining {
            return Err(OfdError::ResourceLimit(format!(
                "cumulative entry reads require at least {} bytes; limit is {}",
                self.total_read_bytes.saturating_add(declared),
                self.limits.max_total_uncompressed_bytes
            )));
        }
        let compressed = file.compressed_size();
        let ratio_limit = if compressed == 0 {
            u64::MAX
        } else {
            compressed.saturating_mul(self.limits.max_compression_ratio)
        };
        let read_limit = self.limits.max_entry_bytes.min(remaining).min(ratio_limit);
        let initial_capacity =
            usize::try_from(declared.min(read_limit).min(1024 * 1024)).unwrap_or(0);
        let mut buf = Vec::with_capacity(initial_capacity);
        let read_result = (&mut file)
            .take(read_limit.saturating_add(1))
            .read_to_end(&mut buf);

        // Charge bytes even when decompression later reports CRC/truncation or
        // the entry exceeds its declared size. Otherwise repeated references to
        // the same malicious entry can restart expensive failed reads for free.
        let bytes_read = buf.len() as u64;
        let total = self
            .total_read_bytes
            .checked_add(bytes_read)
            .ok_or_else(|| OfdError::ResourceLimit("cumulative entry reads overflow".into()))?;
        self.total_read_bytes = total.min(self.limits.max_total_uncompressed_bytes);
        if total > self.limits.max_total_uncompressed_bytes || bytes_read > read_limit {
            return Err(OfdError::ResourceLimit(format!(
                "entry {name:?} exceeds its {read_limit} byte read budget"
            )));
        }
        if compressed > 0
            && bytes_read > compressed.saturating_mul(self.limits.max_compression_ratio)
        {
            return Err(OfdError::ResourceLimit(format!(
                "entry {name:?} exceeds compression-ratio limit {}:1 while reading",
                self.limits.max_compression_ratio
            )));
        }
        read_result?;
        Ok(buf)
    }

    /// Read a standards-defined absolute `ST_Loc` without compatibility
    /// normalization. Signature `FileRef` values use this path because digest
    /// verification must bind to the exact, case-sensitive ZIP entry name.
    pub(crate) fn read_absolute_exact(&mut self, location: &str) -> Result<Vec<u8>> {
        let name = location.strip_prefix('/').ok_or_else(|| {
            OfdError::Malformed(format!("absolute package path expected, got {location:?}"))
        })?;
        if name.is_empty() || name.starts_with('/') || name.contains('\\') {
            return Err(OfdError::Malformed(format!(
                "invalid absolute package path {location:?}"
            )));
        }
        self.read(name)
    }

    /// Read an entry, tolerating a leading `/` and case differences in the
    /// path separators that some producers emit.
    pub fn read_normalized(&mut self, name: &str) -> Result<Vec<u8>> {
        let trimmed = name.trim_start_matches('/');
        match self.read(trimmed) {
            Ok(bytes) => {
                if trimmed.contains('\\') {
                    self.record_compatibility_path(trimmed, trimmed);
                }
                return Ok(bytes);
            }
            Err(OfdError::MissingEntry(_)) => {}
            Err(error) => return Err(error),
        }
        // Fall back to the pre-indexed case-insensitive form used for legacy
        // producers. A collision is rejected rather than selecting an
        // attacker-controlled entry by archive order.
        match self.normalized_names.get(&normalized_name(trimmed)) {
            Some(Some(entry)) => {
                let entry = entry.clone();
                self.record_compatibility_path(trimmed, &entry);
                self.read(&entry)
            }
            Some(None) => Err(OfdError::Malformed(format!(
                "ambiguous case-insensitive ZIP path {name:?}"
            ))),
            None => Err(OfdError::MissingEntry(name.to_string())),
        }
    }

    fn record_compatibility_path(&mut self, requested: &str, actual: &str) {
        let pair = (requested.to_string(), actual.to_string());
        if self.compatibility_path_keys.insert(pair.clone()) {
            self.compatibility_paths.push(pair);
        }
    }

    /// Drain compatibility lookups performed since the previous call.
    ///
    /// A leading `/` is a valid absolute package location and is not reported.
    /// Case folding and backslash separators are accepted for legacy producers,
    /// but violate the standard's case-sensitive `ST_Loc` rules.
    pub(crate) fn take_compatibility_path_warnings(&mut self) -> Vec<String> {
        self.compatibility_path_keys.clear();
        self.compatibility_paths
            .drain(..)
            .map(|(requested, actual)| {
                format!(
                    "non-standard ST_Loc {requested:?} resolved as ZIP entry {actual:?}; paths are case-sensitive and use '/' separators"
                )
            })
            .collect()
    }

    /// List all entry names in the container.
    pub fn entry_names(&self) -> Vec<String> {
        (0..self.archive.len())
            .filter_map(|i| self.archive.name_for_index(i).map(|s| s.to_string()))
            .collect()
    }
}

fn normalized_name(name: &str) -> String {
    name.trim_start_matches('/')
        .replace('\\', "/")
        .to_ascii_lowercase()
}

/// Validate the central directory without decompressing entries. This is a
/// deliberately small ZIP parser: it reads only the standard EOCD/ZIP64
/// metadata and the variable-length central-directory record boundaries, while
/// the `zip` crate remains authoritative for the full archive decoding.
fn inspect_central_directory(bytes: &[u8], max_entries: usize) -> Result<usize> {
    if bytes.len() < CENTRAL_DIRECTORY_END_LEN {
        return Err(OfdError::Malformed(
            "ZIP archive is too short to contain an end record".into(),
        ));
    }

    for eocd_offset in (0..=bytes.len() - CENTRAL_DIRECTORY_END_LEN).rev() {
        if bytes.get(eocd_offset..eocd_offset + 4) != Some(CENTRAL_DIRECTORY_END) {
            continue;
        }
        match inspect_eocd_candidate(bytes, eocd_offset, max_entries)? {
            Some(entry_count) => return Ok(entry_count),
            None => continue,
        }
    }

    Err(OfdError::Malformed(
        "could not locate a consistent ZIP central directory".into(),
    ))
}

fn inspect_eocd_candidate(
    bytes: &[u8],
    eocd_offset: usize,
    max_entries: usize,
) -> Result<Option<usize>> {
    let Some(eocd) = bytes.get(eocd_offset..eocd_offset + CENTRAL_DIRECTORY_END_LEN) else {
        return Ok(None);
    };
    let Some(comment_len) = read_u16(eocd, 20) else {
        return Ok(None);
    };
    let Some(comment_end) =
        (eocd_offset + CENTRAL_DIRECTORY_END_LEN).checked_add(comment_len as usize)
    else {
        return Ok(None);
    };
    // Match the `zip` crate's tolerance for bytes following a valid comment.
    if comment_end > bytes.len() {
        return Ok(None);
    }

    let Some(disk_number) = read_u16(eocd, 4) else {
        return Ok(None);
    };
    let Some(central_directory_disk) = read_u16(eocd, 6) else {
        return Ok(None);
    };
    let Some(entries_on_disk) = read_u16(eocd, 8) else {
        return Ok(None);
    };
    let Some(total_entries) = read_u16(eocd, 10) else {
        return Ok(None);
    };
    if disk_number != central_directory_disk || entries_on_disk != total_entries {
        return Ok(None);
    }

    let Some(central_directory_size) = read_u32(eocd, 12).map(u64::from) else {
        return Ok(None);
    };
    let Some(central_directory_offset) = read_u32(eocd, 16).map(u64::from) else {
        return Ok(None);
    };
    let (start, end, entry_count) =
        if total_entries == u16::MAX || central_directory_offset == u32::MAX as u64 {
            let Some(info) = inspect_zip64_end(bytes, eocd_offset) else {
                return Ok(None);
            };
            info
        } else {
            let Some(start) = (eocd_offset as u64).checked_sub(central_directory_size) else {
                return Ok(None);
            };
            if start < central_directory_offset {
                return Ok(None);
            }
            (start, eocd_offset as u64, entries_on_disk as u64)
        };

    inspect_central_directory_records(bytes, start, end, entry_count, max_entries)
}

fn inspect_zip64_end(bytes: &[u8], eocd_offset: usize) -> Option<(u64, u64, u64)> {
    let locator_offset = eocd_offset.checked_sub(ZIP64_CENTRAL_DIRECTORY_LOCATOR_LEN)?;
    let locator = bytes
        .get(locator_offset..locator_offset.checked_add(ZIP64_CENTRAL_DIRECTORY_LOCATOR_LEN)?)?;
    if locator.get(..4)? != ZIP64_CENTRAL_DIRECTORY_LOCATOR {
        return None;
    }

    let locator_disk = read_u32(locator, 4)?;
    let relative_zip64_offset = read_u64(locator, 8)?;
    let disk_count = read_u32(locator, 16)?;
    if disk_count > 1 || relative_zip64_offset >= locator_offset as u64 {
        return None;
    }

    // A self-extracting archive can have bytes prepended, so the locator's
    // relative offset is only a lower bound on the physical ZIP64 record.
    let mut search_offset = usize::try_from(relative_zip64_offset).ok()?;
    while search_offset.checked_add(ZIP64_CENTRAL_DIRECTORY_END_MIN_LEN)? <= locator_offset {
        let relative = bytes
            .get(search_offset..locator_offset)?
            .windows(4)
            .position(|window| window == ZIP64_CENTRAL_DIRECTORY_END)?;
        let record_offset = search_offset.checked_add(relative)?;
        let record = bytes.get(record_offset..locator_offset)?;
        let record_size = read_u64(record, 4)?;
        let record_len = record_size.checked_add(12)?;
        if record_size >= 44
            && record_len == locator_offset.checked_sub(record_offset)? as u64
            && read_u32(record, 20) == Some(locator_disk)
        {
            let version_made_by = read_u16(record, 12)?;
            let version_needed = read_u16(record, 14)?;
            let disk_number = read_u32(record, 16)?;
            let central_directory_disk = read_u32(record, 20)?;
            let entries_on_disk = read_u64(record, 24)?;
            let total_entries = read_u64(record, 32)?;
            let central_directory_size = read_u64(record, 40)?;
            let central_directory_offset = read_u64(record, 48)?;
            if version_needed > version_made_by
                || disk_number != central_directory_disk
                || entries_on_disk != total_entries
            {
                return None;
            }

            let physical_record_offset = record_offset as u64;
            let archive_offset = physical_record_offset.checked_sub(relative_zip64_offset)?;
            let physical_directory_offset = central_directory_offset.checked_add(archive_offset)?;
            if physical_directory_offset.checked_add(central_directory_size)?
                != physical_record_offset
            {
                return None;
            }
            return Some((
                physical_directory_offset,
                physical_record_offset,
                entries_on_disk,
            ));
        }
        search_offset = record_offset.checked_add(1)?;
    }
    None
}

fn inspect_central_directory_records(
    bytes: &[u8],
    start: u64,
    end: u64,
    entry_count: u64,
    max_entries: usize,
) -> Result<Option<usize>> {
    let Some(directory_size) = end.checked_sub(start) else {
        return Ok(None);
    };
    let Some(minimum_size) = entry_count.checked_mul(CENTRAL_DIRECTORY_HEADER_LEN as u64) else {
        return Ok(None);
    };
    if minimum_size > directory_size {
        return Ok(None);
    }

    let Some(mut offset) = usize::try_from(start).ok() else {
        return Ok(None);
    };
    let Some(end) = usize::try_from(end).ok() else {
        return Ok(None);
    };
    if end > bytes.len() {
        return Ok(None);
    }

    let max_entries = u64::try_from(max_entries).unwrap_or(u64::MAX);
    let collect_names = entry_count <= max_entries;
    let name_capacity = if collect_names {
        usize::try_from(entry_count).unwrap_or(0)
    } else {
        0
    };
    let mut exact_names: HashSet<&[u8]> = HashSet::with_capacity(name_capacity);
    let mut duplicate_name = None;
    for _ in 0..entry_count {
        let Some(header_end) = offset.checked_add(CENTRAL_DIRECTORY_HEADER_LEN) else {
            return Ok(None);
        };
        let Some(header) = bytes.get(offset..header_end) else {
            return Ok(None);
        };
        if header.get(..4) != Some(CENTRAL_DIRECTORY_HEADER) {
            return Ok(None);
        }
        let Some(name_len) = read_u16(header, 28).map(usize::from) else {
            return Ok(None);
        };
        let Some(extra_len) = read_u16(header, 30).map(usize::from) else {
            return Ok(None);
        };
        let Some(comment_len) = read_u16(header, 32).map(usize::from) else {
            return Ok(None);
        };
        let Some(name_end) = header_end.checked_add(name_len) else {
            return Ok(None);
        };
        let Some(next) = name_end
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
        else {
            return Ok(None);
        };
        if next > end {
            return Ok(None);
        }
        let Some(name) = bytes.get(header_end..name_end) else {
            return Ok(None);
        };
        if collect_names && !exact_names.insert(name) && duplicate_name.is_none() {
            duplicate_name = Some(String::from_utf8_lossy(name).into_owned());
        }
        offset = next;
    }

    if !inspect_central_directory_tail(bytes, offset, end) {
        return Ok(None);
    }
    if entry_count > max_entries {
        return Err(OfdError::ResourceLimit(format!(
            "archive has {entry_count} entries; limit is {max_entries}"
        )));
    }
    if let Some(name) = duplicate_name {
        return Err(OfdError::Malformed(format!(
            "duplicate ZIP entry name {name:?}"
        )));
    }
    Ok(usize::try_from(entry_count).ok())
}

fn inspect_central_directory_tail(bytes: &[u8], mut offset: usize, end: usize) -> bool {
    while offset < end {
        let Some(signature) = bytes.get(offset..offset.saturating_add(4)) else {
            return false;
        };
        let record_len = if signature == CENTRAL_DIRECTORY_SIGNATURE {
            let Some(length) = read_u16(bytes, offset.saturating_add(4)) else {
                return false;
            };
            6usize.checked_add(length as usize)
        } else if signature == ARCHIVE_EXTRA_DATA {
            let Some(length) = read_u32(bytes, offset.saturating_add(4)) else {
                return false;
            };
            usize::try_from(length)
                .ok()
                .and_then(|length| 8usize.checked_add(length))
        } else {
            return false;
        };
        let Some(next) = record_len.and_then(|length| offset.checked_add(length)) else {
            return false;
        };
        if next > end {
            return false;
        }
        offset = next;
    }
    true
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn rejects_declared_entry_and_total_sizes_over_limits() {
        let bytes = archive(&[("a", b"12345"), ("b", b"67890")]);
        let limits = ContainerLimits {
            max_archive_bytes: bytes.len() as u64,
            max_entries: 2,
            max_entry_bytes: 4,
            max_total_uncompressed_bytes: 100,
            max_compression_ratio: 10_000,
        };
        assert!(matches!(
            Container::open_with_limits(bytes.clone(), limits),
            Err(OfdError::ResourceLimit(_))
        ));

        let limits = ContainerLimits {
            max_entry_bytes: 5,
            max_total_uncompressed_bytes: 9,
            ..limits
        };
        assert!(matches!(
            Container::open_with_limits(bytes, limits),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn reads_normal_entry_under_limits() {
        let bytes = archive(&[("OFD.xml", b"<OFD/>")]);
        let mut container = Container::open(bytes).unwrap();
        assert_eq!(container.read("OFD.xml").unwrap(), b"<OFD/>");
    }

    #[test]
    fn accepts_self_extracting_prefixes() {
        let archive = archive(&[("OFD.xml", b"<OFD/>")]);
        let mut prefixed = b"native launcher prefix that is not ZIP data".to_vec();
        prefixed.extend_from_slice(&archive);

        let mut container = Container::open(prefixed).unwrap();
        assert_eq!(container.read("OFD.xml").unwrap(), b"<OFD/>");
    }

    #[test]
    fn skips_false_eocd_signatures_inside_the_real_comment() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("OFD.xml", options).unwrap();
        writer.write_all(b"<OFD/>").unwrap();

        // The fake record is long enough to look parseable, but declares
        // inconsistent disk numbers. Both our preflight and `zip` must skip it
        // and continue backwards to the real EOCD that owns this comment.
        let mut comment = b"comment-prefix".to_vec();
        comment.extend_from_slice(CENTRAL_DIRECTORY_END);
        comment.extend_from_slice(&1u16.to_le_bytes());
        comment.extend_from_slice(&0u16.to_le_bytes());
        comment.extend_from_slice(&0u16.to_le_bytes());
        comment.extend_from_slice(&0u16.to_le_bytes());
        comment.extend_from_slice(&0u32.to_le_bytes());
        comment.extend_from_slice(&0u32.to_le_bytes());
        comment.extend_from_slice(&0u16.to_le_bytes());
        comment.extend_from_slice(b"comment-suffix");
        writer.set_raw_comment(comment.into_boxed_slice());

        let bytes = writer.finish().unwrap().into_inner();
        let mut container = Container::open(bytes).unwrap();
        assert_eq!(container.read("OFD.xml").unwrap(), b"<OFD/>");
    }

    #[test]
    fn repeated_reads_share_the_total_uncompressed_budget() {
        let bytes = archive(&[("a", b"12345")]);
        let limits = ContainerLimits {
            max_archive_bytes: bytes.len() as u64,
            max_entries: 1,
            max_entry_bytes: 5,
            max_total_uncompressed_bytes: 5,
            max_compression_ratio: 10_000,
        };
        let mut container = Container::open_with_limits(bytes, limits).unwrap();
        assert_eq!(container.read("a").unwrap(), b"12345");
        assert!(matches!(
            container.read("a"),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn failed_oversized_reads_still_exhaust_the_budget() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("a", options).unwrap();
        writer.write_all(b"12345").unwrap();
        let mut bytes = writer.finish().unwrap().into_inner();

        let central = bytes
            .windows(4)
            .position(|window| window == CENTRAL_DIRECTORY_HEADER)
            .unwrap();
        // Lie only in the authoritative central directory: declared size 1,
        // actual stored data 5. This gets past open-time declared-size checks.
        bytes[central + 24..central + 28].copy_from_slice(&1u32.to_le_bytes());
        let limits = ContainerLimits {
            max_archive_bytes: bytes.len() as u64,
            max_entries: 1,
            max_entry_bytes: 4,
            max_total_uncompressed_bytes: 4,
            max_compression_ratio: 10_000,
        };
        let mut container = Container::open_with_limits(bytes, limits).unwrap();
        assert!(matches!(
            container.read("a"),
            Err(OfdError::ResourceLimit(_))
        ));
        assert_eq!(container.total_read_bytes, 4);
        assert!(matches!(
            container.read("a"),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn compression_ratio_bounds_streamed_output_before_full_decompression() {
        let payload = vec![b'A'; 8192];
        let mut bytes = archive(&[("a", &payload)]);
        let central = bytes
            .windows(4)
            .position(|window| window == CENTRAL_DIRECTORY_HEADER)
            .unwrap();
        let compressed = u64::from(read_u32(&bytes, central + 20).unwrap());
        assert!(compressed < payload.len() as u64);
        // The central directory lies about the uncompressed length so the
        // open-time declared-ratio check passes. The stream itself still
        // expands well past the compressed-byte ratio budget.
        bytes[central + 24..central + 28].copy_from_slice(&1u32.to_le_bytes());
        let limits = ContainerLimits {
            max_archive_bytes: bytes.len() as u64,
            max_entries: 1,
            max_entry_bytes: payload.len() as u64,
            max_total_uncompressed_bytes: payload.len() as u64,
            max_compression_ratio: 1,
        };
        let mut container = Container::open_with_limits(bytes, limits).unwrap();
        assert!(matches!(
            container.read("a"),
            Err(OfdError::ResourceLimit(_))
        ));
        assert_eq!(container.total_read_bytes, compressed + 1);
    }

    #[test]
    fn rejects_duplicate_and_ambiguous_entry_names() {
        let mut duplicate = archive(&[("unique_a_name", b"one"), ("unique_b_name", b"two")]);
        let mut replacements = 0;
        for offset in 0..=duplicate.len() - "unique_b_name".len() {
            if duplicate[offset..].starts_with(b"unique_b_name") {
                duplicate[offset..offset + "unique_a_name".len()].copy_from_slice(b"unique_a_name");
                replacements += 1;
            }
        }
        assert_eq!(replacements, 2, "local and central ZIP names");
        let raw = zip::ZipArchive::new(Cursor::new(duplicate.clone())).unwrap();
        assert_eq!(raw.len(), 1, "zip crate hides duplicate names");
        assert!(matches!(
            Container::open(duplicate),
            Err(OfdError::Malformed(_))
        ));

        let bytes = archive(&[("A.xml", b"upper"), ("a.xml", b"lower")]);
        let mut container = Container::open(bytes).unwrap();
        assert_eq!(container.read_normalized("A.xml").unwrap(), b"upper");
        assert!(matches!(
            container.read_normalized("a.XML"),
            Err(OfdError::Malformed(_))
        ));
    }

    #[test]
    fn normalized_lookup_accepts_legacy_case_and_slashes() {
        let bytes = archive(&[("Doc_0/Pages/Page_0.xml", b"page")]);
        let mut container = Container::open(bytes).unwrap();
        assert_eq!(
            container
                .read_normalized("/doc_0\\pages\\page_0.XML")
                .unwrap(),
            b"page"
        );
        assert_eq!(
            container.take_compatibility_path_warnings(),
            ["non-standard ST_Loc \"doc_0\\\\pages\\\\page_0.XML\" resolved as ZIP entry \"Doc_0/Pages/Page_0.xml\"; paths are case-sensitive and use '/' separators"]
        );

        let bytes = archive(&[("Legacy\\Page.xml", b"page")]);
        let mut container = Container::open(bytes).unwrap();
        assert_eq!(
            container.read_normalized("Legacy\\Page.xml").unwrap(),
            b"page"
        );
        assert_eq!(container.take_compatibility_path_warnings().len(), 1);
    }

    #[test]
    fn enforces_raw_entry_limit_before_opening_archive() {
        let bytes = archive(&[("a", b"one"), ("b", b"two")]);
        let limits = ContainerLimits {
            max_archive_bytes: bytes.len() as u64,
            max_entries: 1,
            ..ContainerLimits::default()
        };
        assert!(matches!(
            Container::open_with_limits(bytes, limits),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn inspects_zip64_central_directory_counts() {
        let zip32 = archive(&[("a", b"one"), ("b", b"two")]);
        let eocd_offset = zip32.len() - CENTRAL_DIRECTORY_END_LEN;
        let eocd = &zip32[eocd_offset..];
        let central_size = read_u32(eocd, 12).unwrap();
        let central_offset = read_u32(eocd, 16).unwrap();

        let mut zip64 = zip32[..eocd_offset].to_vec();
        zip64.extend_from_slice(ZIP64_CENTRAL_DIRECTORY_END);
        zip64.extend_from_slice(&44u64.to_le_bytes());
        zip64.extend_from_slice(&45u16.to_le_bytes());
        zip64.extend_from_slice(&45u16.to_le_bytes());
        zip64.extend_from_slice(&0u32.to_le_bytes());
        zip64.extend_from_slice(&0u32.to_le_bytes());
        zip64.extend_from_slice(&2u64.to_le_bytes());
        zip64.extend_from_slice(&2u64.to_le_bytes());
        zip64.extend_from_slice(&(central_size as u64).to_le_bytes());
        zip64.extend_from_slice(&(central_offset as u64).to_le_bytes());
        zip64.extend_from_slice(ZIP64_CENTRAL_DIRECTORY_LOCATOR);
        zip64.extend_from_slice(&0u32.to_le_bytes());
        zip64.extend_from_slice(&(eocd_offset as u64).to_le_bytes());
        zip64.extend_from_slice(&1u32.to_le_bytes());

        let mut zip64_eocd = eocd.to_vec();
        zip64_eocd[8..12].fill(0xff);
        zip64_eocd[16..20].fill(0xff);
        zip64.extend_from_slice(&zip64_eocd);

        assert_eq!(inspect_central_directory(&zip64, 2).unwrap(), 2);
        assert!(Container::open(zip64.clone()).is_ok());
        assert!(matches!(
            inspect_central_directory(&zip64, 1),
            Err(OfdError::ResourceLimit(_))
        ));
    }
}
