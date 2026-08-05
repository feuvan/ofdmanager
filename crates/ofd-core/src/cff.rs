//! Support for **bare CFF** embedded fonts.
//!
//! OFD producers sometimes embed a raw CFF table (a "bare" CFF font, header
//! `01 00 ..`) instead of a TTF or a CFF wrapped in an OpenType (`OTTO`)
//! container. `ttf-parser` (like most engines) only reads `sfnt`-wrapped fonts,
//! so we synthesise a minimal `OTTO` wrapper around the CFF — providing the
//! `head`/`hhea`/`maxp`/`hmtx` tables it requires — letting the document's own
//! font be used instead of a substitute.
//!
//! Bare CFF fonts are often **CID-keyed**: a glyph is addressed by a CID, and a
//! `charset` maps GID → CID. OFD `CGTransform` records glyph ids as **CIDs**,
//! but `ttf-parser` outlines by **GID**, so we also parse the charset and return
//! a CID → GID map the renderer applies before outlining.

use std::collections::HashMap;

/// A font prepared for `ttf-parser`, with an optional CID → GID map.
pub struct PreparedFont {
    /// `sfnt`-wrapped font bytes that `ttf-parser` can read.
    pub data: Vec<u8>,
    /// For CID-keyed CFFs: maps a CID (as used by `CGTransform`) to a GID.
    pub cid_to_gid: Option<HashMap<u16, u16>>,
}

/// True if `data` looks like a bare CFF (not an `sfnt`-wrapped font).
pub fn is_bare_cff(data: &[u8]) -> bool {
    // CFF header: major(=1) minor hdrSize(>=4) offSize(1..=4).
    data.len() > 4 && data[0] == 1 && (1..=4).contains(&data[2]) && (1..=4).contains(&data[3])
}

/// Return a font usable by `ttf-parser`: the input as-is when it already parses,
/// a synthesised `OTTO` wrapper when it is a bare CFF, else `None`.
pub fn usable_font(data: &[u8]) -> Option<PreparedFont> {
    if let Ok(face) = ttf_parser::Face::parse(data, 0) {
        // A producer may embed an already sfnt-wrapped CID-keyed CFF. Its
        // CGTransform values are still CIDs, so inspect the inner CFF table just
        // as we do for a bare CFF instead of returning early without the map.
        let cid_to_gid = face
            .raw_face()
            .table(ttf_parser::Tag::from_bytes(b"CFF "))
            .and_then(cid_to_gid);
        return Some(PreparedFont {
            data: data.to_vec(),
            cid_to_gid,
        });
    }
    if is_bare_cff(data) {
        if let Some(wrapped) = wrap_bare_cff(data) {
            if ttf_parser::Face::parse(&wrapped, 0).is_ok() {
                return Some(PreparedFont {
                    data: wrapped,
                    cid_to_gid: cid_to_gid(data),
                });
            }
        }
    }
    None
}

/// Wrap a bare CFF table in a minimal `OTTO` sfnt.
fn wrap_bare_cff(cff: &[u8]) -> Option<Vec<u8>> {
    let top = top_dict(cff)?;
    let (num_glyphs, upm) = metrics(cff, &top)?;
    let tables: [(&[u8; 4], Vec<u8>); 5] = [
        (b"CFF ", cff.to_vec()),
        (b"head", build_head(upm)),
        (b"hhea", build_hhea(upm)),
        (b"hmtx", build_hmtx(upm)),
        (b"maxp", build_maxp(num_glyphs)),
    ];
    Some(assemble_sfnt(0x4F54_544F, &tables)) // 'OTTO'
}

// ---- CFF parsing -----------------------------------------------------------

/// The Top DICT fields we use.
struct TopDict {
    charstrings: usize,
    charset: usize,
    font_matrix: Option<f64>,
    is_cid: bool,
}

fn top_dict(d: &[u8]) -> Option<TopDict> {
    if d.len() < 4 || d[0] != 1 {
        return None;
    }
    let p = d[2] as usize; // skip header
    let (_name, p) = parse_index(d, p)?; // Name INDEX
    let (top_dicts, _p) = parse_index(d, p)?; // Top DICT INDEX
    let (s, e) = *top_dicts.first()?;
    parse_top_dict(&d[s..e])
}

