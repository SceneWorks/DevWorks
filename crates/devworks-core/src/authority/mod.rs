//! Tool authority and trust boundaries (SEC-01..05): host-created authority, role/tool/profile
//! restrictions, scoped grants and revocation, the destructive-operation guard, verified native
//! reads, the dedicated primary-repository deletion and secret redaction.
//!
//! # Boundary
//!
//! * **Authority comes from host records only.** [`HostAuthority`] is the production
//!   [`crate::ports::AuthorityPort`] the gateway calls for every UI, agent tool, schedule,
//!   package and MCP request. It resolves the host-registered principal, its
//!   [`crate::ports::storage::PrincipalBinding`] (role, project, selected tools, revocation),
//!   the effective access policy from settings and any host-minted grant the invocation links.
//!   Text claiming to be the user, an actor ID in JSON, a package declaring Full access or a
//!   copied grant ID cannot mint authority: request bodies are never read for authority, and a
//!   grant is usable only by its recorded holder within its recorded binding.
//! * **Mandatory restrictions hold under Full access.** Code reviewers are read-only and limited
//!   to verified reads; main is never force-pushed; explicit approvals stay required; the
//!   destructive guard always runs; primary repository deletion is only the user's dedicated
//!   confirmation ([`deletion`]). A denial dominates.
//! * **Unsupported profiles are denied.** An unknown stored access profile, an agent without a
//!   role, a role on a non-agent, or native execution under Project scope without a reviewed
//!   native grant (no confined launcher is qualified) are refused, never relaxed to Full.
//! * **Revocation races.** Every authorization carries [`crate::ports::AuthorityFence`]s and
//!   grant uses at the revisions it resolved. The command unit of work re-checks the fences in
//!   its transaction and spends grants by compare-and-set; a read re-checks them on a fresh
//!   snapshot before publication. A change that commits first fences the request.
//!
//! # Honest limits
//!
//! Full access means ordinary desktop-user rights, not an OS sandbox; a worktree is a place to
//! write, not isolation. Harness checks inspect known destructive intent on supported paths
//! (typed filesystem, worktree cleanup, recognized shell syntax, declared MCP/package effects);
//! they do not intercept arbitrary native code, and no platform confinement is claimed.
pub mod deletion;
pub mod destructive;
pub mod disclosure;
pub mod policy;
pub mod reads;
pub mod redact;
pub mod service;
pub mod shell;

pub use policy::{AccessProfile, DenialReason, EffectivePolicy, Verdict};
pub use service::{
    AdminError, Denied, EffectivePermissions, GrantBinding, HostAuthority, HostRoles,
    authorization_holds, fences_hold,
};
