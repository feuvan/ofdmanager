//! Structured decoding of GB/T 38540 / GM/T 0031 **security electronic seals**
//! (SES), referenced by OFD signatures (GB/T 33190 §18) as `Seal.esl` or
//! embedded in the `SignedValue.dat` signature value.
//!
//! We decode the ASN.1 DER structure to navigate to the seal picture rather
//! than scanning for byte markers. The relevant hierarchy (both v1 and v4) is:
//!
//! ```text
//! SES_Signature ::= SEQUENCE { toSign TBSSign, cert, signAlgID, signature, ... }
//! TBSSign       ::= SEQUENCE { version INTEGER, eseal SESeal, ... }
//! SESeal        ::= SEQUENCE { eSealInfo SES_ESealInfo, cert, signAlgID, signedValue }
//! SES_ESealInfo ::= SEQUENCE { header SES_Header, esID, property, picture SES_ESPictureInfo, ... }
//! SES_Header    ::= SEQUENCE { id IA5String, version INTEGER, vid IA5String }
//! SES_ESPictureInfo ::= SEQUENCE { type IA5String, data OCTET STRING, width REAL/INT, height REAL/INT }
//! ```
//!
//! A `.esl` may hold the full `SES_Signature`, a bare `SESeal`, or a bare
//! `SES_ESealInfo`; we locate the `SES_ESealInfo` structurally (validated by its
//! `SES_Header`) wherever it sits and read its picture field.

/// The seal face decoded from a SES structure.
#[derive(Debug, Clone)]
pub struct SealPicture {
    /// Picture type, lowercased (`"ofd"`, `"png"`, `"jpg"`, …).
    pub kind: String,
    /// Raw picture bytes (an OFD package for `ofd`, else a raster image).
    pub data: Vec<u8>,
    /// Declared dimensions (hundredths of a millimetre per the standard), if present.
    pub width: Option<i64>,
    pub height: Option<i64>,
}

// ---- minimal DER reader ----------------------------------------------------

/// One DER TLV: its tag octet and the raw content bytes.
struct Tlv<'a> {
    tag: u8,
    content: &'a [u8],
}

impl<'a> Tlv<'a> {
    fn is_constructed(&self) -> bool {
        self.tag & 0x20 != 0
    }
}

/// Read one TLV from the front of `input`, returning it and the remainder.
fn read_tlv(input: &[u8]) -> Option<(Tlv<'_>, &[u8])> {
    if input.len() < 2 {
        return None;
    }
    let tag = input[0];
    let len_byte = input[1];
    let (len, hdr) = if len_byte < 0x80 {
        (len_byte as usize, 2)
    } else {
        let n = (len_byte & 0x7f) as usize;
        if n == 0 || n > 4 || input.len() < 2 + n {
            return None;
        }
        let mut l = 0usize;
        for i in 0..n {
            l = (l << 8) | input[2 + i] as usize;
        }
        (l, 2 + n)
    };
    let end = hdr.checked_add(len)?;
    if input.len() < end {
        return None;
    }
    Some((
        Tlv {
            tag,
            content: &input[hdr..end],
        },
        &input[end..],
    ))
}

/// Parse all TLVs directly contained in `content`.
fn children(content: &[u8]) -> Vec<Tlv<'_>> {
    const MAX_CHILDREN: usize = 4096;
    let mut out = Vec::new();
    let mut rest = content;
    while out.len() < MAX_CHILDREN {
        let Some((tlv, next)) = read_tlv(rest) else {
            break;
        };
        out.push(tlv);
        if next.len() == rest.len() {
            break; // no progress, avoid loop
        }
        rest = next;
    }
    out
}

// ASN.1 universal tags we care about.
const SEQUENCE: u8 = 0x30;
const OCTET_STRING: u8 = 0x04;
const INTEGER: u8 = 0x02;

fn is_string(tag: u8) -> bool {
    // IA5String / UTF8String / PrintableString (the SES type/id fields).
    matches!(tag, 0x16 | 0x0c | 0x13)
}

fn int_value(tlv: &Tlv) -> Option<i64> {
    if tlv.tag != INTEGER || tlv.content.is_empty() || tlv.content.len() > 8 {
        return None;
    }
    let mut v: i64 = if tlv.content[0] & 0x80 != 0 { -1 } else { 0 };
    for &b in tlv.content {
        v = (v << 8) | b as i64;
    }
    Some(v)
}

// ---- SES navigation --------------------------------------------------------

/// Decode the seal picture from a SES DER blob (a `Seal.esl` or `SignedValue.dat`).
pub fn extract_seal_picture(der: &[u8]) -> Option<SealPicture> {
    find_in(der)
}

