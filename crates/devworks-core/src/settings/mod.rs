//! ConfigurationService (DW-4): typed, scoped configuration with explicit origin/effective
//! values, revision guards, staged agent proposals and a toolkit-independent view model.
//!
//! * [`catalog`] — every accepted key with category, label, Help ID, scopes, value schema,
//!   default, adoption boundary and harness-enforced policies. Unknown keys fail closed.
//! * [`validate`] — pure value validation shared by every surface (including credential and
//!   directory checks).
//! * [`model`] — stored revision bodies, Workspace → Project inheritance, source chains and the
//!   complete-revision plan.
//! * [`service`] — [`service::SettingsService`], the gateway handler for the four `settings.*`
//!   operations, and the transaction-scoped proposal/rollback/adoption functions.
//! * [`view`] — the Settings view model the native shell renders.
pub mod catalog;
pub mod model;
pub mod service;
pub mod validate;
pub mod view;