fn metrics(d: &[u8], top: &TopDict) -> Option<(u16, u16)> {
    let (charstrings, _) = parse_index(d, top.charstrings)?;
    let num_glyphs = charstrings.len().min(u16::MAX as usize) as u16;
    let upm = top
        .font_matrix
        .filter(|m| *m > 0.0)
        .map(|m| (1.0 / m).round().clamp(16.0, 16384.0) as u16)
        .unwrap_or(1000);
    Some((num_glyphs, upm))
}

/// For a CID-keyed CFF, parse the charset into a CID → GID map.
fn cid_to_gid(d: &[u8]) -> Option<HashMap<u16, u16>> {
    let top = top_dict(d)?;
    if !top.is_cid || top.charset <= 2 {
        // Not CID-keyed, or a predefined charset (not used by CID fonts here).
        return None;
    }
    let (charstrings, _) = parse_index(d, top.charstrings)?;
    let num_glyphs = charstrings.len().min(u16::MAX as usize) as u16;
    parse_charset(d, top.charset, num_glyphs)
}

/// Parse a CFF charset (CID-keyed: SIDs are CIDs) into a CID → GID map.
fn parse_charset(d: &[u8], off: usize, num_glyphs: u16) -> Option<HashMap<u16, u16>> {
    let format = *d.get(off)?;
    let mut map = HashMap::new();
    map.insert(0u16, 0u16); // GID 0 is .notdef = CID 0
    let mut p = off.checked_add(1)?;
    let mut gid: u16 = 1;
    let be16 = |d: &[u8], at: usize| -> Option<u16> {
        Some(u16::from_be_bytes([
            *d.get(at)?,
            *d.get(at.checked_add(1)?)?,
        ]))
    };
    match format {
        0 => {
            while gid < num_glyphs {
                let cid = be16(d, p)?;
                map.insert(cid, gid);
                p = p.checked_add(2)?;
                gid += 1;
            }
        }
        1 | 2 => {
            while gid < num_glyphs {
                let first = be16(d, p)?;
                p = p.checked_add(2)?;
                let n_left = if format == 1 {
                    let v = *d.get(p)? as u16;
                    p = p.checked_add(1)?;
                    v
                } else {
                    let v = be16(d, p)?;
                    p = p.checked_add(2)?;
                    v
                };
                for k in 0..=n_left {
                    if gid >= num_glyphs {
                        break;
                    }
                    map.insert(first.checked_add(k)?, gid);
                    gid += 1;
                }
            }
        }
        _ => return None,
    }
    Some(map)
}

/// Read a CFF INDEX at `p`; return each entry's byte range and the end offset.
fn parse_index(d: &[u8], p: usize) -> Option<(Vec<(usize, usize)>, usize)> {
    let count_end = p.checked_add(2)?;
    if count_end > d.len() {
        return None;
    }
    let count = u16::from_be_bytes([d[p], d[p + 1]]) as usize;
    if count == 0 {
        return Some((Vec::new(), p + 2));
    }
    let off_size = *d.get(count_end)? as usize;
    if !(1..=4).contains(&off_size) {
        return None;
    }
    let offs_start = p.checked_add(3)?;
    let read_off = |i: usize| -> Option<usize> {
        let at = offs_start.checked_add(i.checked_mul(off_size)?)?;
        let end = at.checked_add(off_size)?;
        let bytes = d.get(at..end)?;
        Some(bytes.iter().fold(0usize, |a, &b| (a << 8) | b as usize))
    };
    let offsets = count.checked_add(1)?.checked_mul(off_size)?;
    let data_base = offs_start.checked_add(offsets)?.checked_sub(1)?; // offsets are 1-based
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let s = data_base.checked_add(read_off(i)?)?;
        let e = data_base.checked_add(read_off(i.checked_add(1)?)?)?;
        if e > d.len() || s > e {
            return None;
        }
        entries.push((s, e));
    }
    let end = data_base.checked_add(read_off(count)?)?;
    if end > d.len() {
        return None;
    }
    Some((entries, end))
}

