//! Test doubles and seeding helpers shared by the DevWorks test trees. A dev-dependency only:
//! no production crate depends on it. It depends on `devworks-core` alone, never on the adapters,
//! so a test inside the adapters crate still sees one copy of every core type.
//!
//! - [`TestClock`], [`FixedClock`], [`TestIds`] and [`TempRoot`] replace the per-file clocks,
//!   identifier sources and temporary-directory guards.
//! - [`FaultFiles`] decorates any `ArtifactFiles` with failure, gating and observation hooks.
//! - [`LoseAcks`] decorates any `ApplicationStore` so a commit's acknowledgment is lost.
//! - [`seed`] writes projects and work items through the repository ports.
//! - [`Unmodelled`] answers "not modelled" for every storage repository method; its impls are
//!   generated from the port traits (`tools/core-codegen/stubs.py`).
#![allow(clippy::result_large_err)]

mod acks;
mod clock;
mod files;
pub mod seed;
mod temp;
mod unmodelled;
mod unmodelled_generated;

pub use acks::LoseAcks;
pub use clock::{FixedClock, HookedClock, NumberedIds, T0, TestClock, TestIds};
pub use files::{Call, FaultFiles, FileOp};
pub use temp::TempRoot;
pub use unmodelled::{Unmodelled, unmodelled};
