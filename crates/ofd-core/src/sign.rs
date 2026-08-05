//! Signature verification (GB/T 33190 §18).
//!
//! An OFD signature protects the document by recording, in `SignedInfo`, the
//! binary digest of every protected file (`References/Reference` →
//! `FileRef` + `CheckValue`). Tampering with any protected file changes its
//! digest and breaks the signature. This module re-computes each file's digest
//! with the declared `CheckMethod` and compares it to `CheckValue`, giving an
//! **integrity** result.
//!
//! The cryptographic **authenticity** layer — verifying the `SignedValue`
//! signature (CMS/SM2 over the signature description, plus certificate
//! validation) — is a separate, heavier concern and is not performed here.

use base64::Engine;
use digest::Digest;

use crate::container::{Container, ContainerLimits};
use crate::error::{OfdError, Result};
use crate::model::{Signature, SignatureType};

/// Per-reference digest check outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestStatus {
    /// File present and its digest matches `CheckValue`.
    Ok,
    /// File present but the digest differs — the file was modified.
    Mismatch,
    /// The protected file is missing from the package.
    FileMissing,
    /// Reading the protected files exceeded the configured container budget.
    ResourceLimit,
    /// The entry exists but ZIP decoding, CRC validation, or I/O failed.
    ReadError,
    /// The declared digest algorithm is not supported here.
    UnsupportedMethod,
    /// `CheckValue` was not valid base64.
    BadCheckValue,
}

/// Result of checking one protected file.
#[derive(Debug, Clone)]
pub struct ReferenceReport {
    pub file_ref: String,
    pub method: String,
    pub status: DigestStatus,
}

/// Integrity result for one signature.
#[derive(Debug, Clone)]
pub struct SignatureReport {
    pub id: String,
    pub sig_type: SignatureType,
    pub provider: Option<String>,
    pub signature_method: Option<String>,
    pub signature_date_time: Option<String>,
    pub references: Vec<ReferenceReport>,
}

impl SignatureReport {
    /// True when every protected file is present and matches its digest.
    pub fn integrity_ok(&self) -> bool {
        !self.references.is_empty() && self.references.iter().all(|r| r.status == DigestStatus::Ok)
    }
}

/// Verify the file-digest integrity of each signature against the package bytes.
pub fn verify(ofd_bytes: Vec<u8>, signatures: &[Signature]) -> Result<Vec<SignatureReport>> {
    verify_with_limits(ofd_bytes, signatures, ContainerLimits::default())
}

/// Verify file digests with caller-selected ZIP/read limits.
///
/// The cumulative read budget is shared by every signature in this call. This
/// prevents a document from multiplying decompression and hashing work by
/// repeating the same protected files across many signatures.
pub fn verify_with_limits(
    ofd_bytes: Vec<u8>,
    signatures: &[Signature],
    limits: ContainerLimits,
) -> Result<Vec<SignatureReport>> {
    let mut container = Container::open_with_limits(ofd_bytes, limits)?;
    Ok(signatures
        .iter()
        .map(|signature| verify_one(&mut container, signature))
        .collect())
}

fn verify_one(c: &mut Container, sig: &Signature) -> SignatureReport {
    let references = sig
        .references
        .iter()
        .map(|r| {
            let status = match c.read_absolute_exact(&r.file_ref) {
                Ok(data) => match digest(&r.check_method, &data) {
                    Some(actual) => match decode_check_value(&r.check_value) {
                        Ok(expected) if expected == actual => DigestStatus::Ok,
                        Ok(_) => DigestStatus::Mismatch,
                        Err(_) => DigestStatus::BadCheckValue,
                    },
                    None => DigestStatus::UnsupportedMethod,
                },
                Err(OfdError::ResourceLimit(_)) => DigestStatus::ResourceLimit,
                Err(OfdError::MissingEntry(_)) => DigestStatus::FileMissing,
                Err(_) => DigestStatus::ReadError,
            };
            ReferenceReport {
                file_ref: r.file_ref.clone(),
                method: method_name(&r.check_method).to_string(),
                status,
            }
        })
        .collect();

    SignatureReport {
        id: sig.id.clone(),
        sig_type: sig.sig_type,
        provider: sig.provider.clone(),
        signature_method: sig.signature_method.clone(),
        signature_date_time: sig.signature_date_time.clone(),
        references,
    }
}

/// XML Schema `base64Binary` permits the four XML formatting whitespace
/// characters anywhere in its lexical representation. `base64` intentionally
/// rejects them, so normalize the value before decoding.
fn decode_check_value(value: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
    let compact: Vec<u8> = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD.decode(compact)
}

/// Compute a file digest by the declared `CheckMethod` (OID or name).
fn digest(method: &str, data: &[u8]) -> Option<Vec<u8>> {
    match resolve_method(method) {
        Method::Sm3 => Some(sm3::Sm3::digest(data).to_vec()),
        Method::Sha1 => Some(sha1::Sha1::digest(data).to_vec()),
        Method::Sha256 => Some(sha2::Sha256::digest(data).to_vec()),
        Method::Md5 => Some(md5::Md5::digest(data).to_vec()),
        Method::Unknown => None,
    }
}

enum Method {
    Sm3,
    Sha1,
    Sha256,
    Md5,
    Unknown,
}