/// Parse a Top DICT for the CharStrings offset, charset offset, FontMatrix
/// x-scale, and whether it is CID-keyed (`ROS`).
fn parse_top_dict(d: &[u8]) -> Option<TopDict> {
    let mut operands: Vec<f64> = Vec::new();
    let mut charstrings: Option<usize> = None;
    let mut charset = 0usize;
    let mut font_matrix: Option<f64> = None;
    let mut is_cid = false;
    let mut i = 0;
    while i < d.len() {
        let b = d[i];
        match b {
            0..=21 => {
                let op = if b == 12 {
                    i += 1;
                    1200 + *d.get(i)? as u16
                } else {
                    b as u16
                };
                i += 1;
                match op {
                    15 => charset = operands.last().map(|v| *v as usize).unwrap_or(0),
                    17 => charstrings = operands.last().map(|v| *v as usize),
                    1207 => font_matrix = operands.first().copied(),
                    1230 => is_cid = true, // ROS
                    _ => {}
                }
                operands.clear();
            }
            28 => {
                operands.push(i16::from_be_bytes([*d.get(i + 1)?, *d.get(i + 2)?]) as f64);
                i += 3;
            }
            29 => {
                operands.push(i32::from_be_bytes([
                    *d.get(i + 1)?,
                    *d.get(i + 2)?,
                    *d.get(i + 3)?,
                    *d.get(i + 4)?,
                ]) as f64);
                i += 5;
            }
            30 => {
                i += 1;
                let mut s = String::new();
                'real: loop {
                    let byte = *d.get(i)?;
                    i += 1;
                    for nib in [byte >> 4, byte & 0x0f] {
                        match nib {
                            0..=9 => s.push((b'0' + nib) as char),
                            0xa => s.push('.'),
                            0xb => s.push('E'),
                            0xc => s.push_str("E-"),
                            0xe => s.push('-'),
                            0xf => break 'real,
                            _ => {}
                        }
                    }
                }
                operands.push(s.parse().unwrap_or(0.0));
            }
            32..=246 => {
                operands.push(b as f64 - 139.0);
                i += 1;
            }
            247..=250 => {
                operands.push((b as f64 - 247.0) * 256.0 + *d.get(i + 1)? as f64 + 108.0);
                i += 2;
            }
            251..=254 => {
                operands.push(-(b as f64 - 251.0) * 256.0 - *d.get(i + 1)? as f64 - 108.0);
                i += 2;
            }
            _ => i += 1,
        }
    }
    Some(TopDict {
        charstrings: charstrings?,
        charset,
        font_matrix,
        is_cid,
    })
}

// ---- minimal sfnt table synthesis ------------------------------------------

fn build_head(upm: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(54);
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // fontRevision
    t.extend_from_slice(&0u32.to_be_bytes()); // checkSumAdjustment
    t.extend_from_slice(&0x5F0F_3CF5u32.to_be_bytes()); // magicNumber
    t.extend_from_slice(&0u16.to_be_bytes()); // flags
    t.extend_from_slice(&upm.to_be_bytes()); // unitsPerEm
    t.extend_from_slice(&0u64.to_be_bytes()); // created
    t.extend_from_slice(&0u64.to_be_bytes()); // modified
    let ipm = upm as i16;
    for v in [0i16, -(ipm / 4), ipm, ipm] {
        t.extend_from_slice(&v.to_be_bytes()); // xMin yMin xMax yMax
    }
    t.extend_from_slice(&0u16.to_be_bytes()); // macStyle
    t.extend_from_slice(&8u16.to_be_bytes()); // lowestRecPPEM
    t.extend_from_slice(&2i16.to_be_bytes()); // fontDirectionHint
    t.extend_from_slice(&0i16.to_be_bytes()); // indexToLocFormat
    t.extend_from_slice(&0i16.to_be_bytes()); // glyphDataFormat
    t
}

