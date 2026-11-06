//! Planned work records (DW-012): features, task cards (a standalone task is an issue: one
//! durable identity), their repository context, project-wide blocking links, queue revisions,
//! the project workflow statuses, the feature-owned plan slot, plan-approval requests and the
//! feature's execution-readiness assessment.
//!
//! The repository enforces record invariants (kinds, compare-and-set revisions, one parent kind,
//! composite project references, bounded listings, folded search keys written by the caller).
//! Visibility, authority, plan approval, dependency acyclicity and everything a command may
//! change belong to `crate::services::work`, which writes inside one unit of work on the sole
//! write owner: a graph read inside that unit of work cannot be raced by another link write.
//!
//! Work items are never deleted as rows. Removing work from a view (a re-plan closing a task)
//! changes its workflow status; history and links stay.
use super::StorageError;
use crate::protocol::{ActorId, Id, ProjectId, RootId};
use serde_json::Value;

/// The stored kind of a feature.
pub const FEATURE: &str = "feature";
/// The stored kind of every planning card. Rows written as `issue` (earlier fixtures) read
/// alike: whether a card is a task or an issue is a view of its feature membership.
pub const TASK: &str = "task";
/// The project queue: features and standalone issues compete in one priority order.
pub const PROJECT_QUEUE: &str = "project";
/// Advances with every ordering change anywhere in the project (list cursors bind it).
pub const ANY_QUEUE: &str = "*";
/// The project's workflow (its status set) revision.
pub const WORKFLOW_QUEUE: &str = "workflow";
/// The task queue of one feature.
pub fn feature_queue(feature: &Id) -> String {
    format!("feature:{}", feature.as_str())
}

/// How a card reached its completion: a human Done transition without verified acceptance
/// evidence, or every acceptance criterion met with evidence.
pub const MANUAL: &str = "Manual";
pub const VERIFIED: &str = "Verified";

/// One work item.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredWork {
    pub id: Id,
    pub project_id: ProjectId,
    /// [`FEATURE`] or [`TASK`] (`issue` reads as a task).
    pub kind: String,
    /// The one authoritative optional feature membership of a card.
    pub parent_feature_id: Option<Id>,
    pub status_id: Option<Id>,
    pub title: String,
    pub priority: i64,
    /// Explicit rank within its queue (ascending).
    pub manual_order: i64,
    pub revision: i64,
    pub scope_revision: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub created_by: Option<ActorId>,
    pub updated_by: Option<ActorId>,
    /// Title and description folded by the service's one case folding (search).
    pub search_key: String,
    /// The caller's plan reference (kept so a re-plan matches by it).
    pub caller_ref: Option<Id>,
    /// [`MANUAL`] or [`VERIFIED`] while the card is in a Done status.
    pub completion: Option<String>,
    /// The service's brief (description, goals, acceptance, links, comments, handoffs, ...).
    pub brief: Value,
}
impl StoredWork {
    pub fn is_feature(&self) -> bool {
        self.kind == FEATURE
    }
    /// The queue this item is ranked in.
    pub fn queue(&self) -> String {
        match &self.parent_feature_id {
            Some(f) if !self.is_feature() => feature_queue(f),
            _ => PROJECT_QUEUE.to_owned(),
        }
    }
}

/// Which items a listing covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkSelection {
    Features,
    /// Cards without a feature (the issue view).
    Standalone,
    /// Every card.
    Tasks,
    /// The cards of one feature.
    FeatureTasks(Id),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkFilter {
    pub project: ProjectId,
    pub selection: WorkSelection,
    /// Any of these statuses (empty: every status).
    pub status_ids: Vec<Id>,
    /// Naming any of these repositories (empty: any).
    pub repository_ids: Vec<Id>,
    /// Substring of the folded search key (already folded by the caller).
    pub text: Option<String>,
}

/// Queue order: priority descending, explicit rank, creation, ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkPosition {
    pub priority: i64,
    pub manual_order: i64,
    pub created_at_ms: i64,
    pub id: Id,
}
impl WorkPosition {
    pub fn of(w: &StoredWork) -> Self {
        Self {
            priority: w.priority,
            manual_order: w.manual_order,
            created_at_ms: w.created_at_ms,
            id: w.id.clone(),
        }
    }
}

