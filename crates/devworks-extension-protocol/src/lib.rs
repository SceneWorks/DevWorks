//! Public, versioned wire DTOs exchanged between the DevWorks host and executable packages.
//!
//! # Boundary rationale (ARCH-07)
//!
//! Independently installed package executables need a contract that is released and versioned
//! separately from the host's private domain model, so this crate is the only one the future
//! package SDK depends on. It deliberately has no dependency on `devworks-core`, egui, a database
//! driver or a provider SDK. Scalars such as opaque IDs and canonical decimal counters are
//! mirrored in [`scalars`] rather than shared; the host converts these frames into core
//! operations at its boundary and re-applies its own authority, quiescence and journaling.
//!
//! The DTOs and their structural/cross-field checks are adopted unchanged in wire shape from the
//! recorded `1.0-draft.1` architecture candidates (`tools/contracts-eval/src/package.rs` and the
//! codec portion of `provider.rs`). A frame that decodes and validates here is not evidence that
//! the host permits it: capability, token, revision and scope checks remain host obligations.
//!
//! Callers must bound and frame bytes (1 MiB, duplicate JSON members rejected) before typed
//! decoding; the host does this with the same strict framing used by its application gateway.

// Frames mirror the recorded variant shapes; boxing large variants changes only ergonomics.
#![allow(clippy::large_enum_variant)]

pub mod codec;
pub mod package;
pub mod scalars;