fn build_hhea(upm: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(36);
    let upm_i32 = i32::from(upm);
    let ascender = ((upm_i32 * 4) / 5) as i16;
    let descender = (-(upm_i32 / 5)) as i16;
    t.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // version
    t.extend_from_slice(&ascender.to_be_bytes()); // ascender ~0.8em
    t.extend_from_slice(&descender.to_be_bytes()); // descender ~-0.2em
    t.extend_from_slice(&0i16.to_be_bytes()); // lineGap
    t.extend_from_slice(&upm.to_be_bytes()); // advanceWidthMax
    t.extend_from_slice(&0i16.to_be_bytes()); // minLeftSideBearing
    t.extend_from_slice(&0i16.to_be_bytes()); // minRightSideBearing
    t.extend_from_slice(&(upm as i16).to_be_bytes()); // xMaxExtent
    t.extend_from_slice(&1i16.to_be_bytes()); // caretSlopeRise
    t.extend_from_slice(&0i16.to_be_bytes()); // caretSlopeRun
    t.extend_from_slice(&0i16.to_be_bytes()); // caretOffset
    t.extend_from_slice(&[0u8; 8]); // 4 reserved i16
    t.extend_from_slice(&0i16.to_be_bytes()); // metricDataFormat
    t.extend_from_slice(&1u16.to_be_bytes()); // numberOfHMetrics
    t
}

fn build_hmtx(upm: u16) -> Vec<u8> {
    // numberOfHMetrics = 1: one (advanceWidth, lsb) pair; glyphs reuse it.
    let mut t = Vec::with_capacity(4);
    t.extend_from_slice(&(upm / 2).to_be_bytes()); // advanceWidth
    t.extend_from_slice(&0i16.to_be_bytes()); // lsb
    t
}

fn build_maxp(num_glyphs: u16) -> Vec<u8> {
    let mut t = Vec::with_capacity(6);
    t.extend_from_slice(&0x0000_5000u32.to_be_bytes()); // version 0.5 (CFF)
    t.extend_from_slice(&num_glyphs.to_be_bytes());
    t
}

/// Assemble an sfnt from `(tag, data)` tables (tags must already be sorted).
fn assemble_sfnt(sfnt_version: u32, tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = tables.len() as u16;
    let entry_selector = (15 - n.leading_zeros() as u16).min(15);
    let search_range = (1u16 << entry_selector) * 16;
    let range_shift = n * 16 - search_range;

    let mut out = Vec::new();
    out.extend_from_slice(&sfnt_version.to_be_bytes());
    out.extend_from_slice(&n.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    let mut offset = 12 + tables.len() * 16;
    let mut records = Vec::new();
    let mut bodies = Vec::new();
    for (tag, data) in tables {
        let len = data.len();
        records.extend_from_slice(*tag);
        records.extend_from_slice(&0u32.to_be_bytes()); // checksum (unverified)
        records.extend_from_slice(&(offset as u32).to_be_bytes());
        records.extend_from_slice(&(len as u32).to_be_bytes());
        bodies.push(data.clone());
        offset += (len + 3) & !3;
    }
    out.extend_from_slice(&records);
    for data in bodies {
        out.extend_from_slice(&data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malicious_offsets_are_rejected_without_integer_wraparound() {
        assert!(parse_charset(&[], usize::MAX, 2).is_none());

        let index = [
            0x00, 0x01, 0x04, // count=1, offSize=4
            0x00, 0x00, 0x00, 0x01, // first offset
            0xff, 0xff, 0xff, 0xff, // attacker-controlled end offset
        ];
        assert!(parse_index(&index, 0).is_none());
    }

    #[test]
    fn maximum_supported_upm_does_not_overflow_hhea_metrics() {
        let hhea = build_hhea(16_384);
        assert_eq!(hhea.len(), 36);
        assert_eq!(i16::from_be_bytes([hhea[4], hhea[5]]), 13_107);
        assert_eq!(i16::from_be_bytes([hhea[6], hhea[7]]), -3_276);
    }
}