/// One workflow status of a project (stable identity, separately configured meaning).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredStatus {
    pub id: Id,
    pub project_id: ProjectId,
    pub name: String,
    /// `Backlog`, `Ready`, `InProgress`, `InReview`, `Done` or `Closed`.
    pub semantic: String,
    pub sort_order: i64,
    pub revision: i64,
}

/// A feature's plan slot: the active approved plan and at most one pending revision. Its
/// revision advances with every change of either.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredFeaturePlan {
    pub feature_id: Id,
    pub revision: i64,
    pub approved: Option<Value>,
    pub approved_by: Option<ActorId>,
    pub approved_at_ms: Option<i64>,
    pub pending: Option<Value>,
    pub pending_revision: Option<i64>,
}

/// The states of a plan-approval request (`ApprovalState` wire names).
pub const PROPOSAL_STATES: [&str; 5] = ["Pending", "Approved", "Rejected", "Superseded", "Expired"];

/// A request for human approval of one exact pending plan revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredProposal {
    pub id: Id,
    pub feature_id: Id,
    pub revision: i64,
    /// The plan slot revision of the pending draft it binds.
    pub plan_revision: i64,
    pub plan_digest: String,
    pub state: String,
    pub requested_by: ActorId,
    pub requested_at_ms: i64,
    pub decided_by: Option<ActorId>,
    pub decided_at_ms: Option<i64>,
    pub summary: String,
}

/// A feature's execution-readiness assessment (the service's JSON body).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredReadiness {
    pub feature_id: Id,
    pub revision: i64,
    pub body: Value,
}

/// Maximum serialized explicit-key allocation replay result.
pub const MAX_ALLOCATION_REPLAY_BYTES: usize = 1_048_576;

/// Immutable response to an explicit allocation key, including successful NoEligible results.
/// The service hashes the caller/session/ingress/project/key scope and validates intent replay.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredAllocationReplay {
    pub key: String,
    pub intent_digest: String,
    pub result: Value,
    pub claim_id: Option<Id>,
}

/// Durable allocation ownership, distinct from an execution or coordinator assignment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAllocationClaim {
    pub id: Id,
    pub project_id: ProjectId,
    pub root_id: RootId,
    pub work_id: Id,
    pub kind: String,
    pub queue_key: String,
    pub owner_id: ActorId,
    pub session_id: Option<Id>,
    pub ingress_namespace: Id,
    pub invocation_id: Id,
    /// Positive ownership generation; advancing ownership fences every prior holder.
    pub fence: i64,
    pub expires_at_ms: i64,
    /// Active, Transferred, Yielding, Released, Expired, Yielded or Invalidated.
    pub state: String,
    pub revision: i64,
    pub work_revision: i64,
    pub parent_feature_id: Option<Id>,
    pub root_control_revision: i64,
    /// Queue revision observed before allocation, not a continuing ownership condition.
    pub queue_revision: i64,
    pub feature_task_set_revision: Option<i64>,
    pub created_at_ms: i64,
    pub execution_id: Option<Id>,
}

