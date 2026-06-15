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

use crate::container::Container;
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
pub fn verify(ofd_bytes: Vec<u8>, signatures: &[Signature]) -> Vec<SignatureReport> {
    let Ok(mut container) = Container::open(ofd_bytes) else {
        return Vec::new();
    };
    signatures
        .iter()
        .map(|s| verify_one(&mut container, s))
        .collect()
}

fn verify_one(c: &mut Container, sig: &Signature) -> SignatureReport {
    let references = sig
        .references
        .iter()
        .map(|r| {
            let status = match c.read_normalized(&r.file_ref) {
                Ok(data) => match digest(&r.check_method, &data) {
                    Some(actual) => match Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        r.check_value.trim(),
                    ) {
                        Ok(expected) if expected == actual => DigestStatus::Ok,
                        Ok(_) => DigestStatus::Mismatch,
                        Err(_) => DigestStatus::BadCheckValue,
                    },
                    None => DigestStatus::UnsupportedMethod,
                },
                Err(_) => DigestStatus::FileMissing,
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

/// Compute a file digest by the declared `CheckMethod` (OID or name).
fn digest(method: &str, data: &[u8]) -> Option<Vec<u8>> {
    match resolve_method(method) {
        Method::Sm3 => Some(sm3::Sm3::digest(data).to_vec()),
        Method::Sha256 => Some(sha2::Sha256::digest(data).to_vec()),
        Method::Md5 => Some(md5::Md5::digest(data).to_vec()),
        Method::Unknown => None,
    }
}

enum Method {
    Sm3,
    Sha256,
    Md5,
    Unknown,
}

fn resolve_method(method: &str) -> Method {
    let m = method.trim();
    // GM SM3: 1.2.156.10197.1.401 (with id) / .400 (raw).
    if m.eq_ignore_ascii_case("SM3") || m == "1.2.156.10197.1.401" || m == "1.2.156.10197.1.400" {
        Method::Sm3
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
        Method::Sha256 => "SHA-256",
        Method::Md5 => "MD5",
        Method::Unknown => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn method_resolution() {
        assert!(matches!(resolve_method("1.2.156.10197.1.401"), Method::Sm3));
        assert!(matches!(resolve_method("MD5"), Method::Md5));
        assert!(matches!(resolve_method("whatever"), Method::Unknown));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
