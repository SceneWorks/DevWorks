//! Intent fingerprints: SHA-256 over the RFC 8785 canonical form of a versioned, normalized
//! request representation.
//!
//! The fingerprinted document is
//! `{"contract", "operation", "mode", "schemaRevision", "input"}` where `input` is the typed,
//! re-serialized selected input (optional absence and explicit null are identical, covered hold
//! and tag sets are sorted, user text and ordered arrays are untouched) and `schemaRevision` is the
//! revision the request negotiated (absent means 1, so a request that names revision 1 and one
//! that names none are the same intent). Request keys, query IDs,
//! transport retry counters and deadlines are not part of it; a changed expected revision is.
//! A fingerprint is hashing semantics, never a permission.
use crate::jcs::{self, JcsError};
use crate::protocol::CONTRACT;
use crate::registry::{Mode, OperationDescriptor, SchemaRevision};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::fmt;

/// Lowercase hexadecimal SHA-256 digest of a canonical request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fingerprint(String);
impl Fingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Reconstructs a stored fingerprint; it must be 64 lowercase hex characters.
    pub fn from_hex(hex: &str) -> Option<Self> {
        is_sha256_hex(hex).then(|| Self(hex.to_owned()))
    }
}
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// SHA-256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `s` is exactly `len` lowercase hexadecimal characters. Uppercase is refused: every
/// digest and object ID DevWorks writes is lowercase, and the store refuses any other spelling.
pub fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `s` is a SHA-256 digest in its one canonical spelling: 64 lowercase hex characters.
pub fn is_sha256_hex(s: &str) -> bool {
    is_lower_hex(s, 64)
}

/// SHA-256 (lowercase hex) of the RFC 8785 canonical form of `value`.
pub fn canonical_digest(value: &Value) -> Result<String, JcsError> {
    jcs::canonicalize(value).map(|canonical| sha256_hex(canonical.as_bytes()))
}

/// [`canonical_digest`], except that a value JCS cannot represent (an exact integer outside the
/// I-JSON range) is hashed as its serde_json text, so the exact integer is kept rather than
/// refused. Stored digests of such values were computed this way.
pub fn canonical_digest_or_text(value: &Value) -> String {
    canonical_digest(value).unwrap_or_else(|_| sha256_hex(value.to_string().as_bytes()))
}

/// The versioned document that is canonicalized and hashed for one admitted request.
pub fn document(
    descriptor: &OperationDescriptor,
    mode: Mode,
    revision: SchemaRevision,
    normalized_input: &Value,
) -> Value {
    json!({
        "contract": CONTRACT,
        "operation": descriptor.name,
        "mode": mode.as_str(),
        "schemaRevision": revision.0,
        "input": normalized_input,
    })
}

pub fn compute(
    descriptor: &OperationDescriptor,
    mode: Mode,
    revision: SchemaRevision,
    normalized_input: &Value,
) -> Result<Fingerprint, JcsError> {
    canonical_digest(&document(descriptor, mode, revision, normalized_input)).map(Fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_has_one_spelling_and_one_canonical_form() {
        let digest = sha256_hex(b"x");
        assert!(is_sha256_hex(&digest));
        assert!(!is_sha256_hex(&digest.to_ascii_uppercase()));
        assert!(!is_sha256_hex(&digest[..63]));
        assert!(is_lower_hex(&digest[..40], 40));
        // Key order does not change the canonical digest.
        assert_eq!(
            canonical_digest(&json!({"a": 1, "b": 2})).unwrap(),
            canonical_digest(&json!({"b": 2, "a": 1})).unwrap()
        );
        // An integer JCS cannot represent is refused, or hashed exactly as text.
        let huge = json!({"n": u64::MAX});
        assert!(canonical_digest(&huge).is_err());
        assert_eq!(
            canonical_digest_or_text(&huge),
            sha256_hex(huge.to_string().as_bytes())
        );
    }
}
