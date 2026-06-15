//! OFD container access. An `.ofd` file is a ZIP archive; this module reads
//! entries by name as raw bytes. It performs no XML parsing and holds no model
//! state, so it stays trivially portable to wasm (bytes in, bytes out).

use std::io::{Cursor, Read};

use crate::error::{OfdError, Result};

/// A read-only view over the entries of an OFD ZIP container.
pub struct Container {
    archive: zip::ZipArchive<Cursor<Vec<u8>>>,
}

impl Container {
    /// Open a container from the full file bytes.
    pub fn open(bytes: Vec<u8>) -> Result<Self> {
        let archive = zip::ZipArchive::new(Cursor::new(bytes))?;
        Ok(Self { archive })
    }

    /// Read a single entry by its archive path (e.g. `"OFD.xml"`).
    ///
    /// OFD paths in XML are often absolute (`/Doc_0/Document.xml`); callers
    /// should strip the leading slash before calling, or use
    /// [`Container::read_normalized`].
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>> {
        let mut file = self
            .archive
            .by_name(name)
            .map_err(|_| OfdError::MissingEntry(name.to_string()))?;
        let mut buf = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Read an entry, tolerating a leading `/` and case differences in the
    /// path separators that some producers emit.
    pub fn read_normalized(&mut self, name: &str) -> Result<Vec<u8>> {
        let trimmed = name.trim_start_matches('/');
        if let Ok(bytes) = self.read(trimmed) {
            return Ok(bytes);
        }
        // Fall back to a case-insensitive scan over entry names.
        let target = trimmed.replace('\\', "/").to_ascii_lowercase();
        let names: Vec<String> = self.entry_names();
        for entry in names {
            if entry.replace('\\', "/").to_ascii_lowercase() == target {
                return self.read(&entry);
            }
        }
        Err(OfdError::MissingEntry(name.to_string()))
    }

    /// List all entry names in the container.
    pub fn entry_names(&self) -> Vec<String> {
        (0..self.archive.len())
            .filter_map(|i| self.archive.name_for_index(i).map(|s| s.to_string()))
            .collect()
    }
}
