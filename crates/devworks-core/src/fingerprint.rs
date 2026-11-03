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
        (hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| Self(hex.to_owned()))
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
    let canonical = jcs::canonicalize(&document(descriptor, mode, revision, normalized_input))?;
    Ok(Fingerprint(sha256_hex(canonical.as_bytes())))
}
