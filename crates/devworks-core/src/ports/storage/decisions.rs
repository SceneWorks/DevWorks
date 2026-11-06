//! Human decision projections and immutable reviewed snapshots. Settings and feature-plan
//! proposals retain their owning repositories; the inbox reads references to those records.
use super::{ConfigScope, StorageError};
use crate::context::PrincipalKind;
use crate::protocol::{ActorId, Id, ProjectId, RootId};
use serde_json::Value;

pub const MAX_DECISION_JSON_BYTES: usize = 1_048_576;
pub const MAX_SAVED_OPERATION_BYTES: usize = 524_288;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ApprovalSource {
    Settings,
    FeaturePlan,
    Generic,
}
impl ApprovalSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::FeaturePlan => "FeaturePlan",
            Self::Generic => "Generic",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Settings" => Some(Self::Settings),
            "FeaturePlan" => Some(Self::FeaturePlan),
            "Generic" => Some(Self::Generic),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalSourceRef {
    pub source: ApprovalSource,
    pub id: Id,
    pub project_id: Option<ProjectId>,
    pub created_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalSourcePosition {
    pub created_at_ms: i64,
    pub id: Id,
    pub source: ApprovalSource,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalSourceFilter {
    pub scope: ConfigScope,
    pub state: Option<String>,
}

/// Only the core's FindingRisk and SavedOperation registry creates generic proposals.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredApproval {
    pub id: Id,
    pub scope: ConfigScope,
    pub revision: i64,
    pub state: String,
    pub operation_kind: String,
    pub target_id: Id,
    pub target_revision: i64,
    pub content_digest: String,
    pub scope_digest: String,
    pub summary: String,
    pub artifact: Option<Value>,
    pub payload: Value,
    pub author: ActorId,
    pub author_kind: PrincipalKind,
    pub created_at_ms: i64,
    pub decided_by: Option<ActorId>,
    pub decided_at_ms: Option<i64>,
    pub rationale: Option<String>,
    pub restore_epoch: i64,
}
/// One immutable audit of the content actually reviewed, including source-owned proposals.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredHumanDecision {
    pub source: ApprovalSource,
    pub proposal_id: Id,
    pub reviewed_proposal_revision: i64,
    pub decision: String,
    pub rationale: Option<String>,
    pub actor: ActorId,
    pub at_ms: i64,
    pub content_digest: String,
    pub scope_digest: String,
    pub reviewed_snapshot: Value,
}
/// Private host outbox; never a protocol DTO or a source of tool-visible grants/context.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredApprovalResume {
    pub approval_id: Id,
    pub revision: i64,
    pub state: String,
    pub principal: ActorId,
    pub principal_kind: PrincipalKind,
    pub invocation: Value,
    pub operation: String,
    pub schema_revision: i64,
    pub input: Value,
    pub request_key: Id,
    pub request_digest: String,
    pub grant_id: Option<Id>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub result: Option<Value>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct StoredNotification {
    pub id: Id,
    pub revision: i64,
    pub user_id: ActorId,
    pub project_id: Option<ProjectId>,
    pub root_id: Option<RootId>,
    pub decision_id: Option<Id>,
    pub category: String,
    pub priority: i64,
    pub title: String,
    pub body: String,
    pub source_id: String,
    pub links: Value,
    pub requires_input: bool,
    pub read: bool,
    pub acknowledged: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationFilter {
    pub user_id: ActorId,
    pub scope: ConfigScope,
    pub categories: Vec<String>,
    pub read: Option<bool>,
    pub unacknowledged_only: bool,
    pub completion_only: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationPosition {
    pub created_at_ms: i64,
    pub id: Id,
}

/// OS submission evidence, independent from read/ack revisions and approval authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredOsNotificationDelivery {
    pub user_id: ActorId,
    pub notification_id: Id,
    pub revision: i64,
    pub boot_id: Id,
    pub attempt: i64,
    pub state: String,
    pub detail: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OsNotificationDeliveryFence {
    pub revision: i64,
    pub boot_id: Id,
    pub attempt: i64,
}
impl StoredOsNotificationDelivery {
    pub fn fence(&self) -> OsNotificationDeliveryFence {
        OsNotificationDeliveryFence {
            revision: self.revision,
            boot_id: self.boot_id.clone(),
            attempt: self.attempt,
        }
    }
}

pub trait DecisionRead {
    fn os_notification_delivery(
        &self,
        user: &ActorId,
        notification: &Id,
    ) -> Result<Option<StoredOsNotificationDelivery>, StorageError>;
    /// The host-bound canonical Human audience (at most one); never every historical actor.
    fn notification_users(&self) -> Result<Vec<ActorId>, StorageError>;
    /// Newest first by timestamp, ID and source; exclusive cursor; limit 1..=MAX_PAGE.
    fn list_approval_sources(
        &self,
        filter: &ApprovalSourceFilter,
        after: Option<&ApprovalSourcePosition>,
        limit: u16,
    ) -> Result<Vec<ApprovalSourceRef>, StorageError>;
    fn approval(&self, id: &Id) -> Result<Option<StoredApproval>, StorageError>;
    fn human_decision(
        &self,
        source: ApprovalSource,
        proposal: &Id,
    ) -> Result<Option<StoredHumanDecision>, StorageError>;
    fn approval_resume(&self, approval: &Id) -> Result<Option<StoredApprovalResume>, StorageError>;
    fn pending_approval_resumes(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredApprovalResume>, StorageError>;
    fn notification(
        &self,
        user: &ActorId,
        id: &Id,
    ) -> Result<Option<StoredNotification>, StorageError>;
    fn notification_by_source(
        &self,
        user: &ActorId,
        source: &str,
    ) -> Result<Option<StoredNotification>, StorageError>;
    fn notifications(
        &self,
        filter: &NotificationFilter,
        after: Option<&NotificationPosition>,
        limit: u16,
    ) -> Result<Vec<StoredNotification>, StorageError>;
}
pub trait DecisionRepository: DecisionRead {
    /// Claims only an eligible notification of the current host-bound Human audience.
    fn claim_os_notification_delivery(
        &mut self,
        user: &ActorId,
        notification: &Id,
        expected_revision: Option<i64>,
        retry: bool,
    ) -> Result<StoredOsNotificationDelivery, StorageError>;
    fn settle_os_notification_delivery(
        &mut self,
        user: &ActorId,
        notification: &Id,
        fence: &OsNotificationDeliveryFence,
        state: &str,
        detail: Option<&str>,
    ) -> Result<StoredOsNotificationDelivery, StorageError>;
    /// Prior-boot in-flight submissions become Unknown; they are never automatically resent.
    fn recover_os_notification_deliveries(&mut self) -> Result<u64, StorageError>;
    /// Host composition binds the actual local UI identity; no tool operation exposes this.
    fn bind_notification_user(&mut self, user: &ActorId) -> Result<(), StorageError>;
    /// Immutable content/binding; a Pending row may take one terminal decision at revision+1.
    fn put_approval(
        &mut self,
        approval: &StoredApproval,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    fn insert_human_decision(&mut self, decision: &StoredHumanDecision)
    -> Result<(), StorageError>;
    fn put_approval_resume(
        &mut self,
        resume: &StoredApprovalResume,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
    /// Owner/source/content are immutable; only read/acknowledgment advance with a CAS revision.
    fn put_notification(
        &mut self,
        notification: &StoredNotification,
        expected: Option<i64>,
    ) -> Result<(), StorageError>;
}
