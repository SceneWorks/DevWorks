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
///
/// Without it a transaction reaches authority records only through reads. The read compiles:
///
/// ```
/// use devworks_core::ports::storage::*;
/// fn read(store: &dyn ApplicationStore, actor: devworks_core::protocol::ActorId) {
///     let _ = run_write(store, WriteOptions::fixture(), move |tx| {
///         tx.authority().principal(&actor).map(|p| p.is_some())
///     });
/// }
/// ```
///
/// and the same transaction cannot write (`E0599`: no such method on the read-only view):
///
/// ```compile_fail,E0599
/// use devworks_core::ports::storage::*;
/// fn forge(store: &dyn ApplicationStore) {
///     let _ = run_write(store, WriteOptions::fixture(), |tx| {
///         // `authority()` is read-only; issuing a grant needs `authority_writes(&capability, ..)`.
///         tx.authority().issue_grant(todo!()).map(|_| ())
///         // The resume-grant ledger is read-only the same way: `tx.grants().issue(..)` is an error.
///     });
/// }
/// ```
#[derive(Debug)]
pub struct AuthorityCapability {
    _sealed: (),
}
impl AuthorityCapability {
    /// A capability for seeding authority records in fixtures. Compiled only with the
    /// `contract-suite` feature, which production crates do not enable.
    #[cfg(feature = "contract-suite")]
    pub fn fixture() -> Self {
        Self { _sealed: () }
    }
}

/// The authority ledger of `tx` with its writes, for fixtures that seed principals and grants on
/// the store's own transaction. Compiled only with the `contract-suite` feature.
#[cfg(feature = "contract-suite")]
pub fn fixture_writes(
    tx: &mut dyn crate::ports::storage::WriteTransaction,
) -> Result<&mut dyn crate::ports::storage::AuthorityLedger, crate::ports::storage::StorageError> {
    let actor = tx.store().host_actor.clone();
    tx.authority_writes(&AuthorityCapability::fixture(), &actor)
}

/// The resume-grant ledger of `tx` with its writes, for fixtures (see [`fixture_writes`]).
#[cfg(feature = "contract-suite")]
pub fn fixture_grants(
    tx: &mut dyn crate::ports::storage::WriteTransaction,
) -> Result<&mut dyn crate::ports::storage::GrantLedger, crate::ports::storage::StorageError> {
    let actor = tx.store().host_actor.clone();
    tx.grant_writes(&AuthorityCapability::fixture(), &actor)
}
const CAPABILITY: AuthorityCapability = AuthorityCapability { _sealed: () };

/// Whether `event` is an authority-history record: a principal, grant or repository-deletion
/// record, by aggregate or by kind. Only [`crate::ports::storage::WriteTransaction::authority_history`]
/// (which needs the [`AuthorityCapability`]) may append these; the history of a plain
/// transaction and the gateway's handler view refuse them, so no raw write can forge one.
pub fn is_authority_history(event: &crate::ports::storage::NewEvent) -> bool {
    use deletion::DELETION_AGGREGATE;
    use service::{GRANT_AGGREGATE, PRINCIPAL_AGGREGATE};
    matches!(
        event.aggregate_kind.as_str(),
        PRINCIPAL_AGGREGATE | GRANT_AGGREGATE | DELETION_AGGREGATE
    ) || matches!(
        event.kind.as_str(),
        "PrincipalAuthorityBound"
            | "PrincipalAuthorityRevoked"
            | "AuthorityGrantIssued"
            | "AuthorityGrantRevoked"
            | "RepositoryDeletionConfirmed"
            | "RepositoryDeletionAuthorized"
            | "RepositoryDeletionCompleted"
            | "RepositoryDeletionPartial"
    )
}

/// The refusal a history log returns for an authority-history record it may not take.
pub fn authority_history_refused() -> crate::ports::storage::StorageError {
    crate::ports::storage::StorageError::InvalidRecord(
        "authority history is recorded only through the authority seams".into(),
    )
}

/// Spends one use of a host grant on the unwrapped store transaction. This is the gateway's own
/// consumption step; it presents the capability here so the gateway never holds one.
pub(crate) fn consume_host_grant(
    tx: &mut dyn crate::ports::storage::WriteTransaction,
    grant: &crate::ports::GrantUse,
    actor: &crate::protocol::ActorId,
) -> Result<crate::ports::storage::AuthorityGrant, crate::ports::storage::StorageError> {
    tx.authority_writes(&CAPABILITY, actor)?
        .consume_grant(grant, actor)
}

/// Spends one use of a resume grant on the unwrapped store transaction (the gateway's own step).
pub(crate) fn consume_resume_grant(
    tx: &mut dyn crate::ports::storage::WriteTransaction,
    grant: &crate::ports::GrantUse,
    actor: &crate::protocol::ActorId,
) -> Result<crate::ports::storage::GrantRecord, crate::ports::storage::StorageError> {
    tx.grant_writes(&CAPABILITY, actor)?.consume(grant, actor)
}

pub(crate) mod database;
pub mod deletion;
pub mod destructive;
pub mod disclosure;
mod git;
pub mod policy;
pub mod reach;
pub mod reads;
pub mod redact;
pub mod service;
pub mod shell;

pub use policy::{AccessProfile, DenialReason, EffectivePolicy, Verdict};
pub use service::{
    AdminError, AuthorityFencesOnly, Denied, EffectivePermissions, GrantBinding, HostAuthority,
    HostRoles, ServiceFences, authorization_holds_in_with, authorization_holds_with,
    fences_hold_with, mint_resume_grant, revoke_resume_grant,
};