fn resolve_method(method: &str) -> Method {
    let m = method.trim();
    // GM SM3: 1.2.156.10197.1.401 (with id) / .400 (raw).
    if m.eq_ignore_ascii_case("SM3") || m == "1.2.156.10197.1.401" || m == "1.2.156.10197.1.400" {
        Method::Sm3
    } else if m.eq_ignore_ascii_case("SHA1")
        || m.eq_ignore_ascii_case("SHA-1")
        || m == "1.3.14.3.2.26"
    {
        Method::Sha1
    } else if m.eq_ignore_ascii_case("SHA-256")
        || m.eq_ignore_ascii_case("SHA256")
        || m == "2.16.840.1.101.3.4.2.1"
    {
        Method::Sha256
    } else if m.eq_ignore_ascii_case("MD5") || m == "1.2.840.113549.2.5" {
        Method::Md5
    } else {
        Method::Unknown
    }
}

fn method_name(method: &str) -> &'static str {
    match resolve_method(method) {
        Method::Sm3 => "SM3",
        Method::Sha1 => "SHA-1",
        Method::Sha256 => "SHA-256",
        Method::Md5 => "MD5",
        Method::Unknown => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn sm3_known_vector() {
        // SM3("abc") per GB/T 32905.
        let d = digest("SM3", b"abc").unwrap();
        assert_eq!(
            hex(&d),
            "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0"
        );
    }

    #[test]
    fn sha1_known_vector() {
        // SHA-1("abc") from FIPS PUB 180-1; SHA1 is the second digest method
        // explicitly enumerated by GB/T 33190 Signature.xsd.
        let digest = digest("SHA1", b"abc").unwrap();
        assert_eq!(hex(&digest), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(method_name("1.3.14.3.2.26"), "SHA-1");
    }

    #[test]
    fn method_resolution() {
        assert!(matches!(resolve_method("1.2.156.10197.1.401"), Method::Sm3));
        assert!(matches!(resolve_method("SHA1"), Method::Sha1));
        assert!(matches!(resolve_method("MD5"), Method::Md5));
        assert!(matches!(resolve_method("whatever"), Method::Unknown));
    }

    #[test]
    fn malformed_container_is_an_error_not_an_empty_report() {
        assert!(verify(b"not a zip".to_vec(), &[]).is_err());
    }

    #[test]
    fn signatures_share_the_cumulative_read_budget() {
        let bytes = archive(&[("protected", b"12345")]);
        let limits = ContainerLimits {
            max_archive_bytes: bytes.len() as u64,
            max_entries: 1,
            max_entry_bytes: 5,
            max_total_uncompressed_bytes: 5,
            max_compression_ratio: 10_000,
        };
        let signature = Signature {
            id: "sig".into(),
            sig_type: SignatureType::Sign,
            provider: None,
            signature_method: None,
            signature_date_time: None,
            references: vec![crate::model::SignReference {
                file_ref: "/protected".into(),
                check_method: "MD5".into(),
                check_value: String::new(),
            }],
            signed_value: None,
        };

        let reports = verify_with_limits(bytes, &[signature.clone(), signature], limits).unwrap();
        assert_eq!(reports[0].references[0].status, DigestStatus::Mismatch);
        assert_eq!(reports[1].references[0].status, DigestStatus::ResourceLimit);
    }

    #[test]
    fn corrupt_present_entry_is_not_reported_as_missing() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer.start_file("protected", options).unwrap();
        writer.write_all(b"unique protected bytes").unwrap();
        let mut bytes = writer.finish().unwrap().into_inner();
        let data_offset = bytes
            .windows(b"unique protected bytes".len())
            .position(|window| window == b"unique protected bytes")
            .unwrap();
        bytes[data_offset] ^= 0xff;

        let signature = Signature {
            id: "sig".into(),
            sig_type: SignatureType::Sign,
            provider: None,
            signature_method: None,
            signature_date_time: None,
            references: vec![crate::model::SignReference {
                file_ref: "/protected".into(),
                check_method: "MD5".into(),
                check_value: String::new(),
            }],
            signed_value: None,
        };
        let reports = verify(bytes, &[signature]).unwrap();
        assert_eq!(reports[0].references[0].status, DigestStatus::ReadError);
    }

    #[test]
    fn check_value_accepts_xml_schema_base64_whitespace() {
        let bytes = archive(&[("protected", b"abc")]);
        let digest = md5::Md5::digest(b"abc");
        let encoded = base64::engine::general_purpose::STANDARD.encode(digest);
        let signature = Signature {
            id: "sig".into(),
            sig_type: SignatureType::Sign,
            provider: None,
            signature_method: None,
            signature_date_time: None,
            references: vec![crate::model::SignReference {
                file_ref: "/protected".into(),
                check_method: "MD5".into(),
                check_value: format!("  {}\n\t{}\r ", &encoded[..8], &encoded[8..]),
            }],
            signed_value: None,
        };

        let reports = verify(bytes, &[signature]).unwrap();
        assert_eq!(reports[0].references[0].status, DigestStatus::Ok);
    }

    #[test]
    fn file_ref_requires_an_exact_case_sensitive_absolute_path() {
        let bytes = archive(&[("Protected.xml", b"abc")]);
        let check_value =
            base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(b"abc"));
        let report_for = |file_ref: &str| {
            let signature = Signature {
                id: "sig".into(),
                sig_type: SignatureType::Sign,
                provider: None,
                signature_method: None,
                signature_date_time: None,
                references: vec![crate::model::SignReference {
                    file_ref: file_ref.into(),
                    check_method: "MD5".into(),
                    check_value: check_value.clone(),
                }],
                signed_value: None,
            };
            verify(bytes.clone(), &[signature]).unwrap()[0].references[0].status
        };

        assert_eq!(report_for("/Protected.xml"), DigestStatus::Ok);
        assert_eq!(report_for("/protected.xml"), DigestStatus::FileMissing);
        assert_eq!(report_for("Protected.xml"), DigestStatus::ReadError);
        assert_eq!(report_for("/Dir\\Protected.xml"), DigestStatus::ReadError);
    }

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

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
