//! Selected container backend operations and owned agent-work lifecycle (DW-49).
pub mod backend;
mod commands;
mod effects;
pub use effects::EffectOutcome;
pub mod execution;
pub mod inspection;
pub mod intent;
pub mod probe;
pub mod runner;
pub mod selection;
pub mod service;
