//! Configuration repository: scoped, revisioned configuration documents and the staged
//! proposals that agents may author but never apply.
//!
//! A scope's configuration in one namespace is an append-only chain of **complete** revisions.
//! Revision 1 of every scope is its implicit initial state (nothing configured; nothing is
//! stored for it), so the first saved revision is 2. [`ConfigurationRepository::append_revision`] is a
//! compare-and-set on the scope's current revision: a stale expectation is a
//! [`StorageError::RevisionConflict`] in the `Configuration` dimension and nothing is written.
//! Earlier revisions are never rewritten, which is what makes rollback and "instructions used
//! for completed work" exact.
//!
//! Proposals are separate records bound to the revision they were drafted against. Only the
//! settings use case decides who may stage, reconcile, approve or reject them; this port only
//! enforces the record invariants (pending-only transitions, expected proposal revision).
use super::StorageError;
use crate::protocol::{ActorId, Id, ProjectId};
use serde_json::Value;

/// Where a configuration document applies. Inheritance runs Workspace → Project.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConfigScope {
    Workspace,
    Project(ProjectId),
}
impl From<&crate::protocol::data::DataScope> for ConfigScope {
    fn from(scope: &crate::protocol::data::DataScope) -> Self {
        match scope {
            crate::protocol::data::DataScope::Workspace => Self::Workspace,
            crate::protocol::data::DataScope::Project { project_id } => {
                Self::Project(project_id.clone())
            }
        }
    }
}
impl From<&ConfigScope> for crate::protocol::data::DataScope {
    fn from(scope: &ConfigScope) -> Self {
        match scope {
            ConfigScope::Workspace => Self::Workspace,
            ConfigScope::Project(project_id) => Self::Project {
                project_id: project_id.clone(),
            },
        }
    }
}
impl ConfigScope {
    /// Stable storage key: `workspace` or `project:<id>`. Opaque IDs cannot contain `:`.
    pub fn key(&self) -> String {
        match self {
            Self::Workspace => "workspace".into(),
            Self::Project(p) => format!("project:{}", p.0.as_str()),
        }
    }
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "workspace" => Some(Self::Workspace),
            _ => key
                .strip_prefix("project:")
                .and_then(|id| Id::new(id).ok())
                .map(|id| Self::Project(ProjectId(id))),
        }
    }
    /// Identifier used to address the scope in faults (`workspace` or the project ID).
    pub fn target_id(&self) -> Id {
        match self {
            Self::Workspace => Id::new("workspace").expect("static id"),
            Self::Project(p) => p.0.clone(),
        }
    }
    pub fn project_id(&self) -> Option<&ProjectId> {
        match self {
            Self::Workspace => None,
            Self::Project(p) => Some(p),
        }
    }
}

