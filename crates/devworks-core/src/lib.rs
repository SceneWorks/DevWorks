//! DevWorks core: versioned domain contracts, the operation registry and the shared admission
//! gateway used by every entry point (UI, agent tools, schedules, package host, MCP).
//!
//! # Boundary rationale (ARCH-07, ARCH-08)
//!
//! * **Dependencies point inward.** Core depends only on serde, serde_json, schemars and sha2.
//!   It has no UI, database-driver, HTTP or async-runtime dependency; persistence, authority,
//!   clocks and identifiers are reached through the object-safe `Send + Sync` traits in [`ports`].
//!   `devworks-adapters::storage::sqlite` implements [`ports::storage`]; [`authority::HostAuthority`]
//!   is the host authority port over those records; tests replace both with deterministic
//!   substitutes. There is no global mutable state.
//! * **One admission path (ARCH-02).** [`gateway::Gateway`] is the only way to reach a use case:
//!   handlers receive [`gateway::AdmittedQuery`]/[`gateway::AdmittedCommand`], which only the
//!   gateway can construct after strict framing, version, selected-operation/mode, typed decode,
//!   cross-field rules, fingerprinting, authority and receipt checks. A UI action and an agent
//!   tool therefore share validation, revision checks, attribution and receipts by construction.
//!   A command's receipt lookup, use-case decision, history, outbox and receipt commit run as one
//!   unit of work on the storage write owner ([`ports::storage::WriteTransaction`]).
//! * **Selection by the host, not the body.** The operation registry ([`registry`]) is keyed by
//!   the operation the host adapter selected. A body is decoded directly into that operation's
//!   input type; its discriminator can only confirm the selection. The probe's single large
//!   request union is not reproduced, so one operation's payload can never authorize another,
//!   and inputs shared by several operations (disclosed aliases) keep separate entitlements.
//! * **Per-family modules.** [`operations`] has one module per catalog family with that family's
//!   typed inputs; shared domain records live in [`protocol::records`]. Both, the result union
//!   and the registry are generated from the recorded `domain-spec.json`/`family-coverage.json`
//!   by `tools/core-codegen/generate.py` so the 353 bindings cannot drift from the register.
//! * **Pure where possible.** [`admission`] is pure; [`framing`], [`jcs`] and [`fingerprint`]
//!   are deterministic functions. The gateway sequences ports but holds no state.
//!
//! What core does not claim: a valid DTO is never evidence that live relationship, authority,
//! descriptor-schema, clock, lease or ownership checks passed; those belong to the owning use
//! cases and host ports. Fixture tests here are contract qualification, not product acceptance.

// Wire DTOs mirror the recorded variant shapes; boxing large variants would change only Rust
// ergonomics, not the contract, and none of these values sit on a hot path.
#![allow(clippy::large_enum_variant, clippy::result_large_err)]

pub mod admission;
pub mod authority;
pub mod context;
pub mod fingerprint;
pub mod framing;
pub mod gateway;
pub mod jcs;
pub mod operations;
pub mod ports;
pub mod protocol;
pub mod registry;
pub mod services;
pub mod settings;
