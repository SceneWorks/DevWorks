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
//! * **Authority records are written only by their seams.** A use case sees the authority and
//!   resume-grant ledgers through the gateway's handler view: reads are served, every direct
//!   write is refused. Binding, minting and revoking write through [`service`], which alone holds
//!   the [`AuthorityCapability`], and inside a use case only for the principal of a user- or
//!   host-ingress command: a handler running for an agent, package, MCP client or schedule is
//!   refused whatever context it forges or wrapper it adds. Contexts come only from
//!   [`crate::context::HostIngress`], which needs the store. The deletion confirmation
//!   ([`deletion`]) takes the store and runs its own unit of work, so no handler reaches it.
//!   Grant consumption is the gateway's own step on the unwrapped transaction.
//! * **Typed denials are visible.** A policy refusal carries its [`DenialReason`]: a query fault
//!   names it (`authority:<DenialReason>`), and a denied command records one attributed
//!   `AuthorityDenied` history event keyed by the rejection's correlation ID.
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
/// Proof that a ledger write comes from an authority seam ([`service`], [`deletion`]). Its field
/// is private to this module, so no use case, adapter or package can construct one; a use case
/// reaching the authority or grant ledger through the gateway's handler view can read but every
/// write it attempts directly is refused.
///
/// ```compile_fail
/// // Nothing outside `devworks_core::authority` can mint the capability.
/// let _ = devworks_core::authority::AuthorityCapability { _sealed: () };
/// ```
#[derive(Debug)]
pub struct AuthorityCapability {
    _sealed: (),
}
const CAPABILITY: AuthorityCapability = AuthorityCapability { _sealed: () };

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