/// One stored complete configuration revision.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigRevision {
    pub scope: ConfigScope,
    pub namespace: String,
    /// Positive; 2 or greater for a stored revision.
    pub revision: i64,
    /// Version of the namespace's body schema.
    pub schema_version: i64,
    /// Versioned JSON object owned by the namespace's service.
    pub body: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProposalState {
    Pending,
    Approved,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct NewProposal {
    pub id: Id,
    pub scope: ConfigScope,
    pub namespace: String,
    /// The scope revision the proposal was drafted against.
    pub base_revision: i64,
    pub author: ActorId,
    /// Versioned JSON object (proposed entries, digest, provenance).
    pub body: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProposalRecord {
    pub id: Id,
    pub scope: ConfigScope,
    pub namespace: String,
    pub base_revision: i64,
    /// Revision of the proposal record itself (1 when staged, +1 per change).
    pub revision: i64,
    pub state: ProposalState,
    pub author: ActorId,
    pub created_at_ms: i64,
    pub decided_by: Option<ActorId>,
    pub decided_at_ms: Option<i64>,
    /// The configuration revision an approval produced.
    pub applied_revision: Option<i64>,
    pub body: Value,
}

/// A change to a pending proposal.
#[derive(Clone, Debug)]
pub enum ProposalChange {
    /// Rebind the (possibly edited) proposal to a newer base revision.
    Reconcile { base_revision: i64, body: Value },
    /// Terminal decision. `applied_revision` is required for `Approved` and absent otherwise.
    Decide {
        state: ProposalState,
        decided_by: ActorId,
        applied_revision: Option<i64>,
    },
}

/// Durable custody of a settings credential handle. Values never enter application storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSecretState {
    Writing,
    Ready,
    Retained,
    Deleting,
    Deleted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSettingsSecret {
    pub handle: Id,
    pub state: SettingsSecretState,
    pub expires_at_ms: i64,
}

pub trait ConfigurationRead {
    fn settings_secret(&self, handle: &Id) -> Result<Option<StoredSettingsSecret>, StorageError>;
    /// All retained settings revisions and proposals, across scopes, including rollback targets.
    fn settings_secret_referenced(&self, handle: &Id) -> Result<bool, StorageError>;

    /// Whether the scope's owner exists (always true for the workspace).
    fn scope_exists(&self, scope: &ConfigScope) -> Result<bool, StorageError>;
    /// The scope's current stored revision; `None` means it is still at implicit revision 1.
    fn current(
        &self,
        scope: &ConfigScope,
        namespace: &str,
    ) -> Result<Option<ConfigRevision>, StorageError>;
    fn revision(
        &self,
        scope: &ConfigScope,
        namespace: &str,
        revision: i64,
    ) -> Result<Option<ConfigRevision>, StorageError>;
    /// Stored revisions newest first, strictly below `before` when given, at most `limit`
    /// (1..=500).
    fn history(
        &self,
        scope: &ConfigScope,
        namespace: &str,
        before: Option<i64>,
        limit: u16,
    ) -> Result<Vec<ConfigRevision>, StorageError>;
    fn proposal(&self, id: &Id) -> Result<Option<ProposalRecord>, StorageError>;
    /// Proposals of one scope/namespace in stable ID order after `after`, at most `limit`
    /// (1..=500), optionally only in `state`.
    fn proposals(
        &self,
        scope: &ConfigScope,
        namespace: &str,
        state: Option<ProposalState>,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<ProposalRecord>, StorageError>;
}

pub trait ConfigurationRepository: ConfigurationRead {
    /// Reserve a fresh handle before its external write; duplicate identifiers are refused.
    fn reserve_settings_secret(
        &mut self,
        handle: &Id,
        expires_at_ms: i64,
    ) -> Result<(), StorageError>;
    /// Publish only a live Writing stage. False means expiry/retirement won the race.
    fn publish_settings_secret(&mut self, handle: &Id) -> Result<bool, StorageError>;
    /// Same writer unit as introducing the reference. Refuse Writing, expired Ready or retired handles.
    /// Untracked legacy references are registered as Retained.
    fn retain_settings_secret(&mut self, handle: &Id) -> Result<(), StorageError>;
    /// Fresh reference check and durable retirement before any external deletion. A legacy orphan
    /// first receives legacy_expires_at_ms; abandon is only for an unpublished failed intake.
    fn retire_settings_secret(
        &mut self,
        handle: &Id,
        legacy_expires_at_ms: i64,
        abandon: bool,
    ) -> Result<bool, StorageError>;
    fn finish_settings_secret_deletion(&mut self, handle: &Id) -> Result<(), StorageError>;

    /// Appends revision `expected + 1` when the scope is currently at `expected` (1 when nothing
    /// is stored) and makes it current. A different current revision is a `Configuration`
    /// [`StorageError::RevisionConflict`]; an unknown project scope is [`StorageError::NotFound`].
    fn append_revision(
        &mut self,
        scope: &ConfigScope,
        namespace: &str,
        expected: i64,
        schema_version: i64,
        body: Value,
    ) -> Result<ConfigRevision, StorageError>;
    /// Records a new `Pending` proposal at proposal revision 1.
    fn stage(&mut self, proposal: NewProposal) -> Result<ProposalRecord, StorageError>;
    /// Changes a `Pending` proposal at `expected_revision`. A different revision is a
    /// `Configuration` [`StorageError::RevisionConflict`]; a decided proposal is
    /// [`StorageError::InvalidRecord`].
    fn update_proposal(
        &mut self,
        id: &Id,
        expected_revision: i64,
        change: ProposalChange,
    ) -> Result<ProposalRecord, StorageError>;
}