pub trait WorkRead {
    fn allocation_replay(&self, key: &str) -> Result<Option<StoredAllocationReplay>, StorageError>;
    fn allocation_claim(&self, id: &Id) -> Result<Option<StoredAllocationClaim>, StorageError>;
    /// ID-ordered, exclusive cursor, limit 1..=MAX_PAGE. Includes historical rows.
    fn allocation_claims(
        &self,
        project: &ProjectId,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredAllocationClaim>, StorageError>;
    fn allocation_by_invocation(
        &self,
        namespace: &Id,
        invocation: &Id,
    ) -> Result<Option<StoredAllocationClaim>, StorageError>;
    fn work(&self, id: &Id) -> Result<Option<StoredWork>, StorageError>;
    /// Items matching `filter` in queue order after `after`, at most `limit` (1..=500).
    fn works(
        &self,
        filter: &WorkFilter,
        after: Option<&WorkPosition>,
        limit: u16,
    ) -> Result<Vec<StoredWork>, StorageError>;
    /// The repositories the item names, in ID order.
    fn work_repositories(&self, id: &Id) -> Result<Vec<Id>, StorageError>;
    /// Its prerequisites (the items that block it: Blocked by), in ID order.
    fn prerequisites(&self, id: &Id) -> Result<Vec<Id>, StorageError>;
    /// The items it blocks (Blocks), in ID order.
    fn dependents(&self, id: &Id) -> Result<Vec<Id>, StorageError>;
    /// Prerequisites not completed with verified acceptance (a Done status with
    /// [`VERIFIED`] completion), in ID order.
    fn open_prerequisites(&self, id: &Id) -> Result<Vec<Id>, StorageError>;
    /// The revision of one queue of the project (1 before its first change).
    fn queue_revision(&self, project: &ProjectId, queue: &str) -> Result<i64, StorageError>;
    /// The next rank at the end of a queue.
    fn next_rank(&self, project: &ProjectId, queue: &str) -> Result<i64, StorageError>;
    /// The project's workflow statuses in their order.
    fn statuses(&self, project: &ProjectId) -> Result<Vec<StoredStatus>, StorageError>;
    /// The item of the project with this caller reference.
    fn by_caller_ref(
        &self,
        project: &ProjectId,
        caller_ref: &Id,
    ) -> Result<Option<Id>, StorageError>;
    fn feature_plan(&self, feature: &Id) -> Result<Option<StoredFeaturePlan>, StorageError>;
    fn proposal(&self, id: &Id) -> Result<Option<StoredProposal>, StorageError>;
    /// The feature's newest approval request, if any.
    fn latest_proposal(&self, feature: &Id) -> Result<Option<StoredProposal>, StorageError>;
    fn readiness(&self, feature: &Id) -> Result<Option<StoredReadiness>, StorageError>;
}

pub trait WorkRepository: WorkRead {
    fn insert_allocation_replay(
        &mut self,
        replay: &StoredAllocationReplay,
    ) -> Result<(), StorageError>;
    fn insert_allocation_claim(
        &mut self,
        claim: &StoredAllocationClaim,
    ) -> Result<(), StorageError>;
    fn update_allocation_claim(
        &mut self,
        claim: &StoredAllocationClaim,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Inserts an item with its detail row. An existing ID, a second item with the same caller
    /// reference in the project, or a parent that is not a feature of the same project is
    /// refused.
    fn insert_work(&mut self, work: &StoredWork) -> Result<(), StorageError>;
    /// Replaces the item when its stored revision is `expected` (a `Work` conflict otherwise).
    fn update_work(&mut self, work: &StoredWork, expected: i64) -> Result<(), StorageError>;
    /// Sets the item's explicit rank in its queue. Ranks are governed by the queue revision
    /// (bulk ordering checks it), not the item revision, so reordering other items never makes
    /// an open edit of this one stale.
    fn set_rank(&mut self, id: &Id, rank: i64) -> Result<(), StorageError>;
    /// Replaces the repositories the item names.
    fn set_work_repositories(&mut self, id: &Id, repositories: &[Id]) -> Result<(), StorageError>;
    /// Replaces the item's prerequisites (its incoming blocking links).
    fn set_prerequisites(&mut self, id: &Id, prerequisites: &[Id]) -> Result<(), StorageError>;
    /// Advances a queue revision of the project; returns the new revision.
    fn bump_queue(&mut self, project: &ProjectId, queue: &str) -> Result<i64, StorageError>;
    fn insert_status(&mut self, status: &StoredStatus) -> Result<(), StorageError>;
    /// Rename/reorder; stable semantic and project, revision advances exactly once.
    fn update_status(&mut self, status: &StoredStatus, expected: i64) -> Result<(), StorageError>;
    /// Refuses occupied statuses. Reassign work and append history in the same transaction.
    fn delete_status(
        &mut self,
        project: &ProjectId,
        id: &Id,
        expected: i64,
    ) -> Result<(), StorageError>;
    /// Writes the plan slot: an insert when `expected` is `None`, otherwise a compare-and-set
    /// on its revision (a `Plan` conflict when it changed).
    fn put_feature_plan(
        &mut self,
        plan: &StoredFeaturePlan,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn put_proposal(
        &mut self,
        proposal: &StoredProposal,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn put_readiness(
        &mut self,
        readiness: &StoredReadiness,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
}
