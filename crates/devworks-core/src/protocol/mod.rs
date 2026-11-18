//! Versioned wire DTOs for the `1.0-draft.1` application contract.
//!
//! Hand-maintained modules hold the shared scalars, receipts, events, faults and the specialized
//! envelopes adopted from the recorded candidates. [`records`] and [`results`] are generated from
//! `tools/contracts-eval/domain-spec.json` by `tools/core-codegen/generate.py`; do not edit them
//! by hand. Per-operation input types live in [`crate::operations`].
pub mod addons;
pub mod data;
pub mod helpers;
pub mod provider;
pub mod records;
pub mod results;
pub mod rules;
pub mod shared;
pub mod support;

pub use shared::*;
pub use support::{Check, DomainErrorCode, DomainFault, RevisionDimension};

/// The wire spelling of a value that serializes as one string, such as a unit-variant enum.
/// Empty when the value is not a string, so a caller never shows a Rust `Debug` name.
pub fn wire_name<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The only contract revision this build admits. Unknown revisions never grant anything.
pub const CONTRACT: &str = "1.0-draft.1";
