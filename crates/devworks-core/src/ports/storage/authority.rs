//! Host authority records: principal bindings and host-minted scoped grants.
//!
//! A non-human principal (agent, package, MCP client, schedule) acts only through an active
//! [`PrincipalBinding`] the host created; revoking it advances its revision, which fences every
//! authorization resolved earlier. [`AuthorityGrant`]s are minted by a human through the trusted
//! local input path and exercised only by their recorded holder within their recorded binding.
//! Consumption is a compare-and-set on the grant revision, so a grant revoked or changed between
//! authorization and commit is never spent. Only `crate::authority` decides who may write these
//! records; this port enforces the record invariants.
use super::StorageError;
use crate::context::PrincipalKind;
use crate::ports::GrantUse;
use crate::protocol::{ActorId, Id, ProjectId};
use crate::settings::catalog::AgentRole;
use serde_json::Value;

/// The host's current authority record for one non-human principal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalBinding {
    pub actor: ActorId,
    pub kind: PrincipalKind,
    /// Required for agents, absent for every other kind.
    pub role: Option<AgentRole>,
    /// Project the principal is confined to; `None` is workspace-wide.
    pub project_id: Option<ProjectId>,
    /// Selected tool families; `None` means every family the role and policy allow.
    pub tool_families: Option<Vec<String>>,
    pub active: bool,
    pub revision: i64,
    pub bound_by: ActorId,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPrincipalBinding {
    pub actor: ActorId,
    pub kind: PrincipalKind,
    pub role: Option<AgentRole>,
    pub project_id: Option<ProjectId>,
    pub tool_families: Option<Vec<String>>,
    pub bound_by: ActorId,
}

/// What a grant authorizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GrantKind {
    /// One selected operation (optionally one exact request) that needs explicit approval or a
    /// host grant for this principal.
    Operation,
    /// A reviewed native execution outside the confined profile: shown as native with user
    /// permissions, never as project-confined.
    NativeExecution,
    /// The dedicated user confirmation of one primary repository deletion. Never satisfied by an
    /// operation approval and never exercised by an agent.
    PrimaryRepositoryDeletion,
}
impl GrantKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operation => "Operation",
            Self::NativeExecution => "NativeExecution",
            Self::PrimaryRepositoryDeletion => "PrimaryRepositoryDeletion",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Operation,
            Self::NativeExecution,
            Self::PrimaryRepositoryDeletion,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GrantState {
    Active,
    /// Every use has been spent.
    Consumed,
    Revoked,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NewAuthorityGrant {
    pub id: Id,
    /// The principal that may exercise the grant.
    pub actor: ActorId,
    pub kind: GrantKind,
    /// The human whose trusted local action minted it.
    pub issued_by: ActorId,
    /// JSON object (see `crate::authority::GrantBinding`).
    pub binding: Value,
    pub expires_at_ms: Option<i64>,
    pub uses: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthorityGrant {
    pub id: Id,
    pub actor: ActorId,
    pub kind: GrantKind,
    pub issued_by: ActorId,
    pub revision: i64,
    pub state: GrantState,
    pub binding: Value,
    pub expires_at_ms: Option<i64>,
    pub uses_remaining: Option<i64>,
    pub created_at_ms: i64,
    pub revoked_by: Option<ActorId>,
}
impl AuthorityGrant {
    /// Whether `actor` may still use it at `now_ms` (state, holder, expiry and remaining uses).
    pub fn usable_by(&self, actor: &ActorId, now_ms: i64) -> bool {
        self.state == GrantState::Active
            && &self.actor == actor
            && self.expires_at_ms.is_none_or(|t| t > now_ms)
            && self.uses_remaining.is_none_or(|u| u > 0)
    }
}

/// A registered primary repository root as recorded by the repository registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredRepository {
    pub id: Id,
    pub location: String,
    pub revision: i64,
}

pub trait AuthorityRead {
    fn principal(&self, actor: &ActorId) -> Result<Option<PrincipalBinding>, StorageError>;
    fn authority_grant(&self, id: &Id) -> Result<Option<AuthorityGrant>, StorageError>;
    /// Every registered primary repository root, in ID order (protected-root registry).
    fn registered_repositories(&self) -> Result<Vec<RegisteredRepository>, StorageError>;
}

pub trait AuthorityLedger: AuthorityRead {
    /// Creates a binding (`expected_revision` `None`, revision 1) or replaces the current one
    /// at `expected_revision` (revision + 1, active again). A stale expectation is an
    /// `Authority` [`StorageError::RevisionConflict`]; creating an existing binding is
    /// [`StorageError::Duplicate`]. The actor must be registered with the same kind.
    fn bind_principal(
        &mut self,
        binding: NewPrincipalBinding,
        expected_revision: Option<i64>,
    ) -> Result<PrincipalBinding, StorageError>;
    /// Marks the binding revoked at `expected_revision` (revision + 1).
    fn revoke_principal(
        &mut self,
        actor: &ActorId,
        expected_revision: i64,
    ) -> Result<PrincipalBinding, StorageError>;
    /// Records a host-minted grant at revision 1.
    fn issue_grant(&mut self, grant: NewAuthorityGrant) -> Result<AuthorityGrant, StorageError>;
    /// Revokes an active grant at `expected_revision` (revision + 1).
    fn revoke_grant(
        &mut self,
        id: &Id,
        expected_revision: i64,
        revoked_by: &ActorId,
    ) -> Result<AuthorityGrant, StorageError>;
    /// Spends one use for `actor` at the revision authority resolved. Inactive, expired,
    /// exhausted or foreign grants are [`StorageError::ScopeDenied`]; a changed revision is an
    /// `Authority` [`StorageError::RevisionConflict`].
    fn consume_grant(
        &mut self,
        grant: &GrantUse,
        actor: &ActorId,
    ) -> Result<AuthorityGrant, StorageError>;
}