/// Depth-first search for the `SES_ESealInfo` and its picture.
fn find_in(content: &[u8]) -> Option<SealPicture> {
    const MAX_DEPTH: usize = 64;
    const MAX_NODES: usize = 1_000_000;

    let mut stack = vec![(content, 0usize)];
    let mut visited = 0usize;
    while let Some((level, depth)) = stack.pop() {
        let mut rest = level;
        while let Some((tlv, next)) = read_tlv(rest) {
            visited += 1;
            if visited > MAX_NODES {
                return None;
            }
            if tlv.is_constructed() {
                if tlv.tag == SEQUENCE {
                    if let Some(pic) = picture_from_eseal_info(&tlv) {
                        return Some(pic);
                    }
                }
                if depth < MAX_DEPTH {
                    stack.push((tlv.content, depth + 1));
                }
            }
            if next.len() == rest.len() {
                break;
            }
            rest = next;
        }
    }
    None
}

/// If `node` is a `SES_ESealInfo` (a SEQUENCE led by a `SES_Header`), read its
/// `SES_ESPictureInfo`.
fn picture_from_eseal_info(node: &Tlv) -> Option<SealPicture> {
    let fields = children(node.content);
    if fields.len() < 4 {
        return None;
    }
    // SES_Header: SEQUENCE { id IA5String, version INTEGER, vid IA5String }.
    let header = &fields[0];
    if header.tag != SEQUENCE {
        return None;
    }
    let hfields = children(header.content);
    if hfields.len() < 3
        || !is_string(hfields[0].tag)
        || hfields[1].tag != INTEGER
        || !is_string(hfields[2].tag)
    {
        return None;
    }
    // The picture is field index 3 by the standard; fall back to the first
    // field that has the SES_ESPictureInfo shape.
    let pic = fields
        .get(3)
        .filter(|t| is_picture(t))
        .or_else(|| fields.iter().find(|t| is_picture(t)))?;
    read_picture(pic)
}

/// A `SES_ESPictureInfo`: SEQUENCE { IA5String type, OCTET STRING data, … }.
fn is_picture(node: &Tlv) -> bool {
    if node.tag != SEQUENCE {
        return false;
    }
    let c = children(node.content);
    c.len() >= 2 && is_string(c[0].tag) && c[1].tag == OCTET_STRING
}

fn read_picture(node: &Tlv) -> Option<SealPicture> {
    let c = children(node.content);
    let kind = std::str::from_utf8(c[0].content)
        .ok()?
        .trim()
        .to_ascii_lowercase();
    let data = c[1].content.to_vec();
    Some(SealPicture {
        kind,
        data,
        width: c.get(2).and_then(int_value),
        height: c.get(3).and_then(int_value),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut v = vec![tag];
        if content.len() < 128 {
            v.push(content.len() as u8);
        } else {
            v.push(0x82);
            v.push((content.len() >> 8) as u8);
            v.push(content.len() as u8);
        }
        v.extend_from_slice(content);
        v
    }
    fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
        tlv(SEQUENCE, &parts.concat())
    }
    fn ia5(s: &str) -> Vec<u8> {
        tlv(0x16, s.as_bytes())
    }
    fn int(n: u8) -> Vec<u8> {
        tlv(INTEGER, &[n])
    }
    fn oct(d: &[u8]) -> Vec<u8> {
        tlv(OCTET_STRING, d)
    }

    fn eseal_info(pic_type: &str, data: &[u8]) -> Vec<u8> {
        let header = seq(&[ia5("ES"), int(4), ia5("VID")]);
        let property = seq(&[int(1)]);
        let picture = seq(&[ia5(pic_type), oct(data), int(30), int(20)]);
        seq(&[header, ia5("esid"), property, picture])
    }

    #[test]
    fn decodes_bare_eseal_info() {
        let der = eseal_info("png", b"PNGDATA");
        let p = extract_seal_picture(&der).expect("picture");
        assert_eq!(p.kind, "png");
        assert_eq!(p.data, b"PNGDATA");
        assert_eq!((p.width, p.height), (Some(30), Some(20)));
    }

    #[test]
    fn decodes_picture_nested_in_signature() {
        // SES_Signature { toSign SEQ { version, eseal SEQ { eSealInfo, … } }, … }
        let eseal = seq(&[
            eseal_info("ofd", b"PK\x03\x04zip"),
            oct(b"c"),
            oct(b"a"),
            oct(b"s"),
        ]);
        let tbs = seq(&[int(1), eseal]);
        let signature = seq(&[tbs, oct(b"cert"), oct(b"alg"), oct(b"sig")]);
        let p = extract_seal_picture(&signature).expect("picture");
        assert_eq!(p.kind, "ofd");
        assert_eq!(p.data, b"PK\x03\x04zip");
    }

    #[test]
    fn ignores_unrelated_der() {
        let der = seq(&[int(1), oct(b"hello"), ia5("world")]);
        assert!(extract_seal_picture(&der).is_none());
    }

    #[test]
    fn rejects_picture_shape_with_an_incomplete_ses_header() {
        let incomplete_header = seq(&[ia5("ES")]);
        let picture = seq(&[ia5("png"), oct(b"PNGDATA"), int(30), int(20)]);
        let lookalike = seq(&[incomplete_header, ia5("esid"), seq(&[int(1)]), picture]);
        assert!(extract_seal_picture(&lookalike).is_none());
    }
}
