//! Generated architecture DTOs. Edit domain_spec.py and regenerate; no services.
use super::domain_support::*;
use crate::data_delivery::{
    ArtifactTags, Coverage, DataScope, DeliveryBarrier, DeliverySnapshot, GitOid, GraphPage,
    RetrievalPage, ViewSelection,
};
use crate::provider::{ArtifactRef, Digest, SchemaRef, TranscriptPage};
use crate::{
    Activation, ActorId, AssignmentId, Blockers, CommandId, CommandReply, CorrelationId, Counter,
    EligibilityTarget, Event, EventId, Id, OperationId, OperationState, ProjectId, Protocol,
    Purpose, QueryId, ReadStamp, RequestKey, ResourceId, Revision, RootControl, RootId, StoreId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum LifeState {
    Active,
    Archived,
    Deleted,
}
impl Check for LifeState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum WorkKind {
    Feature,
    Issue,
    Task,
}
impl Check for WorkKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum AccessMode {
    Full,
    Project,
}
impl Check for AccessMode {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ApprovalState {
    Pending,
    Approved,
    Rejected,
    Superseded,
    Expired,
}
impl Check for ApprovalState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReviewState {
    Pending,
    Running,
    ChangesRequired,
    Ready,
    Blocked,
}
impl Check for ReviewState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum TrustLevel {
    Trusted,
    Untrusted,
    Inferred,
    Unresolved,
}
impl Check for TrustLevel {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ItemState {
    Enabled,
    Disabled,
    Unavailable,
    Partial,
    Healthy,
    Failed,
}
impl Check for ItemState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum UsagePriceKind {
    Reported,
    Estimated,
    Free,
    Untracked,
}
impl Check for UsagePriceKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Severity {
    Info,
    Warning,
    Error,
}
impl Check for Severity {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DiagnosticOutcome {
    Running,
    Succeeded,
    Partial,
    Stale,
    Cancelled,
    Retryable,
    Failed,
}
impl Check for DiagnosticOutcome {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DiagnosticErrorClass {
    Transient,
    Timeout,
    Cancelled,
    Configuration,
    Authentication,
    Permission,
    NotFound,
    InvalidInput,
    ResourceExhausted,
    Dependency,
    Corruption,
    Internal,
    Unknown,
}
impl Check for DiagnosticErrorClass {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum HealthState {
    Healthy,
    Degraded,
    Failed,
    Unavailable,
}
impl Check for HealthState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DiagnosticOrigin {
    Host,
    Reported,
}
impl Check for DiagnosticOrigin {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum MetricAvailability {
    Available,
    Unavailable,
}
impl Check for MetricAvailability {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum TierName {
    Max,
    High,
    Mid,
    Low,
}
impl Check for TierName {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum EffortName {
    Default,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}
impl Check for EffortName {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DbEngine {
    Sqlite,
    MariaDb,
    MySql,
    Postgres,
    SqlServer,
}
impl Check for DbEngine {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum JobKind {
    Native,
    Inference,
}
impl Check for JobKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ProposalDecision {
    Approve,
    Reject,
}
impl Check for ProposalDecision {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum GitActionKind {
    Create,
    Attach,
    Inspect,
}
impl Check for GitActionKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum LeaseState {
    Waiting,
    Held,
    Releasing,
    Uncertain,
    Released,
}
impl Check for LeaseState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ResourcePriority {
    Low,
    Medium,
    High,
    Critical,
}
impl Check for ResourcePriority {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ResourceScope {
    Machine,
    Project,
}
impl Check for ResourceScope {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum LeaseEnd {
    Released,
    Expired,
    Cancelled,
    Reconciled,
}
impl Check for LeaseEnd {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DeliveryMode {
    ReadOnly,
    WriteEnabled,
}
impl Check for DeliveryMode {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PriceOrigin {
    OfficialSnapshot,
    ProviderReported,
    UserConfigured,
    LocalFree,
    Untracked,
}
impl Check for PriceOrigin {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DecisionQuestionType {
    #[serde(rename = "choice")]
    Choice,
    #[serde(rename = "noul")]
    Noul,
    #[serde(rename = "score")]
    Score,
    #[serde(rename = "box")]
    Box,
}
impl Check for DecisionQuestionType {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum AccountAction {
    BeginSignIn,
    CompleteSignIn,
    Refresh,
    Disconnect,
    Inspect,
}
impl Check for AccountAction {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum EndpointKind {
    OpenAiResponses,
    CodexSubscriptionResponses,
    AnthropicMessages,
    ClaudeSubscriptionMessages,
    OpenRouterChat,
    CompatibleChat,
    Embedding,
    SystemOne,
}
impl Check for EndpointKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ToolProfile {
    VerifiedRead,
    Full,
    Project,
    Confined,
}
impl Check for ToolProfile {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PruneKind {
    Diagnostics,
    IndexRuns,
    Messages,
    Sessions,
}
impl Check for PruneKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum QueueResolution {
    Retry,
    Reconcile,
    AcknowledgePoison,
    CancelBeforeDispatch,
}
impl Check for QueueResolution {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum UpdateChannel {
    Stable,
    Prerelease,
}
impl Check for UpdateChannel {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReleasePolicy {
    Automatic,
    AskMe,
    Disabled,
}
impl Check for ReleasePolicy {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum SortDirection {
    Ascending,
    Descending,
}
impl Check for SortDirection {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum MetadataObjectKind {
    Catalog,
    Schema,
    Table,
    View,
    Column,
    Index,
    Constraint,
    Procedure,
    Type,
    GraphEntity,
    GraphEdge,
}
impl Check for MetadataObjectKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum KnowledgeKind {
    Fact,
    Decision,
    Lesson,
    Constraint,
    Finding,
    Proposal,
}
impl Check for KnowledgeKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum KnowledgeState {
    Active,
    Challenged,
    Verified,
    Retracted,
}
impl Check for KnowledgeState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum MessageClass {
    Instruction,
    Status,
    Question,
    Evidence,
}
impl Check for MessageClass {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageRequest {
    pub limit: Limit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Cursor>,
}
impl Check for PageRequest {
    fn check(&self) -> Result<(), String> {
        self.limit.check()?;
        self.cursor.check()?;
        super::domain_rules::record(
            "PageRequest",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RevisionedRef {
    pub id: Id,
    pub revision: Revision,
}
impl Check for RevisionedRef {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        super::domain_rules::record(
            "RevisionedRef",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisteredPath {
    pub root_id: Id,
    pub relative: RelativePath,
}
impl Check for RegisteredPath {
    fn check(&self) -> Result<(), String> {
        self.root_id.check()?;
        self.relative.check()?;
        super::domain_rules::record(
            "RegisteredPath",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileSource {
    pub artifact: ArtifactRef,
}
impl Check for FileSource {
    fn check(&self) -> Result<(), String> {
        self.artifact.check()?;
        super::domain_rules::record(
            "FileSource",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationRef {
    pub operation_id: OperationId,
    pub state: OperationState,
}
impl Check for OperationRef {
    fn check(&self) -> Result<(), String> {
        self.operation_id.check()?;
        self.state.check()?;
        super::domain_rules::record(
            "OperationRef",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MutationSummary {
    pub id: Id,
    pub revision: Revision,
    pub changed: bool,
}
impl Check for MutationSummary {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.changed.check()?;
        super::domain_rules::record(
            "MutationSummary",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanApproval {
    pub proposal_id: Id,
    pub plan_revision: Revision,
    pub plan_digest: Digest,
    pub state: ApprovalState,
}
impl Check for PlanApproval {
    fn check(&self) -> Result<(), String> {
        self.proposal_id.check()?;
        self.plan_revision.check()?;
        self.plan_digest.check()?;
        self.state.check()?;
        super::domain_rules::record(
            "PlanApproval",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub revision: Revision,
    pub name: Label,
    pub description: Content,
    pub repository_ids: Items<Id>,
    pub lifecycle: LifeState,
    pub access_mode: AccessMode,
}
impl Check for ProjectRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.description.check()?;
        self.repository_ids.check()?;
        self.lifecycle.check()?;
        self.access_mode.check()?;
        super::domain_rules::record(
            "ProjectRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkRecord {
    pub id: Id,
    pub revision: Revision,
    pub project_id: ProjectId,
    pub kind: WorkKind,
    pub title: Label,
    pub description: Content,
    pub status_id: Id,
    pub priority: i32,
    pub repository_ids: Items<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<PlanApproval>,
    pub dependency_ids: Items<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_agent_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_ref: Option<Id>,
    pub goals: Items<Content>,
    pub requirements: Items<RequirementItem>,
    pub acceptance: Items<AcceptanceCriterion>,
    pub inherits_requirement_ids: Items<Id>,
    pub touches: Items<TouchRef>,
    pub links: Items<WorkLink>,
    pub comments: Rows<WorkComment>,
    pub handoffs: Rows<HandoffRecord>,
    pub attempts: Rows<WorkAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimRecord>,
    pub merged_awaiting_acceptance: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_report: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_cursor: Option<Cursor>,
    pub detail_coverage: Coverage,
    pub comment_count: Counter,
    pub handoff_count: Counter,
    pub attempt_count: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_plan: Option<PendingFeaturePlan>,
    pub dependent_ids: Items<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_order: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_issue: Option<GithubIssueTracking>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ReadinessAssessment", extend("x-since" = 2))]
    pub readiness: Option<ReadinessAssessment>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 2))]
    pub queue_revision: Option<Revision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 2))]
    pub workflow_revision: Option<Revision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<AcceptanceCriterion>", extend("x-since" = 2))]
    pub acceptance_tests: Option<Items<AcceptanceCriterion>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ReadinessAssessment", extend("x-since" = 2))]
    pub pending_readiness: Option<ReadinessAssessment>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "WorkItemType", extend("x-since" = 3))]
    pub task_type: Option<WorkItemType>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 3))]
    pub labels: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "WorkInheritanceOrigin", extend("x-since" = 3))]
    pub previous_inheritance: Option<WorkInheritanceOrigin>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "FeatureBudgetView", extend("x-since" = 3))]
    pub budget: Option<FeatureBudgetView>,
}
impl Check for WorkRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.project_id.check()?;
        self.kind.check()?;
        self.title.check()?;
        self.description.check()?;
        self.status_id.check()?;
        self.priority.check()?;
        self.repository_ids.check()?;
        self.parent_id.check()?;
        self.plan.check()?;
        self.approval.check()?;
        self.dependency_ids.check()?;
        self.assigned_agent_id.check()?;
        self.caller_ref.check()?;
        self.goals.check()?;
        self.requirements.check()?;
        self.acceptance.check()?;
        self.inherits_requirement_ids.check()?;
        self.touches.check()?;
        self.links.check()?;
        self.comments.check()?;
        self.handoffs.check()?;
        self.attempts.check()?;
        self.claim.check()?;
        self.merged_awaiting_acceptance.check()?;
        self.acceptance_report.check()?;
        self.detail_cursor.check()?;
        self.detail_coverage.check()?;
        self.comment_count.check()?;
        self.handoff_count.check()?;
        self.attempt_count.check()?;
        self.pending_plan.check()?;
        self.dependent_ids.check()?;
        self.manual_order.check()?;
        self.github_issue.check()?;
        self.readiness.check()?;
        self.queue_revision.check()?;
        self.workflow_revision.check()?;
        self.acceptance_tests.check()?;
        self.pending_readiness.check()?;
        self.task_type.check()?;
        self.labels.check()?;
        self.previous_inheritance.check()?;
        self.budget.check()?;
        super::domain_rules::record(
            "WorkRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for WorkRecord {
    fn retain_revision(&mut self, revision: u32) {
        self.pending_plan.retain_revision(revision);
        if revision < 2 {
            self.readiness = None;
        }
        if revision < 2 {
            self.queue_revision = None;
        }
        if revision < 2 {
            self.workflow_revision = None;
        }
        if revision < 2 {
            self.acceptance_tests = None;
        }
        if revision < 2 {
            self.pending_readiness = None;
        }
        if revision < 3 {
            self.task_type = None;
        }
        if revision < 3 {
            self.labels = None;
        }
        if revision < 3 {
            self.previous_inheritance = None;
        }
        if revision < 3 {
            self.budget = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanDraft {
    pub content: ArtifactRef,
    pub acceptance: Items<AcceptanceCriterion>,
    pub repository_ids: Items<Id>,
    pub goals: Items<Content>,
    pub requirements: Items<RequirementItem>,
    pub acceptance_tests: Items<AcceptanceCriterion>,
}
impl Check for PlanDraft {
    fn check(&self) -> Result<(), String> {
        self.content.check()?;
        self.acceptance.check()?;
        self.repository_ids.check()?;
        self.goals.check()?;
        self.requirements.check()?;
        self.acceptance_tests.check()?;
        super::domain_rules::record(
            "PlanDraft",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptanceCriterion {
    pub id: Id,
    pub text: Content,
    pub evidence_ids: Items<Id>,
    pub state: CriterionState,
}
impl Check for AcceptanceCriterion {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.text.check()?;
        self.evidence_ids.check()?;
        self.state.check()?;
        super::domain_rules::record(
            "AcceptanceCriterion",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_mode: Option<AccessMode>,
}
impl Check for ProjectPatch {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.description.check()?;
        self.access_mode.check()?;
        super::domain_rules::record(
            "ProjectPatch",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DependencyEdge {
    pub predecessor: Id,
    pub successor: Id,
}
impl Check for DependencyEdge {
    fn check(&self) -> Result<(), String> {
        self.predecessor.check()?;
        self.successor.check()?;
        super::domain_rules::record(
            "DependencyEdge",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DependencyReplacement {
    pub edges: Items<DependencyEdge>,
}
impl Check for DependencyReplacement {
    fn check(&self) -> Result<(), String> {
        self.edges.check()?;
        super::domain_rules::record(
            "DependencyReplacement",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceLink {
    pub id: Id,
    pub target_id: Id,
    pub artifact: ArtifactRef,
    pub summary: Content,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ActorId", extend("x-since" = 2))]
    pub actor_id: Option<ActorId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub occurred_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub repository_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub revision: Option<Text>,
}
impl Check for EvidenceLink {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.target_id.check()?;
        self.artifact.check()?;
        self.summary.check()?;
        self.actor_id.check()?;
        self.occurred_at_ms.check()?;
        self.repository_id.check()?;
        self.revision.check()?;
        super::domain_rules::record(
            "EvidenceLink",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for EvidenceLink {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.actor_id = None;
        }
        if revision < 2 {
            self.occurred_at_ms = None;
        }
        if revision < 2 {
            self.repository_id = None;
        }
        if revision < 2 {
            self.revision = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusRecord {
    pub id: Id,
    pub revision: Revision,
    pub label: Label,
    pub category: Label,
    pub rank: Counter,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 3))]
    pub workflow_revision: Option<Revision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Digest", extend("x-since" = 3))]
    pub affected_work_digest: Option<Digest>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 3))]
    pub affected_work_count: Option<Counter>,
}
impl Check for StatusRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.label.check()?;
        self.category.check()?;
        self.rank.check()?;
        self.workflow_revision.check()?;
        self.affected_work_digest.check()?;
        self.affected_work_count.check()?;
        super::domain_rules::record(
            "StatusRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for StatusRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.workflow_revision = None;
        }
        if revision < 3 {
            self.affected_work_digest = None;
        }
        if revision < 3 {
            self.affected_work_count = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimRecord {
    pub id: Id,
    pub work_id: Id,
    pub owner_id: ActorId,
    pub fence: Counter,
    pub expires_at_ms: Counter,
    pub state: LeaseState,
}
impl Check for ClaimRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.work_id.check()?;
        self.owner_id.check()?;
        self.fence.check()?;
        self.expires_at_ms.check()?;
        self.state.check()?;
        super::domain_rules::record(
            "ClaimRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LineageRecord {
    pub id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Id>,
    pub kind: Label,
    pub related_id: Id,
    pub summary: Content,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ActorId", extend("x-since" = 2))]
    pub actor_id: Option<ActorId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub occurred_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub repository_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub revision: Option<Text>,
}
impl Check for LineageRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.parent_id.check()?;
        self.kind.check()?;
        self.related_id.check()?;
        self.summary.check()?;
        self.actor_id.check()?;
        self.occurred_at_ms.check()?;
        self.repository_id.check()?;
        self.revision.check()?;
        super::domain_rules::record(
            "LineageRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for LineageRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.actor_id = None;
        }
        if revision < 2 {
            self.occurred_at_ms = None;
        }
        if revision < 2 {
            self.repository_id = None;
        }
        if revision < 2 {
            self.revision = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRecord {
    pub assignment_id: AssignmentId,
    pub root_id: RootId,
    pub revision: Revision,
    pub role_id: Id,
    pub state: OperationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<ArtifactRef>,
    pub blockers: Items<Id>,
}
impl Check for AgentRecord {
    fn check(&self) -> Result<(), String> {
        self.assignment_id.check()?;
        self.root_id.check()?;
        self.revision.check()?;
        self.role_id.check()?;
        self.state.check()?;
        self.checkpoint.check()?;
        self.blockers.check()?;
        super::domain_rules::record(
            "AgentRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentMessage {
    pub id: Id,
    pub revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_assignment_id: Option<AssignmentId>,
    pub to_assignment_id: AssignmentId,
    pub message_class: MessageClass,
    pub content: Content,
    pub artifact_refs: Items<ArtifactRef>,
    pub acknowledged: bool,
}
impl Check for AgentMessage {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.from_assignment_id.check()?;
        self.to_assignment_id.check()?;
        self.message_class.check()?;
        self.content.check()?;
        self.artifact_refs.check()?;
        self.acknowledged.check()?;
        super::domain_rules::record(
            "AgentMessage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckpointRecord {
    pub id: Id,
    pub revision: Revision,
    pub assignment_id: AssignmentId,
    pub transcript_sequence: Counter,
    pub content: ArtifactRef,
}
impl Check for CheckpointRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.assignment_id.check()?;
        self.transcript_sequence.check()?;
        self.content.check()?;
        super::domain_rules::record(
            "CheckpointRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRecord {
    pub id: Id,
    pub revision: Revision,
    pub title: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<RootId>,
    pub context_epoch: Counter,
    pub closed: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub created_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub last_activity_ms: Option<Counter>,
}
impl Check for SessionRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.title.check()?;
        self.root_id.check()?;
        self.context_epoch.check()?;
        self.closed.check()?;
        self.created_at_ms.check()?;
        self.last_activity_ms.check()?;
        super::domain_rules::record(
            "SessionRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for SessionRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.created_at_ms = None;
        }
        if revision < 2 {
            self.last_activity_ms = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextSource {
    pub id: Id,
    pub kind: Label,
    pub source_revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
    pub token_estimate: Counter,
}
impl Check for ContextSource {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.kind.check()?;
        self.source_revision.check()?;
        self.artifact.check()?;
        self.token_estimate.check()?;
        super::domain_rules::record(
            "ContextSource",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScreenContext {
    pub screen_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<RootId>,
    pub selected_ids: Items<Id>,
    pub help_topic_id: Id,
    pub revision: Revision,
}
impl Check for ScreenContext {
    fn check(&self) -> Result<(), String> {
        self.screen_id.check()?;
        self.project_id.check()?;
        self.root_id.check()?;
        self.selected_ids.check()?;
        self.help_topic_id.check()?;
        self.revision.check()?;
        super::domain_rules::record(
            "ScreenContext",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryRecord {
    pub id: Id,
    pub revision: Revision,
    pub name: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<Url>,
    pub project_ids: Items<ProjectId>,
    pub primary: bool,
    pub lifecycle: LifeState,
    pub coverage: Coverage,
    pub indexed_view_ids: Items<Id>,
}
impl Check for RepositoryRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.remote_url.check()?;
        self.project_ids.check()?;
        self.primary.check()?;
        self.lifecycle.check()?;
        self.coverage.check()?;
        self.indexed_view_ids.check()?;
        super::domain_rules::record(
            "RepositoryRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryText {
    pub repository_id: Id,
    pub view_id: Id,
    pub content: ArtifactRef,
    pub coverage: Coverage,
}
impl Check for RepositoryText {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.view_id.check()?;
        self.content.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "RepositoryText",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexRunRecord {
    pub id: Id,
    pub revision: Revision,
    pub repository_ids: Items<Id>,
    pub stage: Label,
    pub state: OperationState,
    pub completed_units: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_units: Option<Counter>,
    pub output_views: Items<Id>,
    pub diagnostics: Items<Diagnostic>,
}
impl Check for IndexRunRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.repository_ids.check()?;
        self.stage.check()?;
        self.state.check()?;
        self.completed_units.check()?;
        self.total_units.check()?;
        self.output_views.check()?;
        self.diagnostics.check()?;
        super::domain_rules::record(
            "IndexRunRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexSourceConfig {
    pub structural_languages: Items<Label>,
    pub semantic_package_ids: Items<Id>,
    pub exclude_patterns: Items<Text>,
    pub max_file_bytes: Counter,
}
impl Check for IndexSourceConfig {
    fn check(&self) -> Result<(), String> {
        self.structural_languages.check()?;
        self.semantic_package_ids.check()?;
        self.exclude_patterns.check()?;
        self.max_file_bytes.check()?;
        super::domain_rules::record(
            "IndexSourceConfig",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnalysisRecord {
    pub id: Id,
    pub revision: Revision,
    pub views: ViewSelection,
    pub kind: Label,
    pub state: OperationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ArtifactRef>,
    pub coverage: Coverage,
}
impl Check for AnalysisRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.views.check()?;
        self.kind.check()?;
        self.state.check()?;
        self.report.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "AnalysisRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClusterRecord {
    pub id: Id,
    pub view_id: Id,
    pub name: Label,
    pub member_node_ids: Items<Id>,
    pub score: Finite,
}
impl Check for ClusterRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.view_id.check()?;
        self.name.check()?;
        self.member_node_ids.check()?;
        self.score.check()?;
        super::domain_rules::record(
            "ClusterRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricRecord {
    pub name: Label,
    pub value: Finite,
    pub unit: Label,
    pub source_id: Id,
    pub inferred: bool,
}
impl Check for MetricRecord {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.value.check()?;
        self.unit.check()?;
        self.source_id.check()?;
        self.inferred.check()?;
        super::domain_rules::record(
            "MetricRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaConnection {
    pub id: Id,
    pub revision: Revision,
    pub name: Label,
    pub engine: DbEngine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<Id>,
    pub state: ItemState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_view_id: Option<Id>,
    pub writes_enabled: bool,
    pub metadata_only: bool,
}
impl Check for SchemaConnection {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.engine.check()?;
        self.credential_ref.check()?;
        self.state.check()?;
        self.current_view_id.check()?;
        self.writes_enabled.check()?;
        self.metadata_only.check()?;
        super::domain_rules::record(
            "SchemaConnection",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionConfig {
    pub name: Label,
    pub engine: DbEngine,
    pub endpoint: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<Id>,
    pub catalogs: Items<Label>,
    pub schemas: Items<Label>,
    pub include_patterns: Items<Text>,
    pub exclude_patterns: Items<Text>,
    pub writes_enabled: bool,
}
impl Check for ConnectionConfig {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.engine.check()?;
        self.endpoint.check()?;
        self.credential_ref.check()?;
        self.catalogs.check()?;
        self.schemas.check()?;
        self.include_patterns.check()?;
        self.exclude_patterns.check()?;
        self.writes_enabled.check()?;
        super::domain_rules::record(
            "ConnectionConfig",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaObject {
    pub id: Id,
    pub view_id: Id,
    pub kind: MetadataObjectKind,
    pub qualified_name: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Id>,
    pub coverage: Coverage,
}
impl Check for SchemaObject {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.view_id.check()?;
        self.kind.check()?;
        self.qualified_name.check()?;
        self.definition.check()?;
        self.parent_id.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "SchemaObject",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaRelation {
    pub id: Id,
    pub view_id: Id,
    pub from_object_id: Id,
    pub to_object_id: Id,
    pub relation: Label,
    pub inferred: bool,
}
impl Check for SchemaRelation {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.view_id.check()?;
        self.from_object_id.check()?;
        self.to_object_id.check()?;
        self.relation.check()?;
        self.inferred.check()?;
        super::domain_rules::record(
            "SchemaRelation",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DbScalar {
    Null {},
    Boolean { value: bool },
    Integer { value: SignedCounter },
    Decimal { value: Text },
    Text { value: Content },
    Bytes { value: ArtifactRef },
}
impl Check for DbScalar {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Null {} => {}
            Self::Boolean { value } => {
                value.check()?;
            }
            Self::Integer { value } => {
                value.check()?;
            }
            Self::Decimal { value } => {
                value.check()?;
            }
            Self::Text { value } => {
                value.check()?;
            }
            Self::Bytes { value } => {
                value.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DbParameter {
    pub name: Label,
    pub value: DbScalar,
}
impl Check for DbParameter {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.value.check()?;
        super::domain_rules::record(
            "DbParameter",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DbColumn {
    pub name: Label,
    pub data_type: Label,
    pub sensitive: bool,
}
impl Check for DbColumn {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.data_type.check()?;
        self.sensitive.check()?;
        super::domain_rules::record(
            "DbColumn",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DbRow {
    pub cells: Items<DbScalar>,
}
impl Check for DbRow {
    fn check(&self) -> Result<(), String> {
        self.cells.check()?;
        super::domain_rules::record(
            "DbRow",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DbReadResult {
    pub columns: Items<DbColumn>,
    pub rows: Rows<DbRow>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<Revision>,
}
impl Check for DbReadResult {
    fn check(&self) -> Result<(), String> {
        self.columns.check()?;
        self.rows.check()?;
        self.truncated.check()?;
        self.next_cursor.check()?;
        self.source_revision.check()?;
        super::domain_rules::record(
            "DbReadResult",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DbWriteResult {
    pub connection_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_rows: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<Id>,
    pub state: OperationState,
    pub diagnostics: Items<Diagnostic>,
}
impl Check for DbWriteResult {
    fn check(&self) -> Result<(), String> {
        self.connection_id.check()?;
        self.affected_rows.check()?;
        self.transaction_id.check()?;
        self.state.check()?;
        self.diagnostics.check()?;
        super::domain_rules::record(
            "DbWriteResult",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceEvidence {
    pub id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<Id>,
    pub source: EvidenceLocator,
    pub snippet: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SchemaValue>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub source_revision: Option<Text>,
}
impl Check for SourceEvidence {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.claim_id.check()?;
        self.source.check()?;
        self.snippet.check()?;
        self.metadata.check()?;
        self.source_revision.check()?;
        super::domain_rules::record(
            "SourceEvidence",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for SourceEvidence {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.source_revision = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeEntity {
    pub id: Id,
    pub revision: Revision,
    pub label: Label,
    pub entity_type: Label,
    pub aliases: Items<Label>,
    pub scope: DataScope,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub canonical_name: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub external_id: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub source: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ActorId", extend("x-since" = 2))]
    pub author_id: Option<ActorId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub created_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub updated_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub deleted: Option<bool>,
}
impl Check for KnowledgeEntity {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.label.check()?;
        self.entity_type.check()?;
        self.aliases.check()?;
        self.scope.check()?;
        self.canonical_name.check()?;
        self.external_id.check()?;
        self.source.check()?;
        self.author_id.check()?;
        self.created_at_ms.check()?;
        self.updated_at_ms.check()?;
        self.deleted.check()?;
        super::domain_rules::record(
            "KnowledgeEntity",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for KnowledgeEntity {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.canonical_name = None;
        }
        if revision < 2 {
            self.external_id = None;
        }
        if revision < 2 {
            self.source = None;
        }
        if revision < 2 {
            self.author_id = None;
        }
        if revision < 2 {
            self.created_at_ms = None;
        }
        if revision < 2 {
            self.updated_at_ms = None;
        }
        if revision < 2 {
            self.deleted = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeClaim {
    pub id: Id,
    pub revision: Revision,
    pub kind: KnowledgeKind,
    pub subject_id: Id,
    pub predicate: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Content>,
    pub confidence: Probability,
    pub state: KnowledgeState,
    pub evidence_ids: Items<Id>,
    pub tags: Items<Label>,
    pub scope: DataScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_json: Option<DynamicJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_text: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "KnowledgeEntryKind", extend("x-since" = 2))]
    pub entry_kind: Option<KnowledgeEntryKind>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "KnowledgeLifecycle", extend("x-since" = 2))]
    pub lifecycle: Option<KnowledgeLifecycle>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "KnowledgeOrigin", extend("x-since" = 2))]
    pub origin: Option<KnowledgeOrigin>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub origin_ref: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub autonomous: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ActorId", extend("x-since" = 2))]
    pub author_id: Option<ActorId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub session_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub created_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub updated_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub verified_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub verified_source_revision: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub stale: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub superseded_by: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "LessonRecord", extend("x-since" = 2))]
    pub lesson: Option<LessonRecord>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<LessonChallenge>", extend("x-since" = 3))]
    pub challenges: Option<Items<LessonChallenge>>,
}
impl Check for KnowledgeClaim {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.kind.check()?;
        self.subject_id.check()?;
        self.predicate.check()?;
        self.object_id.check()?;
        self.value.check()?;
        self.confidence.check()?;
        self.state.check()?;
        self.evidence_ids.check()?;
        self.tags.check()?;
        self.scope.check()?;
        self.value_json.check()?;
        self.normalized_text.check()?;
        self.effective_at_ms.check()?;
        self.supersedes.check()?;
        self.source.check()?;
        self.entry_kind.check()?;
        self.lifecycle.check()?;
        self.origin.check()?;
        self.origin_ref.check()?;
        self.autonomous.check()?;
        self.author_id.check()?;
        self.session_id.check()?;
        self.created_at_ms.check()?;
        self.updated_at_ms.check()?;
        self.verified_at_ms.check()?;
        self.verified_source_revision.check()?;
        self.stale.check()?;
        self.superseded_by.check()?;
        self.lesson.check()?;
        self.challenges.check()?;
        super::domain_rules::record(
            "KnowledgeClaim",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for KnowledgeClaim {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.entry_kind = None;
        }
        if revision < 2 {
            self.lifecycle = None;
        }
        if revision < 2 {
            self.origin = None;
        }
        if revision < 2 {
            self.origin_ref = None;
        }
        if revision < 2 {
            self.autonomous = None;
        }
        if revision < 2 {
            self.author_id = None;
        }
        if revision < 2 {
            self.session_id = None;
        }
        if revision < 2 {
            self.created_at_ms = None;
        }
        if revision < 2 {
            self.updated_at_ms = None;
        }
        if revision < 2 {
            self.verified_at_ms = None;
        }
        if revision < 2 {
            self.verified_source_revision = None;
        }
        if revision < 2 {
            self.stale = None;
        }
        if revision < 2 {
            self.superseded_by = None;
        }
        if revision < 2 {
            self.lesson = None;
        }
        if revision < 3 {
            self.challenges = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimDraft {
    pub kind: KnowledgeKind,
    pub subject_id: Id,
    pub predicate: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Content>,
    pub confidence: Probability,
    pub evidence_ids: Items<Id>,
    pub tags: Items<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_json: Option<DynamicJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_text: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "KnowledgeEntryKind", extend("x-since" = 2))]
    pub entry_kind: Option<KnowledgeEntryKind>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "KnowledgeOrigin", extend("x-since" = 2))]
    pub origin: Option<KnowledgeOrigin>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub origin_ref: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "LessonDraft", extend("x-since" = 2))]
    pub lesson: Option<LessonDraft>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub source_revision: Option<Text>,
}
impl Check for ClaimDraft {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.subject_id.check()?;
        self.predicate.check()?;
        self.object_id.check()?;
        self.value.check()?;
        self.confidence.check()?;
        self.evidence_ids.check()?;
        self.tags.check()?;
        self.value_json.check()?;
        self.normalized_text.check()?;
        self.effective_at_ms.check()?;
        self.supersedes.check()?;
        self.source.check()?;
        self.entry_kind.check()?;
        self.origin.check()?;
        self.origin_ref.check()?;
        self.lesson.check()?;
        self.source_revision.check()?;
        super::domain_rules::record(
            "ClaimDraft",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for ClaimDraft {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.entry_kind = None;
        }
        if revision < 2 {
            self.origin = None;
        }
        if revision < 2 {
            self.origin_ref = None;
        }
        if revision < 2 {
            self.lesson = None;
        }
        if revision < 2 {
            self.source_revision = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeBatch {
    pub entities: Items<KnowledgeEntityDraft>,
    pub claims: Items<ClaimDraft>,
    pub evidence: Items<SourceEvidence>,
}
impl Check for KnowledgeBatch {
    fn check(&self) -> Result<(), String> {
        self.entities.check()?;
        self.claims.check()?;
        self.evidence.check()?;
        super::domain_rules::record(
            "KnowledgeBatch",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for KnowledgeBatch {
    fn retain_revision(&mut self, revision: u32) {
        self.claims.retain_revision(revision);
        self.evidence.retain_revision(revision);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeEntityDraft {
    pub client_id: Id,
    pub label: Label,
    pub entity_type: Label,
    pub aliases: Items<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_name: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Label>,
}
impl Check for KnowledgeEntityDraft {
    fn check(&self) -> Result<(), String> {
        self.client_id.check()?;
        self.label.check()?;
        self.entity_type.check()?;
        self.aliases.check()?;
        self.canonical_name.check()?;
        self.external_id.check()?;
        self.source.check()?;
        super::domain_rules::record(
            "KnowledgeEntityDraft",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeBundle {
    pub entities: Rows<KnowledgeEntity>,
    pub claims: Rows<KnowledgeClaim>,
    pub evidence: Rows<SourceEvidence>,
    pub frontier: Items<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub truncated: bool,
    pub conflicts: Rows<KnowledgeConflict>,
    pub supersession_ids: Items<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Content>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Rows<KnowledgeRelation>", extend("x-since" = 2))]
    pub relations: Option<Rows<KnowledgeRelation>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Rows<KnowledgeRevisionEntry>", extend("x-since" = 2))]
    pub revisions: Option<Rows<KnowledgeRevisionEntry>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 2))]
    pub scope_revision: Option<Revision>,
}
impl Check for KnowledgeBundle {
    fn check(&self) -> Result<(), String> {
        self.entities.check()?;
        self.claims.check()?;
        self.evidence.check()?;
        self.frontier.check()?;
        self.next_cursor.check()?;
        self.truncated.check()?;
        self.conflicts.check()?;
        self.supersession_ids.check()?;
        self.summary.check()?;
        self.relations.check()?;
        self.revisions.check()?;
        self.scope_revision.check()?;
        super::domain_rules::record(
            "KnowledgeBundle",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for KnowledgeBundle {
    fn retain_revision(&mut self, revision: u32) {
        self.entities.retain_revision(revision);
        self.claims.retain_revision(revision);
        self.evidence.retain_revision(revision);
        if revision < 2 {
            self.relations = None;
        }
        if revision < 2 {
            self.revisions = None;
        }
        if revision < 2 {
            self.scope_revision = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeQuery {
    pub entity_ids: Items<Id>,
    pub claim_ids: Items<Id>,
    pub predicates: Items<Label>,
    pub tags: Items<Label>,
    pub states: Items<KnowledgeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Text>,
    pub max_depth: Depth,
    pub page: PageRequest,
    pub entity_limit: Limit,
    pub claim_limit: Limit,
    pub evidence_limit: Limit,
    pub neighbor_limit: Limit,
    pub include_superseded: bool,
    pub include_conflicts: bool,
    pub include_supersession_chain: bool,
    pub include_evidence: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_min: Option<Probability>,
}
impl Check for KnowledgeQuery {
    fn check(&self) -> Result<(), String> {
        self.entity_ids.check()?;
        self.claim_ids.check()?;
        self.predicates.check()?;
        self.tags.check()?;
        self.states.check()?;
        self.text.check()?;
        self.max_depth.check()?;
        self.page.check()?;
        self.entity_limit.check()?;
        self.claim_limit.check()?;
        self.evidence_limit.check()?;
        self.neighbor_limit.check()?;
        self.include_superseded.check()?;
        self.include_conflicts.check()?;
        self.include_supersession_chain.check()?;
        self.include_evidence.check()?;
        self.confidence_min.check()?;
        super::domain_rules::record(
            "KnowledgeQuery",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeWriteReceipt {
    pub write_id: Id,
    pub state: OperationState,
    pub entity_ids: Items<Id>,
    pub claim_ids: Items<Id>,
    pub revision: Revision,
    pub diagnostics: Items<Diagnostic>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub entity_count: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub claim_count: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Cursor", extend("x-since" = 2))]
    pub next_cursor: Option<Cursor>,
}
impl Check for KnowledgeWriteReceipt {
    fn check(&self) -> Result<(), String> {
        self.write_id.check()?;
        self.state.check()?;
        self.entity_ids.check()?;
        self.claim_ids.check()?;
        self.revision.check()?;
        self.diagnostics.check()?;
        self.entity_count.check()?;
        self.claim_count.check()?;
        self.next_cursor.check()?;
        super::domain_rules::record(
            "KnowledgeWriteReceipt",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for KnowledgeWriteReceipt {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.entity_count = None;
        }
        if revision < 2 {
            self.claim_count = None;
        }
        if revision < 2 {
            self.next_cursor = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DreamRun {
    pub id: Id,
    pub revision: Revision,
    pub scope: DataScope,
    pub state: OperationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ArtifactRef>,
    pub proposal_ids: Items<Id>,
}
impl Check for DreamRun {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.scope.check()?;
        self.state.check()?;
        self.checkpoint.check()?;
        self.report.check()?;
        self.proposal_ids.check()?;
        super::domain_rules::record(
            "DreamRun",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DreamConfig {
    pub enabled: bool,
    pub instructions: ArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_id: Option<Id>,
    pub batch_limit: Limit,
}
impl Check for DreamConfig {
    fn check(&self) -> Result<(), String> {
        self.enabled.check()?;
        self.instructions.check()?;
        self.schedule_id.check()?;
        self.batch_limit.check()?;
        super::domain_rules::record(
            "DreamConfig",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentRecord {
    pub id: Id,
    pub revision: Revision,
    pub scope: DataScope,
    pub title: Label,
    pub current_version: ArtifactRef,
    pub retained_versions: Rows<ArtifactRef>,
    pub retrieval_enabled: bool,
    pub extraction_state: ItemState,
    pub coverage: Coverage,
}
impl Check for DocumentRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.scope.check()?;
        self.title.check()?;
        self.current_version.check()?;
        self.retained_versions.check()?;
        self.retrieval_enabled.check()?;
        self.extraction_state.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "DocumentRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstructionRecord {
    pub id: Id,
    pub revision: Revision,
    pub name: Label,
    pub scope: DataScope,
    pub content: ArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<RegisteredPath>,
    pub approved: bool,
    pub applicability: Items<Label>,
}
impl Check for InstructionRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.scope.check()?;
        self.content.check()?;
        self.source_path.check()?;
        self.approved.check()?;
        self.applicability.check()?;
        super::domain_rules::record(
            "InstructionRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalRecord {
    pub id: Id,
    pub revision: Revision,
    pub content_digest: Digest,
    pub state: ApprovalState,
    pub operation_kind: Label,
    pub target_id: Id,
    pub target_revision: Revision,
    pub summary: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}
impl Check for ProposalRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.content_digest.check()?;
        self.state.check()?;
        self.operation_kind.check()?;
        self.target_id.check()?;
        self.target_revision.check()?;
        self.summary.check()?;
        self.artifact.check()?;
        super::domain_rules::record(
            "ProposalRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotificationRecord {
    pub id: Id,
    pub revision: Revision,
    pub title: Label,
    pub body: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<RootId>,
    pub acknowledged: bool,
    pub created_at_ms: Counter,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 3))]
    pub category: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ProjectId", extend("x-since" = 3))]
    pub project_id: Option<ProjectId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub read: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 3))]
    pub source_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<NotificationLink>", extend("x-since" = 3))]
    pub links: Option<Items<NotificationLink>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub requires_input: Option<bool>,
}
impl Check for NotificationRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.title.check()?;
        self.body.check()?;
        self.decision_id.check()?;
        self.root_id.check()?;
        self.acknowledged.check()?;
        self.created_at_ms.check()?;
        self.category.check()?;
        self.project_id.check()?;
        self.read.check()?;
        self.source_id.check()?;
        self.links.check()?;
        self.requires_input.check()?;
        super::domain_rules::record(
            "NotificationRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for NotificationRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.category = None;
        }
        if revision < 3 {
            self.project_id = None;
        }
        if revision < 3 {
            self.read = None;
        }
        if revision < 3 {
            self.source_id = None;
        }
        if revision < 3 {
            self.links = None;
        }
        if revision < 3 {
            self.requires_input = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotificationConfig {
    pub desktop_enabled: bool,
    pub kinds: Items<Label>,
}
impl Check for NotificationConfig {
    fn check(&self) -> Result<(), String> {
        self.desktop_enabled.check()?;
        self.kinds.check()?;
        super::domain_rules::record(
            "NotificationConfig",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewRecord {
    pub id: Id,
    pub revision: Revision,
    pub root_id: RootId,
    pub state: ReviewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier: Option<ArtifactRef>,
    pub finding_ids: Items<Id>,
    pub head_digest: Digest,
    pub sources: Items<ReviewSourceEvidence>,
}
impl Check for ReviewRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.root_id.check()?;
        self.state.check()?;
        self.barrier.check()?;
        self.finding_ids.check()?;
        self.head_digest.check()?;
        self.sources.check()?;
        super::domain_rules::record(
            "ReviewRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewFinding {
    pub id: Id,
    pub revision: Revision,
    pub review_id: Id,
    pub summary: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Text>,
    pub severity: Severity,
    pub actionable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Content>,
    pub evidence: Items<ArtifactRef>,
}
impl Check for ReviewFinding {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.review_id.check()?;
        self.summary.check()?;
        self.location.check()?;
        self.severity.check()?;
        self.actionable.check()?;
        self.response.check()?;
        self.evidence.check()?;
        super::domain_rules::record(
            "ReviewFinding",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewReadiness {
    pub ready: bool,
    pub review_ids: Items<Id>,
    pub blocker_ids: Items<Id>,
    pub head_digest: Digest,
    pub policy_revision: Revision,
}
impl Check for ReviewReadiness {
    fn check(&self) -> Result<(), String> {
        self.ready.check()?;
        self.review_ids.check()?;
        self.blocker_ids.check()?;
        self.head_digest.check()?;
        self.policy_revision.check()?;
        super::domain_rules::record(
            "ReviewReadiness",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitInspection {
    pub repository_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<Id>,
    pub head: GitOid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<Text>,
    pub dirty: bool,
    pub state: OperationState,
    pub evidence: Items<ArtifactRef>,
}
impl Check for GitInspection {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.worktree_id.check()?;
        self.head.check()?;
        self.branch.check()?;
        self.dirty.check()?;
        self.state.check()?;
        self.evidence.check()?;
        super::domain_rules::record(
            "GitInspection",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitLink {
    pub repository_id: Id,
    pub work_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<Url>,
    pub head: GitOid,
}
impl Check for GitLink {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.work_id.check()?;
        self.worktree_id.check()?;
        self.branch.check()?;
        self.pull_request_url.check()?;
        self.head.check()?;
        super::domain_rules::record(
            "GitLink",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("x-earlier" = {"name": "GitLink", "before": 2}))]
pub struct AttributedGitLink {
    pub repository_id: Id,
    pub work_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<Url>,
    pub head: GitOid,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ActorId", extend("x-since" = 2))]
    pub actor_id: Option<ActorId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub occurred_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 3))]
    pub worktree_path: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub worktree_removed: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 3))]
    pub agent_session_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Url", extend("x-since" = 3))]
    pub repository_url: Option<Url>,
}
impl Check for AttributedGitLink {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.work_id.check()?;
        self.worktree_id.check()?;
        self.branch.check()?;
        self.pull_request_url.check()?;
        self.head.check()?;
        self.actor_id.check()?;
        self.occurred_at_ms.check()?;
        self.worktree_path.check()?;
        self.worktree_removed.check()?;
        self.agent_session_id.check()?;
        self.repository_url.check()?;
        super::domain_rules::record(
            "AttributedGitLink",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for AttributedGitLink {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.actor_id = None;
        }
        if revision < 2 {
            self.occurred_at_ms = None;
        }
        if revision < 3 {
            self.worktree_path = None;
        }
        if revision < 3 {
            self.worktree_removed = None;
        }
        if revision < 3 {
            self.agent_session_id = None;
        }
        if revision < 3 {
            self.repository_url = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseRecord {
    pub id: Id,
    pub revision: Revision,
    pub root_id: RootId,
    pub state: OperationState,
    pub version: Label,
    pub artifact_ids: Items<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_url: Option<Url>,
    pub deployment_state: OperationState,
}
impl Check for ReleaseRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.root_id.check()?;
        self.state.check()?;
        self.version.check()?;
        self.artifact_ids.check()?;
        self.release_url.check()?;
        self.deployment_state.check()?;
        super::domain_rules::record(
            "ReleaseRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GithubAccount {
    pub id: Id,
    pub login: Label,
    pub state: ItemState,
    pub permission_revision: Revision,
}
impl Check for GithubAccount {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.login.check()?;
        self.state.check()?;
        self.permission_revision.check()?;
        super::domain_rules::record(
            "GithubAccount",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GithubObservation {
    pub repository_id: Id,
    pub subject_id: Id,
    pub kind: Label,
    pub state: Label,
    pub revision: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}
impl Check for GithubObservation {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.subject_id.check()?;
        self.kind.check()?;
        self.state.check()?;
        self.revision.check()?;
        self.url.check()?;
        self.artifact.check()?;
        super::domain_rules::record(
            "GithubObservation",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceRecord {
    pub id: ResourceId,
    pub canonical_key: Text,
    pub revision: Revision,
    pub capacity: Limit,
    pub holders: Items<Id>,
    pub waiters: Items<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ResourceScope", extend("x-since" = 2))]
    pub scope: Option<ResourceScope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ProjectId", extend("x-since" = 2))]
    pub project_id: Option<ProjectId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub physical: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 2))]
    pub labels: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub used: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub available: Option<bool>,
}
impl Check for ResourceRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.canonical_key.check()?;
        self.revision.check()?;
        self.capacity.check()?;
        self.holders.check()?;
        self.waiters.check()?;
        self.scope.check()?;
        self.project_id.check()?;
        self.physical.check()?;
        self.labels.check()?;
        self.used.check()?;
        self.available.check()?;
        super::domain_rules::record(
            "ResourceRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for ResourceRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.scope = None;
        }
        if revision < 2 {
            self.project_id = None;
        }
        if revision < 2 {
            self.physical = None;
        }
        if revision < 2 {
            self.labels = None;
        }
        if revision < 2 {
            self.used = None;
        }
        if revision < 2 {
            self.available = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseRecord {
    pub id: Id,
    pub resource_id: ResourceId,
    pub owner_id: ActorId,
    pub fence: Counter,
    pub expires_at_ms: Counter,
    pub state: LeaseState,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Limit", extend("x-since" = 2))]
    pub units: Option<Limit>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub purpose: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ResourcePriority", extend("x-since" = 2))]
    pub priority: Option<ResourcePriority>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub queue_position: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub requested_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub granted_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub renewed_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub ended_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "LeaseEnd", extend("x-since" = 2))]
    pub end_reason: Option<LeaseEnd>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub owner_unreachable_since_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ProjectId", extend("x-since" = 2))]
    pub project_id: Option<ProjectId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "RootId", extend("x-since" = 2))]
    pub root_id: Option<RootId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "AssignmentId", extend("x-since" = 2))]
    pub assignment_id: Option<AssignmentId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub session_id: Option<Id>,
}
impl Check for LeaseRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.resource_id.check()?;
        self.owner_id.check()?;
        self.fence.check()?;
        self.expires_at_ms.check()?;
        self.state.check()?;
        self.units.check()?;
        self.purpose.check()?;
        self.priority.check()?;
        self.queue_position.check()?;
        self.requested_at_ms.check()?;
        self.granted_at_ms.check()?;
        self.renewed_at_ms.check()?;
        self.ended_at_ms.check()?;
        self.end_reason.check()?;
        self.owner_unreachable_since_ms.check()?;
        self.project_id.check()?;
        self.root_id.check()?;
        self.assignment_id.check()?;
        self.session_id.check()?;
        super::domain_rules::record(
            "LeaseRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for LeaseRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.units = None;
        }
        if revision < 2 {
            self.purpose = None;
        }
        if revision < 2 {
            self.priority = None;
        }
        if revision < 2 {
            self.queue_position = None;
        }
        if revision < 2 {
            self.requested_at_ms = None;
        }
        if revision < 2 {
            self.granted_at_ms = None;
        }
        if revision < 2 {
            self.renewed_at_ms = None;
        }
        if revision < 2 {
            self.ended_at_ms = None;
        }
        if revision < 2 {
            self.end_reason = None;
        }
        if revision < 2 {
            self.owner_unreachable_since_ms = None;
        }
        if revision < 2 {
            self.project_id = None;
        }
        if revision < 2 {
            self.root_id = None;
        }
        if revision < 2 {
            self.assignment_id = None;
        }
        if revision < 2 {
            self.session_id = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSession {
    pub id: Id,
    pub revision: Revision,
    pub state: ItemState,
    pub target_ids: Items<Id>,
    pub process_owner_id: Id,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "TestedRepositoryRevision", extend("x-since" = 3))]
    pub tested_repository: Option<TestedRepositoryRevision>,
}
impl Check for BrowserSession {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.state.check()?;
        self.target_ids.check()?;
        self.process_owner_id.check()?;
        self.tested_repository.check()?;
        super::domain_rules::record(
            "BrowserSession",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for BrowserSession {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.tested_repository = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserTarget {
    pub session_id: Id,
    pub target_id: Id,
    pub frame_id: Id,
    pub document_revision: Revision,
}
impl Check for BrowserTarget {
    fn check(&self) -> Result<(), String> {
        self.session_id.check()?;
        self.target_id.check()?;
        self.frame_id.check()?;
        self.document_revision.check()?;
        super::domain_rules::record(
            "BrowserTarget",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserNode {
    pub target: BrowserTarget,
    pub node_id: Id,
    pub snapshot_revision: Revision,
}
impl Check for BrowserNode {
    fn check(&self) -> Result<(), String> {
        self.target.check()?;
        self.node_id.check()?;
        self.snapshot_revision.check()?;
        super::domain_rules::record(
            "BrowserNode",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum BrowserInput {
    Click {
        node: BrowserNode,
        button: MouseButton,
    },
    Text {
        node: BrowserNode,
        text: Content,
    },
    Key {
        target: BrowserTarget,
        key: Label,
        modifiers: Items<KeyModifier>,
    },
    Scroll {
        target: BrowserTarget,
        x: i32,
        y: i32,
    },
}
impl Check for BrowserInput {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Click { node, button } => {
                node.check()?;
                button.check()?;
            }
            Self::Text { node, text } => {
                node.check()?;
                text.check()?;
            }
            Self::Key {
                target,
                key,
                modifiers,
            } => {
                target.check()?;
                key.check()?;
                modifiers.check()?;
            }
            Self::Scroll { target, x, y } => {
                target.check()?;
                x.check()?;
                y.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum MouseButton {
    Primary,
    Secondary,
    Middle,
}
impl Check for MouseButton {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum KeyModifier {
    Shift,
    Control,
    Alt,
    Meta,
}
impl Check for KeyModifier {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserSnapshot {
    pub target: BrowserTarget,
    pub url: Text,
    pub title: Label,
    pub accessibility: ArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ArtifactRef>,
}
impl Check for BrowserSnapshot {
    fn check(&self) -> Result<(), String> {
        self.target.check()?;
        self.url.check()?;
        self.title.check()?;
        self.accessibility.check()?;
        self.screenshot.check()?;
        super::domain_rules::record(
            "BrowserSnapshot",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserObservation {
    pub session_id: Id,
    pub sequence: Counter,
    pub evidence: ArtifactRef,
    pub truncated: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "TestedRepositoryRevision", extend("x-since" = 3))]
    pub tested_repository: Option<TestedRepositoryRevision>,
}
impl Check for BrowserObservation {
    fn check(&self) -> Result<(), String> {
        self.session_id.check()?;
        self.sequence.check()?;
        self.evidence.check()?;
        self.truncated.check()?;
        self.tested_repository.check()?;
        super::domain_rules::record(
            "BrowserObservation",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for BrowserObservation {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.tested_repository = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopRecord {
    pub desktop_id: Id,
    pub revision: Revision,
    pub state: ItemState,
    pub permission_state: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_id: Option<Id>,
}
impl Check for DesktopRecord {
    fn check(&self) -> Result<(), String> {
        self.desktop_id.check()?;
        self.revision.check()?;
        self.state.check()?;
        self.permission_state.check()?;
        self.lease_id.check()?;
        self.consent_id.check()?;
        super::domain_rules::record(
            "DesktopRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopCapture {
    pub desktop: DesktopRecord,
    pub image: ArtifactRef,
    pub captured_at_ms: Counter,
}
impl Check for DesktopCapture {
    fn check(&self) -> Result<(), String> {
        self.desktop.check()?;
        self.image.check()?;
        self.captured_at_ms.check()?;
        super::domain_rules::record(
            "DesktopCapture",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DesktopInput {
    Click {
        x: i32,
        y: i32,
        button: MouseButton,
    },
    Text {
        text: Content,
    },
    Key {
        key: Label,
        modifiers: Items<KeyModifier>,
    },
    Scroll {
        x: i32,
        y: i32,
    },
}
impl Check for DesktopInput {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Click { x, y, button } => {
                x.check()?;
                y.check()?;
                button.check()?;
            }
            Self::Text { text } => {
                text.check()?;
            }
            Self::Key { key, modifiers } => {
                key.check()?;
                modifiers.check()?;
            }
            Self::Scroll { x, y } => {
                x.check()?;
                y.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvironmentEntry {
    pub name: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<Id>,
}
impl Check for EnvironmentEntry {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.value.check()?;
        self.credential_ref.check()?;
        super::domain_rules::record(
            "EnvironmentEntry",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessSpec {
    pub executable_ref: Id,
    pub args: Items<Text>,
    pub working_directory: RegisteredPath,
    pub environment: Items<EnvironmentEntry>,
    pub profile: ToolProfile,
    pub timeout_ms: Counter,
    pub output_limit_bytes: Counter,
}
impl Check for ProcessSpec {
    fn check(&self) -> Result<(), String> {
        self.executable_ref.check()?;
        self.args.check()?;
        self.working_directory.check()?;
        self.environment.check()?;
        self.profile.check()?;
        self.timeout_ms.check()?;
        self.output_limit_bytes.check()?;
        super::domain_rules::record(
            "ProcessSpec",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessRecord {
    pub id: Id,
    pub revision: Revision,
    pub state: OperationState,
    pub owner_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ArtifactRef>,
    pub truncated: bool,
}
impl Check for ProcessRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.state.check()?;
        self.owner_id.check()?;
        self.exit_code.check()?;
        self.output.check()?;
        self.truncated.check()?;
        super::domain_rules::record(
            "ProcessRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebResult {
    pub title: Label,
    pub url: Url,
    pub snippet: Content,
    pub observed_at_ms: Counter,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "TrustLevel", extend("x-since" = 2))]
    pub trust: Option<TrustLevel>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub provider_id: Option<Id>,
}
impl Check for WebResult {
    fn check(&self) -> Result<(), String> {
        self.title.check()?;
        self.url.check()?;
        self.snippet.check()?;
        self.observed_at_ms.check()?;
        self.trust.check()?;
        self.provider_id.check()?;
        super::domain_rules::record(
            "WebResult",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for WebResult {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.trust = None;
        }
        if revision < 2 {
            self.provider_id = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebRedirect {
    pub url: Url,
    pub status: i32,
}
impl Check for WebRedirect {
    fn check(&self) -> Result<(), String> {
        self.url.check()?;
        self.status.check()?;
        super::domain_rules::record(
            "WebRedirect",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebDocument {
    pub url: Url,
    pub final_url: Url,
    pub content: ArtifactRef,
    pub media_type: Label,
    pub observed_at_ms: Counter,
    pub truncated: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "TrustLevel", extend("x-since" = 2))]
    pub trust: Option<TrustLevel>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "i32", extend("x-since" = 2))]
    pub status: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<WebRedirect>", extend("x-since" = 2))]
    pub redirects: Option<Items<WebRedirect>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Digest", extend("x-since" = 2))]
    pub source_sha256: Option<Digest>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub source_byte_length: Option<Counter>,
}
impl Check for WebDocument {
    fn check(&self) -> Result<(), String> {
        self.url.check()?;
        self.final_url.check()?;
        self.content.check()?;
        self.media_type.check()?;
        self.observed_at_ms.check()?;
        self.truncated.check()?;
        self.trust.check()?;
        self.status.check()?;
        self.redirects.check()?;
        self.source_sha256.check()?;
        self.source_byte_length.check()?;
        super::domain_rules::record(
            "WebDocument",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for WebDocument {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.trust = None;
        }
        if revision < 2 {
            self.status = None;
        }
        if revision < 2 {
            self.redirects = None;
        }
        if revision < 2 {
            self.source_sha256 = None;
        }
        if revision < 2 {
            self.source_byte_length = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerCapability {
    pub engine_id: Id,
    pub state: ItemState,
    pub capabilities: Items<Label>,
    pub diagnostics: Items<Diagnostic>,
}
impl Check for ContainerCapability {
    fn check(&self) -> Result<(), String> {
        self.engine_id.check()?;
        self.state.check()?;
        self.capabilities.check()?;
        self.diagnostics.check()?;
        super::domain_rules::record(
            "ContainerCapability",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerSpec {
    pub engine_id: Id,
    pub project_id: ProjectId,
    pub image: Text,
    pub command: Items<Text>,
    pub mounts: Items<ContainerMount>,
    pub ports: Items<PortMapping>,
    pub environment: Items<EnvironmentEntry>,
    pub resource_ids: Items<ResourceId>,
}
impl Check for ContainerSpec {
    fn check(&self) -> Result<(), String> {
        self.engine_id.check()?;
        self.project_id.check()?;
        self.image.check()?;
        self.command.check()?;
        self.mounts.check()?;
        self.ports.check()?;
        self.environment.check()?;
        self.resource_ids.check()?;
        super::domain_rules::record(
            "ContainerSpec",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerMount {
    pub source: RegisteredPath,
    pub destination: Text,
    pub read_only: bool,
}
impl Check for ContainerMount {
    fn check(&self) -> Result<(), String> {
        self.source.check()?;
        self.destination.check()?;
        self.read_only.check()?;
        super::domain_rules::record(
            "ContainerMount",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortMapping {
    pub container_port: Port,
    pub host_port: Port,
    pub protocol: PortProtocol,
}
impl Check for PortMapping {
    fn check(&self) -> Result<(), String> {
        self.container_port.check()?;
        self.host_port.check()?;
        self.protocol.check()?;
        super::domain_rules::record(
            "PortMapping",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PortProtocol {
    Tcp,
    Udp,
}
impl Check for PortProtocol {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerRecord {
    pub id: Id,
    pub revision: Revision,
    pub engine_id: Id,
    pub owned: bool,
    pub state: ItemState,
    pub image: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_owner_id: Option<Id>,
}
impl Check for ContainerRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.engine_id.check()?;
        self.owned.check()?;
        self.state.check()?;
        self.image.check()?;
        self.process_owner_id.check()?;
        super::domain_rules::record(
            "ContainerRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Schedule {
    Manual {},
    Interval { seconds: PositiveCounter },
    Calendar { expression: Text, timezone: Label },
}
impl Check for Schedule {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Manual {} => {}
            Self::Interval { seconds } => {
                seconds.check()?;
            }
            Self::Calendar {
                expression,
                timezone,
            } => {
                expression.check()?;
                timezone.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum JobDefinition {
    Native {
        capability_id: Id,
        arguments: SchemaValue,
    },
    Inference {
        prompt: ArtifactRef,
        route: RouteIntent,
        effort: EffortName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feature_id: Option<RootId>,
    },
}
impl Check for JobDefinition {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Native {
                capability_id,
                arguments,
            } => {
                capability_id.check()?;
                arguments.check()?;
            }
            Self::Inference {
                prompt,
                route,
                effort,
                project_id,
                feature_id,
            } => {
                prompt.check()?;
                route.check()?;
                effort.check()?;
                project_id.check()?;
                feature_id.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRecord {
    pub id: Id,
    pub revision: Revision,
    pub title: Label,
    pub schedule: Schedule,
    pub definition: JobDefinition,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_run_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "DataScope", extend("x-since" = 2))]
    pub scope: Option<DataScope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub next_run_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub last_run_started_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub last_run_finished_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "JobRunStatus", extend("x-since" = 2))]
    pub last_run_status: Option<JobRunStatus>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub last_error: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub active_run_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "JobHold", extend("x-since" = 2))]
    pub hold: Option<JobHold>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub authorized: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub created_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub updated_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub deleted_at_ms: Option<Counter>,
}
impl Check for JobRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.title.check()?;
        self.schedule.check()?;
        self.definition.check()?;
        self.enabled.check()?;
        self.interrupted_run_id.check()?;
        self.last_run_id.check()?;
        self.scope.check()?;
        self.next_run_at_ms.check()?;
        self.last_run_started_at_ms.check()?;
        self.last_run_finished_at_ms.check()?;
        self.last_run_status.check()?;
        self.last_error.check()?;
        self.active_run_id.check()?;
        self.hold.check()?;
        self.authorized.check()?;
        self.created_at_ms.check()?;
        self.updated_at_ms.check()?;
        self.deleted_at_ms.check()?;
        super::domain_rules::record(
            "JobRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for JobRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.scope = None;
        }
        if revision < 2 {
            self.next_run_at_ms = None;
        }
        if revision < 2 {
            self.last_run_started_at_ms = None;
        }
        if revision < 2 {
            self.last_run_finished_at_ms = None;
        }
        if revision < 2 {
            self.last_run_status = None;
        }
        if revision < 2 {
            self.last_error = None;
        }
        if revision < 2 {
            self.active_run_id = None;
        }
        if revision < 2 {
            self.hold = None;
        }
        if revision < 2 {
            self.authorized = None;
        }
        if revision < 2 {
            self.created_at_ms = None;
        }
        if revision < 2 {
            self.updated_at_ms = None;
        }
        if revision < 2 {
            self.deleted_at_ms = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobRun {
    pub id: Id,
    pub job_id: Id,
    pub revision: Revision,
    pub occurrence_key: Text,
    pub state: OperationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub usage: UsageReport,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "JobRunStatus", extend("x-since" = 2))]
    pub status: Option<JobRunStatus>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "JobTrigger", extend("x-since" = 2))]
    pub trigger: Option<JobTrigger>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub occurrence_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub started_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub finished_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub reason: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub hold: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub cancel_requested: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub resumptions: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub session_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<RootId>", extend("x-since" = 2))]
    pub linked_root_ids: Option<Items<RootId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub skipped_for_run_id: Option<Id>,
}
impl Check for JobRun {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.job_id.check()?;
        self.revision.check()?;
        self.occurrence_key.check()?;
        self.state.check()?;
        self.operation_id.check()?;
        self.usage.check()?;
        self.status.check()?;
        self.trigger.check()?;
        self.occurrence_at_ms.check()?;
        self.started_at_ms.check()?;
        self.finished_at_ms.check()?;
        self.reason.check()?;
        self.hold.check()?;
        self.cancel_requested.check()?;
        self.resumptions.check()?;
        self.session_id.check()?;
        self.linked_root_ids.check()?;
        self.skipped_for_run_id.check()?;
        super::domain_rules::record(
            "JobRun",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for JobRun {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.status = None;
        }
        if revision < 2 {
            self.trigger = None;
        }
        if revision < 2 {
            self.occurrence_at_ms = None;
        }
        if revision < 2 {
            self.started_at_ms = None;
        }
        if revision < 2 {
            self.finished_at_ms = None;
        }
        if revision < 2 {
            self.reason = None;
        }
        if revision < 2 {
            self.hold = None;
        }
        if revision < 2 {
            self.cancel_requested = None;
        }
        if revision < 2 {
            self.resumptions = None;
        }
        if revision < 2 {
            self.session_id = None;
        }
        if revision < 2 {
            self.linked_root_ids = None;
        }
        if revision < 2 {
            self.skipped_for_run_id = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaValue {
    pub schema: SchemaRef,
    pub value: DynamicJson,
}
impl Check for SchemaValue {
    fn check(&self) -> Result<(), String> {
        self.schema.check()?;
        self.value.check()?;
        super::domain_rules::record(
            "SchemaValue",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingEntry {
    pub key: Label,
    pub value: SchemaValue,
    pub source_scope: DataScope,
    pub revision: Revision,
}
impl Check for SettingEntry {
    fn check(&self) -> Result<(), String> {
        self.key.check()?;
        self.value.check()?;
        self.source_scope.check()?;
        self.revision.check()?;
        super::domain_rules::record(
            "SettingEntry",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationIssue {
    pub path: Text,
    pub code: Label,
    pub message: Content,
}
impl Check for ValidationIssue {
    fn check(&self) -> Result<(), String> {
        self.path.check()?;
        self.code.check()?;
        self.message.check()?;
        super::domain_rules::record(
            "ValidationIssue",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationReport {
    pub valid: bool,
    pub issues: Rows<ValidationIssue>,
}
impl Check for ValidationReport {
    fn check(&self) -> Result<(), String> {
        self.valid.check()?;
        self.issues.check()?;
        super::domain_rules::record(
            "ValidationReport",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum RouteIntent {
    Tier { tier: TierName },
    Model { model_id: Id },
    OpenRouterAuto {},
}
impl Check for RouteIntent {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Tier { tier } => {
                tier.check()?;
            }
            Self::Model { model_id } => {
                model_id.check()?;
            }
            Self::OpenRouterAuto {} => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteModel {
    pub id: Id,
    pub display_name: Label,
    pub endpoint_profile: EndpointKind,
    pub upstream_model_id: Text,
    pub enabled: bool,
    pub tier_memberships: Items<TierName>,
    pub efforts: Items<EffortName>,
    pub image_input: bool,
    pub tools: bool,
    pub revision: Revision,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "PositiveCounter", extend("x-since" = 2))]
    pub context_tokens: Option<PositiveCounter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub connection_id: Option<Id>,
}
impl Check for RouteModel {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.display_name.check()?;
        self.endpoint_profile.check()?;
        self.upstream_model_id.check()?;
        self.enabled.check()?;
        self.tier_memberships.check()?;
        self.efforts.check()?;
        self.image_input.check()?;
        self.tools.check()?;
        self.revision.check()?;
        self.context_tokens.check()?;
        self.connection_id.check()?;
        super::domain_rules::record(
            "RouteModel",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for RouteModel {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.context_tokens = None;
        }
        if revision < 2 {
            self.connection_id = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionRecord {
    pub id: Id,
    pub revision: Revision,
    pub name: Label,
    pub profile: EndpointKind,
    pub state: ItemState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<Id>,
    pub subscription_risk_accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_notice_sha256: Option<Digest>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Url", extend("x-since" = 2))]
    pub endpoint: Option<Url>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ConnectionLifecycle", extend("x-since" = 2))]
    pub lifecycle: Option<ConnectionLifecycle>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub state_reason: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub credential_revision: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub healthy: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub checked_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub discovered_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub discovered_model_count: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Text>", extend("x-since" = 2))]
    pub discovered_model_ids: Option<Items<Text>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub access_expires_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "SignInChallenge", extend("x-since" = 2))]
    pub sign_in: Option<SignInChallenge>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "OpenRouterConstraints", extend("x-since" = 2))]
    pub open_router: Option<OpenRouterConstraints>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub local_free: Option<bool>,
}
impl Check for ConnectionRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.profile.check()?;
        self.state.check()?;
        self.credential_ref.check()?;
        self.subscription_risk_accepted.check()?;
        self.risk_notice_sha256.check()?;
        self.endpoint.check()?;
        self.lifecycle.check()?;
        self.state_reason.check()?;
        self.credential_revision.check()?;
        self.healthy.check()?;
        self.checked_at_ms.check()?;
        self.discovered_at_ms.check()?;
        self.discovered_model_count.check()?;
        self.discovered_model_ids.check()?;
        self.access_expires_at_ms.check()?;
        self.sign_in.check()?;
        self.enabled.check()?;
        self.open_router.check()?;
        self.local_free.check()?;
        super::domain_rules::record(
            "ConnectionRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for ConnectionRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.endpoint = None;
        }
        if revision < 2 {
            self.lifecycle = None;
        }
        if revision < 2 {
            self.state_reason = None;
        }
        if revision < 2 {
            self.credential_revision = None;
        }
        if revision < 2 {
            self.healthy = None;
        }
        if revision < 2 {
            self.checked_at_ms = None;
        }
        if revision < 2 {
            self.discovered_at_ms = None;
        }
        if revision < 2 {
            self.discovered_model_count = None;
        }
        if revision < 2 {
            self.discovered_model_ids = None;
        }
        if revision < 2 {
            self.access_expires_at_ms = None;
        }
        if revision < 2 {
            self.sign_in = None;
        }
        if revision < 2 {
            self.enabled = None;
        }
        if revision < 2 {
            self.open_router = None;
        }
        if revision < 3 {
            self.local_free = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteChoice {
    pub model_id: Id,
    pub connection_id: Id,
    pub intent: RouteIntent,
    pub requested_effort: EffortName,
    pub effective_effort: EffortName,
    pub pin_id: Id,
    pub pin_revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_pin_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 3))]
    pub resolved_upstream_model_id: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "EndpointKind", extend("x-since" = 3))]
    pub resolved_endpoint_profile: Option<EndpointKind>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 3))]
    pub resolved_identity_origin: Option<Label>,
}
impl Check for RouteChoice {
    fn check(&self) -> Result<(), String> {
        self.model_id.check()?;
        self.connection_id.check()?;
        self.intent.check()?;
        self.requested_effort.check()?;
        self.effective_effort.check()?;
        self.pin_id.check()?;
        self.pin_revision.check()?;
        self.replaced_pin_id.check()?;
        self.resolved_upstream_model_id.check()?;
        self.resolved_endpoint_profile.check()?;
        self.resolved_identity_origin.check()?;
        super::domain_rules::record(
            "RouteChoice",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for RouteChoice {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.resolved_upstream_model_id = None;
        }
        if revision < 3 {
            self.resolved_endpoint_profile = None;
        }
        if revision < 3 {
            self.resolved_identity_origin = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteAvailability {
    pub intent: RouteIntent,
    pub available: bool,
    pub eligible_model_ids: Items<Id>,
    pub blocked_reason_codes: Items<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "RouteChoice", extend("x-since" = 2))]
    pub pin: Option<RouteChoice>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub pin_eligible: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub pin_reason: Option<Label>,
}
impl Check for RouteAvailability {
    fn check(&self) -> Result<(), String> {
        self.intent.check()?;
        self.available.check()?;
        self.eligible_model_ids.check()?;
        self.blocked_reason_codes.check()?;
        self.retry_at_ms.check()?;
        self.pin.check()?;
        self.pin_eligible.check()?;
        self.pin_reason.check()?;
        super::domain_rules::record(
            "RouteAvailability",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for RouteAvailability {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.pin = None;
        } else {
            self.pin.retain_revision(revision);
        }
        if revision < 2 {
            self.pin_eligible = None;
        }
        if revision < 2 {
            self.pin_reason = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageItem {
    pub request_id: Id,
    pub purpose: Label,
    pub model_id: Id,
    pub input_tokens: Counter,
    pub output_tokens: Counter,
    pub cache_read_tokens: Counter,
    pub cache_write_tokens: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_usd: Option<Money>,
    pub price_kind: UsagePriceKind,
    pub price_origin: PriceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_revision: Option<Revision>,
    pub incomplete: bool,
}
impl Check for UsageItem {
    fn check(&self) -> Result<(), String> {
        self.request_id.check()?;
        self.purpose.check()?;
        self.model_id.check()?;
        self.input_tokens.check()?;
        self.output_tokens.check()?;
        self.cache_read_tokens.check()?;
        self.cache_write_tokens.check()?;
        self.amount_usd.check()?;
        self.price_kind.check()?;
        self.price_origin.check()?;
        self.price_revision.check()?;
        self.incomplete.check()?;
        super::domain_rules::record(
            "UsageItem",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ToolInvocationOutcome {
    Completed,
    Failed,
    ApprovalRequired,
    DependencyBlocked,
    CancelledBeforeDispatch,
    OutcomeUnknown,
}
impl Check for ToolInvocationOutcome {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolInvocation {
    pub sequence: Revision,
    pub call_id: Id,
    pub session_id: Id,
    pub tool_name: Label,
    pub outcome: ToolInvocationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub actor_id: ActorId,
    pub occurred_at_ms: Counter,
}
impl Check for ToolInvocation {
    fn check(&self) -> Result<(), String> {
        self.sequence.check()?;
        self.call_id.check()?;
        self.session_id.check()?;
        self.tool_name.check()?;
        self.outcome.check()?;
        self.duration_ms.check()?;
        self.error_code.check()?;
        self.operation_id.check()?;
        self.actor_id.check()?;
        self.occurred_at_ms.check()?;
        super::domain_rules::record(
            "ToolInvocation",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(rename = "Page")]
pub struct UsagePage {
    pub items: Rows<UsageItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Cursor", extend("x-since" = 2))]
    pub checkpoint: Option<Cursor>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Rows<Revision>", extend("x-since" = 2))]
    pub item_sequences: Option<Rows<Revision>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Rows<ToolInvocation>", extend("x-since" = 2))]
    pub tool_invocations: Option<Rows<ToolInvocation>>,
}
impl Check for UsagePage {
    fn check(&self) -> Result<(), String> {
        self.items.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        self.checkpoint.check()?;
        self.item_sequences.check()?;
        self.tool_invocations.check()?;
        super::domain_rules::record(
            "UsagePage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for UsagePage {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.checkpoint = None;
        }
        if revision < 2 {
            self.item_sequences = None;
        }
        if revision < 2 {
            self.tool_invocations = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageReport {
    pub items: Rows<UsageItem>,
    pub total_tracked_usd: Money,
    pub untracked_requests: Counter,
    pub incomplete_requests: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
}
impl Check for UsageReport {
    fn check(&self) -> Result<(), String> {
        self.items.check()?;
        self.total_tracked_usd.check()?;
        self.untracked_requests.check()?;
        self.incomplete_requests.check()?;
        self.next_cursor.check()?;
        super::domain_rules::record(
            "UsageReport",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceRecord {
    pub model_id: Id,
    pub revision: Revision,
    pub input_usd_per_million: Money,
    pub output_usd_per_million: Money,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_per_million: Option<Money>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_per_million: Option<Money>,
    pub origin: PriceOrigin,
    pub observed_at_ms: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<Url>,
}
impl Check for PriceRecord {
    fn check(&self) -> Result<(), String> {
        self.model_id.check()?;
        self.revision.check()?;
        self.input_usd_per_million.check()?;
        self.output_usd_per_million.check()?;
        self.cache_read_usd_per_million.check()?;
        self.cache_write_usd_per_million.check()?;
        self.origin.check()?;
        self.observed_at_ms.check()?;
        self.source_url.check()?;
        super::domain_rules::record(
            "PriceRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionModel {
    pub id: Id,
    pub revision: Revision,
    pub display_name: Label,
    pub connection_id: Id,
    pub upstream_model_id: Text,
    pub question_types: Items<DecisionQuestionType>,
    pub accepts_images: bool,
    pub accepts_videos: bool,
    pub enabled: bool,
    pub extension_question_schemas: Items<SchemaRef>,
    pub result_schema: SchemaRef,
}
impl Check for DecisionModel {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.display_name.check()?;
        self.connection_id.check()?;
        self.upstream_model_id.check()?;
        self.question_types.check()?;
        self.accepts_images.check()?;
        self.accepts_videos.check()?;
        self.enabled.check()?;
        self.extension_question_schemas.check()?;
        self.result_schema.check()?;
        super::domain_rules::record(
            "DecisionModel",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionCriterion {
    pub name: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Content>,
}
impl Check for DecisionCriterion {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.description.check()?;
        super::domain_rules::record(
            "DecisionCriterion",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DecisionQuestion {
    Choice {
        instructions: Content,
        criteria: NonEmpty<DecisionCriterion>,
    },
    Noul {
        instructions: Content,
    },
    Score {
        instructions: Content,
        criteria: NonEmpty<Label>,
    },
    Box {
        instructions: Content,
    },
    Extension {
        schema: SchemaRef,
        payload: DynamicJson,
    },
}
impl Check for DecisionQuestion {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Choice {
                instructions,
                criteria,
            } => {
                instructions.check()?;
                criteria.check()?;
            }
            Self::Noul { instructions } => {
                instructions.check()?;
            }
            Self::Score {
                instructions,
                criteria,
            } => {
                instructions.check()?;
                criteria.check()?;
            }
            Self::Box { instructions } => {
                instructions.check()?;
            }
            Self::Extension { schema, payload } => {
                schema.check()?;
                payload.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamedQuestion {
    pub name: Label,
    pub question: DecisionQuestion,
}
impl Check for NamedQuestion {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.question.check()?;
        super::domain_rules::record(
            "NamedQuestion",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum MediaRef {
    Artifact { artifact: ArtifactRef },
    Remote { url: Url, media_type: Label },
}
impl Check for MediaRef {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Artifact { artifact } => {
                artifact.check()?;
            }
            Self::Remote { url, media_type } => {
                url.check()?;
                media_type.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionResult {
    pub model_id: Id,
    pub request_id: Id,
    pub output: SchemaValue,
    pub raw_response: ArtifactRef,
    pub usage: UsageReport,
}
impl Check for DecisionResult {
    fn check(&self) -> Result<(), String> {
        self.model_id.check()?;
        self.request_id.check()?;
        self.output.check()?;
        self.raw_response.check()?;
        self.usage.check()?;
        super::domain_rules::record(
            "DecisionResult",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServer {
    pub id: Id,
    pub revision: Revision,
    pub name: Label,
    pub state: ItemState,
    pub transport: Label,
    pub selected_tool_ids: Items<Id>,
    pub catalog_revision: Revision,
}
impl Check for McpServer {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.state.check()?;
        self.transport.check()?;
        self.selected_tool_ids.check()?;
        self.catalog_revision.check()?;
        super::domain_rules::record(
            "McpServer",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum McpTransport {
    Stdio {
        process: ProcessSpec,
    },
    Http {
        endpoint: Url,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_ref: Option<Id>,
    },
}
impl Check for McpTransport {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Stdio { process } => {
                process.check()?;
            }
            Self::Http {
                endpoint,
                credential_ref,
            } => {
                endpoint.check()?;
                credential_ref.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpCapability {
    pub id: Id,
    pub server_id: Id,
    pub kind: Label,
    pub name: Label,
    pub description: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<SchemaRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<SchemaRef>,
    pub read_only: bool,
    pub revision: Revision,
}
impl Check for McpCapability {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.server_id.check()?;
        self.kind.check()?;
        self.name.check()?;
        self.description.check()?;
        self.input_schema.check()?;
        self.output_schema.check()?;
        self.read_only.check()?;
        self.revision.check()?;
        super::domain_rules::record(
            "McpCapability",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpResource {
    pub server_id: Id,
    pub uri: Text,
    pub content: ArtifactRef,
    pub catalog_revision: Revision,
}
impl Check for McpResource {
    fn check(&self) -> Result<(), String> {
        self.server_id.check()?;
        self.uri.check()?;
        self.content.check()?;
        self.catalog_revision.check()?;
        super::domain_rules::record(
            "McpResource",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpPrompt {
    pub server_id: Id,
    pub name: Label,
    pub messages: ArtifactRef,
    pub catalog_revision: Revision,
}
impl Check for McpPrompt {
    fn check(&self) -> Result<(), String> {
        self.server_id.check()?;
        self.name.check()?;
        self.messages.check()?;
        self.catalog_revision.check()?;
        super::domain_rules::record(
            "McpPrompt",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageRecord {
    pub id: Id,
    pub version: Label,
    pub digest: Digest,
    pub revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher_id: Option<Id>,
    pub enabled: bool,
    pub trust_state: Label,
    pub contribution_ids: Items<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 2))]
    pub registry_revision: Option<Revision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "PackageAvailability", extend("x-since" = 2))]
    pub availability: Option<PackageAvailability>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Text", extend("x-since" = 2))]
    pub availability_reason: Option<Text>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub package_kind: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 2))]
    pub languages: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 2))]
    pub targets: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub host_target_supported: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<PackagePrerequisite>", extend("x-since" = 2))]
    pub prerequisites: Option<Items<PackagePrerequisite>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 2))]
    pub requested_access: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 2))]
    pub effective_access: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 2))]
    pub configuration_revision: Option<Revision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "SchemaRef", extend("x-since" = 2))]
    pub configuration_schema: Option<SchemaRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub state_generation: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub pending_version: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Digest", extend("x-since" = 2))]
    pub pending_digest: Option<Digest>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<PackageWaitReason>", extend("x-since" = 2))]
    pub pending_waits: Option<Items<PackageWaitReason>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub pending_proposal_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub predecessor_version: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Digest", extend("x-since" = 2))]
    pub predecessor_digest: Option<Digest>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub publisher_trusted: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub signer_key: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub failure_code: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Rows<Label>", extend("x-since" = 2))]
    pub contributions: Option<Rows<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ArtifactRef", extend("x-since" = 2))]
    pub pending_bundle: Option<ArtifactRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<PackageOffer>", extend("x-since" = 2))]
    pub available: Option<Items<PackageOffer>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 2))]
    pub installed: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Id>", extend("x-since" = 3))]
    pub disabled_projects: Option<Items<Id>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "SchemaValue", extend("x-since" = 3))]
    pub configuration: Option<SchemaValue>,
}
impl Check for PackageRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.version.check()?;
        self.digest.check()?;
        self.revision.check()?;
        self.publisher_id.check()?;
        self.enabled.check()?;
        self.trust_state.check()?;
        self.contribution_ids.check()?;
        self.registry_revision.check()?;
        self.availability.check()?;
        self.availability_reason.check()?;
        self.package_kind.check()?;
        self.languages.check()?;
        self.targets.check()?;
        self.host_target_supported.check()?;
        self.prerequisites.check()?;
        self.requested_access.check()?;
        self.effective_access.check()?;
        self.configuration_revision.check()?;
        self.configuration_schema.check()?;
        self.state_generation.check()?;
        self.pending_version.check()?;
        self.pending_digest.check()?;
        self.pending_waits.check()?;
        self.pending_proposal_id.check()?;
        self.predecessor_version.check()?;
        self.predecessor_digest.check()?;
        self.publisher_trusted.check()?;
        self.signer_key.check()?;
        self.failure_code.check()?;
        self.contributions.check()?;
        self.pending_bundle.check()?;
        self.available.check()?;
        self.installed.check()?;
        self.disabled_projects.check()?;
        self.configuration.check()?;
        super::domain_rules::record(
            "PackageRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for PackageRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.registry_revision = None;
        }
        if revision < 2 {
            self.availability = None;
        }
        if revision < 2 {
            self.availability_reason = None;
        }
        if revision < 2 {
            self.package_kind = None;
        }
        if revision < 2 {
            self.languages = None;
        }
        if revision < 2 {
            self.targets = None;
        }
        if revision < 2 {
            self.host_target_supported = None;
        }
        if revision < 2 {
            self.prerequisites = None;
        } else {
            self.prerequisites.retain_revision(revision);
        }
        if revision < 2 {
            self.requested_access = None;
        }
        if revision < 2 {
            self.effective_access = None;
        }
        if revision < 2 {
            self.configuration_revision = None;
        }
        if revision < 2 {
            self.configuration_schema = None;
        }
        if revision < 2 {
            self.state_generation = None;
        }
        if revision < 2 {
            self.pending_version = None;
        }
        if revision < 2 {
            self.pending_digest = None;
        }
        if revision < 2 {
            self.pending_waits = None;
        }
        if revision < 2 {
            self.pending_proposal_id = None;
        }
        if revision < 2 {
            self.predecessor_version = None;
        }
        if revision < 2 {
            self.predecessor_digest = None;
        }
        if revision < 2 {
            self.publisher_trusted = None;
        }
        if revision < 2 {
            self.signer_key = None;
        }
        if revision < 2 {
            self.failure_code = None;
        }
        if revision < 2 {
            self.contributions = None;
        }
        if revision < 2 {
            self.pending_bundle = None;
        }
        if revision < 2 {
            self.available = None;
        }
        if revision < 2 {
            self.installed = None;
        }
        if revision < 3 {
            self.disabled_projects = None;
        }
        if revision < 3 {
            self.configuration = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HelpTopic {
    pub id: Id,
    pub version: Label,
    pub title: Label,
    pub content: ArtifactRef,
    pub related_capability_ids: Items<Id>,
}
impl Check for HelpTopic {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.version.check()?;
        self.title.check()?;
        self.content.check()?;
        self.related_capability_ids.check()?;
        super::domain_rules::record(
            "HelpTopic",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticMetric {
    pub name: Label,
    pub availability: MetricAvailability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Text>,
}
impl Check for DiagnosticMetric {
    fn check(&self) -> Result<(), String> {
        self.name.check()?;
        self.availability.check()?;
        self.value.check()?;
        self.unit.check()?;
        self.reason.check()?;
        super::domain_rules::record(
            "DiagnosticMetric",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecoveryAction {
    pub operation: Label,
    pub label: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<Id>,
}
impl Check for RecoveryAction {
    fn check(&self) -> Result<(), String> {
        self.operation.check()?;
        self.label.check()?;
        self.target_kind.check()?;
        self.target_id.check()?;
        super::domain_rules::record(
            "RecoveryAction",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnostic {
    pub id: Id,
    pub severity: Severity,
    pub component: Label,
    pub code: Label,
    pub message: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<CorrelationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ArtifactRef>,
}
impl Check for Diagnostic {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.severity.check()?;
        self.component.check()?;
        self.code.check()?;
        self.message.check()?;
        self.correlation_id.check()?;
        self.evidence.check()?;
        super::domain_rules::record(
            "Diagnostic",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("x-earlier" = {"name": "Diagnostic", "before": 2}))]
pub struct DiagnosticEntry {
    pub id: Id,
    pub severity: Severity,
    pub component: Label,
    pub code: Label,
    pub message: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<CorrelationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ArtifactRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub sequence: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub recorded_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ActorId", extend("x-since" = 2))]
    pub actor_id: Option<ActorId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "DiagnosticOrigin", extend("x-since" = 2))]
    pub origin: Option<DiagnosticOrigin>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ProjectId", extend("x-since" = 2))]
    pub project_id: Option<ProjectId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub operation: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "OperationId", extend("x-since" = 2))]
    pub operation_id: Option<OperationId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub target_kind: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub target_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub stage: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub source_revision: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub attempt: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub started_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub finished_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub progress_completed: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub progress_total: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "DiagnosticOutcome", extend("x-since" = 2))]
    pub outcome: Option<DiagnosticOutcome>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "DiagnosticErrorClass", extend("x-since" = 2))]
    pub error_class: Option<DiagnosticErrorClass>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "HealthState", extend("x-since" = 2))]
    pub health: Option<HealthState>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Content", extend("x-since" = 2))]
    pub output: Option<Content>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub output_omitted_bytes: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub redactions: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<DiagnosticMetric>", extend("x-since" = 2))]
    pub metrics: Option<Items<DiagnosticMetric>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<RecoveryAction>", extend("x-since" = 2))]
    pub recovery: Option<Items<RecoveryAction>>,
}
impl Check for DiagnosticEntry {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.severity.check()?;
        self.component.check()?;
        self.code.check()?;
        self.message.check()?;
        self.correlation_id.check()?;
        self.evidence.check()?;
        self.sequence.check()?;
        self.recorded_at_ms.check()?;
        self.actor_id.check()?;
        self.origin.check()?;
        self.project_id.check()?;
        self.operation.check()?;
        self.operation_id.check()?;
        self.target_kind.check()?;
        self.target_id.check()?;
        self.stage.check()?;
        self.source_revision.check()?;
        self.attempt.check()?;
        self.started_at_ms.check()?;
        self.finished_at_ms.check()?;
        self.progress_completed.check()?;
        self.progress_total.check()?;
        self.outcome.check()?;
        self.error_class.check()?;
        self.health.check()?;
        self.output.check()?;
        self.output_omitted_bytes.check()?;
        self.redactions.check()?;
        self.metrics.check()?;
        self.recovery.check()?;
        super::domain_rules::record(
            "DiagnosticEntry",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for DiagnosticEntry {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.sequence = None;
        }
        if revision < 2 {
            self.recorded_at_ms = None;
        }
        if revision < 2 {
            self.actor_id = None;
        }
        if revision < 2 {
            self.origin = None;
        }
        if revision < 2 {
            self.project_id = None;
        }
        if revision < 2 {
            self.operation = None;
        }
        if revision < 2 {
            self.operation_id = None;
        }
        if revision < 2 {
            self.target_kind = None;
        }
        if revision < 2 {
            self.target_id = None;
        }
        if revision < 2 {
            self.stage = None;
        }
        if revision < 2 {
            self.source_revision = None;
        }
        if revision < 2 {
            self.attempt = None;
        }
        if revision < 2 {
            self.started_at_ms = None;
        }
        if revision < 2 {
            self.finished_at_ms = None;
        }
        if revision < 2 {
            self.progress_completed = None;
        }
        if revision < 2 {
            self.progress_total = None;
        }
        if revision < 2 {
            self.outcome = None;
        }
        if revision < 2 {
            self.error_class = None;
        }
        if revision < 2 {
            self.health = None;
        }
        if revision < 2 {
            self.output = None;
        }
        if revision < 2 {
            self.output_omitted_bytes = None;
        }
        if revision < 2 {
            self.redactions = None;
        }
        if revision < 2 {
            self.metrics = None;
        }
        if revision < 2 {
            self.recovery = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("x-earlier" = {"name": "Page", "before": 2}))]
pub struct DiagnosticPage {
    pub items: Rows<DiagnosticEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 2))]
    pub latest_sequence: Option<Counter>,
}
impl Check for DiagnosticPage {
    fn check(&self) -> Result<(), String> {
        self.items.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        self.latest_sequence.check()?;
        super::domain_rules::record(
            "DiagnosticPage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for DiagnosticPage {
    fn retain_revision(&mut self, revision: u32) {
        self.items.retain_revision(revision);
        if revision < 2 {
            self.latest_sequence = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupRecord {
    pub id: Id,
    pub revision: Revision,
    pub manifest: ArtifactRef,
    pub schema_version: Revision,
    pub created_at_ms: Counter,
    pub state: OperationState,
}
impl Check for BackupRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.manifest.check()?;
        self.schema_version.check()?;
        self.created_at_ms.check()?;
        self.state.check()?;
        super::domain_rules::record(
            "BackupRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateRecord {
    pub id: Id,
    pub revision: Revision,
    pub version: Label,
    pub channel: UpdateChannel,
    pub digest: Digest,
    pub signature_verified: bool,
    pub state: OperationState,
    pub migration_required: bool,
}
impl Check for UpdateRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.version.check()?;
        self.channel.check()?;
        self.digest.check()?;
        self.signature_verified.check()?;
        self.state.check()?;
        self.migration_required.check()?;
        super::domain_rules::record(
            "UpdateRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueDelivery {
    pub id: Id,
    pub event_id: EventId,
    pub consumer_id: Id,
    pub revision: Revision,
    pub state: OperationState,
    pub attempt: Counter,
    pub fence: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Label>,
}
impl Check for QueueDelivery {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.event_id.check()?;
        self.consumer_id.check()?;
        self.revision.check()?;
        self.state.check()?;
        self.attempt.check()?;
        self.fence.check()?;
        self.lease_until_ms.check()?;
        self.error.check()?;
        super::domain_rules::record(
            "QueueDelivery",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportRun {
    pub id: Id,
    pub revision: Revision,
    pub source_snapshot: Digest,
    pub state: OperationState,
    pub imported_ids: Items<Id>,
    pub report: ArtifactRef,
}
impl Check for ImportRun {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.source_snapshot.check()?;
        self.state.check()?;
        self.imported_ids.check()?;
        self.report.check()?;
        super::domain_rules::record(
            "ImportRun",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportConfig {
    pub source: RegisteredPath,
    pub project_ids: Items<ProjectId>,
    pub work_ids: Items<Id>,
    pub include_history: bool,
    pub include_git_links: bool,
}
impl Check for ImportConfig {
    fn check(&self) -> Result<(), String> {
        self.source.check()?;
        self.project_ids.check()?;
        self.work_ids.check()?;
        self.include_history.check()?;
        self.include_git_links.check()?;
        super::domain_rules::record(
            "ImportConfig",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrunePreview {
    pub id: Id,
    pub revision: Revision,
    pub kind: PruneKind,
    pub candidate_ids: Rows<RevisionedRef>,
    pub held_ids: Rows<Id>,
    pub expires_at_ms: Counter,
}
impl Check for PrunePreview {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.kind.check()?;
        self.candidate_ids.check()?;
        self.held_ids.check()?;
        self.expires_at_ms.check()?;
        super::domain_rules::record(
            "PrunePreview",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskPlanEntry {
    pub client_key: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_task_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<Revision>,
    pub title: Label,
    pub description: Content,
    pub repository_ids: Items<Id>,
    pub dependency_keys: Items<Id>,
    pub acceptance: Items<AcceptanceCriterion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "WorkItemType", extend("x-since" = 3))]
    pub task_type: Option<WorkItemType>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub clear_task_type: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 3))]
    pub labels: Option<Items<Label>>,
}
impl Check for TaskPlanEntry {
    fn check(&self) -> Result<(), String> {
        self.client_key.check()?;
        self.existing_task_id.check()?;
        self.expected_revision.check()?;
        self.title.check()?;
        self.description.check()?;
        self.repository_ids.check()?;
        self.dependency_keys.check()?;
        self.acceptance.check()?;
        self.priority.check()?;
        self.task_type.check()?;
        self.clear_task_type.check()?;
        self.labels.check()?;
        super::domain_rules::record(
            "TaskPlanEntry",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for TaskPlanEntry {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.task_type = None;
        }
        if revision < 3 {
            self.clear_task_type = None;
        }
        if revision < 3 {
            self.labels = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum RouterConfigAction {
    Connection {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        name: Label,
        profile: EndpointKind,
        endpoint: Url,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_ref: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 2))]
        enabled: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "OpenRouterConstraints", extend("x-since" = 2))]
        open_router: Option<OpenRouterConstraints>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3))]
        local_free: Option<bool>,
    },
    Model {
        model: RouteModel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
    },
    Tier {
        tier: TierName,
        expected_revision: Revision,
        member_ids: Items<Id>,
    },
    DecisionModel {
        model: DecisionModel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
    },
    SignIn {
        connection_id: Id,
        action: AccountAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flow_id: Option<Id>,
    },
    RemoveConnection {
        connection_id: Id,
        expected_revision: Revision,
    },
    SubscriptionRisk {
        connection_id: Id,
        expected_revision: Revision,
        notice_sha256: Digest,
        accepted: bool,
    },
}
impl Check for RouterConfigAction {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Connection {
                connection_id,
                expected_revision,
                name,
                profile,
                endpoint,
                credential_ref,
                enabled,
                open_router,
                local_free,
            } => {
                connection_id.check()?;
                expected_revision.check()?;
                name.check()?;
                profile.check()?;
                endpoint.check()?;
                credential_ref.check()?;
                enabled.check()?;
                open_router.check()?;
                local_free.check()?;
            }
            Self::Model {
                model,
                expected_revision,
            } => {
                model.check()?;
                expected_revision.check()?;
            }
            Self::Tier {
                tier,
                expected_revision,
                member_ids,
            } => {
                tier.check()?;
                expected_revision.check()?;
                member_ids.check()?;
            }
            Self::DecisionModel {
                model,
                expected_revision,
            } => {
                model.check()?;
                expected_revision.check()?;
            }
            Self::SignIn {
                connection_id,
                action,
                flow_id,
            } => {
                connection_id.check()?;
                action.check()?;
                flow_id.check()?;
            }
            Self::RemoveConnection {
                connection_id,
                expected_revision,
            } => {
                connection_id.check()?;
                expected_revision.check()?;
            }
            Self::SubscriptionRisk {
                connection_id,
                expected_revision,
                notice_sha256,
                accepted,
            } => {
                connection_id.check()?;
                expected_revision.check()?;
                notice_sha256.check()?;
                accepted.check()?;
            }
        }
        Ok(())
    }
}

impl Revisioned for RouterConfigAction {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::Connection {
                enabled,
                open_router,
                local_free,
                ..
            } => {
                if revision < 2 {
                    *enabled = None;
                }
                if revision < 2 {
                    *open_router = None;
                }
                if revision < 3 {
                    *local_free = None;
                }
            }
            Self::Model { model, .. } => {
                model.retain_revision(revision);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootSnapshot {
    pub root: RootControl,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<Activation>,
    pub blockers: Blockers,
}
impl Check for RootSnapshot {
    fn check(&self) -> Result<(), String> {
        self.root.check()?;
        self.activation.check()?;
        self.blockers.check()?;
        super::domain_rules::record(
            "RootSnapshot",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EligibilitySnapshot {
    pub target: EligibilityTarget,
    pub purpose: Purpose,
    pub eligible: bool,
    pub blockers: Blockers,
}
impl Check for EligibilitySnapshot {
    fn check(&self) -> Result<(), String> {
        self.target.check()?;
        self.purpose.check()?;
        self.eligible.check()?;
        self.blockers.check()?;
        super::domain_rules::record(
            "EligibilitySnapshot",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptObservation {
    pub request_key: RequestKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<CommandId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_sequence: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_artifact: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_schema: Option<SchemaRef>,
    pub state: ReceiptLookupState,
}
impl Check for ReceiptObservation {
    fn check(&self) -> Result<(), String> {
        self.request_key.check()?;
        self.command_id.check()?;
        self.committed_sequence.check()?;
        self.operation_id.check()?;
        self.reply_artifact.check()?;
        self.reply_schema.check()?;
        self.state.check()?;
        super::domain_rules::record(
            "ReceiptObservation",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReceiptLookupState {
    NotFound,
    Recorded,
    StatusUnknown,
}
impl Check for ReceiptLookupState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingAssignment {
    pub key: Label,
    pub value: SchemaValue,
}
impl Check for SettingAssignment {
    fn check(&self) -> Result<(), String> {
        self.key.check()?;
        self.value.check()?;
        super::domain_rules::record(
            "SettingAssignment",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceInput {
    pub input_usd_per_million: Money,
    pub output_usd_per_million: Money,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_per_million: Option<Money>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_per_million: Option<Money>,
}
impl Check for PriceInput {
    fn check(&self) -> Result<(), String> {
        self.input_usd_per_million.check()?;
        self.output_usd_per_million.check()?;
        self.cache_read_usd_per_million.check()?;
        self.cache_write_usd_per_million.check()?;
        super::domain_rules::record(
            "PriceInput",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum MessageDirection {
    Inbound,
    Outbound,
    Both,
}
impl Check for MessageDirection {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobInspection {
    pub job: JobRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<JobRun>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Rows<JobRun>", extend("x-since" = 2))]
    pub runs: Option<Rows<JobRun>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Cursor", extend("x-since" = 2))]
    pub runs_next_cursor: Option<Cursor>,
}
impl Check for JobInspection {
    fn check(&self) -> Result<(), String> {
        self.job.check()?;
        self.run.check()?;
        self.runs.check()?;
        self.runs_next_cursor.check()?;
        super::domain_rules::record(
            "JobInspection",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for JobInspection {
    fn retain_revision(&mut self, revision: u32) {
        self.job.retain_revision(revision);
        self.run.retain_revision(revision);
        if revision < 2 {
            self.runs = None;
        } else {
            self.runs.retain_revision(revision);
        }
        if revision < 2 {
            self.runs_next_cursor = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryIndexStatus {
    pub repository_id: Id,
    pub current_view_ids: Items<Id>,
    pub coverage: Coverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<IndexRunRecord>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "IndexFreshness", extend("x-since" = 2))]
    pub freshness: Option<IndexFreshness>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<LanguageCoverage>", extend("x-since" = 2))]
    pub languages: Option<Items<LanguageCoverage>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<SemanticPackStatus>", extend("x-since" = 2))]
    pub semantic_packs: Option<Items<SemanticPackStatus>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<OverlayStatus>", extend("x-since" = 2))]
    pub overlays: Option<Items<OverlayStatus>>,
}
impl Check for RepositoryIndexStatus {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.current_view_ids.check()?;
        self.coverage.check()?;
        self.last_run.check()?;
        self.freshness.check()?;
        self.languages.check()?;
        self.semantic_packs.check()?;
        self.overlays.check()?;
        super::domain_rules::record(
            "RepositoryIndexStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for RepositoryIndexStatus {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.freshness = None;
        }
        if revision < 2 {
            self.languages = None;
        }
        if revision < 2 {
            self.semantic_packs = None;
        }
        if revision < 2 {
            self.overlays = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsumerStatus {
    pub consumer_id: Id,
    pub revision: Revision,
    pub cursor: Counter,
    pub pending_count: Counter,
    pub leased_count: Counter,
    pub poison_count: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at_ms: Option<Counter>,
}
impl Check for ConsumerStatus {
    fn check(&self) -> Result<(), String> {
        self.consumer_id.check()?;
        self.revision.check()?;
        self.cursor.check()?;
        self.pending_count.check()?;
        self.leased_count.check()?;
        self.poison_count.check()?;
        self.retry_at_ms.check()?;
        super::domain_rules::record(
            "ConsumerStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum BrowserSessionAction {
    Create {
        scope: DataScope,
        browser_asset_id: Id,
    },
    Close {
        session_id: Id,
        expected_revision: Revision,
    },
}
impl Check for BrowserSessionAction {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Create {
                scope,
                browser_asset_id,
            } => {
                scope.check()?;
                browser_asset_id.check()?;
            }
            Self::Close {
                session_id,
                expected_revision,
            } => {
                session_id.check()?;
                expected_revision.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GithubAccountState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<GithubAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_flow_id: Option<Id>,
    pub state: ItemState,
}
impl Check for GithubAccountState {
    fn check(&self) -> Result<(), String> {
        self.account.check()?;
        self.auth_flow_id.check()?;
        self.state.check()?;
        super::domain_rules::record(
            "GithubAccountState",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum EvidenceKind {
    Conversation,
    Document,
    Code,
    Observation,
    External,
}
impl Check for EvidenceKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceLocator {
    pub kind: EvidenceKind,
    pub source_ref: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<Text>,
}
impl Check for EvidenceLocator {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.source_ref.check()?;
        self.artifact.check()?;
        self.locator.check()?;
        super::domain_rules::record(
            "EvidenceLocator",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeConflict {
    pub id: Id,
    pub claim_ids: NonEmpty<Id>,
    pub state: Label,
    pub explanation: Content,
}
impl Check for KnowledgeConflict {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.claim_ids.check()?;
        self.state.check()?;
        self.explanation.check()?;
        super::domain_rules::record(
            "KnowledgeConflict",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum RequirementStatus {
    Confirmed,
    Proposed,
    Dropped,
}
impl Check for RequirementStatus {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum CriterionState {
    Unmet,
    Met,
}
impl Check for CriterionState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequirementItem {
    pub id: Id,
    pub statement: Content,
    pub status: RequirementStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_decision: Option<ArtifactRef>,
    pub covered_by_task_ids: Items<Id>,
}
impl Check for RequirementItem {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.statement.check()?;
        self.status.check()?;
        self.drop_decision.check()?;
        self.covered_by_task_ids.check()?;
        super::domain_rules::record(
            "RequirementItem",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkComment {
    pub id: Id,
    pub content: Content,
    pub evidence: Items<ArtifactRef>,
}
impl Check for WorkComment {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.content.check()?;
        self.evidence.check()?;
        super::domain_rules::record(
            "WorkComment",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandoffRecord {
    pub id: Id,
    pub summary: Content,
    pub next_steps: Content,
    pub evidence: Items<ArtifactRef>,
}
impl Check for HandoffRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.summary.check()?;
        self.next_steps.check()?;
        self.evidence.check()?;
        super::domain_rules::record(
            "HandoffRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkAttempt {
    pub id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<AssignmentId>,
    pub state: OperationState,
    pub git_links: Items<GitLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<ArtifactRef>,
}
impl Check for WorkAttempt {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.assignment_id.check()?;
        self.state.check()?;
        self.git_links.check()?;
        self.checkpoint.check()?;
        super::domain_rules::record(
            "WorkAttempt",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkLink {
    pub kind: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
}
impl Check for WorkLink {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.target_id.check()?;
        self.url.check()?;
        self.artifact.check()?;
        super::domain_rules::record(
            "WorkLink",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TouchRef {
    pub repository_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RelativePath>,
}
impl Check for TouchRef {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.view_id.check()?;
        self.node_id.check()?;
        self.path.check()?;
        super::domain_rules::record(
            "TouchRef",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeaturePlanDraft {
    pub caller_ref: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_feature_id: Option<RootId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<Revision>,
    pub title: Label,
    pub description: Content,
    pub repository_ids: Items<Id>,
    pub plan: PlanDraft,
    pub tasks: PlanItems<TaskPlanEntry>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 3))]
    pub labels: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "FeatureBudgetInput", extend("x-since" = 3))]
    pub budget: Option<FeatureBudgetInput>,
}
impl Check for FeaturePlanDraft {
    fn check(&self) -> Result<(), String> {
        self.caller_ref.check()?;
        self.existing_feature_id.check()?;
        self.expected_revision.check()?;
        self.title.check()?;
        self.description.check()?;
        self.repository_ids.check()?;
        self.plan.check()?;
        self.tasks.check()?;
        self.labels.check()?;
        self.budget.check()?;
        super::domain_rules::record(
            "FeaturePlanDraft",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for FeaturePlanDraft {
    fn retain_revision(&mut self, revision: u32) {
        self.tasks.retain_revision(revision);
        if revision < 3 {
            self.labels = None;
        }
        if revision < 3 {
            self.budget = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AtomicWorkPlan {
    pub caller_ref: Id,
    pub features: PlanFeatures<FeaturePlanDraft>,
    pub dependency_edges: Rows<DependencyEdge>,
}
impl Check for AtomicWorkPlan {
    fn check(&self) -> Result<(), String> {
        self.caller_ref.check()?;
        self.features.check()?;
        self.dependency_edges.check()?;
        super::domain_rules::record(
            "AtomicWorkPlan",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for AtomicWorkPlan {
    fn retain_revision(&mut self, revision: u32) {
        self.features.retain_revision(revision);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum FeatureCreation {
    Single {
        title: Label,
        description: Content,
        repository_ids: Items<Id>,
        status_id: Id,
        priority: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<PlanDraft>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<Label>", extend("x-since" = 3))]
        labels: Option<Items<Label>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "FeatureBudgetInput", extend("x-since" = 3))]
        budget: Option<FeatureBudgetInput>,
    },
    AtomicPlan {
        plan: AtomicWorkPlan,
    },
    StagedPlan {
        artifact: ArtifactRef,
    },
}
impl Check for FeatureCreation {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Single {
                title,
                description,
                repository_ids,
                status_id,
                priority,
                plan,
                labels,
                budget,
            } => {
                title.check()?;
                description.check()?;
                repository_ids.check()?;
                status_id.check()?;
                priority.check()?;
                plan.check()?;
                labels.check()?;
                budget.check()?;
            }
            Self::AtomicPlan { plan } => {
                plan.check()?;
            }
            Self::StagedPlan { artifact } => {
                artifact.check()?;
            }
        }
        Ok(())
    }
}

impl Revisioned for FeatureCreation {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::Single { labels, budget, .. } => {
                if revision < 3 {
                    *labels = None;
                }
                if revision < 3 {
                    *budget = None;
                }
            }
            Self::AtomicPlan { plan, .. } => {
                plan.retain_revision(revision);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum BrowserLocation {
    Remote { url: Url },
    Artifact { artifact: ArtifactRef },
    Blank {},
}
impl Check for BrowserLocation {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Remote { url } => {
                url.check()?;
            }
            Self::Artifact { artifact } => {
                artifact.check()?;
            }
            Self::Blank {} => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainerImage {
    pub engine_id: Id,
    pub image: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Text>,
    pub state: ItemState,
    pub owned: bool,
}
impl Check for ContainerImage {
    fn check(&self) -> Result<(), String> {
        self.engine_id.check()?;
        self.image.check()?;
        self.digest.check()?;
        self.state.check()?;
        self.owned.check()?;
        super::domain_rules::record(
            "ContainerImage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum DetailLevel {
    Summary,
    Full,
}
impl Check for DetailLevel {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessOutput {
    pub process_id: Id,
    pub revision: Revision,
    pub state: OperationState,
    pub first_byte: Counter,
    pub next_byte: Counter,
    pub content: Content,
    pub encoding: OutputEncoding,
    pub truncated: bool,
    pub complete: bool,
}
impl Check for ProcessOutput {
    fn check(&self) -> Result<(), String> {
        self.process_id.check()?;
        self.revision.check()?;
        self.state.check()?;
        self.first_byte.check()?;
        self.next_byte.check()?;
        self.content.check()?;
        self.encoding.check()?;
        self.truncated.check()?;
        self.complete.check()?;
        super::domain_rules::record(
            "ProcessOutput",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OutputEncoding {
    Utf8,
    Base64,
}
impl Check for OutputEncoding {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DreamSettingsRecord {
    pub scope: DataScope,
    pub revision: Revision,
    pub config: DreamConfig,
}
impl Check for DreamSettingsRecord {
    fn check(&self) -> Result<(), String> {
        self.scope.check()?;
        self.revision.check()?;
        self.config.check()?;
        super::domain_rules::record(
            "DreamSettingsRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotificationSettingsRecord {
    pub revision: Revision,
    pub config: NotificationConfig,
}
impl Check for NotificationSettingsRecord {
    fn check(&self) -> Result<(), String> {
        self.revision.check()?;
        self.config.check()?;
        super::domain_rules::record(
            "NotificationSettingsRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPriceStatus {
    pub model_id: Id,
    pub price_kind: UsagePriceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<PriceRecord>,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_check_at_ms: Option<Counter>,
}
impl Check for ModelPriceStatus {
    fn check(&self) -> Result<(), String> {
        self.model_id.check()?;
        self.price_kind.check()?;
        self.price.check()?;
        self.stale.check()?;
        self.next_check_at_ms.check()?;
        super::domain_rules::record(
            "ModelPriceStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactRecord {
    pub artifact: ArtifactRef,
    pub name: Label,
    pub created_at_ms: Counter,
    pub scope: DataScope,
    pub reference_count: Counter,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ArtifactTags", extend("x-since" = 2))]
    pub tags: Option<ArtifactTags>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "TestedRepositoryRevision", extend("x-since" = 4))]
    pub tested_repository: Option<TestedRepositoryRevision>,
}
impl Check for ArtifactRecord {
    fn check(&self) -> Result<(), String> {
        self.artifact.check()?;
        self.name.check()?;
        self.created_at_ms.check()?;
        self.scope.check()?;
        self.reference_count.check()?;
        self.tags.check()?;
        self.tested_repository.check()?;
        super::domain_rules::record(
            "ArtifactRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for ArtifactRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.tags = None;
        }
        if revision < 4 {
            self.tested_repository = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ArtifactReadContent {
    Text {
        artifact: ArtifactRef,
        first_byte: Counter,
        next_byte: Counter,
        content: Content,
        truncated: bool,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "TestedRepositoryRevision", extend("x-since" = 4))]
        tested_repository: Option<TestedRepositoryRevision>,
    },
    Binary {
        artifact: ArtifactRef,
        stream_id: Id,
        first_byte: Counter,
        byte_length: Counter,
        expires_at_ms: Counter,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "TestedRepositoryRevision", extend("x-since" = 4))]
        tested_repository: Option<TestedRepositoryRevision>,
    },
}
impl Check for ArtifactReadContent {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Text {
                artifact,
                first_byte,
                next_byte,
                content,
                truncated,
                tested_repository,
            } => {
                artifact.check()?;
                first_byte.check()?;
                next_byte.check()?;
                content.check()?;
                truncated.check()?;
                tested_repository.check()?;
            }
            Self::Binary {
                artifact,
                stream_id,
                first_byte,
                byte_length,
                expires_at_ms,
                tested_repository,
            } => {
                artifact.check()?;
                stream_id.check()?;
                first_byte.check()?;
                byte_length.check()?;
                expires_at_ms.check()?;
                tested_repository.check()?;
            }
        }
        Ok(())
    }
}

impl Revisioned for ArtifactReadContent {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::Text {
                tested_repository, ..
            } => {
                if revision < 4 {
                    *tested_repository = None;
                }
            }
            Self::Binary {
                tested_repository, ..
            } => {
                if revision < 4 {
                    *tested_repository = None;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactPreview {
    pub artifact: ArtifactRef,
    pub preview: ArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<PositiveCounter>,
    pub coverage: Coverage,
}
impl Check for ArtifactPreview {
    fn check(&self) -> Result<(), String> {
        self.artifact.check()?;
        self.preview.check()?;
        self.page.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "ArtifactPreview",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactExport {
    pub artifact: ArtifactRef,
    pub stream_id: Id,
    pub expires_at_ms: Counter,
    pub suggested_name: Label,
}
impl Check for ArtifactExport {
    fn check(&self) -> Result<(), String> {
        self.artifact.check()?;
        self.stream_id.check()?;
        self.expires_at_ms.check()?;
        self.suggested_name.check()?;
        super::domain_rules::record(
            "ArtifactExport",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeaturePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_ids: Option<Items<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goals: Option<Items<Content>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_links: Option<Items<WorkLink>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_link_ids: Option<Items<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_comment: Option<WorkComment>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ReadinessChange", extend("x-since" = 2))]
    pub readiness: Option<ReadinessChange>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "NonEmpty<AcceptanceCriterion>", extend("x-since" = 2))]
    pub acceptance_evidence: Option<NonEmpty<AcceptanceCriterion>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 3))]
    pub labels: Option<Items<Label>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "FeatureBudgetInput", extend("x-since" = 3))]
    pub budget: Option<FeatureBudgetInput>,
}
impl Check for FeaturePatch {
    fn check(&self) -> Result<(), String> {
        self.title.check()?;
        self.description.check()?;
        self.status_id.check()?;
        self.repository_ids.check()?;
        self.goals.check()?;
        self.add_links.check()?;
        self.remove_link_ids.check()?;
        self.add_comment.check()?;
        self.readiness.check()?;
        self.acceptance_evidence.check()?;
        self.labels.check()?;
        self.budget.check()?;
        super::domain_rules::record(
            "FeaturePatch",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for FeaturePatch {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.readiness = None;
        }
        if revision < 2 {
            self.acceptance_evidence = None;
        }
        if revision < 3 {
            self.labels = None;
        }
        if revision < 3 {
            self.budget = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IssuePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_ids: Option<Items<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goals: Option<Items<Content>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<Items<AcceptanceCriterion>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touches: Option<Items<TouchRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_links: Option<Items<WorkLink>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_link_ids: Option<Items<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_comment: Option<WorkComment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<HandoffRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_report: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "WorkItemType", extend("x-since" = 3))]
    pub task_type: Option<WorkItemType>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub clear_task_type: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 3))]
    pub labels: Option<Items<Label>>,
}
impl Check for IssuePatch {
    fn check(&self) -> Result<(), String> {
        self.title.check()?;
        self.description.check()?;
        self.status_id.check()?;
        self.repository_ids.check()?;
        self.goals.check()?;
        self.acceptance.check()?;
        self.touches.check()?;
        self.add_links.check()?;
        self.remove_link_ids.check()?;
        self.add_comment.check()?;
        self.handoff.check()?;
        self.acceptance_report.check()?;
        self.priority.check()?;
        self.task_type.check()?;
        self.clear_task_type.check()?;
        self.labels.check()?;
        super::domain_rules::record(
            "IssuePatch",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for IssuePatch {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.task_type = None;
        }
        if revision < 3 {
            self.clear_task_type = None;
        }
        if revision < 3 {
            self.labels = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_ids: Option<Items<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goals: Option<Items<Content>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<Items<AcceptanceCriterion>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits_requirement_ids: Option<Items<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touches: Option<Items<TouchRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_links: Option<Items<WorkLink>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove_link_ids: Option<Items<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_comment: Option<WorkComment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<HandoffRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_report: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "WorkItemType", extend("x-since" = 3))]
    pub task_type: Option<WorkItemType>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub clear_task_type: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Label>", extend("x-since" = 3))]
    pub labels: Option<Items<Label>>,
}
impl Check for TaskPatch {
    fn check(&self) -> Result<(), String> {
        self.title.check()?;
        self.description.check()?;
        self.status_id.check()?;
        self.repository_ids.check()?;
        self.goals.check()?;
        self.acceptance.check()?;
        self.inherits_requirement_ids.check()?;
        self.touches.check()?;
        self.add_links.check()?;
        self.remove_link_ids.check()?;
        self.add_comment.check()?;
        self.handoff.check()?;
        self.acceptance_report.check()?;
        self.priority.check()?;
        self.task_type.check()?;
        self.clear_task_type.check()?;
        self.labels.check()?;
        super::domain_rules::record(
            "TaskPatch",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for TaskPatch {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.task_type = None;
        }
        if revision < 3 {
            self.clear_task_type = None;
        }
        if revision < 3 {
            self.labels = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum McpServerBinding {
    Stdio,
    LoopbackHttp,
}
impl Check for McpServerBinding {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpInboundSettings {
    pub revision: Revision,
    pub enabled: bool,
    pub bindings: Items<McpServerBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<Port>,
}
impl Check for McpInboundSettings {
    fn check(&self) -> Result<(), String> {
        self.revision.check()?;
        self.enabled.check()?;
        self.bindings.check()?;
        self.http_port.check()?;
        super::domain_rules::record(
            "McpInboundSettings",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpInboundStatus {
    pub settings: McpInboundSettings,
    pub state: ItemState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<Url>,
    pub protocol_versions: Items<Label>,
    pub publication_revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Text>,
}
impl Check for McpInboundStatus {
    fn check(&self) -> Result<(), String> {
        self.settings.check()?;
        self.state.check()?;
        self.endpoint.check()?;
        self.protocol_versions.check()?;
        self.publication_revision.check()?;
        self.diagnostic.check()?;
        super::domain_rules::record(
            "McpInboundStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpClientAccess {
    pub client_id: Id,
    pub revision: Revision,
    pub name: Label,
    pub enabled: bool,
    pub credential_generation: Revision,
    pub project_ids: Items<ProjectId>,
    pub repository_ids: Items<Id>,
    pub worktree_ids: Items<Id>,
    pub read_operations: Items<Label>,
    pub command_operations: Items<Label>,
}
impl Check for McpClientAccess {
    fn check(&self) -> Result<(), String> {
        self.client_id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.enabled.check()?;
        self.credential_generation.check()?;
        self.project_ids.check()?;
        self.repository_ids.check()?;
        self.worktree_ids.check()?;
        self.read_operations.check()?;
        self.command_operations.check()?;
        super::domain_rules::record(
            "McpClientAccess",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum McpClientAccessAction {
    Register {
        name: Label,
    },
    Replace {
        client_id: Id,
        expected_revision: Revision,
        name: Label,
        enabled: bool,
        project_ids: Items<ProjectId>,
        repository_ids: Items<Id>,
        worktree_ids: Items<Id>,
        read_operations: Items<Label>,
        command_operations: Items<Label>,
    },
    Revoke {
        client_id: Id,
        expected_revision: Revision,
    },
    RotateCredential {
        client_id: Id,
        expected_revision: Revision,
    },
}
impl Check for McpClientAccessAction {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Register { name } => {
                name.check()?;
            }
            Self::Replace {
                client_id,
                expected_revision,
                name,
                enabled,
                project_ids,
                repository_ids,
                worktree_ids,
                read_operations,
                command_operations,
            } => {
                client_id.check()?;
                expected_revision.check()?;
                name.check()?;
                enabled.check()?;
                project_ids.check()?;
                repository_ids.check()?;
                worktree_ids.check()?;
                read_operations.check()?;
                command_operations.check()?;
            }
            Self::Revoke {
                client_id,
                expected_revision,
            } => {
                client_id.check()?;
                expected_revision.check()?;
            }
            Self::RotateCredential {
                client_id,
                expected_revision,
            } => {
                client_id.check()?;
                expected_revision.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingFeaturePlan {
    pub revision: Revision,
    pub draft: PlanDraft,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<PlanApproval>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "FeatureBudgetInput", extend("x-since" = 3))]
    pub budget: Option<FeatureBudgetInput>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Digest", extend("x-since" = 3))]
    pub plan_digest: Option<Digest>,
}
impl Check for PendingFeaturePlan {
    fn check(&self) -> Result<(), String> {
        self.revision.check()?;
        self.draft.check()?;
        self.approval.check()?;
        self.budget.check()?;
        self.plan_digest.check()?;
        super::domain_rules::record(
            "PendingFeaturePlan",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for PendingFeaturePlan {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.budget = None;
        }
        if revision < 3 {
            self.plan_digest = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum GithubIssueIntakeState {
    Queued,
    Triaging,
    AwaitingDecision,
    Working,
    Resolved,
    Closed,
}
impl Check for GithubIssueIntakeState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GithubIssueTracking {
    pub repository_id: Id,
    pub remote_issue_number: PositiveCounter,
    pub url: Url,
    pub remote_revision: Text,
    pub remote_state: Label,
    pub intake_state: GithubIssueIntakeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_admission_at: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_operation_id: Option<Id>,
}
impl Check for GithubIssueTracking {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.remote_issue_number.check()?;
        self.url.check()?;
        self.remote_revision.check()?;
        self.remote_state.check()?;
        self.intake_state.check()?;
        self.next_admission_at.check()?;
        self.resolution_operation_id.check()?;
        super::domain_rules::record(
            "GithubIssueTracking",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReviewSourceKind {
    GitHubAutomation,
    InAppIndependent,
    InAppFallback,
}
impl Check for ReviewSourceKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReviewObservationState {
    Requested,
    Running,
    Completed,
    Failed,
    Unavailable,
    TimedOut,
    Stale,
}
impl Check for ReviewObservationState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewSourceEvidence {
    pub source_id: Id,
    pub source_kind: ReviewSourceKind,
    pub repository_id: Id,
    pub pull_request_number: PositiveCounter,
    pub head: GitOid,
    pub head_digest: Digest,
    pub policy_revision: Revision,
    pub cycle: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Id>,
    pub state: ReviewObservationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<Counter>,
    pub finding_count: Counter,
    pub zero_findings_confirmed: bool,
    pub findings_coverage: Coverage,
    pub evidence: Items<ArtifactRef>,
}
impl Check for ReviewSourceEvidence {
    fn check(&self) -> Result<(), String> {
        self.source_id.check()?;
        self.source_kind.check()?;
        self.repository_id.check()?;
        self.pull_request_number.check()?;
        self.head.check()?;
        self.head_digest.check()?;
        self.policy_revision.check()?;
        self.cycle.check()?;
        self.run_id.check()?;
        self.state.check()?;
        self.completed_at.check()?;
        self.finding_count.check()?;
        self.zero_findings_confirmed.check()?;
        self.findings_coverage.check()?;
        self.evidence.check()?;
        super::domain_rules::record(
            "ReviewSourceEvidence",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PackageAvailability {
    NotInstalled,
    Disabled,
    MissingDependency,
    Incompatible,
    Ready,
    Failed,
}
impl Check for PackageAvailability {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PackageWaitReason {
    TrustApproval,
    ExactVersionApproval,
    AccessApproval,
    Compatibility,
    Quiescence,
}
impl Check for PackageWaitReason {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackagePrerequisite {
    pub kind: Label,
    pub id: Label,
    pub requirement: Label,
    pub required: bool,
    pub satisfied: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "PrerequisiteState", extend("x-since" = 2))]
    pub state: Option<PrerequisiteState>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 2))]
    pub detected_version: Option<Label>,
}
impl Check for PackagePrerequisite {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.id.check()?;
        self.requirement.check()?;
        self.required.check()?;
        self.satisfied.check()?;
        self.state.check()?;
        self.detected_version.check()?;
        super::domain_rules::record(
            "PackagePrerequisite",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for PackagePrerequisite {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.state = None;
        }
        if revision < 2 {
            self.detected_version = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PrerequisiteState {
    Missing,
    Incompatible,
    Ready,
}
impl Check for PrerequisiteState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageOffer {
    pub version: Label,
    pub manifest_digest: Digest,
    pub bundle_digest: Digest,
    pub bundle_bytes: Counter,
    pub publisher_id: Id,
    pub source: Label,
    pub host_target_supported: bool,
    pub newer: bool,
}
impl Check for PackageOffer {
    fn check(&self) -> Result<(), String> {
        self.version.check()?;
        self.manifest_digest.check()?;
        self.bundle_digest.check()?;
        self.bundle_bytes.check()?;
        self.publisher_id.check()?;
        self.source.check()?;
        self.host_target_supported.check()?;
        self.newer.check()?;
        super::domain_rules::record(
            "PackageOffer",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum IndexFreshnessState {
    Current,
    Stale,
    Indexing,
    NotIndexed,
    Failed,
}
impl Check for IndexFreshnessState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexFreshness {
    pub state: IndexFreshnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_commit: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<Counter>,
    pub configuration_current: bool,
    pub reasons: Items<Label>,
}
impl Check for IndexFreshness {
    fn check(&self) -> Result<(), String> {
        self.state.check()?;
        self.indexed_commit.check()?;
        self.head_commit.check()?;
        self.head_ref.check()?;
        self.indexed_at_ms.check()?;
        self.observed_at_ms.check()?;
        self.configuration_current.check()?;
        self.reasons.check()?;
        super::domain_rules::record(
            "IndexFreshness",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum LanguageCoverageMode {
    Structural,
    TextFallback,
    NotSelected,
}
impl Check for LanguageCoverageMode {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LanguageCoverage {
    pub language_id: Label,
    pub name: Label,
    pub mode: LanguageCoverageMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grammar: Option<Label>,
    pub files: Counter,
    pub structural_files: Counter,
    pub text_files: Counter,
    pub parse_error_files: Counter,
    pub skipped_files: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Content>,
}
impl Check for LanguageCoverage {
    fn check(&self) -> Result<(), String> {
        self.language_id.check()?;
        self.name.check()?;
        self.mode.check()?;
        self.grammar.check()?;
        self.files.check()?;
        self.structural_files.check()?;
        self.text_files.check()?;
        self.parse_error_files.check()?;
        self.skipped_files.check()?;
        self.note.check()?;
        super::domain_rules::record(
            "LanguageCoverage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum SemanticPackState {
    NotInstalled,
    Unavailable,
    Enabled,
    Applied,
    Disabled,
}
impl Check for SemanticPackState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticPackStatus {
    pub pack_id: Id,
    pub state: SemanticPackState,
    pub languages: Items<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Content>,
}
impl Check for SemanticPackStatus {
    fn check(&self) -> Result<(), String> {
        self.pack_id.check()?;
        self.state.check()?;
        self.languages.check()?;
        self.note.check()?;
        super::domain_rules::record(
            "SemanticPackStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OverlayState {
    Queued,
    Indexing,
    Ready,
    Failed,
    Stale,
    Expired,
    Unavailable,
}
impl Check for OverlayState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OverlayStatus {
    pub overlay_id: Id,
    pub revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<Id>,
    pub state: OverlayState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_ref: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_digest: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Content>,
    pub updated_at_ms: Counter,
    pub expires_at_ms: Counter,
}
impl Check for OverlayStatus {
    fn check(&self) -> Result<(), String> {
        self.overlay_id.check()?;
        self.revision.check()?;
        self.worktree_id.check()?;
        self.state.check()?;
        self.view_id.check()?;
        self.base_commit.check()?;
        self.head_commit.check()?;
        self.head_ref.check()?;
        self.capture_digest.check()?;
        self.run_id.check()?;
        self.reason.check()?;
        self.updated_at_ms.check()?;
        self.expires_at_ms.check()?;
        super::domain_rules::record(
            "OverlayStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum IndexRunState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}
impl Check for IndexRunState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum IndexRunKind {
    Index,
    Reindex,
    Overlay,
}
impl Check for IndexRunKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OpenRouterDataCollection {
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "deny")]
    Deny,
}
impl Check for OpenRouterDataCollection {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OpenRouterQuantization {
    #[serde(rename = "int4")]
    Int4,
    #[serde(rename = "int8")]
    Int8,
    #[serde(rename = "fp4")]
    Fp4,
    #[serde(rename = "mxfp4")]
    Mxfp4,
    #[serde(rename = "nvfp4")]
    Nvfp4,
    #[serde(rename = "fp6")]
    Fp6,
    #[serde(rename = "fp8")]
    Fp8,
    #[serde(rename = "mxfp8")]
    Mxfp8,
    #[serde(rename = "fp16")]
    Fp16,
    #[serde(rename = "bf16")]
    Bf16,
    #[serde(rename = "fp32")]
    Fp32,
    #[serde(rename = "unknown")]
    Unknown,
}
impl Check for OpenRouterQuantization {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OpenRouterSortBy {
    #[serde(rename = "price")]
    Price,
    #[serde(rename = "throughput")]
    Throughput,
    #[serde(rename = "latency")]
    Latency,
}
impl Check for OpenRouterSortBy {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OpenRouterPartition {
    #[serde(rename = "model")]
    Model,
    #[serde(rename = "none")]
    None,
}
impl Check for OpenRouterPartition {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum OpenRouterCostTier {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
    #[serde(rename = "xhigh")]
    Xhigh,
    #[serde(rename = "max")]
    Max,
}
impl Check for OpenRouterCostTier {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRouterSort {
    pub by: OpenRouterSortBy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<OpenRouterPartition>,
}
impl Check for OpenRouterSort {
    fn check(&self) -> Result<(), String> {
        self.by.check()?;
        self.partition.check()?;
        super::domain_rules::record(
            "OpenRouterSort",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRouterPercentiles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50: Option<Finite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p75: Option<Finite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p90: Option<Finite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99: Option<Finite>,
}
impl Check for OpenRouterPercentiles {
    fn check(&self) -> Result<(), String> {
        self.p50.check()?;
        self.p75.check()?;
        self.p90.check()?;
        self.p99.check()?;
        super::domain_rules::record(
            "OpenRouterPercentiles",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRouterMaxPrice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Money>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<Money>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Money>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Money>,
}
impl Check for OpenRouterMaxPrice {
    fn check(&self) -> Result<(), String> {
        self.prompt.check()?;
        self.completion.check()?;
        self.request.check()?;
        self.image.check()?;
        super::domain_rules::record(
            "OpenRouterMaxPrice",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRouterProviderRouting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Items<Label>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<OpenRouterDataCollection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforce_distillable_text: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only: Option<Items<Label>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Items<Label>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantizations: Option<Items<OpenRouterQuantization>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<OpenRouterSort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_min_throughput: Option<OpenRouterPercentiles>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_max_latency: Option<OpenRouterPercentiles>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_price: Option<OpenRouterMaxPrice>,
}
impl Check for OpenRouterProviderRouting {
    fn check(&self) -> Result<(), String> {
        self.order.check()?;
        self.allow_fallbacks.check()?;
        self.require_parameters.check()?;
        self.data_collection.check()?;
        self.zdr.check()?;
        self.enforce_distillable_text.check()?;
        self.only.check()?;
        self.ignore.check()?;
        self.quantizations.check()?;
        self.sort.check()?;
        self.preferred_min_throughput.check()?;
        self.preferred_max_latency.check()?;
        self.max_price.check()?;
        super::domain_rules::record(
            "OpenRouterProviderRouting",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenRouterConstraints {
    pub allowed_models: Items<Text>,
    pub excluded_models: Items<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_tier: Option<OpenRouterCostTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<OpenRouterProviderRouting>,
}
impl Check for OpenRouterConstraints {
    fn check(&self) -> Result<(), String> {
        self.allowed_models.check()?;
        self.excluded_models.check()?;
        self.cost_tier.check()?;
        self.provider.check()?;
        super::domain_rules::record(
            "OpenRouterConstraints",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ConnectionLifecycle {
    Disconnected,
    Authorizing,
    Connected,
    Refreshing,
    ReauthenticationRequired,
    Unavailable,
    Disabled,
}
impl Check for ConnectionLifecycle {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum SignInMethod {
    DeviceCode,
    AuthorizationCode,
}
impl Check for SignInMethod {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignInChallenge {
    pub flow_id: Id,
    pub method: SignInMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<Url>,
    pub expires_at_ms: Counter,
}
impl Check for SignInChallenge {
    fn check(&self) -> Result<(), String> {
        self.flow_id.check()?;
        self.method.check()?;
        self.user_code.check()?;
        self.verification_url.check()?;
        self.authorize_url.check()?;
        self.expires_at_ms.check()?;
        super::domain_rules::record(
            "SignInChallenge",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum KnowledgeEntryKind {
    Goal,
    Decision,
    Constraint,
    Fact,
    Gotcha,
    Attempt,
    Architecture,
    Lesson,
    Finding,
    Proposal,
}
impl Check for KnowledgeEntryKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum KnowledgeLifecycle {
    Active,
    Superseded,
    Retracted,
    Deleted,
}
impl Check for KnowledgeLifecycle {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum KnowledgeOrigin {
    User,
    Agent,
}
impl Check for KnowledgeOrigin {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum LessonSource {
    Prompt,
    Review,
    RelayedPrompt,
    RelayedReview,
    Agent,
    Subagent,
    BotReview,
}
impl Check for LessonSource {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum RuleState {
    Candidate,
    Proposed,
    Effective,
    Rejected,
    Retired,
}
impl Check for RuleState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum RuleEffect {
    UserInstruction,
    Recurrence,
    Promoted,
}
impl Check for RuleEffect {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum LessonAddressKind {
    Code,
    Instruction,
    Skill,
    Documentation,
    Test,
}
impl Check for LessonAddressKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ChallengeOutcome {
    Uphold,
    Retire,
}
impl Check for ChallengeOutcome {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ChallengeState {
    Open,
    Upheld,
    Retired,
}
impl Check for ChallengeState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PromoteAction {
    Effective,
    Propose,
    Reject,
    Address,
}
impl Check for PromoteAction {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum LessonView {
    All,
    RecentlyLearned,
    Recurring,
    Unaddressed,
    Undelivered,
}
impl Check for LessonView {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Applicability {
    pub activities: Items<Label>,
    pub paths: Items<RelativePath>,
    pub tools: Items<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<Label>,
    pub repository_ids: Items<Id>,
}
impl Check for Applicability {
    fn check(&self) -> Result<(), String> {
        self.activities.check()?;
        self.paths.check()?;
        self.tools.check()?;
        self.harness.check()?;
        self.repository_ids.check()?;
        super::domain_rules::record(
            "Applicability",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LessonDraft {
    pub source: LessonSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted_words: Option<Content>,
    pub applicability: Applicability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<Id>,
}
impl Check for LessonDraft {
    fn check(&self) -> Result<(), String> {
        self.source.check()?;
        self.source_ref.check()?;
        self.quoted_words.check()?;
        self.applicability.check()?;
        self.same_as.check()?;
        super::domain_rules::record(
            "LessonDraft",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LessonAddress {
    pub kind: LessonAddressKind,
    pub name: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<Text>,
}
impl Check for LessonAddress {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.name.check()?;
        self.reference.check()?;
        super::domain_rules::record(
            "LessonAddress",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LessonChallenge {
    pub id: Id,
    pub state: ChallengeState,
    pub reason: Content,
    pub evidence_ids: Items<Id>,
    pub author_id: ActorId,
    pub opened_at_ms: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Content>,
}
impl Check for LessonChallenge {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.state.check()?;
        self.reason.check()?;
        self.evidence_ids.check()?;
        self.author_id.check()?;
        self.opened_at_ms.check()?;
        self.resolved_at_ms.check()?;
        self.resolution.check()?;
        super::domain_rules::record(
            "LessonChallenge",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LessonRecord {
    pub rule_state: RuleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<RuleEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at_ms: Option<Counter>,
    pub source: LessonSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quoted_words: Option<Content>,
    pub applicability: Applicability,
    pub recurrence: Counter,
    pub recurrence_threshold: Counter,
    pub challenges: Items<LessonChallenge>,
    pub addressed_by: Items<LessonAddress>,
    pub delivered_sessions: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivered_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Content>,
}
impl Check for LessonRecord {
    fn check(&self) -> Result<(), String> {
        self.rule_state.check()?;
        self.effect.check()?;
        self.effective_at_ms.check()?;
        self.source.check()?;
        self.source_ref.check()?;
        self.quoted_words.check()?;
        self.applicability.check()?;
        self.recurrence.check()?;
        self.recurrence_threshold.check()?;
        self.challenges.check()?;
        self.addressed_by.check()?;
        self.delivered_sessions.check()?;
        self.last_delivered_at_ms.check()?;
        self.reason.check()?;
        super::domain_rules::record(
            "LessonRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeRelation {
    pub id: Id,
    pub subject_id: Id,
    pub predicate: Label,
    pub object_id: Id,
    pub evidence_ids: Items<Id>,
    pub author_id: ActorId,
    pub created_at_ms: Counter,
}
impl Check for KnowledgeRelation {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.subject_id.check()?;
        self.predicate.check()?;
        self.object_id.check()?;
        self.evidence_ids.check()?;
        self.author_id.check()?;
        self.created_at_ms.check()?;
        super::domain_rules::record(
            "KnowledgeRelation",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeRevisionEntry {
    pub revision: Revision,
    pub change: Label,
    pub author_id: ActorId,
    pub at_ms: Counter,
    pub lifecycle: KnowledgeLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Content>,
}
impl Check for KnowledgeRevisionEntry {
    fn check(&self) -> Result<(), String> {
        self.revision.check()?;
        self.change.check()?;
        self.author_id.check()?;
        self.at_ms.check()?;
        self.lifecycle.check()?;
        self.write_id.check()?;
        self.value.check()?;
        super::domain_rules::record(
            "KnowledgeRevisionEntry",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum JobRunStatus {
    Queued,
    Running,
    Holding,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
    OutcomeUnknown,
}
impl Check for JobRunStatus {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum JobTrigger {
    Scheduled,
    Manual,
}
impl Check for JobTrigger {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum JobHold {
    ReactivationRequired,
    AuthorizationRequired,
    ProjectClosed,
}
impl Check for JobHold {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReadinessDisposition {
    ResolvedDecision,
    BoundedInvestigation,
    NeedsUserDecision,
}
impl Check for ReadinessDisposition {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum FindingState {
    Open,
    Resolved,
    RiskAccepted,
}
impl Check for FindingState {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ConfidenceLevel {
    High,
    Medium,
    Low,
}
impl Check for ConfidenceLevel {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ReadinessFreshness {
    Current,
    Stale,
}
impl Check for ReadinessFreshness {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadinessFinding {
    pub id: Id,
    pub disposition: ReadinessDisposition,
    pub blocking: bool,
    pub prerequisite: bool,
    pub state: FindingState,
    pub requirement_ids: Items<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_key: Option<Id>,
    pub affected_work_ids: Items<Id>,
    pub summary: Content,
    pub missing: Content,
    pub consequence: Content,
    pub attempts: Items<Content>,
    pub insufficiency: Content,
    pub closure_condition: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<ConfidenceLevel>,
    pub evidence: Items<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closure_note: Option<Content>,
    pub closure_evidence: Items<ArtifactRef>,
}
impl Check for ReadinessFinding {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.disposition.check()?;
        self.blocking.check()?;
        self.prerequisite.check()?;
        self.state.check()?;
        self.requirement_ids.check()?;
        self.scope_key.check()?;
        self.affected_work_ids.check()?;
        self.summary.check()?;
        self.missing.check()?;
        self.consequence.check()?;
        self.attempts.check()?;
        self.insufficiency.check()?;
        self.closure_condition.check()?;
        self.decision.check()?;
        self.confidence.check()?;
        self.evidence.check()?;
        self.objective.check()?;
        self.limits.check()?;
        self.question.check()?;
        self.recommendation.check()?;
        self.closure_note.check()?;
        self.closure_evidence.check()?;
        super::domain_rules::record(
            "ReadinessFinding",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadinessAssessment {
    pub id: Id,
    pub revision: Revision,
    pub plan_revision: Revision,
    pub plan_digest: Digest,
    pub reviewed_pending: bool,
    pub freshness: ReadinessFreshness,
    pub stale_scope_keys: Items<Id>,
    pub ready: bool,
    pub summary: Content,
    pub findings: Items<ReadinessFinding>,
    pub assessed_by: ActorId,
    pub assessed_at_ms: Counter,
}
impl Check for ReadinessAssessment {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.plan_revision.check()?;
        self.plan_digest.check()?;
        self.reviewed_pending.check()?;
        self.freshness.check()?;
        self.stale_scope_keys.check()?;
        self.ready.check()?;
        self.summary.check()?;
        self.findings.check()?;
        self.assessed_by.check()?;
        self.assessed_at_ms.check()?;
        super::domain_rules::record(
            "ReadinessAssessment",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ReadinessChange {
    Assess {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_assessment_revision: Option<Revision>,
        plan_revision: Revision,
        plan_digest: Digest,
        summary: Content,
        findings: Items<ReadinessFinding>,
    },
    CloseFinding {
        expected_assessment_revision: Revision,
        finding_id: Id,
        note: Content,
        evidence: Items<ArtifactRef>,
    },
}
impl Check for ReadinessChange {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Assess {
                expected_assessment_revision,
                plan_revision,
                plan_digest,
                summary,
                findings,
            } => {
                expected_assessment_revision.check()?;
                plan_revision.check()?;
                plan_digest.check()?;
                summary.check()?;
                findings.check()?;
            }
            Self::CloseFinding {
                expected_assessment_revision,
                finding_id,
                note,
                evidence,
            } => {
                expected_assessment_revision.check()?;
                finding_id.check()?;
                note.check()?;
                evidence.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueuePlacement {
    pub work_id: Id,
    pub anchor_id: Id,
    pub side: QueueSide,
}
impl Check for QueuePlacement {
    fn check(&self) -> Result<(), String> {
        self.work_id.check()?;
        self.anchor_id.check()?;
        self.side.check()?;
        super::domain_rules::record(
            "QueuePlacement",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum QueueSide {
    Before,
    After,
}
impl Check for QueueSide {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestedRepositoryRevision {
    pub repository_id: Id,
    pub worktree_id: Id,
    pub head: GitOid,
    pub dirty: bool,
    pub ownership_revision: Revision,
    pub observed_at_ms: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<Url>,
}
impl Check for TestedRepositoryRevision {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.worktree_id.check()?;
        self.head.check()?;
        self.dirty.check()?;
        self.ownership_revision.check()?;
        self.observed_at_ms.check()?;
        self.repository_url.check()?;
        super::domain_rules::record(
            "TestedRepositoryRevision",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum EvalOutcome {
    #[serde(rename = "passed")]
    Passed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "unclear")]
    Unclear,
    #[serde(rename = "not_applicable")]
    NotApplicable,
    #[serde(rename = "evidence_unavailable")]
    EvidenceUnavailable,
    #[serde(rename = "grader_error")]
    GraderError,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "budget_exhausted")]
    BudgetExhausted,
}
impl Check for EvalOutcome {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum EvalMeasurementKind {
    Proxy,
    EnforcedRule,
}
impl Check for EvalMeasurementKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum EvalPredicate {
    ToolSucceeded {
        operation: Label,
    },
    TranscriptContains {
        entry_kind: Label,
        text: Text,
    },
    RecordFieldEquals {
        source: EvalRecordSource,
        pointer: Text,
        expected: DynamicJson,
    },
}
impl Check for EvalPredicate {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::ToolSucceeded { operation } => {
                operation.check()?;
            }
            Self::TranscriptContains { entry_kind, text } => {
                entry_kind.check()?;
                text.check()?;
            }
            Self::RecordFieldEquals {
                source,
                pointer,
                expected,
            } => {
                source.check()?;
                pointer.check()?;
                expected.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalDeterministicCheck {
    pub id: Id,
    pub rule_ref: Text,
    pub measurement: EvalMeasurementKind,
    pub predicate: EvalPredicate,
}
impl Check for EvalDeterministicCheck {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.rule_ref.check()?;
        self.measurement.check()?;
        self.predicate.check()?;
        super::domain_rules::record(
            "EvalDeterministicCheck",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalSuiteSnapshot {
    pub id: Id,
    pub revision: Revision,
    pub name: Label,
    pub checks: NonEmpty<EvalDeterministicCheck>,
}
impl Check for EvalSuiteSnapshot {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.name.check()?;
        self.checks.check()?;
        super::domain_rules::record(
            "EvalSuiteSnapshot",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalOutcomeCount {
    pub outcome: EvalOutcome,
    pub count: Counter,
}
impl Check for EvalOutcomeCount {
    fn check(&self) -> Result<(), String> {
        self.outcome.check()?;
        self.count.check()?;
        super::domain_rules::record(
            "EvalOutcomeCount",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalCoverage {
    pub eligible: Counter,
    pub graded: Counter,
    pub ungraded: Counter,
    pub passed: Counter,
    pub failed: Counter,
    pub ungraded_reasons: Rows<EvalOutcomeCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<Probability>,
}
impl Check for EvalCoverage {
    fn check(&self) -> Result<(), String> {
        self.eligible.check()?;
        self.graded.check()?;
        self.ungraded.check()?;
        self.passed.check()?;
        self.failed.check()?;
        self.ungraded_reasons.check()?;
        self.pass_rate.check()?;
        super::domain_rules::record(
            "EvalCoverage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalStepSource {
    pub session_id: Id,
    pub sequence: Counter,
    pub entry_id: Id,
    pub kind: Label,
}
impl Check for EvalStepSource {
    fn check(&self) -> Result<(), String> {
        self.session_id.check()?;
        self.sequence.check()?;
        self.entry_id.check()?;
        self.kind.check()?;
        super::domain_rules::record(
            "EvalStepSource",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum EvalRevisionContext {
    Exact {
        context_epoch: Counter,
        sources: NonEmpty<EvalRevisionSource>,
    },
    Mixed {
        context_epoch: Counter,
        sources: Items<EvalRevisionSource>,
        reason: Text,
    },
}
impl Check for EvalRevisionContext {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Exact {
                context_epoch,
                sources,
            } => {
                context_epoch.check()?;
                sources.check()?;
            }
            Self::Mixed {
                context_epoch,
                sources,
                reason,
            } => {
                context_epoch.check()?;
                sources.check()?;
                reason.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalInstanceResult {
    pub check_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EvalStepSource>,
    pub revision: EvalRevisionContext,
    pub outcome: EvalOutcome,
    pub coverage: EvalCoverage,
    pub detail: Text,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_value: Option<EvalObservedValue>,
}
impl Check for EvalInstanceResult {
    fn check(&self) -> Result<(), String> {
        self.check_id.check()?;
        self.source.check()?;
        self.revision.check()?;
        self.outcome.check()?;
        self.coverage.check()?;
        self.detail.check()?;
        self.observed_value.check()?;
        super::domain_rules::record(
            "EvalInstanceResult",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalRunResult {
    pub session_id: Id,
    pub outcome: EvalOutcome,
    pub coverage: EvalCoverage,
    pub instances: Rows<EvalInstanceResult>,
}
impl Check for EvalRunResult {
    fn check(&self) -> Result<(), String> {
        self.session_id.check()?;
        self.outcome.check()?;
        self.coverage.check()?;
        self.instances.check()?;
        super::domain_rules::record(
            "EvalRunResult",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalReport {
    pub id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    pub session_id: Id,
    pub suite: EvalSuiteSnapshot,
    pub created_at_ms: Counter,
    pub source_watermark: Counter,
    pub session_revision: Revision,
    pub measurement_digest: Digest,
    pub outcome: EvalOutcome,
    pub coverage: EvalCoverage,
    pub runs: Rows<EvalRunResult>,
    pub session_sequence: Counter,
    pub evidence_digest: Digest,
}
impl Check for EvalReport {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.project_id.check()?;
        self.session_id.check()?;
        self.suite.check()?;
        self.created_at_ms.check()?;
        self.source_watermark.check()?;
        self.session_revision.check()?;
        self.measurement_digest.check()?;
        self.outcome.check()?;
        self.coverage.check()?;
        self.runs.check()?;
        self.session_sequence.check()?;
        self.evidence_digest.check()?;
        super::domain_rules::record(
            "EvalReport",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalRevisionSource {
    pub source_id: Id,
    pub kind: Label,
    pub revision: Revision,
    pub disposition: Label,
}
impl Check for EvalRevisionSource {
    fn check(&self) -> Result<(), String> {
        self.source_id.check()?;
        self.kind.check()?;
        self.revision.check()?;
        self.disposition.check()?;
        super::domain_rules::record(
            "EvalRevisionSource",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalReportPage {
    pub items: Rows<EvalReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
}
impl Check for EvalReportPage {
    fn check(&self) -> Result<(), String> {
        self.items.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        super::domain_rules::record(
            "EvalReportPage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum EvalRecordSource {
    Session,
    HistoryEvent,
    GitLink,
    PullRequestLink,
    ReviewLink,
    WorkState,
}
impl Check for EvalRecordSource {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvalObservedValue {
    pub value: DynamicJson,
}
impl Check for EvalObservedValue {
    fn check(&self) -> Result<(), String> {
        self.value.check()?;
        super::domain_rules::record(
            "EvalObservedValue",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("x-earlier" = {"name": "RouteModel", "before": 3}))]
pub struct RouteCatalogModel {
    pub id: Id,
    pub display_name: Label,
    pub endpoint_profile: EndpointKind,
    pub upstream_model_id: Text,
    pub enabled: bool,
    pub tier_memberships: Items<TierName>,
    pub efforts: Items<EffortName>,
    pub image_input: bool,
    pub tools: bool,
    pub revision: Revision,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "PositiveCounter", extend("x-since" = 2))]
    pub context_tokens: Option<PositiveCounter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 2))]
    pub connection_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 3))]
    pub latest_family: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Items<Id>", extend("x-since" = 3))]
    pub resolves_to_model_ids: Option<Items<Id>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub available: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 3))]
    pub availability_reason: Option<Label>,
}
impl Check for RouteCatalogModel {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.display_name.check()?;
        self.endpoint_profile.check()?;
        self.upstream_model_id.check()?;
        self.enabled.check()?;
        self.tier_memberships.check()?;
        self.efforts.check()?;
        self.image_input.check()?;
        self.tools.check()?;
        self.revision.check()?;
        self.context_tokens.check()?;
        self.connection_id.check()?;
        self.latest_family.check()?;
        self.resolves_to_model_ids.check()?;
        self.available.check()?;
        self.availability_reason.check()?;
        super::domain_rules::record(
            "RouteCatalogModel",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for RouteCatalogModel {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.context_tokens = None;
        }
        if revision < 2 {
            self.connection_id = None;
        }
        if revision < 3 {
            self.latest_family = None;
        }
        if revision < 3 {
            self.resolves_to_model_ids = None;
        }
        if revision < 3 {
            self.available = None;
        }
        if revision < 3 {
            self.availability_reason = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("x-earlier" = {"name": "ClaimRecord", "before": 3}))]
pub struct AllocationClaimRecord {
    pub id: Id,
    pub work_id: Id,
    pub owner_id: ActorId,
    pub fence: Counter,
    pub expires_at_ms: Counter,
    pub state: LeaseState,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "WorkKind", extend("x-since" = 3))]
    pub work_kind: Option<WorkKind>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 3))]
    pub work_revision: Option<Revision>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ProjectId", extend("x-since" = 3))]
    pub project_id: Option<ProjectId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "RootId", extend("x-since" = 3))]
    pub root_id: Option<RootId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Id", extend("x-since" = 3))]
    pub owner_session_id: Option<Id>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Revision", extend("x-since" = 3))]
    pub queue_revision: Option<Revision>,
}
impl Check for AllocationClaimRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.work_id.check()?;
        self.owner_id.check()?;
        self.fence.check()?;
        self.expires_at_ms.check()?;
        self.state.check()?;
        self.work_kind.check()?;
        self.work_revision.check()?;
        self.project_id.check()?;
        self.root_id.check()?;
        self.owner_session_id.check()?;
        self.queue_revision.check()?;
        super::domain_rules::record(
            "AllocationClaimRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for AllocationClaimRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.work_kind = None;
        }
        if revision < 3 {
            self.work_revision = None;
        }
        if revision < 3 {
            self.project_id = None;
        }
        if revision < 3 {
            self.root_id = None;
        }
        if revision < 3 {
            self.owner_session_id = None;
        }
        if revision < 3 {
            self.queue_revision = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationQueueSnapshot {
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<Id>,
    pub queue_revision: Revision,
    pub workflow_revision: Revision,
}
impl Check for AllocationQueueSnapshot {
    fn check(&self) -> Result<(), String> {
        self.project_id.check()?;
        self.feature_id.check()?;
        self.queue_revision.check()?;
        self.workflow_revision.check()?;
        super::domain_rules::record(
            "AllocationQueueSnapshot",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum WorkItemType {
    Story,
    Task,
    Bug,
}
impl Check for WorkItemType {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationRecovery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_owner_id: Option<ActorId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_session_id: Option<Id>,
    pub links: Items<WorkLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_handoff: Option<HandoffRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<AllocationActivity>,
    pub unmet_acceptance: Items<AcceptanceCriterion>,
}
impl Check for AllocationRecovery {
    fn check(&self) -> Result<(), String> {
        self.previous_owner_id.check()?;
        self.previous_session_id.check()?;
        self.links.check()?;
        self.latest_handoff.check()?;
        self.last_activity.check()?;
        self.unmet_acceptance.check()?;
        super::domain_rules::record(
            "AllocationRecovery",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum AllocationOutcome {
    Claimed {
        claim: AllocationClaimRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work: Option<WorkRecord>,
        reused: bool,
    },
    Recovered {
        claim: AllocationClaimRecord,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work: Option<WorkRecord>,
        reused: bool,
        recovery: AllocationRecovery,
    },
    NoEligible {
        reason: Label,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<DomainFault>,
        reused: bool,
    },
}
impl Check for AllocationOutcome {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Claimed {
                claim,
                work,
                reused,
            } => {
                claim.check()?;
                work.check()?;
                reused.check()?;
            }
            Self::Recovered {
                claim,
                work,
                reused,
                recovery,
            } => {
                claim.check()?;
                work.check()?;
                reused.check()?;
                recovery.check()?;
            }
            Self::NoEligible {
                reason,
                evidence,
                reused,
            } => {
                reason.check()?;
                evidence.check()?;
                reused.check()?;
            }
        }
        Ok(())
    }
}

impl Revisioned for AllocationOutcome {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::Claimed { claim, work, .. } => {
                claim.retain_revision(revision);
                work.retain_revision(revision);
            }
            Self::Recovered { claim, work, .. } => {
                claim.retain_revision(revision);
                work.retain_revision(revision);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("x-earlier" = {"name": "ClaimRecord", "before": 3}))]
pub enum AllocationSelection {
    #[schemars(extend("x-until" = 3))]
    Legacy(ClaimRecord),
    #[schemars(extend("x-since" = 3))]
    Current(AllocationOutcome),
}
impl Check for AllocationSelection {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Legacy(value) => value.check(),
            Self::Current(value) => value.check(),
        }
    }
}
impl Revisioned for AllocationSelection {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            let legacy = match self {
                Self::Current(
                    AllocationOutcome::Claimed { claim, .. }
                    | AllocationOutcome::Recovered { claim, .. },
                ) => Some(ClaimRecord {
                    id: claim.id.clone(),
                    work_id: claim.work_id.clone(),
                    owner_id: claim.owner_id.clone(),
                    fence: claim.fence.clone(),
                    expires_at_ms: claim.expires_at_ms.clone(),
                    state: claim.state.clone(),
                }),
                _ => None,
            };
            if let Some(legacy) = legacy {
                *self = Self::Legacy(legacy);
            }
        }
        if let Self::Current(value) = self {
            value.retain_revision(revision);
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::Legacy(_) if revision >= 3 => {
                Err("legacy allocation result requires its recorded revision".into())
            }
            Self::Current(_) if revision < 3 => {
                Err("allocation outcome cannot be represented at this revision".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationActivityPayload {
    pub value: DynamicJson,
}
impl Check for AllocationActivityPayload {
    fn check(&self) -> Result<(), String> {
        self.value.check()?;
        super::domain_rules::record(
            "AllocationActivityPayload",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationActivity {
    pub event_id: Id,
    pub sequence: Revision,
    pub actor_id: ActorId,
    pub occurred_at_ms: Counter,
    pub kind: Label,
    pub work_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<AllocationActivityPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_unavailable_reason: Option<Label>,
}
impl Check for AllocationActivity {
    fn check(&self) -> Result<(), String> {
        self.event_id.check()?;
        self.sequence.check()?;
        self.actor_id.check()?;
        self.occurred_at_ms.check()?;
        self.kind.check()?;
        self.work_id.check()?;
        self.payload.check()?;
        self.payload_unavailable_reason.check()?;
        super::domain_rules::record(
            "AllocationActivity",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkInheritanceOrigin {
    pub feature_id: Id,
    pub requirement_ids: Items<Id>,
}
impl Check for WorkInheritanceOrigin {
    fn check(&self) -> Result<(), String> {
        self.feature_id.check()?;
        self.requirement_ids.check()?;
        super::domain_rules::record(
            "WorkInheritanceOrigin",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(extend("x-earlier" = {"name": "ProposalRecord", "before": 3}))]
pub struct ApprovalViewRecord {
    pub id: Id,
    pub revision: Revision,
    pub content_digest: Digest,
    pub state: ApprovalState,
    pub operation_kind: Label,
    pub target_id: Id,
    pub target_revision: Revision,
    pub summary: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "DataScope", extend("x-since" = 3))]
    pub scope: Option<DataScope>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ApprovalContent", extend("x-since" = 3))]
    pub payload: Option<ApprovalContent>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "bool", extend("x-since" = 3))]
    pub stale: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ActorId", extend("x-since" = 3))]
    pub requested_by: Option<ActorId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Counter", extend("x-since" = 3))]
    pub created_at_ms: Option<Counter>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Digest", extend("x-since" = 3))]
    pub scope_digest: Option<Digest>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "DynamicJson", extend("x-since" = 3))]
    pub reviewed_content: Option<DynamicJson>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Content", extend("x-since" = 3))]
    pub rationale: Option<Content>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Label", extend("x-since" = 3))]
    pub resume_status: Option<Label>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "OperationId", extend("x-since" = 3))]
    pub resumed_operation_id: Option<OperationId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "SavedOperationPreview", extend("x-since" = 3))]
    pub operation_preview: Option<SavedOperationPreview>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "ApprovalReviewedContent", extend("x-since" = 3))]
    pub reviewed_source: Option<ApprovalReviewedContent>,
}
impl Check for ApprovalViewRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.content_digest.check()?;
        self.state.check()?;
        self.operation_kind.check()?;
        self.target_id.check()?;
        self.target_revision.check()?;
        self.summary.check()?;
        self.artifact.check()?;
        self.scope.check()?;
        self.payload.check()?;
        self.stale.check()?;
        self.requested_by.check()?;
        self.created_at_ms.check()?;
        self.scope_digest.check()?;
        self.reviewed_content.check()?;
        self.rationale.check()?;
        self.resume_status.check()?;
        self.resumed_operation_id.check()?;
        self.operation_preview.check()?;
        self.reviewed_source.check()?;
        super::domain_rules::record(
            "ApprovalViewRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for ApprovalViewRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.scope = None;
        }
        if revision < 3 {
            self.payload = None;
        }
        if revision < 3 {
            self.stale = None;
        }
        if revision < 3 {
            self.requested_by = None;
        }
        if revision < 3 {
            self.created_at_ms = None;
        }
        if revision < 3 {
            self.scope_digest = None;
        }
        if revision < 3 {
            self.reviewed_content = None;
        }
        if revision < 3 {
            self.rationale = None;
        }
        if revision < 3 {
            self.resume_status = None;
        }
        if revision < 3 {
            self.resumed_operation_id = None;
        }
        if revision < 3 {
            self.operation_preview = None;
        }
        if revision < 3 {
            self.reviewed_source = None;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedOperationPreview {
    pub effect: Label,
    pub target_ids: Items<Id>,
    pub project_ids: Items<ProjectId>,
    pub request_digest: Digest,
}
impl Check for SavedOperationPreview {
    fn check(&self) -> Result<(), String> {
        self.effect.check()?;
        self.target_ids.check()?;
        self.project_ids.check()?;
        self.request_digest.check()?;
        super::domain_rules::record(
            "SavedOperationPreview",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ApprovalContent {
    Settings {
        entries: NonEmpty<SettingAssignment>,
    },
    FeaturePlan {
        proposal_id: Id,
    },
    FindingRisk {
        feature_id: Id,
        finding_id: Id,
        assessment_revision: Revision,
    },
    SavedOperation {
        schema_revision: PositiveCounter,
        input: SavedCommandInput,
    },
}
impl Check for ApprovalContent {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Settings { entries } => {
                entries.check()?;
            }
            Self::FeaturePlan { proposal_id } => {
                proposal_id.check()?;
            }
            Self::FindingRisk {
                feature_id,
                finding_id,
                assessment_revision,
            } => {
                feature_id.check()?;
                finding_id.check()?;
                assessment_revision.check()?;
            }
            Self::SavedOperation {
                schema_revision,
                input,
            } => {
                schema_revision.check()?;
                input.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum KnowledgeReviewKind {
    Challenge,
    ProposedRule,
    Contradiction,
}
impl Check for KnowledgeReviewKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeReviewItem {
    pub kind: KnowledgeReviewKind,
    pub entry: KnowledgeClaim,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<KnowledgeClaim>,
    pub priority: Label,
    pub recurrence: Counter,
    pub since_ms: Counter,
    pub challenges: Rows<LessonChallenge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more_challenges: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<ActorId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_author: Option<ActorId>,
    pub informational: bool,
}
impl Check for KnowledgeReviewItem {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.entry.check()?;
        self.other.check()?;
        self.priority.check()?;
        self.recurrence.check()?;
        self.since_ms.check()?;
        self.challenges.check()?;
        self.more_challenges.check()?;
        self.author.check()?;
        self.other_author.check()?;
        self.informational.check()?;
        super::domain_rules::record(
            "KnowledgeReviewItem",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for KnowledgeReviewItem {
    fn retain_revision(&mut self, revision: u32) {
        self.entry.retain_revision(revision);
        self.other.retain_revision(revision);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeReviewPage {
    pub items: Rows<KnowledgeReviewItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
}
impl Check for KnowledgeReviewPage {
    fn check(&self) -> Result<(), String> {
        self.items.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        super::domain_rules::record(
            "KnowledgeReviewPage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for KnowledgeReviewPage {
    fn retain_revision(&mut self, revision: u32) {
        self.items.retain_revision(revision);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotificationLink {
    pub kind: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
}
impl Check for NotificationLink {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.target_id.check()?;
        self.url.check()?;
        self.artifact.check()?;
        self.project_id.check()?;
        super::domain_rules::record(
            "NotificationLink",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ApprovalReviewedContent {
    Settings {
        entries: Items<SettingAssignment>,
    },
    FeaturePlan {
        plan: PlanDraft,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<FeatureBudgetInput>,
    },
}
impl Check for ApprovalReviewedContent {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Settings { entries } => {
                entries.check()?;
            }
            Self::FeaturePlan { plan, budget } => {
                plan.check()?;
                budget.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceTariff {
    pub upstream_model_id: Text,
    pub service_tier: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_lower_exclusive: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_upper_inclusive: Option<Counter>,
    pub modality: PriceModality,
    pub charge_kind: PriceChargeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_seconds: Option<PositiveCounter>,
    pub unit: PriceUnit,
    pub units: PositiveCounter,
    pub amount_usd: Money,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_profile: Option<Label>,
}
impl Check for PriceTariff {
    fn check(&self) -> Result<(), String> {
        self.upstream_model_id.check()?;
        self.service_tier.check()?;
        self.context_lower_exclusive.check()?;
        self.context_upper_inclusive.check()?;
        self.modality.check()?;
        self.charge_kind.check()?;
        self.cache_ttl_seconds.check()?;
        self.unit.check()?;
        self.units.check()?;
        self.amount_usd.check()?;
        self.region.check()?;
        self.endpoint_profile.check()?;
        super::domain_rules::record(
            "PriceTariff",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceSourceSnapshot {
    pub id: Id,
    pub provider_id: Id,
    pub revision: Revision,
    pub source_url: Url,
    pub source_digest: Digest,
    pub parser_version: Label,
    pub fetched_at_ms: Counter,
    pub verified_at_ms: Counter,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_date: Option<Label>,
}
impl Check for PriceSourceSnapshot {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.provider_id.check()?;
        self.revision.check()?;
        self.source_url.check()?;
        self.source_digest.check()?;
        self.parser_version.check()?;
        self.fetched_at_ms.check()?;
        self.verified_at_ms.check()?;
        self.effective_date.check()?;
        super::domain_rules::record(
            "PriceSourceSnapshot",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderPriceStatus {
    pub provider_id: Id,
    pub revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Id>,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_check_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_operation_id: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Text>,
}
impl Check for ProviderPriceStatus {
    fn check(&self) -> Result<(), String> {
        self.provider_id.check()?;
        self.revision.check()?;
        self.snapshot_id.check()?;
        self.stale.check()?;
        self.last_checked_at_ms.check()?;
        self.next_check_at_ms.check()?;
        self.refresh_operation_id.check()?;
        self.error.check()?;
        super::domain_rules::record(
            "ProviderPriceStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceCatalogEntry {
    pub provider_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<Id>,
    pub upstream_model_id: Text,
    pub origin: PriceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_revision: Option<Revision>,
    pub tariff: PriceTariff,
}
impl Check for PriceCatalogEntry {
    fn check(&self) -> Result<(), String> {
        self.provider_id.check()?;
        self.model_id.check()?;
        self.upstream_model_id.check()?;
        self.origin.check()?;
        self.snapshot_id.check()?;
        self.override_revision.check()?;
        self.tariff.check()?;
        super::domain_rules::record(
            "PriceCatalogEntry",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceCatalogView {
    pub catalog_revision: Revision,
    pub items: Rows<PriceCatalogEntry>,
    pub snapshots: Rows<PriceSourceSnapshot>,
    pub providers: Rows<ProviderPriceStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    pub coverage: Coverage,
    pub models: Rows<ModelPriceView>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "gated_member"
    )]
    #[schemars(with = "Rows<PriceOverrideOutcome>", extend("x-since" = 3))]
    pub override_history: Option<Rows<PriceOverrideOutcome>>,
}
impl Check for PriceCatalogView {
    fn check(&self) -> Result<(), String> {
        self.catalog_revision.check()?;
        self.items.check()?;
        self.snapshots.check()?;
        self.providers.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        self.coverage.check()?;
        self.models.check()?;
        self.override_history.check()?;
        super::domain_rules::record(
            "PriceCatalogView",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for PriceCatalogView {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 3 {
            self.override_history = None;
        } else {
            self.override_history.retain_revision(revision);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("x-earlier" = {"name": "Page", "before": 3}))]
pub enum PriceCatalogResult {
    #[schemars(extend("x-until" = 3))]
    Legacy(Page<ModelPriceStatus>),
    #[schemars(extend("x-since" = 3))]
    Current(PriceCatalogView),
}
impl Check for PriceCatalogResult {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Legacy(value) => value.check(),
            Self::Current(value) => value.check(),
        }
    }
}
impl Revisioned for PriceCatalogResult {
    fn retain_revision(&mut self, revision: u32) {
        if let Self::Current(value) = self {
            value.retain_revision(revision);
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::Legacy(_) if revision >= 3 => {
                Err("legacy result requires its recorded revision".into())
            }
            Self::Current(_) if revision < 3 => {
                Err("current result cannot be represented at this revision".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PriceOverrideChange {
    Set { tariffs: NonEmpty<PriceTariff> },
    Clear {},
}
impl Check for PriceOverrideChange {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Set { tariffs } => {
                tariffs.check()?;
            }
            Self::Clear {} => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PriceOverrideOutcome {
    Set {
        model_id: Id,
        revision: Revision,
        tariffs: NonEmpty<PriceTariff>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ActorId", extend("x-since" = 3))]
        updated_by: Option<ActorId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 3))]
        updated_at_ms: Option<Counter>,
    },
    Cleared {
        model_id: Id,
        revision: Revision,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ActorId", extend("x-since" = 3))]
        updated_by: Option<ActorId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 3))]
        updated_at_ms: Option<Counter>,
    },
}
impl Check for PriceOverrideOutcome {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Set {
                model_id,
                revision,
                tariffs,
                updated_by,
                updated_at_ms,
            } => {
                model_id.check()?;
                revision.check()?;
                tariffs.check()?;
                updated_by.check()?;
                updated_at_ms.check()?;
            }
            Self::Cleared {
                model_id,
                revision,
                updated_by,
                updated_at_ms,
            } => {
                model_id.check()?;
                revision.check()?;
                updated_by.check()?;
                updated_at_ms.check()?;
            }
        }
        Ok(())
    }
}

impl Revisioned for PriceOverrideOutcome {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::Set {
                updated_by,
                updated_at_ms,
                ..
            } => {
                if revision < 3 {
                    *updated_by = None;
                }
                if revision < 3 {
                    *updated_at_ms = None;
                }
            }
            Self::Cleared {
                updated_by,
                updated_at_ms,
                ..
            } => {
                if revision < 3 {
                    *updated_by = None;
                }
                if revision < 3 {
                    *updated_at_ms = None;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("x-earlier" = {"name": "PriceRecord", "before": 3}))]
pub enum PriceOverrideResult {
    #[schemars(extend("x-until" = 3))]
    Legacy(PriceRecord),
    #[schemars(extend("x-since" = 3))]
    Current(PriceOverrideOutcome),
}
impl Check for PriceOverrideResult {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Legacy(value) => value.check(),
            Self::Current(value) => value.check(),
        }
    }
}
impl Revisioned for PriceOverrideResult {
    fn retain_revision(&mut self, revision: u32) {
        if let Self::Current(value) = self {
            value.retain_revision(revision);
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::Legacy(_) if revision >= 3 => {
                Err("legacy result requires its recorded revision".into())
            }
            Self::Current(_) if revision < 3 => {
                Err("current result cannot be represented at this revision".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum UsageReportKind {
    Attempts,
    AssistantUsage,
    AssistantActivity,
    McpUsage,
    CodeReviewUsage,
    RepositoryAnalysisUsage,
    Filters,
}
impl Check for UsageReportKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageReportRange {
    pub start_ms: SignedCounter,
    pub end_ms: SignedCounter,
}
impl Check for UsageReportRange {
    fn check(&self) -> Result<(), String> {
        self.start_ms.check()?;
        self.end_ms.check()?;
        super::domain_rules::record(
            "UsageReportRange",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageAppliedFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<DataScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_id: Option<RootId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_run_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<Label>,
}
impl Check for UsageAppliedFilters {
    fn check(&self) -> Result<(), String> {
        self.scope.check()?;
        self.project_id.check()?;
        self.root_id.check()?;
        self.session_id.check()?;
        self.job_run_id.check()?;
        self.user.check()?;
        self.provider.check()?;
        self.model.check()?;
        self.tool.check()?;
        self.purpose.check()?;
        super::domain_rules::record(
            "UsageAppliedFilters",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageQueryContext {
    pub report_kind: UsageReportKind,
    pub range: UsageReportRange,
    pub interval: Label,
    pub applied_filters: UsageAppliedFilters,
    pub read_revision: Revision,
}
impl Check for UsageQueryContext {
    fn check(&self) -> Result<(), String> {
        self.report_kind.check()?;
        self.range.check()?;
        self.interval.check()?;
        self.applied_filters.check()?;
        self.read_revision.check()?;
        super::domain_rules::record(
            "UsageQueryContext",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageCacheDiagnostic {
    pub kind: Label,
    pub inferred: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<Revision>,
}
impl Check for UsageCacheDiagnostic {
    fn check(&self) -> Result<(), String> {
        self.kind.check()?;
        self.inferred.check()?;
        self.detail.check()?;
        self.source_revision.check()?;
        super::domain_rules::record(
            "UsageCacheDiagnostic",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageCostSnapshot {
    pub price_kind: UsageCostKind,
    pub price_origin: PriceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_micro_usd: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_amount: Option<Money>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_snapshot_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_revision: Option<Revision>,
    pub subscription_equivalent: bool,
    pub incomplete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untracked_reason: Option<Content>,
    pub tariffs: Rows<PriceTariff>,
    pub monetary_complete: bool,
}
impl Check for UsageCostSnapshot {
    fn check(&self) -> Result<(), String> {
        self.price_kind.check()?;
        self.price_origin.check()?;
        self.amount_micro_usd.check()?;
        self.currency.check()?;
        self.original_amount.check()?;
        self.classification.check()?;
        self.price_snapshot_id.check()?;
        self.price_revision.check()?;
        self.override_revision.check()?;
        self.subscription_equivalent.check()?;
        self.incomplete.check()?;
        self.untracked_reason.check()?;
        self.tariffs.check()?;
        self.monetary_complete.check()?;
        super::domain_rules::record(
            "UsageCostSnapshot",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageAttemptRecord {
    pub id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_number: Option<Revision>,
    pub source_kind: Label,
    pub source_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<ActorId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<RootId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<AssignmentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_run_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub purpose: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Label>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model_id: Option<Text>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<Counter>,
    pub state: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncached_input_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_micro_usd: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_micro_usd: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_semantics: Option<Label>,
    pub cost: UsageCostSnapshot,
    pub cache_diagnostics: Rows<UsageCacheDiagnostic>,
    pub unknown_tail: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<Counter>,
    pub unit_quantities: Rows<UsageMeasuredQuantity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_revision: Option<Revision>,
}
impl Check for UsageAttemptRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.request_id.check()?;
        self.attempt_id.check()?;
        self.attempt_number.check()?;
        self.source_kind.check()?;
        self.source_id.check()?;
        self.actor_id.check()?;
        self.project_id.check()?;
        self.feature_id.check()?;
        self.work_id.check()?;
        self.assignment_id.check()?;
        self.session_id.check()?;
        self.job_run_id.check()?;
        self.operation_id.check()?;
        self.purpose.check()?;
        self.role.check()?;
        self.provider_id.check()?;
        self.connection_id.check()?;
        self.model_id.check()?;
        self.resolved_model_id.check()?;
        self.started_at_ms.check()?;
        self.finished_at_ms.check()?;
        self.state.check()?;
        self.input_tokens.check()?;
        self.uncached_input_tokens.check()?;
        self.output_tokens.check()?;
        self.cache_read_tokens.check()?;
        self.cache_write_tokens.check()?;
        self.cache_read_micro_usd.check()?;
        self.cache_write_micro_usd.check()?;
        self.input_semantics.check()?;
        self.cost.check()?;
        self.cache_diagnostics.check()?;
        self.unknown_tail.check()?;
        self.reasoning_tokens.check()?;
        self.unit_quantities.check()?;
        self.connection_revision.check()?;
        super::domain_rules::record(
            "UsageAttemptRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageTotals {
    pub attempts: Counter,
    pub tracked_micro_usd: Counter,
    pub reported_micro_usd: Counter,
    pub estimated_micro_usd: Counter,
    pub reserved_micro_usd: Counter,
    pub local_free_attempts: Counter,
    pub untracked_attempts: Counter,
    pub unknown_tail_attempts: Counter,
    pub incomplete_attempts: Counter,
    pub monetary_incomplete_attempts: Counter,
    pub legacy_usage_facts: Counter,
    pub legacy_untracked_facts: Counter,
    pub legacy_observation_micro_usd: Counter,
}
impl Check for UsageTotals {
    fn check(&self) -> Result<(), String> {
        self.attempts.check()?;
        self.tracked_micro_usd.check()?;
        self.reported_micro_usd.check()?;
        self.estimated_micro_usd.check()?;
        self.reserved_micro_usd.check()?;
        self.local_free_attempts.check()?;
        self.untracked_attempts.check()?;
        self.unknown_tail_attempts.check()?;
        self.incomplete_attempts.check()?;
        self.monetary_incomplete_attempts.check()?;
        self.legacy_usage_facts.check()?;
        self.legacy_untracked_facts.check()?;
        self.legacy_observation_micro_usd.check()?;
        super::domain_rules::record(
            "UsageTotals",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageAttemptPage {
    pub context: UsageQueryContext,
    pub items: Rows<UsageAttemptRecord>,
    pub totals: UsageTotals,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    pub coverage: Coverage,
}
impl Check for UsageAttemptPage {
    fn check(&self) -> Result<(), String> {
        self.context.check()?;
        self.items.check()?;
        self.totals.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "UsageAttemptPage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum UsageReportRow {
    Total {
        key: Label,
        label: Label,
        value: Counter,
    },
    SeriesPoint {
        key: Label,
        label: Content,
        bucket_start_ms: SignedCounter,
        value: Counter,
    },
    Breakdown {
        dimension: Label,
        key: Content,
        label: Content,
        value: Counter,
    },
}
impl Check for UsageReportRow {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Total { key, label, value } => {
                key.check()?;
                label.check()?;
                value.check()?;
            }
            Self::SeriesPoint {
                key,
                label,
                bucket_start_ms,
                value,
            } => {
                key.check()?;
                label.check()?;
                bucket_start_ms.check()?;
                value.check()?;
            }
            Self::Breakdown {
                dimension,
                key,
                label,
                value,
            } => {
                dimension.check()?;
                key.check()?;
                label.check()?;
                value.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageAggregatePage {
    pub context: UsageQueryContext,
    pub items: Rows<UsageReportRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    pub coverage: Coverage,
}
impl Check for UsageAggregatePage {
    fn check(&self) -> Result<(), String> {
        self.context.check()?;
        self.items.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "UsageAggregatePage",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageFilterOption {
    pub dimension: Label,
    pub value: Content,
}
impl Check for UsageFilterOption {
    fn check(&self) -> Result<(), String> {
        self.dimension.check()?;
        self.value.check()?;
        super::domain_rules::record(
            "UsageFilterOption",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageFilterOptions {
    pub context: UsageQueryContext,
    pub items: Rows<UsageFilterOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Cursor>,
    pub has_more: bool,
    pub coverage: Coverage,
}
impl Check for UsageFilterOptions {
    fn check(&self) -> Result<(), String> {
        self.context.check()?;
        self.items.check()?;
        self.next_cursor.check()?;
        self.has_more.check()?;
        self.coverage.check()?;
        super::domain_rules::record(
            "UsageFilterOptions",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum UsageQueryResult {
    Attempts { page: UsageAttemptPage },
    Report { report: UsageAggregatePage },
    Filters { filters: UsageFilterOptions },
}
impl Check for UsageQueryResult {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Attempts { page } => {
                page.check()?;
            }
            Self::Report { report } => {
                report.check()?;
            }
            Self::Filters { filters } => {
                filters.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("x-earlier" = {"name": "UsageReport", "before": 3}))]
pub enum RouterUsageResult {
    #[schemars(extend("x-until" = 3))]
    Legacy(UsageReport),
    #[schemars(extend("x-since" = 3))]
    Current(UsageQueryResult),
}
impl Check for RouterUsageResult {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Legacy(value) => value.check(),
            Self::Current(value) => value.check(),
        }
    }
}
impl Revisioned for RouterUsageResult {
    fn retain_revision(&mut self, _revision: u32) {}
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::Legacy(_) if revision >= 3 => {
                Err("legacy result requires its recorded revision".into())
            }
            Self::Current(_) if revision < 3 => {
                Err("current result cannot be represented at this revision".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum FeatureBudgetInput {
    Off {},
    Limit { micro_usd: Counter },
}
impl Check for FeatureBudgetInput {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::Off {} => {}
            Self::Limit { micro_usd } => {
                micro_usd.check()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeatureBudgetView {
    pub revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_micro_usd: Option<Counter>,
    pub spent_micro_usd: Counter,
    pub reserved_micro_usd: Counter,
    pub warning: bool,
    pub continue_required: bool,
    pub untracked_attempts: Counter,
    pub unknown_tail_attempts: Counter,
    pub accounting_ready: bool,
    pub monetary_incomplete_attempts: Counter,
    pub legacy_usage_facts: Counter,
}
impl Check for FeatureBudgetView {
    fn check(&self) -> Result<(), String> {
        self.revision.check()?;
        self.limit_micro_usd.check()?;
        self.spent_micro_usd.check()?;
        self.reserved_micro_usd.check()?;
        self.warning.check()?;
        self.continue_required.check()?;
        self.untracked_attempts.check()?;
        self.unknown_tail_attempts.check()?;
        self.accounting_ready.check()?;
        self.monetary_incomplete_attempts.check()?;
        self.legacy_usage_facts.check()?;
        super::domain_rules::record(
            "FeatureBudgetView",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PriceChargeKind {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Request,
    Runtime,
}
impl Check for PriceChargeKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PriceUnit {
    Token,
    Second,
    Image,
    Request,
    Character,
}
impl Check for PriceUnit {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PriceModality {
    Text,
    Image,
    Audio,
    Video,
    Any,
}
impl Check for PriceModality {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelPriceView {
    pub model_id: Id,
    pub revision: Revision,
    pub price_kind: UsagePriceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untracked_reason: Option<Text>,
}
impl Check for ModelPriceView {
    fn check(&self) -> Result<(), String> {
        self.model_id.check()?;
        self.revision.check()?;
        self.price_kind.check()?;
        self.snapshot_id.check()?;
        self.override_revision.check()?;
        self.untracked_reason.check()?;
        super::domain_rules::record(
            "ModelPriceView",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum UsageCostKind {
    Reported,
    Estimated,
    Free,
    Untracked,
    Unknown,
}
impl Check for UsageCostKind {
    fn check(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageMeasuredQuantity {
    pub modality: Label,
    pub charge_kind: Label,
    pub unit: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_ttl_ms: Option<Counter>,
    pub quantity: Counter,
}
impl Check for UsageMeasuredQuantity {
    fn check(&self) -> Result<(), String> {
        self.modality.check()?;
        self.charge_kind.check()?;
        self.unit.check()?;
        self.cache_write_ttl_ms.check()?;
        self.quantity.check()?;
        super::domain_rules::record(
            "UsageMeasuredQuantity",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DomainQuery {
    #[serde(rename = "projects.list")]
    ProjectsList {
        scope: DataScope,
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lifecycle: Option<LifeState>,
    },
    #[serde(rename = "projects.get")]
    ProjectsGet { project_id: ProjectId },
    #[serde(rename = "features.list")]
    FeaturesList {
        project_id: ProjectId,
        page: PageRequest,
        status_ids: Items<Id>,
        repository_ids: Items<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<Text>,
        detail_level: DetailLevel,
    },
    #[serde(rename = "features.get")]
    FeaturesGet {
        work_id: Id,
        detail_level: DetailLevel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail_page: Option<PageRequest>,
    },
    #[serde(rename = "issues.list")]
    IssuesList {
        project_id: ProjectId,
        page: PageRequest,
        status_ids: Items<Id>,
        repository_ids: Items<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<Text>,
        detail_level: DetailLevel,
    },
    #[serde(rename = "issues.get")]
    IssuesGet {
        work_id: Id,
        detail_level: DetailLevel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail_page: Option<PageRequest>,
    },
    #[serde(rename = "tasks.list")]
    TasksList {
        project_id: ProjectId,
        page: PageRequest,
        status_ids: Items<Id>,
        repository_ids: Items<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<Text>,
        detail_level: DetailLevel,
    },
    #[serde(rename = "tasks.get")]
    TasksGet {
        work_id: Id,
        detail_level: DetailLevel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail_page: Option<PageRequest>,
    },
    #[serde(rename = "workflows.list")]
    WorkflowsList {
        project_id: ProjectId,
        page: PageRequest,
    },
    #[serde(rename = "next_work.inspect_claim")]
    NextWorkInspectClaim { claim_id: Id },
    #[serde(rename = "agents.inspect")]
    AgentsInspect { assignment_id: AssignmentId },
    #[serde(rename = "agents.inbox")]
    AgentsInbox {
        assignment_id: AssignmentId,
        page: PageRequest,
        unacknowledged_only: bool,
        direction: MessageDirection,
    },
    #[serde(rename = "sessions.list")]
    SessionsList {
        scope: DataScope,
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closed: Option<bool>,
    },
    #[serde(rename = "sessions.get")]
    SessionsGet { session_id: Id },
    #[serde(rename = "sessions.briefing")]
    SessionsBriefing {
        session_id: Id,
        expected_context_epoch: Counter,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "RootId", extend("x-since" = 2))]
        item: Option<RootId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "PathSet", extend("x-since" = 2))]
        paths: Option<PathSet>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "TokenBudget", extend("x-since" = 2))]
        budget_tokens: Option<TokenBudget>,
    },
    #[serde(rename = "sessions.context_sources")]
    SessionsContextSources {
        session_id: Id,
        expected_context_epoch: Counter,
        page: PageRequest,
    },
    #[serde(rename = "ui.current_screen")]
    UiCurrentScreen {},
    #[serde(rename = "history.work_lineage")]
    HistoryWorkLineage { root_id: RootId, page: PageRequest },
    #[serde(rename = "history.git_links")]
    HistoryGitLinks { root_id: RootId, page: PageRequest },
    #[serde(rename = "history.evidence_links")]
    HistoryEvidenceLinks { root_id: RootId, page: PageRequest },
    #[serde(rename = "history.tool_usage")]
    #[schemars(extend("oneOf" = [{"required": ["kind", "rootId", "page"]}, {"required": ["kind", "projectId", "page"], "x-since": 2}, {"required": ["kind", "workspace", "page"], "x-since": 2}]))]
    HistoryToolUsage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "RootId")]
        root_id: Option<RootId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ProjectId", extend("x-since" = 2))]
        project_id: Option<ProjectId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 2, "const" = true))]
        workspace: Option<bool>,
        page: PageRequest,
    },
    #[serde(rename = "indexing.discover")]
    IndexingDiscover {
        scope: DataScope,
        roots: Items<Id>,
        page: PageRequest,
    },
    #[serde(rename = "indexing.status")]
    #[schemars(extend("oneOf" = [{"required": ["kind", "repositoryId"]}, {"required": ["kind", "overlayId"], "x-since": 2}]))]
    IndexingStatus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Id")]
        repository_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 2))]
        overlay_id: Option<Id>,
    },
    #[serde(rename = "indexing.runs")]
    IndexingRuns {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "NonEmpty<IndexRunState>", extend("x-since" = 2))]
        states: Option<NonEmpty<IndexRunState>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "NonEmpty<IndexRunKind>", extend("x-since" = 2))]
        kinds: Option<NonEmpty<IndexRunKind>>,
        page: PageRequest,
    },
    #[serde(rename = "analysis.health")]
    AnalysisHealth {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.cross_repo_links")]
    AnalysisCrossRepoLinks {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.status")]
    AnalysisStatus {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.clusters")]
    AnalysisClusters {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.cluster_detail")]
    AnalysisClusterDetail {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        cluster_id: Id,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.security")]
    AnalysisSecurity {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.metrics")]
    AnalysisMetrics {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.hotspots")]
    AnalysisHotspots {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.fleet_health")]
    AnalysisFleetHealth {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.archival_candidates")]
    AnalysisArchivalCandidates {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        filters: Items<Label>,
    },
    #[serde(rename = "analysis.file_hotspot")]
    AnalysisFileHotspot {
        scope: DataScope,
        views: ViewSelection,
        page: PageRequest,
        path: RelativePath,
        filters: Items<Label>,
    },
    #[serde(rename = "schemas.connections")]
    SchemasConnections { scope: DataScope, page: PageRequest },
    #[serde(rename = "schemas.objects")]
    SchemasObjects {
        scope: DataScope,
        views: ViewSelection,
        connection_id: Id,
        kinds: Items<MetadataObjectKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<Text>,
        page: PageRequest,
    },
    #[serde(rename = "schemas.relationships")]
    SchemasRelationships {
        scope: DataScope,
        views: ViewSelection,
        connection_id: Id,
        object_ids: Items<Id>,
        page: PageRequest,
    },
    #[serde(rename = "database.read")]
    DatabaseRead {
        connection_id: Id,
        expected_connection_revision: Revision,
        statement: Content,
        parameters: Items<DbParameter>,
        row_limit: Limit,
        timeout_ms: PositiveCounter,
    },
    #[serde(rename = "knowledge.search")]
    KnowledgeSearch {
        scope: DataScope,
        text: Text,
        tags: Items<Label>,
        kinds: Items<KnowledgeKind>,
        page: PageRequest,
        entity_limit: Limit,
        claim_limit: Limit,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<KnowledgeLifecycle>", extend("x-since" = 2))]
        states: Option<Items<KnowledgeLifecycle>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<KnowledgeEntryKind>", extend("x-since" = 2))]
        entry_kinds: Option<Items<KnowledgeEntryKind>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        source: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 2))]
        stale: Option<bool>,
    },
    #[serde(rename = "knowledge.read")]
    KnowledgeRead {
        scope: DataScope,
        claim_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<Revision>,
    },
    #[serde(rename = "knowledge.diagnostics")]
    KnowledgeDiagnostics {
        scope: DataScope,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 2))]
        source_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ArtifactRef", extend("x-since" = 2))]
        import_source: Option<ArtifactRef>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "SchemaRef", extend("x-since" = 2))]
        import_schema: Option<SchemaRef>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 3))]
        source_label: Option<Label>,
    },
    #[serde(rename = "knowledge.write_status")]
    KnowledgeWriteStatus {
        write_id: Id,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Cursor", extend("x-since" = 2))]
        cursor: Option<Cursor>,
    },
    #[serde(rename = "knowledge.subgraph")]
    KnowledgeSubgraph {
        scope: DataScope,
        query: KnowledgeQuery,
    },
    #[serde(rename = "knowledge.entity_bundle")]
    KnowledgeEntityBundle {
        scope: DataScope,
        query: KnowledgeQuery,
    },
    #[serde(rename = "knowledge.entity_graph")]
    KnowledgeEntityGraph {
        scope: DataScope,
        query: KnowledgeQuery,
    },
    #[serde(rename = "knowledge.claim_bundle")]
    KnowledgeClaimBundle {
        scope: DataScope,
        query: KnowledgeQuery,
    },
    #[serde(rename = "knowledge.expand_frontier")]
    KnowledgeExpandFrontier {
        scope: DataScope,
        query: KnowledgeQuery,
    },
    #[serde(rename = "knowledge.query")]
    KnowledgeQuery {
        scope: DataScope,
        query: KnowledgeQuery,
    },
    #[serde(rename = "knowledge.render_summary")]
    KnowledgeRenderSummary {
        scope: DataScope,
        query: KnowledgeQuery,
    },
    #[serde(rename = "knowledge.tagged_lessons")]
    KnowledgeTaggedLessons {
        scope: DataScope,
        tags: NonEmpty<Label>,
        page: PageRequest,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<RuleState>", extend("x-since" = 2))]
        rule_states: Option<Items<RuleState>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        activity: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "LessonView", extend("x-since" = 2))]
        view: Option<LessonView>,
    },
    #[serde(rename = "dreaming.runs")]
    DreamingRuns { scope: DataScope, page: PageRequest },
    #[serde(rename = "dreaming.report")]
    DreamingReport { run_id: Id },
    #[serde(rename = "dreaming.settings")]
    DreamingSettings { scope: DataScope },
    #[serde(rename = "dreaming.instructions")]
    DreamingInstructions { scope: DataScope },
    #[serde(rename = "documents.list")]
    DocumentsList {
        scope: DataScope,
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retrieval_enabled: Option<bool>,
    },
    #[serde(rename = "conventions.list")]
    ConventionsList {
        scope: DataScope,
        page: PageRequest,
        applicability: Items<Label>,
    },
    #[serde(rename = "conventions.search")]
    ConventionsSearch {
        scope: DataScope,
        page: PageRequest,
        applicability: Items<Label>,
        text: Text,
    },
    #[serde(rename = "conventions.resources")]
    ConventionsResources {
        scope: DataScope,
        page: PageRequest,
        applicability: Items<Label>,
    },
    #[serde(rename = "conventions.read")]
    ConventionsRead {
        convention_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<Revision>,
    },
    #[serde(rename = "conventions.version")]
    ConventionsVersion {
        convention_id: Id,
        page: PageRequest,
    },
    #[serde(rename = "skills.read")]
    SkillsRead {
        instruction_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<Revision>,
    },
    #[serde(rename = "skills.history")]
    SkillsHistory {
        instruction_id: Id,
        page: PageRequest,
    },
    #[serde(rename = "roles.read")]
    RolesRead {
        instruction_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<Revision>,
    },
    #[serde(rename = "roles.history")]
    RolesHistory {
        instruction_id: Id,
        page: PageRequest,
    },
    #[serde(rename = "skills.discover")]
    SkillsDiscover {
        scope: DataScope,
        root_ids: Items<Id>,
        page: PageRequest,
    },
    #[serde(rename = "skills.resources")]
    SkillsResources {
        instruction_id: Id,
        revision: Revision,
        page: PageRequest,
    },
    #[serde(rename = "approvals.list")]
    ApprovalsList {
        scope: DataScope,
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ApprovalState>,
    },
    #[serde(rename = "approvals.inspect")]
    ApprovalsInspect { proposal_id: Id },
    #[serde(rename = "notifications.list")]
    NotificationsList {
        scope: DataScope,
        page: PageRequest,
        unacknowledged_only: bool,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<Label>", extend("x-since" = 3))]
        categories: Option<Items<Label>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3))]
        read: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3))]
        completion_only: Option<bool>,
    },
    #[serde(rename = "notifications.read")]
    NotificationsRead { notification_id: Id },
    #[serde(rename = "notifications.settings")]
    NotificationsSettings {},
    #[serde(rename = "git.inspect")]
    GitInspect {
        repository_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree_id: Option<Id>,
    },
    #[serde(rename = "git.diff")]
    GitDiff {
        repository_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree_id: Option<Id>,
        base: GitOid,
        head: GitOid,
        paths: Items<RelativePath>,
        page: PageRequest,
    },
    #[serde(rename = "reviews.list")]
    ReviewsList {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_id: Option<RootId>,
        page: PageRequest,
    },
    #[serde(rename = "reviews.inspect")]
    ReviewsInspect { review_id: Id },
    #[serde(rename = "reviews.findings")]
    ReviewsFindings { review_id: Id, page: PageRequest },
    #[serde(rename = "reviews.readiness")]
    ReviewsReadiness {
        root_id: RootId,
        barrier: DeliveryBarrier,
        policy_revision: Revision,
    },
    #[serde(rename = "releases.inspect")]
    ReleasesInspect { release_id: Id },
    #[serde(rename = "github.account")]
    GithubAccount {},
    #[serde(rename = "github.repositories")]
    GithubRepositories {
        account_id: Id,
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        organization: Option<Label>,
    },
    #[serde(rename = "github.poll_status")]
    GithubPollStatus { repository_id: Id },
    #[serde(rename = "github.checks")]
    GithubChecks {
        repository_id: Id,
        pull_request_number: PositiveCounter,
        head: GitOid,
        page: PageRequest,
    },
    #[serde(rename = "github.reviews")]
    GithubReviews {
        repository_id: Id,
        pull_request_number: PositiveCounter,
        head: GitOid,
        page: PageRequest,
    },
    #[serde(rename = "resources.list")]
    ResourcesList { scope: DataScope, page: PageRequest },
    #[serde(rename = "resources.holders")]
    ResourcesHolders {
        resource_id: ResourceId,
        page: PageRequest,
    },
    #[serde(rename = "resources.waiters")]
    ResourcesWaiters {
        resource_id: ResourceId,
        page: PageRequest,
    },
    #[serde(rename = "browser.sessions")]
    BrowserSessions { scope: DataScope, page: PageRequest },
    #[serde(rename = "browser.inspect")]
    BrowserInspect { target: BrowserTarget },
    #[serde(rename = "browser.screenshot")]
    BrowserScreenshot {
        target: BrowserTarget,
        full_page: bool,
    },
    #[serde(rename = "browser.console")]
    BrowserConsole {
        session_id: Id,
        after_sequence: Counter,
        page: PageRequest,
    },
    #[serde(rename = "browser.network")]
    BrowserNetwork {
        session_id: Id,
        after_sequence: Counter,
        page: PageRequest,
    },
    #[serde(rename = "desktop.inspect")]
    DesktopInspect {},
    #[serde(rename = "desktop.capture")]
    DesktopCapture {
        desktop_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "process.output")]
    ProcessOutput {
        process_id: Id,
        after_bytes: Counter,
        max_bytes: ByteLimit,
    },
    #[serde(rename = "web.search")]
    WebSearch {
        scope: DataScope,
        query: Text,
        provider_id: Id,
        page: PageRequest,
    },
    #[serde(rename = "web.fetch")]
    WebFetch {
        scope: DataScope,
        url: Url,
        max_bytes: ByteLimit,
        timeout_ms: PositiveCounter,
    },
    #[serde(rename = "containers.capabilities")]
    ContainersCapabilities {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        engine_id: Option<Id>,
    },
    #[serde(rename = "containers.inspect")]
    ContainersInspect { container_id: Id },
    #[serde(rename = "containers.logs")]
    ContainersLogs {
        container_id: Id,
        after_bytes: Counter,
        max_bytes: ByteLimit,
    },
    #[serde(rename = "jobs.list")]
    JobsList {
        scope: DataScope,
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
    },
    #[serde(rename = "jobs.inspect")]
    JobsInspect {
        job_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "PageRequest", extend("x-since" = 2))]
        history: Option<PageRequest>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "PageRequest", extend("x-since" = 2))]
        usage: Option<PageRequest>,
    },
    #[serde(rename = "settings.read_effective")]
    SettingsReadEffective {
        scope: DataScope,
        keys: Items<Label>,
        page: PageRequest,
    },
    #[serde(rename = "settings.validate")]
    SettingsValidate {
        scope: DataScope,
        expected_revision: Revision,
        entries: NonEmpty<SettingAssignment>,
    },
    #[serde(rename = "settings.explain_source")]
    SettingsExplainSource { scope: DataScope, key: Label },
    #[serde(rename = "router.catalog")]
    RouterCatalog {
        page: PageRequest,
        include_disabled: bool,
    },
    #[serde(rename = "router.connections")]
    RouterConnections { page: PageRequest },
    #[serde(rename = "router.availability")]
    RouterAvailability {
        intent: RouteIntent,
        effort: EffortName,
        required_capabilities: Items<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 2))]
        session_id: Option<Id>,
    },
    #[serde(rename = "router.usage")]
    RouterUsage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_id: Option<RootId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_run_id: Option<Id>,
        page: PageRequest,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "UsageReportKind", extend("x-since" = 3))]
        report_kind: Option<UsageReportKind>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "DataScope", extend("x-since" = 3))]
        scope: Option<DataScope>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ProjectId", extend("x-since" = 3))]
        project_id: Option<ProjectId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "SignedCounter", extend("x-since" = 3))]
        start_ms: Option<SignedCounter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "SignedCounter", extend("x-since" = 3))]
        end_ms: Option<SignedCounter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Content", extend("x-since" = 3))]
        interval: Option<Content>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Content", extend("x-since" = 3))]
        user: Option<Content>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Content", extend("x-since" = 3))]
        provider: Option<Content>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Content", extend("x-since" = 3))]
        model: Option<Content>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Content", extend("x-since" = 3))]
        tool: Option<Content>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 3))]
        purpose: Option<Label>,
    },
    #[serde(rename = "router.request_status")]
    RouterRequestStatus { request_id: Id },
    #[serde(rename = "router.price_status")]
    RouterPriceStatus {
        model_ids: Items<Id>,
        page: PageRequest,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<Id>", extend("x-since" = 3))]
        provider_ids: Option<Items<Id>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3))]
        include_history: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<Id>", extend("x-since" = 3))]
        snapshot_ids: Option<Items<Id>>,
    },
    #[serde(rename = "router.decision_models")]
    RouterDecisionModels {
        page: PageRequest,
        include_disabled: bool,
    },
    #[serde(rename = "mcp.servers")]
    McpServers { scope: DataScope, page: PageRequest },
    #[serde(rename = "mcp.resources")]
    McpResources {
        server_id: Id,
        catalog_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<Text>,
        page: PageRequest,
    },
    #[serde(rename = "mcp.prompts")]
    McpPrompts {
        server_id: Id,
        catalog_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<Label>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<SchemaValue>,
        page: PageRequest,
    },
    #[serde(rename = "mcp.capabilities")]
    McpCapabilities {
        server_id: Id,
        catalog_revision: Revision,
        page: PageRequest,
    },
    #[serde(rename = "addons.catalog")]
    AddonsCatalog { page: PageRequest },
    #[serde(rename = "addons.logs")]
    AddonsLogs {
        package_id: Id,
        after_sequence: Counter,
        page: PageRequest,
    },
    #[serde(rename = "help.search")]
    HelpSearch { query: Text, page: PageRequest },
    #[serde(rename = "help.read")]
    HelpRead { topic_id: Id, version: Label },
    #[serde(rename = "help.topic_for_screen")]
    HelpTopicForScreen {
        screen_id: Id,
        capability_revision: Revision,
    },
    #[serde(rename = "help.package_guides")]
    HelpPackageGuides {
        package_id: Id,
        version: Label,
        page: PageRequest,
    },
    #[serde(rename = "workspace.diagnostics")]
    WorkspaceDiagnostics {
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        component: Option<Label>,
    },
    #[serde(rename = "workspace.logs")]
    WorkspaceLogs {
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        component: Option<Label>,
        after_sequence: Counter,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Severity", extend("x-since" = 2))]
        severity: Option<Severity>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "DiagnosticOutcome", extend("x-since" = 2))]
        outcome: Option<DiagnosticOutcome>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        operation: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "OperationId", extend("x-since" = 2))]
        operation_id: Option<OperationId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "CorrelationId", extend("x-since" = 2))]
        correlation_id: Option<CorrelationId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 2))]
        target_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        stage: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ProjectId", extend("x-since" = 2))]
        project_id: Option<ProjectId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        search: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 2))]
        since_ms: Option<Counter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 2))]
        until_ms: Option<Counter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "SortDirection", extend("x-since" = 2))]
        order: Option<SortDirection>,
    },
    #[serde(rename = "workspace.update_status")]
    WorkspaceUpdateStatus {},
    #[serde(rename = "repositories.list")]
    RepositoriesList {
        scope: DataScope,
        page: PageRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<Text>,
    },
    #[serde(rename = "repositories.get")]
    RepositoriesGet { repository_id: Id },
    #[serde(rename = "repositories.nodes")]
    RepositoriesNodes {
        repository_id: Id,
        views: ViewSelection,
        page: PageRequest,
    },
    #[serde(rename = "repositories.readme")]
    RepositoriesReadme {
        repository_id: Id,
        views: ViewSelection,
    },
    #[serde(rename = "repositories.summary")]
    RepositoriesSummary {
        repository_id: Id,
        views: ViewSelection,
    },
    #[serde(rename = "repositories.search")]
    RepositoriesSearch {
        scope: DataScope,
        query: Text,
        language_ids: Items<Label>,
        page: PageRequest,
    },
    #[serde(rename = "queue.inspect")]
    QueueInspect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumer_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<OperationId>,
        page: PageRequest,
    },
    #[serde(rename = "queue.consumer_status")]
    QueueConsumerStatus { consumer_id: Id },
    #[serde(rename = "imports.preview")]
    ImportsPreview { config: ImportConfig },
    #[serde(rename = "imports.runs")]
    ImportsRuns { page: PageRequest },
    #[serde(rename = "imports.inspect")]
    ImportsInspect { run_id: Id },
    #[serde(rename = "mcp_server.status")]
    McpServerStatus {},
    #[serde(rename = "mcp_server.clients")]
    McpServerClients { page: PageRequest },
    #[serde(rename = "evals.results")]
    EvalsResults { session_id: Id, page: PageRequest },
    #[schemars(extend("x-since" = 3))]
    #[serde(rename = "next_work.inspect_queue")]
    NextWorkInspectQueue {
        project_id: ProjectId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feature_id: Option<Id>,
    },
    #[schemars(extend("x-since" = 3))]
    #[serde(rename = "approvals.review_queue")]
    ApprovalsReviewQueue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
        page: PageRequest,
    },
}
impl DomainQuery {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::ProjectsList { .. } => "projects.list",
            Self::ProjectsGet { .. } => "projects.get",
            Self::FeaturesList { .. } => "features.list",
            Self::FeaturesGet { .. } => "features.get",
            Self::IssuesList { .. } => "issues.list",
            Self::IssuesGet { .. } => "issues.get",
            Self::TasksList { .. } => "tasks.list",
            Self::TasksGet { .. } => "tasks.get",
            Self::WorkflowsList { .. } => "workflows.list",
            Self::NextWorkInspectClaim { .. } => "next_work.inspect_claim",
            Self::AgentsInspect { .. } => "agents.inspect",
            Self::AgentsInbox { .. } => "agents.inbox",
            Self::SessionsList { .. } => "sessions.list",
            Self::SessionsGet { .. } => "sessions.get",
            Self::SessionsBriefing { .. } => "sessions.briefing",
            Self::SessionsContextSources { .. } => "sessions.context_sources",
            Self::UiCurrentScreen { .. } => "ui.current_screen",
            Self::HistoryWorkLineage { .. } => "history.work_lineage",
            Self::HistoryGitLinks { .. } => "history.git_links",
            Self::HistoryEvidenceLinks { .. } => "history.evidence_links",
            Self::HistoryToolUsage { .. } => "history.tool_usage",
            Self::IndexingDiscover { .. } => "indexing.discover",
            Self::IndexingStatus { .. } => "indexing.status",
            Self::IndexingRuns { .. } => "indexing.runs",
            Self::AnalysisHealth { .. } => "analysis.health",
            Self::AnalysisCrossRepoLinks { .. } => "analysis.cross_repo_links",
            Self::AnalysisStatus { .. } => "analysis.status",
            Self::AnalysisClusters { .. } => "analysis.clusters",
            Self::AnalysisClusterDetail { .. } => "analysis.cluster_detail",
            Self::AnalysisSecurity { .. } => "analysis.security",
            Self::AnalysisMetrics { .. } => "analysis.metrics",
            Self::AnalysisHotspots { .. } => "analysis.hotspots",
            Self::AnalysisFleetHealth { .. } => "analysis.fleet_health",
            Self::AnalysisArchivalCandidates { .. } => "analysis.archival_candidates",
            Self::AnalysisFileHotspot { .. } => "analysis.file_hotspot",
            Self::SchemasConnections { .. } => "schemas.connections",
            Self::SchemasObjects { .. } => "schemas.objects",
            Self::SchemasRelationships { .. } => "schemas.relationships",
            Self::DatabaseRead { .. } => "database.read",
            Self::KnowledgeSearch { .. } => "knowledge.search",
            Self::KnowledgeRead { .. } => "knowledge.read",
            Self::KnowledgeDiagnostics { .. } => "knowledge.diagnostics",
            Self::KnowledgeWriteStatus { .. } => "knowledge.write_status",
            Self::KnowledgeSubgraph { .. } => "knowledge.subgraph",
            Self::KnowledgeEntityBundle { .. } => "knowledge.entity_bundle",
            Self::KnowledgeEntityGraph { .. } => "knowledge.entity_graph",
            Self::KnowledgeClaimBundle { .. } => "knowledge.claim_bundle",
            Self::KnowledgeExpandFrontier { .. } => "knowledge.expand_frontier",
            Self::KnowledgeQuery { .. } => "knowledge.query",
            Self::KnowledgeRenderSummary { .. } => "knowledge.render_summary",
            Self::KnowledgeTaggedLessons { .. } => "knowledge.tagged_lessons",
            Self::DreamingRuns { .. } => "dreaming.runs",
            Self::DreamingReport { .. } => "dreaming.report",
            Self::DreamingSettings { .. } => "dreaming.settings",
            Self::DreamingInstructions { .. } => "dreaming.instructions",
            Self::DocumentsList { .. } => "documents.list",
            Self::ConventionsList { .. } => "conventions.list",
            Self::ConventionsSearch { .. } => "conventions.search",
            Self::ConventionsResources { .. } => "conventions.resources",
            Self::ConventionsRead { .. } => "conventions.read",
            Self::ConventionsVersion { .. } => "conventions.version",
            Self::SkillsRead { .. } => "skills.read",
            Self::SkillsHistory { .. } => "skills.history",
            Self::RolesRead { .. } => "roles.read",
            Self::RolesHistory { .. } => "roles.history",
            Self::SkillsDiscover { .. } => "skills.discover",
            Self::SkillsResources { .. } => "skills.resources",
            Self::ApprovalsList { .. } => "approvals.list",
            Self::ApprovalsInspect { .. } => "approvals.inspect",
            Self::NotificationsList { .. } => "notifications.list",
            Self::NotificationsRead { .. } => "notifications.read",
            Self::NotificationsSettings { .. } => "notifications.settings",
            Self::GitInspect { .. } => "git.inspect",
            Self::GitDiff { .. } => "git.diff",
            Self::ReviewsList { .. } => "reviews.list",
            Self::ReviewsInspect { .. } => "reviews.inspect",
            Self::ReviewsFindings { .. } => "reviews.findings",
            Self::ReviewsReadiness { .. } => "reviews.readiness",
            Self::ReleasesInspect { .. } => "releases.inspect",
            Self::GithubAccount { .. } => "github.account",
            Self::GithubRepositories { .. } => "github.repositories",
            Self::GithubPollStatus { .. } => "github.poll_status",
            Self::GithubChecks { .. } => "github.checks",
            Self::GithubReviews { .. } => "github.reviews",
            Self::ResourcesList { .. } => "resources.list",
            Self::ResourcesHolders { .. } => "resources.holders",
            Self::ResourcesWaiters { .. } => "resources.waiters",
            Self::BrowserSessions { .. } => "browser.sessions",
            Self::BrowserInspect { .. } => "browser.inspect",
            Self::BrowserScreenshot { .. } => "browser.screenshot",
            Self::BrowserConsole { .. } => "browser.console",
            Self::BrowserNetwork { .. } => "browser.network",
            Self::DesktopInspect { .. } => "desktop.inspect",
            Self::DesktopCapture { .. } => "desktop.capture",
            Self::ProcessOutput { .. } => "process.output",
            Self::WebSearch { .. } => "web.search",
            Self::WebFetch { .. } => "web.fetch",
            Self::ContainersCapabilities { .. } => "containers.capabilities",
            Self::ContainersInspect { .. } => "containers.inspect",
            Self::ContainersLogs { .. } => "containers.logs",
            Self::JobsList { .. } => "jobs.list",
            Self::JobsInspect { .. } => "jobs.inspect",
            Self::SettingsReadEffective { .. } => "settings.read_effective",
            Self::SettingsValidate { .. } => "settings.validate",
            Self::SettingsExplainSource { .. } => "settings.explain_source",
            Self::RouterCatalog { .. } => "router.catalog",
            Self::RouterConnections { .. } => "router.connections",
            Self::RouterAvailability { .. } => "router.availability",
            Self::RouterUsage { .. } => "router.usage",
            Self::RouterRequestStatus { .. } => "router.request_status",
            Self::RouterPriceStatus { .. } => "router.price_status",
            Self::RouterDecisionModels { .. } => "router.decision_models",
            Self::McpServers { .. } => "mcp.servers",
            Self::McpResources { .. } => "mcp.resources",
            Self::McpPrompts { .. } => "mcp.prompts",
            Self::McpCapabilities { .. } => "mcp.capabilities",
            Self::AddonsCatalog { .. } => "addons.catalog",
            Self::AddonsLogs { .. } => "addons.logs",
            Self::HelpSearch { .. } => "help.search",
            Self::HelpRead { .. } => "help.read",
            Self::HelpTopicForScreen { .. } => "help.topic_for_screen",
            Self::HelpPackageGuides { .. } => "help.package_guides",
            Self::WorkspaceDiagnostics { .. } => "workspace.diagnostics",
            Self::WorkspaceLogs { .. } => "workspace.logs",
            Self::WorkspaceUpdateStatus { .. } => "workspace.update_status",
            Self::RepositoriesList { .. } => "repositories.list",
            Self::RepositoriesGet { .. } => "repositories.get",
            Self::RepositoriesNodes { .. } => "repositories.nodes",
            Self::RepositoriesReadme { .. } => "repositories.readme",
            Self::RepositoriesSummary { .. } => "repositories.summary",
            Self::RepositoriesSearch { .. } => "repositories.search",
            Self::QueueInspect { .. } => "queue.inspect",
            Self::QueueConsumerStatus { .. } => "queue.consumer_status",
            Self::ImportsPreview { .. } => "imports.preview",
            Self::ImportsRuns { .. } => "imports.runs",
            Self::ImportsInspect { .. } => "imports.inspect",
            Self::McpServerStatus { .. } => "mcp_server.status",
            Self::McpServerClients { .. } => "mcp_server.clients",
            Self::EvalsResults { .. } => "evals.results",
            Self::NextWorkInspectQueue { .. } => "next_work.inspect_queue",
            Self::ApprovalsReviewQueue { .. } => "approvals.review_queue",
        }
    }
}
impl Check for DomainQuery {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::ProjectsList {
                scope,
                page,
                lifecycle,
            } => {
                scope.check()?;
                page.check()?;
                lifecycle.check()?;
            }
            Self::ProjectsGet { project_id } => {
                project_id.check()?;
            }
            Self::FeaturesList {
                project_id,
                page,
                status_ids,
                repository_ids,
                text,
                detail_level,
            } => {
                project_id.check()?;
                page.check()?;
                status_ids.check()?;
                repository_ids.check()?;
                text.check()?;
                detail_level.check()?;
            }
            Self::FeaturesGet {
                work_id,
                detail_level,
                detail_page,
            } => {
                work_id.check()?;
                detail_level.check()?;
                detail_page.check()?;
            }
            Self::IssuesList {
                project_id,
                page,
                status_ids,
                repository_ids,
                text,
                detail_level,
            } => {
                project_id.check()?;
                page.check()?;
                status_ids.check()?;
                repository_ids.check()?;
                text.check()?;
                detail_level.check()?;
            }
            Self::IssuesGet {
                work_id,
                detail_level,
                detail_page,
            } => {
                work_id.check()?;
                detail_level.check()?;
                detail_page.check()?;
            }
            Self::TasksList {
                project_id,
                page,
                status_ids,
                repository_ids,
                text,
                detail_level,
            } => {
                project_id.check()?;
                page.check()?;
                status_ids.check()?;
                repository_ids.check()?;
                text.check()?;
                detail_level.check()?;
            }
            Self::TasksGet {
                work_id,
                detail_level,
                detail_page,
            } => {
                work_id.check()?;
                detail_level.check()?;
                detail_page.check()?;
            }
            Self::WorkflowsList { project_id, page } => {
                project_id.check()?;
                page.check()?;
            }
            Self::NextWorkInspectClaim { claim_id } => {
                claim_id.check()?;
            }
            Self::AgentsInspect { assignment_id } => {
                assignment_id.check()?;
            }
            Self::AgentsInbox {
                assignment_id,
                page,
                unacknowledged_only,
                direction,
            } => {
                assignment_id.check()?;
                page.check()?;
                unacknowledged_only.check()?;
                direction.check()?;
            }
            Self::SessionsList {
                scope,
                page,
                closed,
            } => {
                scope.check()?;
                page.check()?;
                closed.check()?;
            }
            Self::SessionsGet { session_id } => {
                session_id.check()?;
            }
            Self::SessionsBriefing {
                session_id,
                expected_context_epoch,
                item,
                paths,
                budget_tokens,
            } => {
                session_id.check()?;
                expected_context_epoch.check()?;
                item.check()?;
                paths.check()?;
                budget_tokens.check()?;
            }
            Self::SessionsContextSources {
                session_id,
                expected_context_epoch,
                page,
            } => {
                session_id.check()?;
                expected_context_epoch.check()?;
                page.check()?;
            }
            Self::UiCurrentScreen {} => {}
            Self::HistoryWorkLineage { root_id, page } => {
                root_id.check()?;
                page.check()?;
            }
            Self::HistoryGitLinks { root_id, page } => {
                root_id.check()?;
                page.check()?;
            }
            Self::HistoryEvidenceLinks { root_id, page } => {
                root_id.check()?;
                page.check()?;
            }
            Self::HistoryToolUsage {
                root_id,
                project_id,
                workspace,
                page,
            } => {
                root_id.check()?;
                project_id.check()?;
                workspace.check()?;
                page.check()?;
            }
            Self::IndexingDiscover { scope, roots, page } => {
                scope.check()?;
                roots.check()?;
                page.check()?;
            }
            Self::IndexingStatus {
                repository_id,
                overlay_id,
            } => {
                repository_id.check()?;
                overlay_id.check()?;
            }
            Self::IndexingRuns {
                scope,
                repository_id,
                states,
                kinds,
                page,
            } => {
                scope.check()?;
                repository_id.check()?;
                states.check()?;
                kinds.check()?;
                page.check()?;
            }
            Self::AnalysisHealth {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisCrossRepoLinks {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisStatus {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisClusters {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisClusterDetail {
                scope,
                views,
                page,
                cluster_id,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                cluster_id.check()?;
                filters.check()?;
            }
            Self::AnalysisSecurity {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisMetrics {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisHotspots {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisFleetHealth {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisArchivalCandidates {
                scope,
                views,
                page,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                filters.check()?;
            }
            Self::AnalysisFileHotspot {
                scope,
                views,
                page,
                path,
                filters,
            } => {
                scope.check()?;
                views.check()?;
                page.check()?;
                path.check()?;
                filters.check()?;
            }
            Self::SchemasConnections { scope, page } => {
                scope.check()?;
                page.check()?;
            }
            Self::SchemasObjects {
                scope,
                views,
                connection_id,
                kinds,
                parent_id,
                text,
                page,
            } => {
                scope.check()?;
                views.check()?;
                connection_id.check()?;
                kinds.check()?;
                parent_id.check()?;
                text.check()?;
                page.check()?;
            }
            Self::SchemasRelationships {
                scope,
                views,
                connection_id,
                object_ids,
                page,
            } => {
                scope.check()?;
                views.check()?;
                connection_id.check()?;
                object_ids.check()?;
                page.check()?;
            }
            Self::DatabaseRead {
                connection_id,
                expected_connection_revision,
                statement,
                parameters,
                row_limit,
                timeout_ms,
            } => {
                connection_id.check()?;
                expected_connection_revision.check()?;
                statement.check()?;
                parameters.check()?;
                row_limit.check()?;
                timeout_ms.check()?;
            }
            Self::KnowledgeSearch {
                scope,
                text,
                tags,
                kinds,
                page,
                entity_limit,
                claim_limit,
                states,
                entry_kinds,
                source,
                stale,
            } => {
                scope.check()?;
                text.check()?;
                tags.check()?;
                kinds.check()?;
                page.check()?;
                entity_limit.check()?;
                claim_limit.check()?;
                states.check()?;
                entry_kinds.check()?;
                source.check()?;
                stale.check()?;
            }
            Self::KnowledgeRead {
                scope,
                claim_id,
                revision,
            } => {
                scope.check()?;
                claim_id.check()?;
                revision.check()?;
            }
            Self::KnowledgeDiagnostics {
                scope,
                source_id,
                import_source,
                import_schema,
                source_label,
            } => {
                scope.check()?;
                source_id.check()?;
                import_source.check()?;
                import_schema.check()?;
                source_label.check()?;
            }
            Self::KnowledgeWriteStatus { write_id, cursor } => {
                write_id.check()?;
                cursor.check()?;
            }
            Self::KnowledgeSubgraph { scope, query } => {
                scope.check()?;
                query.check()?;
            }
            Self::KnowledgeEntityBundle { scope, query } => {
                scope.check()?;
                query.check()?;
            }
            Self::KnowledgeEntityGraph { scope, query } => {
                scope.check()?;
                query.check()?;
            }
            Self::KnowledgeClaimBundle { scope, query } => {
                scope.check()?;
                query.check()?;
            }
            Self::KnowledgeExpandFrontier { scope, query } => {
                scope.check()?;
                query.check()?;
            }
            Self::KnowledgeQuery { scope, query } => {
                scope.check()?;
                query.check()?;
            }
            Self::KnowledgeRenderSummary { scope, query } => {
                scope.check()?;
                query.check()?;
            }
            Self::KnowledgeTaggedLessons {
                scope,
                tags,
                page,
                rule_states,
                activity,
                view,
            } => {
                scope.check()?;
                tags.check()?;
                page.check()?;
                rule_states.check()?;
                activity.check()?;
                view.check()?;
            }
            Self::DreamingRuns { scope, page } => {
                scope.check()?;
                page.check()?;
            }
            Self::DreamingReport { run_id } => {
                run_id.check()?;
            }
            Self::DreamingSettings { scope } => {
                scope.check()?;
            }
            Self::DreamingInstructions { scope } => {
                scope.check()?;
            }
            Self::DocumentsList {
                scope,
                page,
                retrieval_enabled,
            } => {
                scope.check()?;
                page.check()?;
                retrieval_enabled.check()?;
            }
            Self::ConventionsList {
                scope,
                page,
                applicability,
            } => {
                scope.check()?;
                page.check()?;
                applicability.check()?;
            }
            Self::ConventionsSearch {
                scope,
                page,
                applicability,
                text,
            } => {
                scope.check()?;
                page.check()?;
                applicability.check()?;
                text.check()?;
            }
            Self::ConventionsResources {
                scope,
                page,
                applicability,
            } => {
                scope.check()?;
                page.check()?;
                applicability.check()?;
            }
            Self::ConventionsRead {
                convention_id,
                revision,
            } => {
                convention_id.check()?;
                revision.check()?;
            }
            Self::ConventionsVersion {
                convention_id,
                page,
            } => {
                convention_id.check()?;
                page.check()?;
            }
            Self::SkillsRead {
                instruction_id,
                revision,
            } => {
                instruction_id.check()?;
                revision.check()?;
            }
            Self::SkillsHistory {
                instruction_id,
                page,
            } => {
                instruction_id.check()?;
                page.check()?;
            }
            Self::RolesRead {
                instruction_id,
                revision,
            } => {
                instruction_id.check()?;
                revision.check()?;
            }
            Self::RolesHistory {
                instruction_id,
                page,
            } => {
                instruction_id.check()?;
                page.check()?;
            }
            Self::SkillsDiscover {
                scope,
                root_ids,
                page,
            } => {
                scope.check()?;
                root_ids.check()?;
                page.check()?;
            }
            Self::SkillsResources {
                instruction_id,
                revision,
                page,
            } => {
                instruction_id.check()?;
                revision.check()?;
                page.check()?;
            }
            Self::ApprovalsList { scope, page, state } => {
                scope.check()?;
                page.check()?;
                state.check()?;
            }
            Self::ApprovalsInspect { proposal_id } => {
                proposal_id.check()?;
            }
            Self::NotificationsList {
                scope,
                page,
                unacknowledged_only,
                categories,
                read,
                completion_only,
            } => {
                scope.check()?;
                page.check()?;
                unacknowledged_only.check()?;
                categories.check()?;
                read.check()?;
                completion_only.check()?;
            }
            Self::NotificationsRead { notification_id } => {
                notification_id.check()?;
            }
            Self::NotificationsSettings {} => {}
            Self::GitInspect {
                repository_id,
                worktree_id,
            } => {
                repository_id.check()?;
                worktree_id.check()?;
            }
            Self::GitDiff {
                repository_id,
                worktree_id,
                base,
                head,
                paths,
                page,
            } => {
                repository_id.check()?;
                worktree_id.check()?;
                base.check()?;
                head.check()?;
                paths.check()?;
                page.check()?;
            }
            Self::ReviewsList {
                scope,
                root_id,
                page,
            } => {
                scope.check()?;
                root_id.check()?;
                page.check()?;
            }
            Self::ReviewsInspect { review_id } => {
                review_id.check()?;
            }
            Self::ReviewsFindings { review_id, page } => {
                review_id.check()?;
                page.check()?;
            }
            Self::ReviewsReadiness {
                root_id,
                barrier,
                policy_revision,
            } => {
                root_id.check()?;
                barrier.check()?;
                policy_revision.check()?;
            }
            Self::ReleasesInspect { release_id } => {
                release_id.check()?;
            }
            Self::GithubAccount {} => {}
            Self::GithubRepositories {
                account_id,
                page,
                organization,
            } => {
                account_id.check()?;
                page.check()?;
                organization.check()?;
            }
            Self::GithubPollStatus { repository_id } => {
                repository_id.check()?;
            }
            Self::GithubChecks {
                repository_id,
                pull_request_number,
                head,
                page,
            } => {
                repository_id.check()?;
                pull_request_number.check()?;
                head.check()?;
                page.check()?;
            }
            Self::GithubReviews {
                repository_id,
                pull_request_number,
                head,
                page,
            } => {
                repository_id.check()?;
                pull_request_number.check()?;
                head.check()?;
                page.check()?;
            }
            Self::ResourcesList { scope, page } => {
                scope.check()?;
                page.check()?;
            }
            Self::ResourcesHolders { resource_id, page } => {
                resource_id.check()?;
                page.check()?;
            }
            Self::ResourcesWaiters { resource_id, page } => {
                resource_id.check()?;
                page.check()?;
            }
            Self::BrowserSessions { scope, page } => {
                scope.check()?;
                page.check()?;
            }
            Self::BrowserInspect { target } => {
                target.check()?;
            }
            Self::BrowserScreenshot { target, full_page } => {
                target.check()?;
                full_page.check()?;
            }
            Self::BrowserConsole {
                session_id,
                after_sequence,
                page,
            } => {
                session_id.check()?;
                after_sequence.check()?;
                page.check()?;
            }
            Self::BrowserNetwork {
                session_id,
                after_sequence,
                page,
            } => {
                session_id.check()?;
                after_sequence.check()?;
                page.check()?;
            }
            Self::DesktopInspect {} => {}
            Self::DesktopCapture {
                desktop_id,
                expected_revision,
            } => {
                desktop_id.check()?;
                expected_revision.check()?;
            }
            Self::ProcessOutput {
                process_id,
                after_bytes,
                max_bytes,
            } => {
                process_id.check()?;
                after_bytes.check()?;
                max_bytes.check()?;
            }
            Self::WebSearch {
                scope,
                query,
                provider_id,
                page,
            } => {
                scope.check()?;
                query.check()?;
                provider_id.check()?;
                page.check()?;
            }
            Self::WebFetch {
                scope,
                url,
                max_bytes,
                timeout_ms,
            } => {
                scope.check()?;
                url.check()?;
                max_bytes.check()?;
                timeout_ms.check()?;
            }
            Self::ContainersCapabilities { engine_id } => {
                engine_id.check()?;
            }
            Self::ContainersInspect { container_id } => {
                container_id.check()?;
            }
            Self::ContainersLogs {
                container_id,
                after_bytes,
                max_bytes,
            } => {
                container_id.check()?;
                after_bytes.check()?;
                max_bytes.check()?;
            }
            Self::JobsList {
                scope,
                page,
                enabled,
            } => {
                scope.check()?;
                page.check()?;
                enabled.check()?;
            }
            Self::JobsInspect {
                job_id,
                run_id,
                history,
                usage,
            } => {
                job_id.check()?;
                run_id.check()?;
                history.check()?;
                usage.check()?;
            }
            Self::SettingsReadEffective { scope, keys, page } => {
                scope.check()?;
                keys.check()?;
                page.check()?;
            }
            Self::SettingsValidate {
                scope,
                expected_revision,
                entries,
            } => {
                scope.check()?;
                expected_revision.check()?;
                entries.check()?;
            }
            Self::SettingsExplainSource { scope, key } => {
                scope.check()?;
                key.check()?;
            }
            Self::RouterCatalog {
                page,
                include_disabled,
            } => {
                page.check()?;
                include_disabled.check()?;
            }
            Self::RouterConnections { page } => {
                page.check()?;
            }
            Self::RouterAvailability {
                intent,
                effort,
                required_capabilities,
                session_id,
            } => {
                intent.check()?;
                effort.check()?;
                required_capabilities.check()?;
                session_id.check()?;
            }
            Self::RouterUsage {
                root_id,
                session_id,
                job_run_id,
                page,
                report_kind,
                scope,
                project_id,
                start_ms,
                end_ms,
                interval,
                user,
                provider,
                model,
                tool,
                purpose,
            } => {
                root_id.check()?;
                session_id.check()?;
                job_run_id.check()?;
                page.check()?;
                report_kind.check()?;
                scope.check()?;
                project_id.check()?;
                start_ms.check()?;
                end_ms.check()?;
                interval.check()?;
                user.check()?;
                provider.check()?;
                model.check()?;
                tool.check()?;
                purpose.check()?;
            }
            Self::RouterRequestStatus { request_id } => {
                request_id.check()?;
            }
            Self::RouterPriceStatus {
                model_ids,
                page,
                provider_ids,
                include_history,
                snapshot_ids,
            } => {
                model_ids.check()?;
                page.check()?;
                provider_ids.check()?;
                include_history.check()?;
                snapshot_ids.check()?;
            }
            Self::RouterDecisionModels {
                page,
                include_disabled,
            } => {
                page.check()?;
                include_disabled.check()?;
            }
            Self::McpServers { scope, page } => {
                scope.check()?;
                page.check()?;
            }
            Self::McpResources {
                server_id,
                catalog_revision,
                uri,
                page,
            } => {
                server_id.check()?;
                catalog_revision.check()?;
                uri.check()?;
                page.check()?;
            }
            Self::McpPrompts {
                server_id,
                catalog_revision,
                name,
                arguments,
                page,
            } => {
                server_id.check()?;
                catalog_revision.check()?;
                name.check()?;
                arguments.check()?;
                page.check()?;
            }
            Self::McpCapabilities {
                server_id,
                catalog_revision,
                page,
            } => {
                server_id.check()?;
                catalog_revision.check()?;
                page.check()?;
            }
            Self::AddonsCatalog { page } => {
                page.check()?;
            }
            Self::AddonsLogs {
                package_id,
                after_sequence,
                page,
            } => {
                package_id.check()?;
                after_sequence.check()?;
                page.check()?;
            }
            Self::HelpSearch { query, page } => {
                query.check()?;
                page.check()?;
            }
            Self::HelpRead { topic_id, version } => {
                topic_id.check()?;
                version.check()?;
            }
            Self::HelpTopicForScreen {
                screen_id,
                capability_revision,
            } => {
                screen_id.check()?;
                capability_revision.check()?;
            }
            Self::HelpPackageGuides {
                package_id,
                version,
                page,
            } => {
                package_id.check()?;
                version.check()?;
                page.check()?;
            }
            Self::WorkspaceDiagnostics { page, component } => {
                page.check()?;
                component.check()?;
            }
            Self::WorkspaceLogs {
                page,
                component,
                after_sequence,
                severity,
                outcome,
                operation,
                operation_id,
                correlation_id,
                target_id,
                stage,
                project_id,
                search,
                since_ms,
                until_ms,
                order,
            } => {
                page.check()?;
                component.check()?;
                after_sequence.check()?;
                severity.check()?;
                outcome.check()?;
                operation.check()?;
                operation_id.check()?;
                correlation_id.check()?;
                target_id.check()?;
                stage.check()?;
                project_id.check()?;
                search.check()?;
                since_ms.check()?;
                until_ms.check()?;
                order.check()?;
            }
            Self::WorkspaceUpdateStatus {} => {}
            Self::RepositoriesList { scope, page, text } => {
                scope.check()?;
                page.check()?;
                text.check()?;
            }
            Self::RepositoriesGet { repository_id } => {
                repository_id.check()?;
            }
            Self::RepositoriesNodes {
                repository_id,
                views,
                page,
            } => {
                repository_id.check()?;
                views.check()?;
                page.check()?;
            }
            Self::RepositoriesReadme {
                repository_id,
                views,
            } => {
                repository_id.check()?;
                views.check()?;
            }
            Self::RepositoriesSummary {
                repository_id,
                views,
            } => {
                repository_id.check()?;
                views.check()?;
            }
            Self::RepositoriesSearch {
                scope,
                query,
                language_ids,
                page,
            } => {
                scope.check()?;
                query.check()?;
                language_ids.check()?;
                page.check()?;
            }
            Self::QueueInspect {
                consumer_id,
                operation_id,
                page,
            } => {
                consumer_id.check()?;
                operation_id.check()?;
                page.check()?;
            }
            Self::QueueConsumerStatus { consumer_id } => {
                consumer_id.check()?;
            }
            Self::ImportsPreview { config } => {
                config.check()?;
            }
            Self::ImportsRuns { page } => {
                page.check()?;
            }
            Self::ImportsInspect { run_id } => {
                run_id.check()?;
            }
            Self::McpServerStatus {} => {}
            Self::McpServerClients { page } => {
                page.check()?;
            }
            Self::EvalsResults { session_id, page } => {
                session_id.check()?;
                page.check()?;
            }
            Self::NextWorkInspectQueue {
                project_id,
                feature_id,
            } => {
                project_id.check()?;
                feature_id.check()?;
            }
            Self::ApprovalsReviewQueue { project_id, page } => {
                project_id.check()?;
                page.check()?;
            }
        }
        super::domain_rules::input(
            self.operation(),
            "Query",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )
    }
}

impl Revisioned for DomainQuery {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::SessionsBriefing {
                item,
                paths,
                budget_tokens,
                ..
            } => {
                if revision < 2 {
                    *item = None;
                }
                if revision < 2 {
                    *paths = None;
                }
                if revision < 2 {
                    *budget_tokens = None;
                }
            }
            Self::HistoryToolUsage {
                project_id,
                workspace,
                ..
            } => {
                if revision < 2 {
                    *project_id = None;
                }
                if revision < 2 {
                    *workspace = None;
                }
            }
            Self::IndexingStatus { overlay_id, .. } => {
                if revision < 2 {
                    *overlay_id = None;
                }
            }
            Self::IndexingRuns { states, kinds, .. } => {
                if revision < 2 {
                    *states = None;
                }
                if revision < 2 {
                    *kinds = None;
                }
            }
            Self::KnowledgeSearch {
                states,
                entry_kinds,
                source,
                stale,
                ..
            } => {
                if revision < 2 {
                    *states = None;
                }
                if revision < 2 {
                    *entry_kinds = None;
                }
                if revision < 2 {
                    *source = None;
                }
                if revision < 2 {
                    *stale = None;
                }
            }
            Self::KnowledgeDiagnostics {
                source_id,
                import_source,
                import_schema,
                source_label,
                ..
            } => {
                if revision < 2 {
                    *source_id = None;
                }
                if revision < 2 {
                    *import_source = None;
                }
                if revision < 2 {
                    *import_schema = None;
                }
                if revision < 3 {
                    *source_label = None;
                }
            }
            Self::KnowledgeWriteStatus { cursor, .. } => {
                if revision < 2 {
                    *cursor = None;
                }
            }
            Self::KnowledgeTaggedLessons {
                rule_states,
                activity,
                view,
                ..
            } => {
                if revision < 2 {
                    *rule_states = None;
                }
                if revision < 2 {
                    *activity = None;
                }
                if revision < 2 {
                    *view = None;
                }
            }
            Self::NotificationsList {
                categories,
                read,
                completion_only,
                ..
            } => {
                if revision < 3 {
                    *categories = None;
                }
                if revision < 3 {
                    *read = None;
                }
                if revision < 3 {
                    *completion_only = None;
                }
            }
            Self::JobsInspect { history, usage, .. } => {
                if revision < 2 {
                    *history = None;
                }
                if revision < 2 {
                    *usage = None;
                }
            }
            Self::RouterAvailability { session_id, .. } => {
                if revision < 2 {
                    *session_id = None;
                }
            }
            Self::RouterUsage {
                report_kind,
                scope,
                project_id,
                start_ms,
                end_ms,
                interval,
                user,
                provider,
                model,
                tool,
                purpose,
                ..
            } => {
                if revision < 3 {
                    *report_kind = None;
                }
                if revision < 3 {
                    *scope = None;
                }
                if revision < 3 {
                    *project_id = None;
                }
                if revision < 3 {
                    *start_ms = None;
                }
                if revision < 3 {
                    *end_ms = None;
                }
                if revision < 3 {
                    *interval = None;
                }
                if revision < 3 {
                    *user = None;
                }
                if revision < 3 {
                    *provider = None;
                }
                if revision < 3 {
                    *model = None;
                }
                if revision < 3 {
                    *tool = None;
                }
                if revision < 3 {
                    *purpose = None;
                }
            }
            Self::RouterPriceStatus {
                provider_ids,
                include_history,
                snapshot_ids,
                ..
            } => {
                if revision < 3 {
                    *provider_ids = None;
                }
                if revision < 3 {
                    *include_history = None;
                }
                if revision < 3 {
                    *snapshot_ids = None;
                }
            }
            Self::WorkspaceLogs {
                severity,
                outcome,
                operation,
                operation_id,
                correlation_id,
                target_id,
                stage,
                project_id,
                search,
                since_ms,
                until_ms,
                order,
                ..
            } => {
                if revision < 2 {
                    *severity = None;
                }
                if revision < 2 {
                    *outcome = None;
                }
                if revision < 2 {
                    *operation = None;
                }
                if revision < 2 {
                    *operation_id = None;
                }
                if revision < 2 {
                    *correlation_id = None;
                }
                if revision < 2 {
                    *target_id = None;
                }
                if revision < 2 {
                    *stage = None;
                }
                if revision < 2 {
                    *project_id = None;
                }
                if revision < 2 {
                    *search = None;
                }
                if revision < 2 {
                    *since_ms = None;
                }
                if revision < 2 {
                    *until_ms = None;
                }
                if revision < 2 {
                    *order = None;
                }
            }
            _ => {}
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::NextWorkInspectQueue { .. } if revision < 3 => {
                Err("operation predates its introduction".into())
            }
            Self::ApprovalsReviewQueue { .. } if revision < 3 => {
                Err("operation predates its introduction".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DomainCommand {
    #[serde(rename = "projects.create")]
    ProjectsCreate {
        name: Label,
        description: Content,
        repository_ids: Items<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial_plan: Option<AtomicWorkPlan>,
    },
    #[serde(rename = "projects.edit")]
    ProjectsEdit {
        project_id: ProjectId,
        expected_revision: Revision,
        changes: ProjectPatch,
    },
    #[serde(rename = "projects.membership")]
    ProjectsMembership {
        project_id: ProjectId,
        expected_revision: Revision,
        repository_ids: Items<Id>,
    },
    #[serde(rename = "projects.archive")]
    ProjectsArchive {
        project_id: ProjectId,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "projects.restore")]
    ProjectsRestore {
        project_id: ProjectId,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "projects.delete")]
    ProjectsDelete {
        project_id: ProjectId,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "features.create")]
    FeaturesCreate {
        project_id: ProjectId,
        creation: FeatureCreation,
    },
    #[serde(rename = "features.edit")]
    FeaturesEdit {
        work_id: Id,
        expected_revision: Revision,
        changes: FeaturePatch,
    },
    #[serde(rename = "issues.create")]
    IssuesCreate {
        project_id: ProjectId,
        title: Label,
        description: Content,
        repository_ids: Items<Id>,
        status_id: Id,
        priority: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feature_id: Option<RootId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "WorkItemType", extend("x-since" = 3))]
        task_type: Option<WorkItemType>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<Label>", extend("x-since" = 3))]
        labels: Option<Items<Label>>,
    },
    #[serde(rename = "issues.edit")]
    IssuesEdit {
        work_id: Id,
        expected_revision: Revision,
        changes: IssuePatch,
    },
    #[serde(rename = "tasks.create")]
    TasksCreate {
        project_id: ProjectId,
        title: Label,
        description: Content,
        repository_ids: Items<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<RootId>,
        status_id: Id,
        priority: i32,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "WorkItemType", extend("x-since" = 3))]
        task_type: Option<WorkItemType>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<Label>", extend("x-since" = 3))]
        labels: Option<Items<Label>>,
    },
    #[serde(rename = "tasks.edit")]
    TasksEdit {
        work_id: Id,
        expected_revision: Revision,
        changes: TaskPatch,
    },
    #[serde(rename = "features.prioritize")]
    #[schemars(extend("oneOf" = [{"required": ["kind", "projectId", "expectedQueueRevision", "orderedWorkIds"]}, {"required": ["kind", "projectId", "expectedQueueRevision", "placement"], "x-since": 2}]))]
    FeaturesPrioritize {
        project_id: ProjectId,
        expected_queue_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "NonEmpty<Id>")]
        ordered_work_ids: Option<NonEmpty<Id>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "QueuePlacement", extend("x-since" = 2))]
        placement: Option<QueuePlacement>,
    },
    #[serde(rename = "issues.prioritize")]
    #[schemars(extend("oneOf" = [{"required": ["kind", "projectId", "expectedQueueRevision", "orderedWorkIds"]}, {"required": ["kind", "projectId", "expectedQueueRevision", "placement"], "x-since": 2}]))]
    IssuesPrioritize {
        project_id: ProjectId,
        expected_queue_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "NonEmpty<Id>")]
        ordered_work_ids: Option<NonEmpty<Id>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "QueuePlacement", extend("x-since" = 2))]
        placement: Option<QueuePlacement>,
    },
    #[serde(rename = "features.edit_plan")]
    FeaturesEditPlan {
        feature_id: RootId,
        expected_revision: Revision,
        plan: PlanDraft,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "FeatureBudgetInput", extend("x-since" = 3))]
        budget: Option<FeatureBudgetInput>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3, "const" = true))]
        clear_budget_proposal: Option<bool>,
    },
    #[serde(rename = "features.dependencies")]
    FeaturesDependencies {
        feature_id: RootId,
        expected_revision: Revision,
        replacement: DependencyReplacement,
    },
    #[serde(rename = "features.request_plan_approval")]
    FeaturesRequestPlanApproval {
        feature_id: RootId,
        expected_revision: Revision,
        plan_revision: Revision,
        plan_digest: Digest,
    },
    #[serde(rename = "issues.publish")]
    IssuesPublish {
        issue_id: RootId,
        expected_revision: Revision,
        repository_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_remote_revision: Option<Text>,
    },
    #[serde(rename = "issues.link")]
    IssuesLink {
        issue_id: RootId,
        expected_revision: Revision,
        repository_id: Id,
        remote_issue_number: PositiveCounter,
    },
    #[serde(rename = "tasks.move")]
    TasksMove {
        task_id: Id,
        expected_revision: Revision,
        status_id: Id,
        expected_workflow_revision: Revision,
    },
    #[serde(rename = "tasks.assign")]
    TasksAssign {
        task_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignment_id: Option<AssignmentId>,
        clear_assignment: bool,
    },
    #[serde(rename = "tasks.dependency_links")]
    TasksDependencyLinks {
        task_id: Id,
        expected_revision: Revision,
        replacement: DependencyReplacement,
    },
    #[serde(rename = "tasks.acceptance_evidence")]
    TasksAcceptanceEvidence {
        task_id: Id,
        expected_revision: Revision,
        criteria: NonEmpty<AcceptanceCriterion>,
    },
    #[serde(rename = "tasks.sync_plan")]
    TasksSyncPlan {
        feature_id: RootId,
        expected_plan_revision: Revision,
        expected_task_set_revision: Revision,
        tasks: PlanItems<TaskPlanEntry>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_plan: Option<AtomicWorkPlan>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        staged_project_plan: Option<ArtifactRef>,
    },
    #[serde(rename = "workflows.create_status")]
    WorkflowsCreateStatus {
        project_id: ProjectId,
        expected_workflow_revision: Revision,
        label: Label,
        category: Label,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_status_id: Option<Id>,
    },
    #[serde(rename = "workflows.rename_status")]
    WorkflowsRenameStatus {
        project_id: ProjectId,
        status_id: Id,
        expected_revision: Revision,
        label: Label,
    },
    #[serde(rename = "workflows.order")]
    WorkflowsOrder {
        project_id: ProjectId,
        expected_workflow_revision: Revision,
        ordered_status_ids: NonEmpty<Id>,
    },
    #[serde(rename = "workflows.reassign_and_remove")]
    WorkflowsReassignAndRemove {
        project_id: ProjectId,
        expected_workflow_revision: Revision,
        remove_status_id: Id,
        destination_status_id: Id,
        expected_affected_work_digest: Digest,
    },
    #[serde(rename = "next_work.claim_project_queue")]
    NextWorkClaimProjectQueue {
        project_id: ProjectId,
        expected_queue_revision: Revision,
        lease_ms: PositiveCounter,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 3))]
        repository_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "WorkItemType", extend("x-since" = 3))]
        task_type: Option<WorkItemType>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 3))]
        label: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3))]
        another: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 3))]
        idempotency_key: Option<Label>,
    },
    #[serde(rename = "next_work.claim_feature_task")]
    NextWorkClaimFeatureTask {
        root_id: RootId,
        expected_task_set_revision: Revision,
        lease_ms: PositiveCounter,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 3))]
        repository_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "WorkItemType", extend("x-since" = 3))]
        task_type: Option<WorkItemType>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 3))]
        label: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "bool", extend("x-since" = 3))]
        another: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 3))]
        idempotency_key: Option<Label>,
    },
    #[serde(rename = "next_work.release_claim")]
    NextWorkReleaseClaim {
        claim_id: Id,
        expected_fence: Counter,
    },
    #[serde(rename = "runs.redirect")]
    RunsRedirect {
        root_id: RootId,
        expected_control_revision: Revision,
        message: Content,
        attachments: Items<ArtifactRef>,
    },
    #[serde(rename = "runs.stop")]
    RunsStop {
        root_id: RootId,
        expected_control_revision: Revision,
    },
    #[serde(rename = "agents.delegate")]
    AgentsDelegate {
        root_id: RootId,
        expected_control_revision: Revision,
        task_ids: Items<Id>,
        role_id: Id,
        instruction: Content,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_checkpoint_id: Option<Id>,
    },
    #[serde(rename = "agents.messages")]
    AgentsMessages {
        recipient_assignment_id: AssignmentId,
        message_class: MessageClass,
        content: Content,
        attachments: Items<ArtifactRef>,
    },
    #[serde(rename = "agents.acknowledge")]
    AgentsAcknowledge {
        assignment_id: AssignmentId,
        message_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "agents.checkpoint")]
    AgentsCheckpoint {
        assignment_id: AssignmentId,
        expected_revision: Revision,
        transcript_sequence: Counter,
        content: ArtifactRef,
    },
    #[serde(rename = "sessions.start")]
    SessionsStart {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root_id: Option<RootId>,
        title: Label,
    },
    #[serde(rename = "sessions.end")]
    SessionsEnd {
        session_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "indexing.index")]
    IndexingIndex {
        scope: DataScope,
        repository_ids: NonEmpty<Id>,
        expected_source_revision: Revision,
        views: ViewSelection,
    },
    #[serde(rename = "indexing.reindex")]
    IndexingReindex {
        scope: DataScope,
        repository_ids: NonEmpty<Id>,
        expected_source_revision: Revision,
        views: ViewSelection,
    },
    #[serde(rename = "indexing.retire_view")]
    IndexingRetireView {
        view_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "indexing.configure_sources")]
    IndexingConfigureSources {
        scope: DataScope,
        expected_revision: Revision,
        config: IndexSourceConfig,
    },
    #[serde(rename = "analysis.community_analysis")]
    AnalysisCommunityAnalysis {
        scope: DataScope,
        views: ViewSelection,
        expected_source_revision: Revision,
        node_ids: Items<Id>,
        batch_limit: Limit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_run_id: Option<Id>,
    },
    #[serde(rename = "analysis.link_and_detect")]
    AnalysisLinkAndDetect {
        scope: DataScope,
        views: ViewSelection,
        expected_source_revision: Revision,
        node_ids: Items<Id>,
        batch_limit: Limit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_run_id: Option<Id>,
    },
    #[serde(rename = "analysis.process_batch")]
    AnalysisProcessBatch {
        scope: DataScope,
        views: ViewSelection,
        expected_source_revision: Revision,
        node_ids: Items<Id>,
        batch_limit: Limit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_run_id: Option<Id>,
    },
    #[serde(rename = "analysis.reanalyze")]
    AnalysisReanalyze {
        scope: DataScope,
        views: ViewSelection,
        expected_source_revision: Revision,
        node_ids: Items<Id>,
        batch_limit: Limit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_run_id: Option<Id>,
    },
    #[serde(rename = "analysis.synthesize")]
    AnalysisSynthesize {
        scope: DataScope,
        views: ViewSelection,
        expected_source_revision: Revision,
        node_ids: Items<Id>,
        batch_limit: Limit,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_run_id: Option<Id>,
    },
    #[serde(rename = "schemas.index")]
    SchemasIndex {
        connection_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "schemas.refresh")]
    SchemasRefresh {
        connection_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "schemas.remove_index")]
    SchemasRemoveIndex {
        connection_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "schemas.configure")]
    SchemasConfigure {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        config: ConnectionConfig,
    },
    #[serde(rename = "database.execute_write")]
    DatabaseExecuteWrite {
        connection_id: Id,
        expected_connection_revision: Revision,
        statement: Content,
        parameters: Items<DbParameter>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
        timeout_ms: PositiveCounter,
    },
    #[serde(rename = "database.direct_schema_change")]
    DatabaseDirectSchemaChange {
        connection_id: Id,
        expected_connection_revision: Revision,
        statement: Content,
        parameters: Items<DbParameter>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
        timeout_ms: PositiveCounter,
    },
    #[serde(rename = "database.apply_migration")]
    DatabaseApplyMigration {
        connection_id: Id,
        expected_connection_revision: Revision,
        migration: ArtifactRef,
        expected_schema_digest: Digest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "database.configure_sensitive_columns")]
    DatabaseConfigureSensitiveColumns {
        connection_id: Id,
        expected_revision: Revision,
        column_ids: Items<Id>,
    },
    #[serde(rename = "knowledge.record")]
    KnowledgeRecord {
        scope: DataScope,
        batch: KnowledgeBatch,
    },
    #[serde(rename = "knowledge.revise")]
    KnowledgeRevise {
        scope: DataScope,
        claim_id: Id,
        expected_revision: Revision,
        replacement: ClaimDraft,
    },
    #[serde(rename = "knowledge.relate")]
    KnowledgeRelate {
        scope: DataScope,
        subject_id: Id,
        predicate: Label,
        object_id: Id,
        evidence_ids: Items<Id>,
    },
    #[serde(rename = "knowledge.tag")]
    KnowledgeTag {
        scope: DataScope,
        claim_ids: NonEmpty<Id>,
        expected_revisions: NonEmpty<RevisionedRef>,
        tags: Items<Label>,
    },
    #[serde(rename = "knowledge.verify")]
    KnowledgeVerify {
        scope: DataScope,
        claim_id: Id,
        expected_revision: Revision,
        reason: Content,
        evidence_ids: Items<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Text", extend("x-since" = 2))]
        source_revision: Option<Text>,
    },
    #[serde(rename = "knowledge.retract")]
    KnowledgeRetract {
        scope: DataScope,
        claim_id: Id,
        expected_revision: Revision,
        reason: Content,
        evidence_ids: Items<Id>,
    },
    #[serde(rename = "knowledge.challenge")]
    KnowledgeChallenge {
        scope: DataScope,
        claim_id: Id,
        expected_revision: Revision,
        reason: Content,
        evidence_ids: Items<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 2))]
        resolve_challenge_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ChallengeOutcome", extend("x-since" = 2))]
        outcome: Option<ChallengeOutcome>,
    },
    #[serde(rename = "knowledge.scope")]
    KnowledgeScope {
        claim_ids: NonEmpty<RevisionedRef>,
        destination: DataScope,
    },
    #[serde(rename = "knowledge.delete_by_source")]
    #[schemars(extend("oneOf" = [{"required": ["kind", "scope", "sourceId", "expectedScopeRevision", "previewDigest"]}, {"required": ["kind", "scope", "expectedScopeRevision", "previewDigest", "sourceLabel"], "x-since": 3}]))]
    KnowledgeDeleteBySource {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "Id")]
        source_id: Option<Id>,
        expected_scope_revision: Revision,
        preview_digest: Digest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 3))]
        source_label: Option<Label>,
    },
    #[serde(rename = "knowledge.delete_selected")]
    KnowledgeDeleteSelected {
        scope: DataScope,
        claims: NonEmpty<RevisionedRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "knowledge.import_user_data")]
    KnowledgeImportUserData {
        scope: DataScope,
        source: ArtifactRef,
        schema: SchemaRef,
        preview_digest: Digest,
    },
    #[serde(rename = "knowledge.promote")]
    KnowledgePromote {
        claim_id: Id,
        expected_revision: Revision,
        destination: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "PromoteAction", extend("x-since" = 2))]
        action: Option<PromoteAction>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "LessonAddress", extend("x-since" = 2))]
        address: Option<LessonAddress>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Content", extend("x-since" = 2))]
        reason: Option<Content>,
    },
    #[serde(rename = "dreaming.run")]
    DreamingRun {
        scope: DataScope,
        expected_settings_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_run_id: Option<Id>,
    },
    #[serde(rename = "dreaming.settings")]
    DreamingSettings {
        scope: DataScope,
        expected_revision: Revision,
        config: DreamConfig,
    },
    #[serde(rename = "dreaming.instructions")]
    DreamingInstructions {
        scope: DataScope,
        expected_revision: Revision,
        content: ArtifactRef,
    },
    #[serde(rename = "dreaming.propose")]
    DreamingPropose {
        run_id: Id,
        expected_revision: Revision,
        claim_ids: NonEmpty<RevisionedRef>,
        content: ArtifactRef,
    },
    #[serde(rename = "dreaming.revert")]
    DreamingRevert {
        run_id: Id,
        expected_revision: Revision,
        proposal_id: Id,
        expected_change_digest: Digest,
    },
    #[serde(rename = "dreaming.review")]
    DreamingReview {
        proposal_id: Id,
        expected_revision: Revision,
        decision: ProposalDecision,
        rationale: Content,
    },
    #[serde(rename = "documents.import")]
    DocumentsImport {
        scope: DataScope,
        source: ArtifactRef,
        title: Label,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replace_document_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
    },
    #[serde(rename = "documents.index")]
    DocumentsIndex {
        document_id: Id,
        version_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "documents.retry")]
    DocumentsRetry {
        document_id: Id,
        version_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "conventions.edit")]
    ConventionsEdit {
        convention_id: Id,
        expected_revision: Revision,
        content: ArtifactRef,
    },
    #[serde(rename = "conventions.applicability")]
    ConventionsApplicability {
        convention_id: Id,
        expected_revision: Revision,
        applicability: Items<Label>,
    },
    #[serde(rename = "conventions.retire")]
    ConventionsRetire {
        convention_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_id: Option<Id>,
    },
    #[serde(rename = "conventions.index")]
    ConventionsIndex {
        convention_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_id: Option<Id>,
    },
    #[serde(rename = "conventions.delete")]
    ConventionsDelete {
        convention_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_id: Option<Id>,
    },
    #[serde(rename = "conventions.delete_resource")]
    ConventionsDeleteResource {
        convention_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resource_id: Option<Id>,
    },
    #[serde(rename = "conventions.import")]
    ConventionsImport {
        scope: DataScope,
        source: ArtifactRef,
        schema: SchemaRef,
    },
    #[serde(rename = "conventions.move")]
    ConventionsMove {
        convention_id: Id,
        expected_revision: Revision,
        destination: DataScope,
    },
    #[serde(rename = "skills.draft")]
    SkillsDraft {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instruction_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        name: Label,
        content: ArtifactRef,
    },
    #[serde(rename = "skills.propose")]
    SkillsPropose {
        instruction_id: Id,
        expected_revision: Revision,
        content_digest: Digest,
    },
    #[serde(rename = "skills.rollback")]
    SkillsRollback {
        instruction_id: Id,
        expected_revision: Revision,
        target_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "roles.draft")]
    RolesDraft {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instruction_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        name: Label,
        content: ArtifactRef,
    },
    #[serde(rename = "roles.propose")]
    RolesPropose {
        instruction_id: Id,
        expected_revision: Revision,
        content_digest: Digest,
    },
    #[serde(rename = "roles.rollback")]
    RolesRollback {
        instruction_id: Id,
        expected_revision: Revision,
        target_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "approvals.propose")]
    ApprovalsPropose {
        scope: DataScope,
        operation_kind: Label,
        target: RevisionedRef,
        content_digest: Digest,
        summary: Content,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<ArtifactRef>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ApprovalContent", extend("x-since" = 3))]
        payload: Option<ApprovalContent>,
    },
    #[serde(rename = "approvals.record_human_decision")]
    ApprovalsRecordHumanDecision {
        proposal_id: Id,
        expected_revision: Revision,
        content_digest: Digest,
        decision: ProposalDecision,
        rationale: Content,
    },
    #[serde(rename = "notifications.acknowledge")]
    NotificationsAcknowledge {
        notification_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "notifications.settings")]
    NotificationsSettings {
        expected_revision: Revision,
        config: NotificationConfig,
    },
    #[serde(rename = "git.worktree")]
    GitWorktree {
        repository_id: Id,
        work_id: Id,
        expected_revision: Revision,
        action: GitActionKind,
        base: GitOid,
        registered_root_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree_id: Option<Id>,
    },
    #[serde(rename = "reviews.request")]
    ReviewsRequest {
        root_id: RootId,
        expected_revision: Revision,
        barrier: DeliveryBarrier,
        policy_revision: Revision,
    },
    #[serde(rename = "reviews.respond")]
    ReviewsRespond {
        finding_id: Id,
        expected_revision: Revision,
        response: Content,
        evidence: Items<ArtifactRef>,
    },
    #[serde(rename = "reviews.adjudicate")]
    ReviewsAdjudicate {
        finding_id: Id,
        expected_revision: Revision,
        actionable: bool,
        rationale: Content,
        evidence: Items<ArtifactRef>,
    },
    #[serde(rename = "github.account")]
    GithubAccount {
        action: AccountAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_flow_id: Option<Id>,
    },
    #[serde(rename = "github.issue_sync")]
    GithubIssueSync {
        repository_id: Id,
        expected_cursor_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_issue_number: Option<PositiveCounter>,
    },
    #[serde(rename = "resources.acquire")]
    ResourcesAcquire {
        resource_id: ResourceId,
        expected_revision: Revision,
        units: Limit,
        lease_ms: PositiveCounter,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Text", extend("x-since" = 2))]
        purpose: Option<Text>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "ResourcePriority", extend("x-since" = 2))]
        priority: Option<ResourcePriority>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Limit", extend("x-since" = 2))]
        capacity: Option<Limit>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Items<Label>", extend("x-since" = 2))]
        labels: Option<Items<Label>>,
    },
    #[serde(rename = "resources.renew")]
    ResourcesRenew {
        lease_id: Id,
        expected_fence: Counter,
        lease_ms: PositiveCounter,
    },
    #[serde(rename = "resources.release")]
    ResourcesRelease {
        lease_id: Id,
        expected_fence: Counter,
    },
    #[serde(rename = "browser.sessions")]
    BrowserSessions { action: BrowserSessionAction },
    #[serde(rename = "browser.navigate")]
    BrowserNavigate {
        session_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_id: Option<Id>,
        location: BrowserLocation,
    },
    #[serde(rename = "browser.input")]
    BrowserInput {
        session_id: Id,
        expected_revision: Revision,
        action: BrowserInput,
    },
    #[serde(rename = "browser.upload")]
    BrowserUpload {
        node: BrowserNode,
        files: NonEmpty<ArtifactRef>,
    },
    #[serde(rename = "desktop.request_control")]
    DesktopRequestControl {
        desktop_id: Id,
        expected_revision: Revision,
        reason: Content,
        lease_ms: PositiveCounter,
    },
    #[serde(rename = "desktop.input")]
    DesktopInput {
        desktop_id: Id,
        lease_id: Id,
        expected_fence: Counter,
        consent_id: Id,
        expected_desktop_revision: Revision,
        action: DesktopInput,
    },
    #[serde(rename = "desktop.release_control")]
    DesktopReleaseControl {
        desktop_id: Id,
        lease_id: Id,
        expected_fence: Counter,
    },
    #[serde(rename = "process.execute")]
    ProcessExecute { spec: ProcessSpec },
    #[serde(rename = "process.cancel")]
    ProcessCancel {
        process_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "process.install_project_dependencies")]
    ProcessInstallProjectDependencies {
        repository_id: Id,
        expected_revision: Revision,
        manifest: ArtifactRef,
        profile: ToolProfile,
    },
    #[serde(rename = "process.request_system_install")]
    ProcessRequestSystemInstall {
        package_spec: ArtifactRef,
        content_digest: Digest,
        reason: Content,
    },
    #[serde(rename = "containers.build")]
    ContainersBuild {
        engine_id: Id,
        project_id: ProjectId,
        context: RegisteredPath,
        definition: ArtifactRef,
        tag: Text,
    },
    #[serde(rename = "containers.pull")]
    ContainersPull {
        engine_id: Id,
        image: Text,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_ref: Option<Id>,
    },
    #[serde(rename = "containers.create")]
    ContainersCreate { spec: ContainerSpec },
    #[serde(rename = "containers.start")]
    ContainersStart {
        container_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "containers.stop")]
    ContainersStop {
        container_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "containers.exec")]
    ContainersExec {
        container_id: Id,
        expected_revision: Revision,
        command: NonEmpty<Text>,
        environment: Items<EnvironmentEntry>,
        timeout_ms: PositiveCounter,
    },
    #[serde(rename = "jobs.create")]
    JobsCreate {
        scope: DataScope,
        title: Label,
        schedule: Schedule,
        definition: JobDefinition,
        enabled: bool,
    },
    #[serde(rename = "jobs.edit")]
    JobsEdit {
        job_id: Id,
        expected_revision: Revision,
        title: Label,
        schedule: Schedule,
        definition: JobDefinition,
    },
    #[serde(rename = "jobs.enable")]
    JobsEnable {
        job_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "jobs.disable")]
    JobsDisable {
        job_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "jobs.delete")]
    JobsDelete {
        job_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "jobs.run_now")]
    JobsRunNow {
        job_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "jobs.cancel")]
    JobsCancel {
        job_id: Id,
        run_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "settings.save_revision")]
    SettingsSaveRevision {
        scope: DataScope,
        expected_revision: Revision,
        entries: NonEmpty<SettingAssignment>,
    },
    #[serde(rename = "router.route")]
    RouterRoute {
        session_id: Id,
        expected_context_epoch: Counter,
        intent: RouteIntent,
        effort: EffortName,
        required_capabilities: Items<Label>,
    },
    #[serde(rename = "router.pin")]
    RouterPin {
        session_id: Id,
        expected_pin_revision: Revision,
        intent: RouteIntent,
        effort: EffortName,
    },
    #[serde(rename = "router.price_refresh")]
    RouterPriceRefresh {
        provider_ids: NonEmpty<Id>,
        expected_price_revision: Revision,
    },
    #[serde(rename = "router.price_override")]
    #[schemars(extend("oneOf" = [{"required": ["kind", "modelId", "expectedRevision", "price"]}, {"required": ["kind", "modelId", "expectedRevision", "change"], "x-since": 3}]))]
    RouterPriceOverride {
        model_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "PriceInput")]
        price: Option<PriceInput>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "PriceOverrideChange", extend("x-since" = 3))]
        change: Option<PriceOverrideChange>,
    },
    #[serde(rename = "router.configure")]
    RouterConfigure { action: RouterConfigAction },
    #[serde(rename = "router.decide")]
    RouterDecide {
        model_id: Id,
        expected_model_revision: Revision,
        state: DynamicJson,
        questions: NonEmpty<NamedQuestion>,
        images: Items<MediaRef>,
        videos: Items<MediaRef>,
        timeout_ms: PositiveCounter,
    },
    #[serde(rename = "mcp.configure")]
    McpConfigure {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        name: Label,
        transport: McpTransport,
    },
    #[serde(rename = "mcp.connect")]
    McpConnect {
        server_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "mcp.discover")]
    McpDiscover {
        server_id: Id,
        expected_catalog_revision: Revision,
    },
    #[serde(rename = "mcp.select_tools")]
    McpSelectTools {
        server_id: Id,
        expected_revision: Revision,
        expected_catalog_revision: Revision,
        tool_ids: Items<Id>,
    },
    #[serde(rename = "addons.configure")]
    AddonsConfigure {
        package_id: Id,
        expected_revision: Revision,
        config: SchemaValue,
    },
    #[serde(rename = "addons.propose")]
    AddonsPropose {
        source: ArtifactRef,
        manifest_digest: Digest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_package_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
    },
    #[serde(rename = "help.generate_capability_docs")]
    HelpGenerateCapabilityDocs {
        registry_revision: Revision,
        expected_help_revision: Revision,
    },
    #[serde(rename = "workspace.record_diagnostic")]
    WorkspaceRecordDiagnostic {
        component: Label,
        code: Label,
        severity: Severity,
        message: Content,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<ArtifactRef>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        operation: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "OperationId", extend("x-since" = 2))]
        operation_id: Option<OperationId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        target_kind: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Id", extend("x-since" = 2))]
        target_id: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        stage: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Label", extend("x-since" = 2))]
        source_revision: Option<Label>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 2))]
        attempt: Option<Counter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 2))]
        started_at_ms: Option<Counter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 2))]
        finished_at_ms: Option<Counter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 2))]
        progress_completed: Option<Counter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Counter", extend("x-since" = 2))]
        progress_total: Option<Counter>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "DiagnosticOutcome", extend("x-since" = 2))]
        outcome: Option<DiagnosticOutcome>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "DiagnosticErrorClass", extend("x-since" = 2))]
        error_class: Option<DiagnosticErrorClass>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "Content", extend("x-since" = 2))]
        output: Option<Content>,
    },
    #[serde(rename = "workspace.backup")]
    WorkspaceBackup {
        destination_grant_id: Id,
        expected_store_sequence: Counter,
    },
    #[serde(rename = "workspace.restore")]
    WorkspaceRestore {
        backup: ArtifactRef,
        expected_store_id: StoreId,
        expected_restore_epoch: Counter,
        proposal_id: Id,
    },
    #[serde(rename = "workspace.migrate")]
    WorkspaceMigrate {
        migration_manifest: ArtifactRef,
        expected_schema_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
    },
    #[serde(rename = "workspace.approve_install")]
    WorkspaceApproveInstall {
        update_id: Id,
        expected_revision: Revision,
        binary_digest: Digest,
        proposal_id: Id,
    },
    #[serde(rename = "repositories.remove_index")]
    RepositoriesRemoveIndex {
        repository_id: Id,
        expected_revision: Revision,
    },
    #[serde(rename = "queue.resolve")]
    QueueResolve {
        delivery_id: Id,
        expected_revision: Revision,
        expected_fence: Counter,
        resolution: QueueResolution,
        reason: Content,
    },
    #[serde(rename = "queue.retry")]
    QueueRetry {
        delivery_id: Id,
        expected_revision: Revision,
        expected_fence: Counter,
        resolution: QueueResolution,
        reason: Content,
    },
    #[serde(rename = "queue.reconcile")]
    QueueReconcile {
        delivery_id: Id,
        expected_revision: Revision,
        expected_fence: Counter,
        resolution: QueueResolution,
        reason: Content,
    },
    #[serde(rename = "imports.configure")]
    ImportsConfigure {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        config: ImportConfig,
    },
    #[serde(rename = "imports.apply")]
    ImportsApply {
        config_id: Id,
        expected_revision: Revision,
        source_snapshot: Digest,
        preview_digest: Digest,
    },
    #[serde(rename = "maintenance.prune_diagnostics")]
    MaintenancePruneDiagnostics {
        preview_only: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        older_than_ms: Counter,
        max_items: Limit,
    },
    #[serde(rename = "maintenance.prune_index_runs")]
    MaintenancePruneIndexRuns {
        preview_only: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        older_than_ms: Counter,
        max_items: Limit,
    },
    #[serde(rename = "maintenance.prune_messages")]
    MaintenancePruneMessages {
        preview_only: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        older_than_ms: Counter,
        max_items: Limit,
    },
    #[serde(rename = "maintenance.prune_sessions")]
    MaintenancePruneSessions {
        preview_only: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview_id: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<Revision>,
        older_than_ms: Counter,
        max_items: Limit,
    },
    #[serde(rename = "repository_docs.generate")]
    RepositoryDocsGenerate {
        repository_id: Id,
        views: ViewSelection,
        expected_revision: Revision,
        output_root_id: Id,
        topics: NonEmpty<Label>,
    },
    #[serde(rename = "mcp_server.configure")]
    McpServerConfigure {
        expected_revision: Revision,
        enabled: bool,
        bindings: Items<McpServerBinding>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_port: Option<Port>,
    },
    #[serde(rename = "mcp_server.set_client_access")]
    McpServerSetClientAccess { action: McpClientAccessAction },
    #[serde(rename = "tasks.prioritize")]
    #[schemars(extend("oneOf" = [{"required": ["kind", "projectId", "expectedQueueRevision", "priority", "orderedTaskIds"]}, {"required": ["kind", "projectId", "expectedQueueRevision", "priority", "placement"], "x-since": 2}]))]
    TasksPrioritize {
        project_id: ProjectId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feature_id: Option<RootId>,
        expected_queue_revision: Revision,
        priority: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "NonEmpty<Id>")]
        ordered_task_ids: Option<NonEmpty<Id>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "gated_member"
        )]
        #[schemars(with = "QueuePlacement", extend("x-since" = 2))]
        placement: Option<QueuePlacement>,
    },
    #[serde(rename = "tasks.set_feature")]
    TasksSetFeature {
        task_id: Id,
        expected_revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feature_id: Option<RootId>,
        clear_feature: bool,
    },
    #[serde(rename = "artifacts.tag")]
    ArtifactsTag {
        artifact_id: Id,
        expected_revision: Revision,
        tags: ArtifactTags,
    },
    #[serde(rename = "evals.grade")]
    EvalsGrade {
        session_id: Id,
        suite: EvalSuiteSnapshot,
    },
    #[schemars(extend("x-since" = 3))]
    #[serde(rename = "notifications.mark_read")]
    NotificationsMarkRead {
        notification_id: Id,
        expected_revision: Revision,
        read: bool,
    },
}
impl DomainCommand {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::ProjectsCreate { .. } => "projects.create",
            Self::ProjectsEdit { .. } => "projects.edit",
            Self::ProjectsMembership { .. } => "projects.membership",
            Self::ProjectsArchive { .. } => "projects.archive",
            Self::ProjectsRestore { .. } => "projects.restore",
            Self::ProjectsDelete { .. } => "projects.delete",
            Self::FeaturesCreate { .. } => "features.create",
            Self::FeaturesEdit { .. } => "features.edit",
            Self::IssuesCreate { .. } => "issues.create",
            Self::IssuesEdit { .. } => "issues.edit",
            Self::TasksCreate { .. } => "tasks.create",
            Self::TasksEdit { .. } => "tasks.edit",
            Self::FeaturesPrioritize { .. } => "features.prioritize",
            Self::IssuesPrioritize { .. } => "issues.prioritize",
            Self::FeaturesEditPlan { .. } => "features.edit_plan",
            Self::FeaturesDependencies { .. } => "features.dependencies",
            Self::FeaturesRequestPlanApproval { .. } => "features.request_plan_approval",
            Self::IssuesPublish { .. } => "issues.publish",
            Self::IssuesLink { .. } => "issues.link",
            Self::TasksMove { .. } => "tasks.move",
            Self::TasksAssign { .. } => "tasks.assign",
            Self::TasksDependencyLinks { .. } => "tasks.dependency_links",
            Self::TasksAcceptanceEvidence { .. } => "tasks.acceptance_evidence",
            Self::TasksSyncPlan { .. } => "tasks.sync_plan",
            Self::WorkflowsCreateStatus { .. } => "workflows.create_status",
            Self::WorkflowsRenameStatus { .. } => "workflows.rename_status",
            Self::WorkflowsOrder { .. } => "workflows.order",
            Self::WorkflowsReassignAndRemove { .. } => "workflows.reassign_and_remove",
            Self::NextWorkClaimProjectQueue { .. } => "next_work.claim_project_queue",
            Self::NextWorkClaimFeatureTask { .. } => "next_work.claim_feature_task",
            Self::NextWorkReleaseClaim { .. } => "next_work.release_claim",
            Self::RunsRedirect { .. } => "runs.redirect",
            Self::RunsStop { .. } => "runs.stop",
            Self::AgentsDelegate { .. } => "agents.delegate",
            Self::AgentsMessages { .. } => "agents.messages",
            Self::AgentsAcknowledge { .. } => "agents.acknowledge",
            Self::AgentsCheckpoint { .. } => "agents.checkpoint",
            Self::SessionsStart { .. } => "sessions.start",
            Self::SessionsEnd { .. } => "sessions.end",
            Self::IndexingIndex { .. } => "indexing.index",
            Self::IndexingReindex { .. } => "indexing.reindex",
            Self::IndexingRetireView { .. } => "indexing.retire_view",
            Self::IndexingConfigureSources { .. } => "indexing.configure_sources",
            Self::AnalysisCommunityAnalysis { .. } => "analysis.community_analysis",
            Self::AnalysisLinkAndDetect { .. } => "analysis.link_and_detect",
            Self::AnalysisProcessBatch { .. } => "analysis.process_batch",
            Self::AnalysisReanalyze { .. } => "analysis.reanalyze",
            Self::AnalysisSynthesize { .. } => "analysis.synthesize",
            Self::SchemasIndex { .. } => "schemas.index",
            Self::SchemasRefresh { .. } => "schemas.refresh",
            Self::SchemasRemoveIndex { .. } => "schemas.remove_index",
            Self::SchemasConfigure { .. } => "schemas.configure",
            Self::DatabaseExecuteWrite { .. } => "database.execute_write",
            Self::DatabaseDirectSchemaChange { .. } => "database.direct_schema_change",
            Self::DatabaseApplyMigration { .. } => "database.apply_migration",
            Self::DatabaseConfigureSensitiveColumns { .. } => {
                "database.configure_sensitive_columns"
            }
            Self::KnowledgeRecord { .. } => "knowledge.record",
            Self::KnowledgeRevise { .. } => "knowledge.revise",
            Self::KnowledgeRelate { .. } => "knowledge.relate",
            Self::KnowledgeTag { .. } => "knowledge.tag",
            Self::KnowledgeVerify { .. } => "knowledge.verify",
            Self::KnowledgeRetract { .. } => "knowledge.retract",
            Self::KnowledgeChallenge { .. } => "knowledge.challenge",
            Self::KnowledgeScope { .. } => "knowledge.scope",
            Self::KnowledgeDeleteBySource { .. } => "knowledge.delete_by_source",
            Self::KnowledgeDeleteSelected { .. } => "knowledge.delete_selected",
            Self::KnowledgeImportUserData { .. } => "knowledge.import_user_data",
            Self::KnowledgePromote { .. } => "knowledge.promote",
            Self::DreamingRun { .. } => "dreaming.run",
            Self::DreamingSettings { .. } => "dreaming.settings",
            Self::DreamingInstructions { .. } => "dreaming.instructions",
            Self::DreamingPropose { .. } => "dreaming.propose",
            Self::DreamingRevert { .. } => "dreaming.revert",
            Self::DreamingReview { .. } => "dreaming.review",
            Self::DocumentsImport { .. } => "documents.import",
            Self::DocumentsIndex { .. } => "documents.index",
            Self::DocumentsRetry { .. } => "documents.retry",
            Self::ConventionsEdit { .. } => "conventions.edit",
            Self::ConventionsApplicability { .. } => "conventions.applicability",
            Self::ConventionsRetire { .. } => "conventions.retire",
            Self::ConventionsIndex { .. } => "conventions.index",
            Self::ConventionsDelete { .. } => "conventions.delete",
            Self::ConventionsDeleteResource { .. } => "conventions.delete_resource",
            Self::ConventionsImport { .. } => "conventions.import",
            Self::ConventionsMove { .. } => "conventions.move",
            Self::SkillsDraft { .. } => "skills.draft",
            Self::SkillsPropose { .. } => "skills.propose",
            Self::SkillsRollback { .. } => "skills.rollback",
            Self::RolesDraft { .. } => "roles.draft",
            Self::RolesPropose { .. } => "roles.propose",
            Self::RolesRollback { .. } => "roles.rollback",
            Self::ApprovalsPropose { .. } => "approvals.propose",
            Self::ApprovalsRecordHumanDecision { .. } => "approvals.record_human_decision",
            Self::NotificationsAcknowledge { .. } => "notifications.acknowledge",
            Self::NotificationsSettings { .. } => "notifications.settings",
            Self::GitWorktree { .. } => "git.worktree",
            Self::ReviewsRequest { .. } => "reviews.request",
            Self::ReviewsRespond { .. } => "reviews.respond",
            Self::ReviewsAdjudicate { .. } => "reviews.adjudicate",
            Self::GithubAccount { .. } => "github.account",
            Self::GithubIssueSync { .. } => "github.issue_sync",
            Self::ResourcesAcquire { .. } => "resources.acquire",
            Self::ResourcesRenew { .. } => "resources.renew",
            Self::ResourcesRelease { .. } => "resources.release",
            Self::BrowserSessions { .. } => "browser.sessions",
            Self::BrowserNavigate { .. } => "browser.navigate",
            Self::BrowserInput { .. } => "browser.input",
            Self::BrowserUpload { .. } => "browser.upload",
            Self::DesktopRequestControl { .. } => "desktop.request_control",
            Self::DesktopInput { .. } => "desktop.input",
            Self::DesktopReleaseControl { .. } => "desktop.release_control",
            Self::ProcessExecute { .. } => "process.execute",
            Self::ProcessCancel { .. } => "process.cancel",
            Self::ProcessInstallProjectDependencies { .. } => {
                "process.install_project_dependencies"
            }
            Self::ProcessRequestSystemInstall { .. } => "process.request_system_install",
            Self::ContainersBuild { .. } => "containers.build",
            Self::ContainersPull { .. } => "containers.pull",
            Self::ContainersCreate { .. } => "containers.create",
            Self::ContainersStart { .. } => "containers.start",
            Self::ContainersStop { .. } => "containers.stop",
            Self::ContainersExec { .. } => "containers.exec",
            Self::JobsCreate { .. } => "jobs.create",
            Self::JobsEdit { .. } => "jobs.edit",
            Self::JobsEnable { .. } => "jobs.enable",
            Self::JobsDisable { .. } => "jobs.disable",
            Self::JobsDelete { .. } => "jobs.delete",
            Self::JobsRunNow { .. } => "jobs.run_now",
            Self::JobsCancel { .. } => "jobs.cancel",
            Self::SettingsSaveRevision { .. } => "settings.save_revision",
            Self::RouterRoute { .. } => "router.route",
            Self::RouterPin { .. } => "router.pin",
            Self::RouterPriceRefresh { .. } => "router.price_refresh",
            Self::RouterPriceOverride { .. } => "router.price_override",
            Self::RouterConfigure { .. } => "router.configure",
            Self::RouterDecide { .. } => "router.decide",
            Self::McpConfigure { .. } => "mcp.configure",
            Self::McpConnect { .. } => "mcp.connect",
            Self::McpDiscover { .. } => "mcp.discover",
            Self::McpSelectTools { .. } => "mcp.select_tools",
            Self::AddonsConfigure { .. } => "addons.configure",
            Self::AddonsPropose { .. } => "addons.propose",
            Self::HelpGenerateCapabilityDocs { .. } => "help.generate_capability_docs",
            Self::WorkspaceRecordDiagnostic { .. } => "workspace.record_diagnostic",
            Self::WorkspaceBackup { .. } => "workspace.backup",
            Self::WorkspaceRestore { .. } => "workspace.restore",
            Self::WorkspaceMigrate { .. } => "workspace.migrate",
            Self::WorkspaceApproveInstall { .. } => "workspace.approve_install",
            Self::RepositoriesRemoveIndex { .. } => "repositories.remove_index",
            Self::QueueResolve { .. } => "queue.resolve",
            Self::QueueRetry { .. } => "queue.retry",
            Self::QueueReconcile { .. } => "queue.reconcile",
            Self::ImportsConfigure { .. } => "imports.configure",
            Self::ImportsApply { .. } => "imports.apply",
            Self::MaintenancePruneDiagnostics { .. } => "maintenance.prune_diagnostics",
            Self::MaintenancePruneIndexRuns { .. } => "maintenance.prune_index_runs",
            Self::MaintenancePruneMessages { .. } => "maintenance.prune_messages",
            Self::MaintenancePruneSessions { .. } => "maintenance.prune_sessions",
            Self::RepositoryDocsGenerate { .. } => "repository_docs.generate",
            Self::McpServerConfigure { .. } => "mcp_server.configure",
            Self::McpServerSetClientAccess { .. } => "mcp_server.set_client_access",
            Self::TasksPrioritize { .. } => "tasks.prioritize",
            Self::TasksSetFeature { .. } => "tasks.set_feature",
            Self::ArtifactsTag { .. } => "artifacts.tag",
            Self::EvalsGrade { .. } => "evals.grade",
            Self::NotificationsMarkRead { .. } => "notifications.mark_read",
        }
    }
}
impl Check for DomainCommand {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::ProjectsCreate {
                name,
                description,
                repository_ids,
                initial_plan,
            } => {
                name.check()?;
                description.check()?;
                repository_ids.check()?;
                initial_plan.check()?;
            }
            Self::ProjectsEdit {
                project_id,
                expected_revision,
                changes,
            } => {
                project_id.check()?;
                expected_revision.check()?;
                changes.check()?;
            }
            Self::ProjectsMembership {
                project_id,
                expected_revision,
                repository_ids,
            } => {
                project_id.check()?;
                expected_revision.check()?;
                repository_ids.check()?;
            }
            Self::ProjectsArchive {
                project_id,
                expected_revision,
                proposal_id,
            } => {
                project_id.check()?;
                expected_revision.check()?;
                proposal_id.check()?;
            }
            Self::ProjectsRestore {
                project_id,
                expected_revision,
                proposal_id,
            } => {
                project_id.check()?;
                expected_revision.check()?;
                proposal_id.check()?;
            }
            Self::ProjectsDelete {
                project_id,
                expected_revision,
                proposal_id,
            } => {
                project_id.check()?;
                expected_revision.check()?;
                proposal_id.check()?;
            }
            Self::FeaturesCreate {
                project_id,
                creation,
            } => {
                project_id.check()?;
                creation.check()?;
            }
            Self::FeaturesEdit {
                work_id,
                expected_revision,
                changes,
            } => {
                work_id.check()?;
                expected_revision.check()?;
                changes.check()?;
            }
            Self::IssuesCreate {
                project_id,
                title,
                description,
                repository_ids,
                status_id,
                priority,
                feature_id,
                task_type,
                labels,
            } => {
                project_id.check()?;
                title.check()?;
                description.check()?;
                repository_ids.check()?;
                status_id.check()?;
                priority.check()?;
                feature_id.check()?;
                task_type.check()?;
                labels.check()?;
            }
            Self::IssuesEdit {
                work_id,
                expected_revision,
                changes,
            } => {
                work_id.check()?;
                expected_revision.check()?;
                changes.check()?;
            }
            Self::TasksCreate {
                project_id,
                title,
                description,
                repository_ids,
                parent_id,
                status_id,
                priority,
                task_type,
                labels,
            } => {
                project_id.check()?;
                title.check()?;
                description.check()?;
                repository_ids.check()?;
                parent_id.check()?;
                status_id.check()?;
                priority.check()?;
                task_type.check()?;
                labels.check()?;
            }
            Self::TasksEdit {
                work_id,
                expected_revision,
                changes,
            } => {
                work_id.check()?;
                expected_revision.check()?;
                changes.check()?;
            }
            Self::FeaturesPrioritize {
                project_id,
                expected_queue_revision,
                ordered_work_ids,
                placement,
            } => {
                project_id.check()?;
                expected_queue_revision.check()?;
                ordered_work_ids.check()?;
                placement.check()?;
            }
            Self::IssuesPrioritize {
                project_id,
                expected_queue_revision,
                ordered_work_ids,
                placement,
            } => {
                project_id.check()?;
                expected_queue_revision.check()?;
                ordered_work_ids.check()?;
                placement.check()?;
            }
            Self::FeaturesEditPlan {
                feature_id,
                expected_revision,
                plan,
                budget,
                clear_budget_proposal,
            } => {
                feature_id.check()?;
                expected_revision.check()?;
                plan.check()?;
                budget.check()?;
                clear_budget_proposal.check()?;
            }
            Self::FeaturesDependencies {
                feature_id,
                expected_revision,
                replacement,
            } => {
                feature_id.check()?;
                expected_revision.check()?;
                replacement.check()?;
            }
            Self::FeaturesRequestPlanApproval {
                feature_id,
                expected_revision,
                plan_revision,
                plan_digest,
            } => {
                feature_id.check()?;
                expected_revision.check()?;
                plan_revision.check()?;
                plan_digest.check()?;
            }
            Self::IssuesPublish {
                issue_id,
                expected_revision,
                repository_id,
                expected_remote_revision,
            } => {
                issue_id.check()?;
                expected_revision.check()?;
                repository_id.check()?;
                expected_remote_revision.check()?;
            }
            Self::IssuesLink {
                issue_id,
                expected_revision,
                repository_id,
                remote_issue_number,
            } => {
                issue_id.check()?;
                expected_revision.check()?;
                repository_id.check()?;
                remote_issue_number.check()?;
            }
            Self::TasksMove {
                task_id,
                expected_revision,
                status_id,
                expected_workflow_revision,
            } => {
                task_id.check()?;
                expected_revision.check()?;
                status_id.check()?;
                expected_workflow_revision.check()?;
            }
            Self::TasksAssign {
                task_id,
                expected_revision,
                assignment_id,
                clear_assignment,
            } => {
                task_id.check()?;
                expected_revision.check()?;
                assignment_id.check()?;
                clear_assignment.check()?;
            }
            Self::TasksDependencyLinks {
                task_id,
                expected_revision,
                replacement,
            } => {
                task_id.check()?;
                expected_revision.check()?;
                replacement.check()?;
            }
            Self::TasksAcceptanceEvidence {
                task_id,
                expected_revision,
                criteria,
            } => {
                task_id.check()?;
                expected_revision.check()?;
                criteria.check()?;
            }
            Self::TasksSyncPlan {
                feature_id,
                expected_plan_revision,
                expected_task_set_revision,
                tasks,
                project_plan,
                staged_project_plan,
            } => {
                feature_id.check()?;
                expected_plan_revision.check()?;
                expected_task_set_revision.check()?;
                tasks.check()?;
                project_plan.check()?;
                staged_project_plan.check()?;
            }
            Self::WorkflowsCreateStatus {
                project_id,
                expected_workflow_revision,
                label,
                category,
                after_status_id,
            } => {
                project_id.check()?;
                expected_workflow_revision.check()?;
                label.check()?;
                category.check()?;
                after_status_id.check()?;
            }
            Self::WorkflowsRenameStatus {
                project_id,
                status_id,
                expected_revision,
                label,
            } => {
                project_id.check()?;
                status_id.check()?;
                expected_revision.check()?;
                label.check()?;
            }
            Self::WorkflowsOrder {
                project_id,
                expected_workflow_revision,
                ordered_status_ids,
            } => {
                project_id.check()?;
                expected_workflow_revision.check()?;
                ordered_status_ids.check()?;
            }
            Self::WorkflowsReassignAndRemove {
                project_id,
                expected_workflow_revision,
                remove_status_id,
                destination_status_id,
                expected_affected_work_digest,
            } => {
                project_id.check()?;
                expected_workflow_revision.check()?;
                remove_status_id.check()?;
                destination_status_id.check()?;
                expected_affected_work_digest.check()?;
            }
            Self::NextWorkClaimProjectQueue {
                project_id,
                expected_queue_revision,
                lease_ms,
                repository_id,
                task_type,
                label,
                another,
                idempotency_key,
            } => {
                project_id.check()?;
                expected_queue_revision.check()?;
                lease_ms.check()?;
                repository_id.check()?;
                task_type.check()?;
                label.check()?;
                another.check()?;
                idempotency_key.check()?;
            }
            Self::NextWorkClaimFeatureTask {
                root_id,
                expected_task_set_revision,
                lease_ms,
                repository_id,
                task_type,
                label,
                another,
                idempotency_key,
            } => {
                root_id.check()?;
                expected_task_set_revision.check()?;
                lease_ms.check()?;
                repository_id.check()?;
                task_type.check()?;
                label.check()?;
                another.check()?;
                idempotency_key.check()?;
            }
            Self::NextWorkReleaseClaim {
                claim_id,
                expected_fence,
            } => {
                claim_id.check()?;
                expected_fence.check()?;
            }
            Self::RunsRedirect {
                root_id,
                expected_control_revision,
                message,
                attachments,
            } => {
                root_id.check()?;
                expected_control_revision.check()?;
                message.check()?;
                attachments.check()?;
            }
            Self::RunsStop {
                root_id,
                expected_control_revision,
            } => {
                root_id.check()?;
                expected_control_revision.check()?;
            }
            Self::AgentsDelegate {
                root_id,
                expected_control_revision,
                task_ids,
                role_id,
                instruction,
                source_checkpoint_id,
            } => {
                root_id.check()?;
                expected_control_revision.check()?;
                task_ids.check()?;
                role_id.check()?;
                instruction.check()?;
                source_checkpoint_id.check()?;
            }
            Self::AgentsMessages {
                recipient_assignment_id,
                message_class,
                content,
                attachments,
            } => {
                recipient_assignment_id.check()?;
                message_class.check()?;
                content.check()?;
                attachments.check()?;
            }
            Self::AgentsAcknowledge {
                assignment_id,
                message_id,
                expected_revision,
            } => {
                assignment_id.check()?;
                message_id.check()?;
                expected_revision.check()?;
            }
            Self::AgentsCheckpoint {
                assignment_id,
                expected_revision,
                transcript_sequence,
                content,
            } => {
                assignment_id.check()?;
                expected_revision.check()?;
                transcript_sequence.check()?;
                content.check()?;
            }
            Self::SessionsStart {
                scope,
                root_id,
                title,
            } => {
                scope.check()?;
                root_id.check()?;
                title.check()?;
            }
            Self::SessionsEnd {
                session_id,
                expected_revision,
            } => {
                session_id.check()?;
                expected_revision.check()?;
            }
            Self::IndexingIndex {
                scope,
                repository_ids,
                expected_source_revision,
                views,
            } => {
                scope.check()?;
                repository_ids.check()?;
                expected_source_revision.check()?;
                views.check()?;
            }
            Self::IndexingReindex {
                scope,
                repository_ids,
                expected_source_revision,
                views,
            } => {
                scope.check()?;
                repository_ids.check()?;
                expected_source_revision.check()?;
                views.check()?;
            }
            Self::IndexingRetireView {
                view_id,
                expected_revision,
            } => {
                view_id.check()?;
                expected_revision.check()?;
            }
            Self::IndexingConfigureSources {
                scope,
                expected_revision,
                config,
            } => {
                scope.check()?;
                expected_revision.check()?;
                config.check()?;
            }
            Self::AnalysisCommunityAnalysis {
                scope,
                views,
                expected_source_revision,
                node_ids,
                batch_limit,
                resume_run_id,
            } => {
                scope.check()?;
                views.check()?;
                expected_source_revision.check()?;
                node_ids.check()?;
                batch_limit.check()?;
                resume_run_id.check()?;
            }
            Self::AnalysisLinkAndDetect {
                scope,
                views,
                expected_source_revision,
                node_ids,
                batch_limit,
                resume_run_id,
            } => {
                scope.check()?;
                views.check()?;
                expected_source_revision.check()?;
                node_ids.check()?;
                batch_limit.check()?;
                resume_run_id.check()?;
            }
            Self::AnalysisProcessBatch {
                scope,
                views,
                expected_source_revision,
                node_ids,
                batch_limit,
                resume_run_id,
            } => {
                scope.check()?;
                views.check()?;
                expected_source_revision.check()?;
                node_ids.check()?;
                batch_limit.check()?;
                resume_run_id.check()?;
            }
            Self::AnalysisReanalyze {
                scope,
                views,
                expected_source_revision,
                node_ids,
                batch_limit,
                resume_run_id,
            } => {
                scope.check()?;
                views.check()?;
                expected_source_revision.check()?;
                node_ids.check()?;
                batch_limit.check()?;
                resume_run_id.check()?;
            }
            Self::AnalysisSynthesize {
                scope,
                views,
                expected_source_revision,
                node_ids,
                batch_limit,
                resume_run_id,
            } => {
                scope.check()?;
                views.check()?;
                expected_source_revision.check()?;
                node_ids.check()?;
                batch_limit.check()?;
                resume_run_id.check()?;
            }
            Self::SchemasIndex {
                connection_id,
                expected_revision,
            } => {
                connection_id.check()?;
                expected_revision.check()?;
            }
            Self::SchemasRefresh {
                connection_id,
                expected_revision,
            } => {
                connection_id.check()?;
                expected_revision.check()?;
            }
            Self::SchemasRemoveIndex {
                connection_id,
                expected_revision,
            } => {
                connection_id.check()?;
                expected_revision.check()?;
            }
            Self::SchemasConfigure {
                scope,
                connection_id,
                expected_revision,
                config,
            } => {
                scope.check()?;
                connection_id.check()?;
                expected_revision.check()?;
                config.check()?;
            }
            Self::DatabaseExecuteWrite {
                connection_id,
                expected_connection_revision,
                statement,
                parameters,
                proposal_id,
                timeout_ms,
            } => {
                connection_id.check()?;
                expected_connection_revision.check()?;
                statement.check()?;
                parameters.check()?;
                proposal_id.check()?;
                timeout_ms.check()?;
            }
            Self::DatabaseDirectSchemaChange {
                connection_id,
                expected_connection_revision,
                statement,
                parameters,
                proposal_id,
                timeout_ms,
            } => {
                connection_id.check()?;
                expected_connection_revision.check()?;
                statement.check()?;
                parameters.check()?;
                proposal_id.check()?;
                timeout_ms.check()?;
            }
            Self::DatabaseApplyMigration {
                connection_id,
                expected_connection_revision,
                migration,
                expected_schema_digest,
                proposal_id,
            } => {
                connection_id.check()?;
                expected_connection_revision.check()?;
                migration.check()?;
                expected_schema_digest.check()?;
                proposal_id.check()?;
            }
            Self::DatabaseConfigureSensitiveColumns {
                connection_id,
                expected_revision,
                column_ids,
            } => {
                connection_id.check()?;
                expected_revision.check()?;
                column_ids.check()?;
            }
            Self::KnowledgeRecord { scope, batch } => {
                scope.check()?;
                batch.check()?;
            }
            Self::KnowledgeRevise {
                scope,
                claim_id,
                expected_revision,
                replacement,
            } => {
                scope.check()?;
                claim_id.check()?;
                expected_revision.check()?;
                replacement.check()?;
            }
            Self::KnowledgeRelate {
                scope,
                subject_id,
                predicate,
                object_id,
                evidence_ids,
            } => {
                scope.check()?;
                subject_id.check()?;
                predicate.check()?;
                object_id.check()?;
                evidence_ids.check()?;
            }
            Self::KnowledgeTag {
                scope,
                claim_ids,
                expected_revisions,
                tags,
            } => {
                scope.check()?;
                claim_ids.check()?;
                expected_revisions.check()?;
                tags.check()?;
            }
            Self::KnowledgeVerify {
                scope,
                claim_id,
                expected_revision,
                reason,
                evidence_ids,
                source_revision,
            } => {
                scope.check()?;
                claim_id.check()?;
                expected_revision.check()?;
                reason.check()?;
                evidence_ids.check()?;
                source_revision.check()?;
            }
            Self::KnowledgeRetract {
                scope,
                claim_id,
                expected_revision,
                reason,
                evidence_ids,
            } => {
                scope.check()?;
                claim_id.check()?;
                expected_revision.check()?;
                reason.check()?;
                evidence_ids.check()?;
            }
            Self::KnowledgeChallenge {
                scope,
                claim_id,
                expected_revision,
                reason,
                evidence_ids,
                resolve_challenge_id,
                outcome,
            } => {
                scope.check()?;
                claim_id.check()?;
                expected_revision.check()?;
                reason.check()?;
                evidence_ids.check()?;
                resolve_challenge_id.check()?;
                outcome.check()?;
            }
            Self::KnowledgeScope {
                claim_ids,
                destination,
            } => {
                claim_ids.check()?;
                destination.check()?;
            }
            Self::KnowledgeDeleteBySource {
                scope,
                source_id,
                expected_scope_revision,
                preview_digest,
                proposal_id,
                source_label,
            } => {
                scope.check()?;
                source_id.check()?;
                expected_scope_revision.check()?;
                preview_digest.check()?;
                proposal_id.check()?;
                source_label.check()?;
            }
            Self::KnowledgeDeleteSelected {
                scope,
                claims,
                proposal_id,
            } => {
                scope.check()?;
                claims.check()?;
                proposal_id.check()?;
            }
            Self::KnowledgeImportUserData {
                scope,
                source,
                schema,
                preview_digest,
            } => {
                scope.check()?;
                source.check()?;
                schema.check()?;
                preview_digest.check()?;
            }
            Self::KnowledgePromote {
                claim_id,
                expected_revision,
                destination,
                proposal_id,
                action,
                address,
                reason,
            } => {
                claim_id.check()?;
                expected_revision.check()?;
                destination.check()?;
                proposal_id.check()?;
                action.check()?;
                address.check()?;
                reason.check()?;
            }
            Self::DreamingRun {
                scope,
                expected_settings_revision,
                resume_run_id,
            } => {
                scope.check()?;
                expected_settings_revision.check()?;
                resume_run_id.check()?;
            }
            Self::DreamingSettings {
                scope,
                expected_revision,
                config,
            } => {
                scope.check()?;
                expected_revision.check()?;
                config.check()?;
            }
            Self::DreamingInstructions {
                scope,
                expected_revision,
                content,
            } => {
                scope.check()?;
                expected_revision.check()?;
                content.check()?;
            }
            Self::DreamingPropose {
                run_id,
                expected_revision,
                claim_ids,
                content,
            } => {
                run_id.check()?;
                expected_revision.check()?;
                claim_ids.check()?;
                content.check()?;
            }
            Self::DreamingRevert {
                run_id,
                expected_revision,
                proposal_id,
                expected_change_digest,
            } => {
                run_id.check()?;
                expected_revision.check()?;
                proposal_id.check()?;
                expected_change_digest.check()?;
            }
            Self::DreamingReview {
                proposal_id,
                expected_revision,
                decision,
                rationale,
            } => {
                proposal_id.check()?;
                expected_revision.check()?;
                decision.check()?;
                rationale.check()?;
            }
            Self::DocumentsImport {
                scope,
                source,
                title,
                replace_document_id,
                expected_revision,
            } => {
                scope.check()?;
                source.check()?;
                title.check()?;
                replace_document_id.check()?;
                expected_revision.check()?;
            }
            Self::DocumentsIndex {
                document_id,
                version_id,
                expected_revision,
            } => {
                document_id.check()?;
                version_id.check()?;
                expected_revision.check()?;
            }
            Self::DocumentsRetry {
                document_id,
                version_id,
                expected_revision,
            } => {
                document_id.check()?;
                version_id.check()?;
                expected_revision.check()?;
            }
            Self::ConventionsEdit {
                convention_id,
                expected_revision,
                content,
            } => {
                convention_id.check()?;
                expected_revision.check()?;
                content.check()?;
            }
            Self::ConventionsApplicability {
                convention_id,
                expected_revision,
                applicability,
            } => {
                convention_id.check()?;
                expected_revision.check()?;
                applicability.check()?;
            }
            Self::ConventionsRetire {
                convention_id,
                expected_revision,
                resource_id,
            } => {
                convention_id.check()?;
                expected_revision.check()?;
                resource_id.check()?;
            }
            Self::ConventionsIndex {
                convention_id,
                expected_revision,
                resource_id,
            } => {
                convention_id.check()?;
                expected_revision.check()?;
                resource_id.check()?;
            }
            Self::ConventionsDelete {
                convention_id,
                expected_revision,
                resource_id,
            } => {
                convention_id.check()?;
                expected_revision.check()?;
                resource_id.check()?;
            }
            Self::ConventionsDeleteResource {
                convention_id,
                expected_revision,
                resource_id,
            } => {
                convention_id.check()?;
                expected_revision.check()?;
                resource_id.check()?;
            }
            Self::ConventionsImport {
                scope,
                source,
                schema,
            } => {
                scope.check()?;
                source.check()?;
                schema.check()?;
            }
            Self::ConventionsMove {
                convention_id,
                expected_revision,
                destination,
            } => {
                convention_id.check()?;
                expected_revision.check()?;
                destination.check()?;
            }
            Self::SkillsDraft {
                scope,
                instruction_id,
                expected_revision,
                name,
                content,
            } => {
                scope.check()?;
                instruction_id.check()?;
                expected_revision.check()?;
                name.check()?;
                content.check()?;
            }
            Self::SkillsPropose {
                instruction_id,
                expected_revision,
                content_digest,
            } => {
                instruction_id.check()?;
                expected_revision.check()?;
                content_digest.check()?;
            }
            Self::SkillsRollback {
                instruction_id,
                expected_revision,
                target_revision,
                proposal_id,
            } => {
                instruction_id.check()?;
                expected_revision.check()?;
                target_revision.check()?;
                proposal_id.check()?;
            }
            Self::RolesDraft {
                scope,
                instruction_id,
                expected_revision,
                name,
                content,
            } => {
                scope.check()?;
                instruction_id.check()?;
                expected_revision.check()?;
                name.check()?;
                content.check()?;
            }
            Self::RolesPropose {
                instruction_id,
                expected_revision,
                content_digest,
            } => {
                instruction_id.check()?;
                expected_revision.check()?;
                content_digest.check()?;
            }
            Self::RolesRollback {
                instruction_id,
                expected_revision,
                target_revision,
                proposal_id,
            } => {
                instruction_id.check()?;
                expected_revision.check()?;
                target_revision.check()?;
                proposal_id.check()?;
            }
            Self::ApprovalsPropose {
                scope,
                operation_kind,
                target,
                content_digest,
                summary,
                artifact,
                payload,
            } => {
                scope.check()?;
                operation_kind.check()?;
                target.check()?;
                content_digest.check()?;
                summary.check()?;
                artifact.check()?;
                payload.check()?;
            }
            Self::ApprovalsRecordHumanDecision {
                proposal_id,
                expected_revision,
                content_digest,
                decision,
                rationale,
            } => {
                proposal_id.check()?;
                expected_revision.check()?;
                content_digest.check()?;
                decision.check()?;
                rationale.check()?;
            }
            Self::NotificationsAcknowledge {
                notification_id,
                expected_revision,
            } => {
                notification_id.check()?;
                expected_revision.check()?;
            }
            Self::NotificationsSettings {
                expected_revision,
                config,
            } => {
                expected_revision.check()?;
                config.check()?;
            }
            Self::GitWorktree {
                repository_id,
                work_id,
                expected_revision,
                action,
                base,
                registered_root_id,
                worktree_id,
            } => {
                repository_id.check()?;
                work_id.check()?;
                expected_revision.check()?;
                action.check()?;
                base.check()?;
                registered_root_id.check()?;
                worktree_id.check()?;
            }
            Self::ReviewsRequest {
                root_id,
                expected_revision,
                barrier,
                policy_revision,
            } => {
                root_id.check()?;
                expected_revision.check()?;
                barrier.check()?;
                policy_revision.check()?;
            }
            Self::ReviewsRespond {
                finding_id,
                expected_revision,
                response,
                evidence,
            } => {
                finding_id.check()?;
                expected_revision.check()?;
                response.check()?;
                evidence.check()?;
            }
            Self::ReviewsAdjudicate {
                finding_id,
                expected_revision,
                actionable,
                rationale,
                evidence,
            } => {
                finding_id.check()?;
                expected_revision.check()?;
                actionable.check()?;
                rationale.check()?;
                evidence.check()?;
            }
            Self::GithubAccount {
                action,
                connection_id,
                auth_flow_id,
            } => {
                action.check()?;
                connection_id.check()?;
                auth_flow_id.check()?;
            }
            Self::GithubIssueSync {
                repository_id,
                expected_cursor_revision,
                remote_issue_number,
            } => {
                repository_id.check()?;
                expected_cursor_revision.check()?;
                remote_issue_number.check()?;
            }
            Self::ResourcesAcquire {
                resource_id,
                expected_revision,
                units,
                lease_ms,
                purpose,
                priority,
                capacity,
                labels,
            } => {
                resource_id.check()?;
                expected_revision.check()?;
                units.check()?;
                lease_ms.check()?;
                purpose.check()?;
                priority.check()?;
                capacity.check()?;
                labels.check()?;
            }
            Self::ResourcesRenew {
                lease_id,
                expected_fence,
                lease_ms,
            } => {
                lease_id.check()?;
                expected_fence.check()?;
                lease_ms.check()?;
            }
            Self::ResourcesRelease {
                lease_id,
                expected_fence,
            } => {
                lease_id.check()?;
                expected_fence.check()?;
            }
            Self::BrowserSessions { action } => {
                action.check()?;
            }
            Self::BrowserNavigate {
                session_id,
                expected_revision,
                target_id,
                location,
            } => {
                session_id.check()?;
                expected_revision.check()?;
                target_id.check()?;
                location.check()?;
            }
            Self::BrowserInput {
                session_id,
                expected_revision,
                action,
            } => {
                session_id.check()?;
                expected_revision.check()?;
                action.check()?;
            }
            Self::BrowserUpload { node, files } => {
                node.check()?;
                files.check()?;
            }
            Self::DesktopRequestControl {
                desktop_id,
                expected_revision,
                reason,
                lease_ms,
            } => {
                desktop_id.check()?;
                expected_revision.check()?;
                reason.check()?;
                lease_ms.check()?;
            }
            Self::DesktopInput {
                desktop_id,
                lease_id,
                expected_fence,
                consent_id,
                expected_desktop_revision,
                action,
            } => {
                desktop_id.check()?;
                lease_id.check()?;
                expected_fence.check()?;
                consent_id.check()?;
                expected_desktop_revision.check()?;
                action.check()?;
            }
            Self::DesktopReleaseControl {
                desktop_id,
                lease_id,
                expected_fence,
            } => {
                desktop_id.check()?;
                lease_id.check()?;
                expected_fence.check()?;
            }
            Self::ProcessExecute { spec } => {
                spec.check()?;
            }
            Self::ProcessCancel {
                process_id,
                expected_revision,
            } => {
                process_id.check()?;
                expected_revision.check()?;
            }
            Self::ProcessInstallProjectDependencies {
                repository_id,
                expected_revision,
                manifest,
                profile,
            } => {
                repository_id.check()?;
                expected_revision.check()?;
                manifest.check()?;
                profile.check()?;
            }
            Self::ProcessRequestSystemInstall {
                package_spec,
                content_digest,
                reason,
            } => {
                package_spec.check()?;
                content_digest.check()?;
                reason.check()?;
            }
            Self::ContainersBuild {
                engine_id,
                project_id,
                context,
                definition,
                tag,
            } => {
                engine_id.check()?;
                project_id.check()?;
                context.check()?;
                definition.check()?;
                tag.check()?;
            }
            Self::ContainersPull {
                engine_id,
                image,
                credential_ref,
            } => {
                engine_id.check()?;
                image.check()?;
                credential_ref.check()?;
            }
            Self::ContainersCreate { spec } => {
                spec.check()?;
            }
            Self::ContainersStart {
                container_id,
                expected_revision,
            } => {
                container_id.check()?;
                expected_revision.check()?;
            }
            Self::ContainersStop {
                container_id,
                expected_revision,
            } => {
                container_id.check()?;
                expected_revision.check()?;
            }
            Self::ContainersExec {
                container_id,
                expected_revision,
                command,
                environment,
                timeout_ms,
            } => {
                container_id.check()?;
                expected_revision.check()?;
                command.check()?;
                environment.check()?;
                timeout_ms.check()?;
            }
            Self::JobsCreate {
                scope,
                title,
                schedule,
                definition,
                enabled,
            } => {
                scope.check()?;
                title.check()?;
                schedule.check()?;
                definition.check()?;
                enabled.check()?;
            }
            Self::JobsEdit {
                job_id,
                expected_revision,
                title,
                schedule,
                definition,
            } => {
                job_id.check()?;
                expected_revision.check()?;
                title.check()?;
                schedule.check()?;
                definition.check()?;
            }
            Self::JobsEnable {
                job_id,
                expected_revision,
            } => {
                job_id.check()?;
                expected_revision.check()?;
            }
            Self::JobsDisable {
                job_id,
                expected_revision,
            } => {
                job_id.check()?;
                expected_revision.check()?;
            }
            Self::JobsDelete {
                job_id,
                expected_revision,
            } => {
                job_id.check()?;
                expected_revision.check()?;
            }
            Self::JobsRunNow {
                job_id,
                expected_revision,
            } => {
                job_id.check()?;
                expected_revision.check()?;
            }
            Self::JobsCancel {
                job_id,
                run_id,
                expected_revision,
            } => {
                job_id.check()?;
                run_id.check()?;
                expected_revision.check()?;
            }
            Self::SettingsSaveRevision {
                scope,
                expected_revision,
                entries,
            } => {
                scope.check()?;
                expected_revision.check()?;
                entries.check()?;
            }
            Self::RouterRoute {
                session_id,
                expected_context_epoch,
                intent,
                effort,
                required_capabilities,
            } => {
                session_id.check()?;
                expected_context_epoch.check()?;
                intent.check()?;
                effort.check()?;
                required_capabilities.check()?;
            }
            Self::RouterPin {
                session_id,
                expected_pin_revision,
                intent,
                effort,
            } => {
                session_id.check()?;
                expected_pin_revision.check()?;
                intent.check()?;
                effort.check()?;
            }
            Self::RouterPriceRefresh {
                provider_ids,
                expected_price_revision,
            } => {
                provider_ids.check()?;
                expected_price_revision.check()?;
            }
            Self::RouterPriceOverride {
                model_id,
                expected_revision,
                price,
                change,
            } => {
                model_id.check()?;
                expected_revision.check()?;
                price.check()?;
                change.check()?;
            }
            Self::RouterConfigure { action } => {
                action.check()?;
            }
            Self::RouterDecide {
                model_id,
                expected_model_revision,
                state,
                questions,
                images,
                videos,
                timeout_ms,
            } => {
                model_id.check()?;
                expected_model_revision.check()?;
                state.check()?;
                questions.check()?;
                images.check()?;
                videos.check()?;
                timeout_ms.check()?;
            }
            Self::McpConfigure {
                scope,
                server_id,
                expected_revision,
                name,
                transport,
            } => {
                scope.check()?;
                server_id.check()?;
                expected_revision.check()?;
                name.check()?;
                transport.check()?;
            }
            Self::McpConnect {
                server_id,
                expected_revision,
            } => {
                server_id.check()?;
                expected_revision.check()?;
            }
            Self::McpDiscover {
                server_id,
                expected_catalog_revision,
            } => {
                server_id.check()?;
                expected_catalog_revision.check()?;
            }
            Self::McpSelectTools {
                server_id,
                expected_revision,
                expected_catalog_revision,
                tool_ids,
            } => {
                server_id.check()?;
                expected_revision.check()?;
                expected_catalog_revision.check()?;
                tool_ids.check()?;
            }
            Self::AddonsConfigure {
                package_id,
                expected_revision,
                config,
            } => {
                package_id.check()?;
                expected_revision.check()?;
                config.check()?;
            }
            Self::AddonsPropose {
                source,
                manifest_digest,
                base_package_id,
                expected_revision,
            } => {
                source.check()?;
                manifest_digest.check()?;
                base_package_id.check()?;
                expected_revision.check()?;
            }
            Self::HelpGenerateCapabilityDocs {
                registry_revision,
                expected_help_revision,
            } => {
                registry_revision.check()?;
                expected_help_revision.check()?;
            }
            Self::WorkspaceRecordDiagnostic {
                component,
                code,
                severity,
                message,
                evidence,
                operation,
                operation_id,
                target_kind,
                target_id,
                stage,
                source_revision,
                attempt,
                started_at_ms,
                finished_at_ms,
                progress_completed,
                progress_total,
                outcome,
                error_class,
                output,
            } => {
                component.check()?;
                code.check()?;
                severity.check()?;
                message.check()?;
                evidence.check()?;
                operation.check()?;
                operation_id.check()?;
                target_kind.check()?;
                target_id.check()?;
                stage.check()?;
                source_revision.check()?;
                attempt.check()?;
                started_at_ms.check()?;
                finished_at_ms.check()?;
                progress_completed.check()?;
                progress_total.check()?;
                outcome.check()?;
                error_class.check()?;
                output.check()?;
            }
            Self::WorkspaceBackup {
                destination_grant_id,
                expected_store_sequence,
            } => {
                destination_grant_id.check()?;
                expected_store_sequence.check()?;
            }
            Self::WorkspaceRestore {
                backup,
                expected_store_id,
                expected_restore_epoch,
                proposal_id,
            } => {
                backup.check()?;
                expected_store_id.check()?;
                expected_restore_epoch.check()?;
                proposal_id.check()?;
            }
            Self::WorkspaceMigrate {
                migration_manifest,
                expected_schema_revision,
                proposal_id,
            } => {
                migration_manifest.check()?;
                expected_schema_revision.check()?;
                proposal_id.check()?;
            }
            Self::WorkspaceApproveInstall {
                update_id,
                expected_revision,
                binary_digest,
                proposal_id,
            } => {
                update_id.check()?;
                expected_revision.check()?;
                binary_digest.check()?;
                proposal_id.check()?;
            }
            Self::RepositoriesRemoveIndex {
                repository_id,
                expected_revision,
            } => {
                repository_id.check()?;
                expected_revision.check()?;
            }
            Self::QueueResolve {
                delivery_id,
                expected_revision,
                expected_fence,
                resolution,
                reason,
            } => {
                delivery_id.check()?;
                expected_revision.check()?;
                expected_fence.check()?;
                resolution.check()?;
                reason.check()?;
            }
            Self::QueueRetry {
                delivery_id,
                expected_revision,
                expected_fence,
                resolution,
                reason,
            } => {
                delivery_id.check()?;
                expected_revision.check()?;
                expected_fence.check()?;
                resolution.check()?;
                reason.check()?;
            }
            Self::QueueReconcile {
                delivery_id,
                expected_revision,
                expected_fence,
                resolution,
                reason,
            } => {
                delivery_id.check()?;
                expected_revision.check()?;
                expected_fence.check()?;
                resolution.check()?;
                reason.check()?;
            }
            Self::ImportsConfigure {
                config_id,
                expected_revision,
                config,
            } => {
                config_id.check()?;
                expected_revision.check()?;
                config.check()?;
            }
            Self::ImportsApply {
                config_id,
                expected_revision,
                source_snapshot,
                preview_digest,
            } => {
                config_id.check()?;
                expected_revision.check()?;
                source_snapshot.check()?;
                preview_digest.check()?;
            }
            Self::MaintenancePruneDiagnostics {
                preview_only,
                preview_id,
                expected_revision,
                older_than_ms,
                max_items,
            } => {
                preview_only.check()?;
                preview_id.check()?;
                expected_revision.check()?;
                older_than_ms.check()?;
                max_items.check()?;
            }
            Self::MaintenancePruneIndexRuns {
                preview_only,
                preview_id,
                expected_revision,
                older_than_ms,
                max_items,
            } => {
                preview_only.check()?;
                preview_id.check()?;
                expected_revision.check()?;
                older_than_ms.check()?;
                max_items.check()?;
            }
            Self::MaintenancePruneMessages {
                preview_only,
                preview_id,
                expected_revision,
                older_than_ms,
                max_items,
            } => {
                preview_only.check()?;
                preview_id.check()?;
                expected_revision.check()?;
                older_than_ms.check()?;
                max_items.check()?;
            }
            Self::MaintenancePruneSessions {
                preview_only,
                preview_id,
                expected_revision,
                older_than_ms,
                max_items,
            } => {
                preview_only.check()?;
                preview_id.check()?;
                expected_revision.check()?;
                older_than_ms.check()?;
                max_items.check()?;
            }
            Self::RepositoryDocsGenerate {
                repository_id,
                views,
                expected_revision,
                output_root_id,
                topics,
            } => {
                repository_id.check()?;
                views.check()?;
                expected_revision.check()?;
                output_root_id.check()?;
                topics.check()?;
            }
            Self::McpServerConfigure {
                expected_revision,
                enabled,
                bindings,
                http_port,
            } => {
                expected_revision.check()?;
                enabled.check()?;
                bindings.check()?;
                http_port.check()?;
            }
            Self::McpServerSetClientAccess { action } => {
                action.check()?;
            }
            Self::TasksPrioritize {
                project_id,
                feature_id,
                expected_queue_revision,
                priority,
                ordered_task_ids,
                placement,
            } => {
                project_id.check()?;
                feature_id.check()?;
                expected_queue_revision.check()?;
                priority.check()?;
                ordered_task_ids.check()?;
                placement.check()?;
            }
            Self::TasksSetFeature {
                task_id,
                expected_revision,
                feature_id,
                clear_feature,
            } => {
                task_id.check()?;
                expected_revision.check()?;
                feature_id.check()?;
                clear_feature.check()?;
            }
            Self::ArtifactsTag {
                artifact_id,
                expected_revision,
                tags,
            } => {
                artifact_id.check()?;
                expected_revision.check()?;
                tags.check()?;
            }
            Self::EvalsGrade { session_id, suite } => {
                session_id.check()?;
                suite.check()?;
            }
            Self::NotificationsMarkRead {
                notification_id,
                expected_revision,
                read,
            } => {
                notification_id.check()?;
                expected_revision.check()?;
                read.check()?;
            }
        }
        super::domain_rules::input(
            self.operation(),
            "Command",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )
    }
}

impl Revisioned for DomainCommand {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::ProjectsCreate { initial_plan, .. } => {
                initial_plan.retain_revision(revision);
            }
            Self::FeaturesCreate { creation, .. } => {
                creation.retain_revision(revision);
            }
            Self::FeaturesEdit { changes, .. } => {
                changes.retain_revision(revision);
            }
            Self::IssuesCreate {
                task_type, labels, ..
            } => {
                if revision < 3 {
                    *task_type = None;
                }
                if revision < 3 {
                    *labels = None;
                }
            }
            Self::IssuesEdit { changes, .. } => {
                changes.retain_revision(revision);
            }
            Self::TasksCreate {
                task_type, labels, ..
            } => {
                if revision < 3 {
                    *task_type = None;
                }
                if revision < 3 {
                    *labels = None;
                }
            }
            Self::TasksEdit { changes, .. } => {
                changes.retain_revision(revision);
            }
            Self::FeaturesPrioritize { placement, .. } => {
                if revision < 2 {
                    *placement = None;
                }
            }
            Self::IssuesPrioritize { placement, .. } => {
                if revision < 2 {
                    *placement = None;
                }
            }
            Self::FeaturesEditPlan {
                budget,
                clear_budget_proposal,
                ..
            } => {
                if revision < 3 {
                    *budget = None;
                }
                if revision < 3 {
                    *clear_budget_proposal = None;
                }
            }
            Self::TasksSyncPlan {
                tasks,
                project_plan,
                ..
            } => {
                tasks.retain_revision(revision);
                project_plan.retain_revision(revision);
            }
            Self::NextWorkClaimProjectQueue {
                repository_id,
                task_type,
                label,
                another,
                idempotency_key,
                ..
            } => {
                if revision < 3 {
                    *repository_id = None;
                }
                if revision < 3 {
                    *task_type = None;
                }
                if revision < 3 {
                    *label = None;
                }
                if revision < 3 {
                    *another = None;
                }
                if revision < 3 {
                    *idempotency_key = None;
                }
            }
            Self::NextWorkClaimFeatureTask {
                repository_id,
                task_type,
                label,
                another,
                idempotency_key,
                ..
            } => {
                if revision < 3 {
                    *repository_id = None;
                }
                if revision < 3 {
                    *task_type = None;
                }
                if revision < 3 {
                    *label = None;
                }
                if revision < 3 {
                    *another = None;
                }
                if revision < 3 {
                    *idempotency_key = None;
                }
            }
            Self::KnowledgeRecord { batch, .. } => {
                batch.retain_revision(revision);
            }
            Self::KnowledgeRevise { replacement, .. } => {
                replacement.retain_revision(revision);
            }
            Self::KnowledgeVerify {
                source_revision, ..
            } => {
                if revision < 2 {
                    *source_revision = None;
                }
            }
            Self::KnowledgeChallenge {
                resolve_challenge_id,
                outcome,
                ..
            } => {
                if revision < 2 {
                    *resolve_challenge_id = None;
                }
                if revision < 2 {
                    *outcome = None;
                }
            }
            Self::KnowledgeDeleteBySource { source_label, .. } => {
                if revision < 3 {
                    *source_label = None;
                }
            }
            Self::KnowledgePromote {
                action,
                address,
                reason,
                ..
            } => {
                if revision < 2 {
                    *action = None;
                }
                if revision < 2 {
                    *address = None;
                }
                if revision < 2 {
                    *reason = None;
                }
            }
            Self::ApprovalsPropose { payload, .. } => {
                if revision < 3 {
                    *payload = None;
                }
            }
            Self::ResourcesAcquire {
                purpose,
                priority,
                capacity,
                labels,
                ..
            } => {
                if revision < 2 {
                    *purpose = None;
                }
                if revision < 2 {
                    *priority = None;
                }
                if revision < 2 {
                    *capacity = None;
                }
                if revision < 2 {
                    *labels = None;
                }
            }
            Self::RouterPriceOverride { change, .. } => {
                if revision < 3 {
                    *change = None;
                }
            }
            Self::RouterConfigure { action, .. } => {
                action.retain_revision(revision);
            }
            Self::WorkspaceRecordDiagnostic {
                operation,
                operation_id,
                target_kind,
                target_id,
                stage,
                source_revision,
                attempt,
                started_at_ms,
                finished_at_ms,
                progress_completed,
                progress_total,
                outcome,
                error_class,
                output,
                ..
            } => {
                if revision < 2 {
                    *operation = None;
                }
                if revision < 2 {
                    *operation_id = None;
                }
                if revision < 2 {
                    *target_kind = None;
                }
                if revision < 2 {
                    *target_id = None;
                }
                if revision < 2 {
                    *stage = None;
                }
                if revision < 2 {
                    *source_revision = None;
                }
                if revision < 2 {
                    *attempt = None;
                }
                if revision < 2 {
                    *started_at_ms = None;
                }
                if revision < 2 {
                    *finished_at_ms = None;
                }
                if revision < 2 {
                    *progress_completed = None;
                }
                if revision < 2 {
                    *progress_total = None;
                }
                if revision < 2 {
                    *outcome = None;
                }
                if revision < 2 {
                    *error_class = None;
                }
                if revision < 2 {
                    *output = None;
                }
            }
            Self::TasksPrioritize { placement, .. } => {
                if revision < 2 {
                    *placement = None;
                }
            }
            _ => {}
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::NotificationsMarkRead { .. } if revision < 3 => {
                Err("operation predates its introduction".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DomainRequest {
    Query {
        protocol: Protocol,
        query_id: QueryId,
        body: DomainQuery,
    },
    Command {
        protocol: Protocol,
        request_key: RequestKey,
        body: DomainCommand,
    },
}
impl DomainRequest {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Query { body, .. } => body.check(),
            Self::Command { body, .. } => body.check(),
        }
    }
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Query { body, .. } => body.operation(),
            Self::Command { body, .. } => body.operation(),
        }
    }
}
impl Revisioned for DomainRequest {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::Query { body, .. } => body.retain_revision(revision),
            Self::Command { body, .. } => body.retain_revision(revision),
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::Query { body, .. } => body.check_revision(revision),
            Self::Command { body, .. } => body.check_revision(revision),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum OperationResult {
    #[serde(rename = "projects.list")]
    ProjectsList { value: Page<ProjectRecord> },
    #[serde(rename = "projects.get")]
    ProjectsGet { value: ProjectRecord },
    #[serde(rename = "projects.create")]
    ProjectsCreate { value: ProjectRecord },
    #[serde(rename = "projects.edit")]
    ProjectsEdit { value: ProjectRecord },
    #[serde(rename = "projects.membership")]
    ProjectsMembership { value: ProjectRecord },
    #[serde(rename = "projects.archive")]
    ProjectsArchive { value: ProjectRecord },
    #[serde(rename = "projects.restore")]
    ProjectsRestore { value: ProjectRecord },
    #[serde(rename = "projects.delete")]
    ProjectsDelete { value: ProjectRecord },
    #[serde(rename = "features.list")]
    FeaturesList { value: Page<WorkRecord> },
    #[serde(rename = "features.get")]
    FeaturesGet { value: WorkRecord },
    #[serde(rename = "features.create")]
    FeaturesCreate { value: Page<WorkRecord> },
    #[serde(rename = "features.edit")]
    FeaturesEdit { value: WorkRecord },
    #[serde(rename = "issues.list")]
    IssuesList { value: Page<WorkRecord> },
    #[serde(rename = "issues.get")]
    IssuesGet { value: WorkRecord },
    #[serde(rename = "issues.create")]
    IssuesCreate { value: WorkRecord },
    #[serde(rename = "issues.edit")]
    IssuesEdit { value: WorkRecord },
    #[serde(rename = "tasks.list")]
    TasksList { value: Page<WorkRecord> },
    #[serde(rename = "tasks.get")]
    TasksGet { value: WorkRecord },
    #[serde(rename = "tasks.create")]
    TasksCreate { value: WorkRecord },
    #[serde(rename = "tasks.edit")]
    TasksEdit { value: WorkRecord },
    #[serde(rename = "features.prioritize")]
    FeaturesPrioritize { value: MutationSummary },
    #[serde(rename = "issues.prioritize")]
    IssuesPrioritize { value: MutationSummary },
    #[serde(rename = "features.edit_plan")]
    FeaturesEditPlan { value: WorkRecord },
    #[serde(rename = "features.dependencies")]
    FeaturesDependencies { value: WorkRecord },
    #[serde(rename = "features.request_plan_approval")]
    FeaturesRequestPlanApproval { value: ProposalRecord },
    #[serde(rename = "issues.publish")]
    IssuesPublish { value: GitLink },
    #[serde(rename = "issues.link")]
    IssuesLink { value: GitLink },
    #[serde(rename = "tasks.move")]
    TasksMove { value: WorkRecord },
    #[serde(rename = "tasks.assign")]
    TasksAssign { value: WorkRecord },
    #[serde(rename = "tasks.dependency_links")]
    TasksDependencyLinks { value: WorkRecord },
    #[serde(rename = "tasks.acceptance_evidence")]
    TasksAcceptanceEvidence { value: WorkRecord },
    #[serde(rename = "tasks.sync_plan")]
    TasksSyncPlan { value: Page<WorkRecord> },
    #[serde(rename = "workflows.list")]
    WorkflowsList { value: Page<StatusRecord> },
    #[serde(rename = "workflows.create_status")]
    WorkflowsCreateStatus { value: StatusRecord },
    #[serde(rename = "workflows.rename_status")]
    WorkflowsRenameStatus { value: StatusRecord },
    #[serde(rename = "workflows.order")]
    WorkflowsOrder { value: Page<StatusRecord> },
    #[serde(rename = "workflows.reassign_and_remove")]
    WorkflowsReassignAndRemove { value: MutationSummary },
    #[serde(rename = "next_work.claim_project_queue")]
    NextWorkClaimProjectQueue { value: AllocationSelection },
    #[serde(rename = "next_work.claim_feature_task")]
    NextWorkClaimFeatureTask { value: AllocationSelection },
    #[serde(rename = "next_work.inspect_claim")]
    NextWorkInspectClaim { value: AllocationClaimRecord },
    #[serde(rename = "next_work.release_claim")]
    NextWorkReleaseClaim { value: AllocationClaimRecord },
    #[serde(rename = "runs.redirect")]
    RunsRedirect { value: MutationSummary },
    #[serde(rename = "runs.stop")]
    RunsStop { value: OperationRef },
    #[serde(rename = "agents.inspect")]
    AgentsInspect { value: AgentRecord },
    #[serde(rename = "agents.delegate")]
    AgentsDelegate { value: AgentRecord },
    #[serde(rename = "agents.messages")]
    AgentsMessages { value: AgentMessage },
    #[serde(rename = "agents.inbox")]
    AgentsInbox { value: Page<AgentMessage> },
    #[serde(rename = "agents.acknowledge")]
    AgentsAcknowledge { value: AgentMessage },
    #[serde(rename = "agents.checkpoint")]
    AgentsCheckpoint { value: CheckpointRecord },
    #[serde(rename = "sessions.list")]
    SessionsList { value: Page<SessionRecord> },
    #[serde(rename = "sessions.get")]
    SessionsGet { value: SessionRecord },
    #[serde(rename = "sessions.briefing")]
    SessionsBriefing { value: Page<ContextSource> },
    #[serde(rename = "sessions.context_sources")]
    SessionsContextSources { value: Page<ContextSource> },
    #[serde(rename = "sessions.start")]
    SessionsStart { value: SessionRecord },
    #[serde(rename = "sessions.end")]
    SessionsEnd { value: SessionRecord },
    #[serde(rename = "ui.current_screen")]
    UiCurrentScreen { value: ScreenContext },
    #[serde(rename = "history.work_lineage")]
    HistoryWorkLineage { value: HistoryPage<LineageRecord> },
    #[serde(rename = "history.git_links")]
    HistoryGitLinks {
        value: HistoryPage<AttributedGitLink>,
    },
    #[serde(rename = "history.evidence_links")]
    HistoryEvidenceLinks { value: HistoryPage<EvidenceLink> },
    #[serde(rename = "history.tool_usage")]
    HistoryToolUsage { value: UsagePage },
    #[serde(rename = "indexing.discover")]
    IndexingDiscover { value: Page<RepositoryRecord> },
    #[serde(rename = "indexing.index")]
    IndexingIndex { value: IndexRunRecord },
    #[serde(rename = "indexing.reindex")]
    IndexingReindex { value: IndexRunRecord },
    #[serde(rename = "indexing.retire_view")]
    IndexingRetireView { value: MutationSummary },
    #[serde(rename = "indexing.status")]
    IndexingStatus { value: RepositoryIndexStatus },
    #[serde(rename = "indexing.runs")]
    IndexingRuns { value: Page<IndexRunRecord> },
    #[serde(rename = "indexing.configure_sources")]
    IndexingConfigureSources { value: MutationSummary },
    #[serde(rename = "analysis.health")]
    AnalysisHealth { value: AnalysisRecord },
    #[serde(rename = "analysis.cross_repo_links")]
    AnalysisCrossRepoLinks { value: Page<MetricRecord> },
    #[serde(rename = "analysis.status")]
    AnalysisStatus { value: AnalysisRecord },
    #[serde(rename = "analysis.clusters")]
    AnalysisClusters { value: Page<ClusterRecord> },
    #[serde(rename = "analysis.cluster_detail")]
    AnalysisClusterDetail { value: ClusterRecord },
    #[serde(rename = "analysis.security")]
    AnalysisSecurity { value: AnalysisRecord },
    #[serde(rename = "analysis.metrics")]
    AnalysisMetrics { value: Page<MetricRecord> },
    #[serde(rename = "analysis.hotspots")]
    AnalysisHotspots { value: Page<MetricRecord> },
    #[serde(rename = "analysis.fleet_health")]
    AnalysisFleetHealth { value: Page<MetricRecord> },
    #[serde(rename = "analysis.archival_candidates")]
    AnalysisArchivalCandidates { value: Page<MetricRecord> },
    #[serde(rename = "analysis.file_hotspot")]
    AnalysisFileHotspot { value: Page<MetricRecord> },
    #[serde(rename = "analysis.community_analysis")]
    AnalysisCommunityAnalysis { value: AnalysisRecord },
    #[serde(rename = "analysis.link_and_detect")]
    AnalysisLinkAndDetect { value: AnalysisRecord },
    #[serde(rename = "analysis.process_batch")]
    AnalysisProcessBatch { value: AnalysisRecord },
    #[serde(rename = "analysis.reanalyze")]
    AnalysisReanalyze { value: AnalysisRecord },
    #[serde(rename = "analysis.synthesize")]
    AnalysisSynthesize { value: AnalysisRecord },
    #[serde(rename = "schemas.connections")]
    SchemasConnections { value: Page<SchemaConnection> },
    #[serde(rename = "schemas.objects")]
    SchemasObjects { value: Page<SchemaObject> },
    #[serde(rename = "schemas.relationships")]
    SchemasRelationships { value: Page<SchemaRelation> },
    #[serde(rename = "schemas.index")]
    SchemasIndex { value: IndexRunRecord },
    #[serde(rename = "schemas.refresh")]
    SchemasRefresh { value: IndexRunRecord },
    #[serde(rename = "schemas.remove_index")]
    SchemasRemoveIndex { value: MutationSummary },
    #[serde(rename = "schemas.configure")]
    SchemasConfigure { value: SchemaConnection },
    #[serde(rename = "database.read")]
    DatabaseRead { value: DbReadResult },
    #[serde(rename = "database.execute_write")]
    DatabaseExecuteWrite { value: DbWriteResult },
    #[serde(rename = "database.direct_schema_change")]
    DatabaseDirectSchemaChange { value: DbWriteResult },
    #[serde(rename = "database.apply_migration")]
    DatabaseApplyMigration { value: DbWriteResult },
    #[serde(rename = "database.configure_sensitive_columns")]
    DatabaseConfigureSensitiveColumns { value: SchemaConnection },
    #[serde(rename = "knowledge.search")]
    KnowledgeSearch { value: Page<KnowledgeClaim> },
    #[serde(rename = "knowledge.read")]
    KnowledgeRead { value: KnowledgeBundle },
    #[serde(rename = "knowledge.record")]
    KnowledgeRecord { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.revise")]
    KnowledgeRevise { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.relate")]
    KnowledgeRelate { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.tag")]
    KnowledgeTag { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.verify")]
    KnowledgeVerify { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.retract")]
    KnowledgeRetract { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.challenge")]
    KnowledgeChallenge { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.scope")]
    KnowledgeScope { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.diagnostics")]
    KnowledgeDiagnostics { value: Page<Diagnostic> },
    #[serde(rename = "knowledge.delete_by_source")]
    KnowledgeDeleteBySource { value: MutationSummary },
    #[serde(rename = "knowledge.delete_selected")]
    KnowledgeDeleteSelected { value: MutationSummary },
    #[serde(rename = "knowledge.write_status")]
    KnowledgeWriteStatus { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.subgraph")]
    KnowledgeSubgraph { value: KnowledgeBundle },
    #[serde(rename = "knowledge.entity_bundle")]
    KnowledgeEntityBundle { value: KnowledgeBundle },
    #[serde(rename = "knowledge.entity_graph")]
    KnowledgeEntityGraph { value: KnowledgeBundle },
    #[serde(rename = "knowledge.claim_bundle")]
    KnowledgeClaimBundle { value: KnowledgeBundle },
    #[serde(rename = "knowledge.expand_frontier")]
    KnowledgeExpandFrontier { value: KnowledgeBundle },
    #[serde(rename = "knowledge.query")]
    KnowledgeQuery { value: KnowledgeBundle },
    #[serde(rename = "knowledge.render_summary")]
    KnowledgeRenderSummary { value: ArtifactRef },
    #[serde(rename = "knowledge.import_user_data")]
    KnowledgeImportUserData { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.promote")]
    KnowledgePromote { value: KnowledgeWriteReceipt },
    #[serde(rename = "knowledge.tagged_lessons")]
    KnowledgeTaggedLessons { value: Page<KnowledgeClaim> },
    #[serde(rename = "dreaming.run")]
    DreamingRun { value: DreamRun },
    #[serde(rename = "dreaming.runs")]
    DreamingRuns { value: Page<DreamRun> },
    #[serde(rename = "dreaming.report")]
    DreamingReport { value: DreamRun },
    #[serde(rename = "dreaming.settings")]
    DreamingSettings { value: DreamSettingsRecord },
    #[serde(rename = "dreaming.instructions")]
    DreamingInstructions { value: Page<InstructionRecord> },
    #[serde(rename = "dreaming.propose")]
    DreamingPropose { value: ProposalRecord },
    #[serde(rename = "dreaming.revert")]
    DreamingRevert { value: KnowledgeWriteReceipt },
    #[serde(rename = "dreaming.review")]
    DreamingReview { value: ProposalRecord },
    #[serde(rename = "documents.import")]
    DocumentsImport { value: DocumentRecord },
    #[serde(rename = "documents.list")]
    DocumentsList { value: Page<DocumentRecord> },
    #[serde(rename = "documents.index")]
    DocumentsIndex { value: IndexRunRecord },
    #[serde(rename = "documents.retry")]
    DocumentsRetry { value: IndexRunRecord },
    #[serde(rename = "conventions.list")]
    ConventionsList { value: Page<InstructionRecord> },
    #[serde(rename = "conventions.search")]
    ConventionsSearch { value: Page<InstructionRecord> },
    #[serde(rename = "conventions.resources")]
    ConventionsResources { value: Page<InstructionRecord> },
    #[serde(rename = "conventions.read")]
    ConventionsRead { value: InstructionRecord },
    #[serde(rename = "conventions.version")]
    ConventionsVersion { value: Page<InstructionRecord> },
    #[serde(rename = "conventions.edit")]
    ConventionsEdit { value: InstructionRecord },
    #[serde(rename = "conventions.applicability")]
    ConventionsApplicability { value: InstructionRecord },
    #[serde(rename = "conventions.retire")]
    ConventionsRetire { value: MutationSummary },
    #[serde(rename = "conventions.index")]
    ConventionsIndex { value: MutationSummary },
    #[serde(rename = "conventions.delete")]
    ConventionsDelete { value: MutationSummary },
    #[serde(rename = "conventions.delete_resource")]
    ConventionsDeleteResource { value: MutationSummary },
    #[serde(rename = "conventions.import")]
    ConventionsImport { value: InstructionRecord },
    #[serde(rename = "conventions.move")]
    ConventionsMove { value: InstructionRecord },
    #[serde(rename = "skills.read")]
    SkillsRead { value: InstructionRecord },
    #[serde(rename = "skills.draft")]
    SkillsDraft { value: InstructionRecord },
    #[serde(rename = "skills.propose")]
    SkillsPropose { value: ProposalRecord },
    #[serde(rename = "skills.history")]
    SkillsHistory { value: Page<InstructionRecord> },
    #[serde(rename = "skills.rollback")]
    SkillsRollback { value: InstructionRecord },
    #[serde(rename = "roles.read")]
    RolesRead { value: InstructionRecord },
    #[serde(rename = "roles.draft")]
    RolesDraft { value: InstructionRecord },
    #[serde(rename = "roles.propose")]
    RolesPropose { value: ProposalRecord },
    #[serde(rename = "roles.history")]
    RolesHistory { value: Page<InstructionRecord> },
    #[serde(rename = "roles.rollback")]
    RolesRollback { value: InstructionRecord },
    #[serde(rename = "skills.discover")]
    SkillsDiscover { value: Page<InstructionRecord> },
    #[serde(rename = "skills.resources")]
    SkillsResources { value: Page<ArtifactRef> },
    #[serde(rename = "approvals.list")]
    ApprovalsList { value: Page<ApprovalViewRecord> },
    #[serde(rename = "approvals.inspect")]
    ApprovalsInspect { value: ApprovalViewRecord },
    #[serde(rename = "approvals.propose")]
    ApprovalsPropose { value: ApprovalViewRecord },
    #[serde(rename = "approvals.record_human_decision")]
    ApprovalsRecordHumanDecision { value: ApprovalViewRecord },
    #[serde(rename = "notifications.list")]
    NotificationsList { value: Page<NotificationRecord> },
    #[serde(rename = "notifications.read")]
    NotificationsRead { value: NotificationRecord },
    #[serde(rename = "notifications.acknowledge")]
    NotificationsAcknowledge { value: NotificationRecord },
    #[serde(rename = "notifications.settings")]
    NotificationsSettings { value: NotificationSettingsRecord },
    #[serde(rename = "git.worktree")]
    GitWorktree { value: GitInspection },
    #[serde(rename = "git.inspect")]
    GitInspect { value: GitInspection },
    #[serde(rename = "git.diff")]
    GitDiff { value: Page<ArtifactRef> },
    #[serde(rename = "reviews.list")]
    ReviewsList { value: Page<ReviewRecord> },
    #[serde(rename = "reviews.inspect")]
    ReviewsInspect { value: ReviewRecord },
    #[serde(rename = "reviews.request")]
    ReviewsRequest { value: ReviewRecord },
    #[serde(rename = "reviews.findings")]
    ReviewsFindings { value: Page<ReviewFinding> },
    #[serde(rename = "reviews.respond")]
    ReviewsRespond { value: ReviewFinding },
    #[serde(rename = "reviews.adjudicate")]
    ReviewsAdjudicate { value: ReviewFinding },
    #[serde(rename = "reviews.readiness")]
    ReviewsReadiness { value: ReviewReadiness },
    #[serde(rename = "releases.inspect")]
    ReleasesInspect { value: ReleaseRecord },
    #[serde(rename = "github.account")]
    GithubAccount { value: GithubAccountState },
    #[serde(rename = "github.repositories")]
    GithubRepositories { value: Page<RepositoryRecord> },
    #[serde(rename = "github.poll_status")]
    GithubPollStatus { value: Page<GithubObservation> },
    #[serde(rename = "github.issue_sync")]
    GithubIssueSync { value: OperationRef },
    #[serde(rename = "github.checks")]
    GithubChecks { value: Page<GithubObservation> },
    #[serde(rename = "github.reviews")]
    GithubReviews { value: Page<GithubObservation> },
    #[serde(rename = "resources.list")]
    ResourcesList { value: Page<ResourceRecord> },
    #[serde(rename = "resources.acquire")]
    ResourcesAcquire { value: LeaseRecord },
    #[serde(rename = "resources.renew")]
    ResourcesRenew { value: LeaseRecord },
    #[serde(rename = "resources.release")]
    ResourcesRelease { value: LeaseRecord },
    #[serde(rename = "resources.holders")]
    ResourcesHolders { value: Page<LeaseRecord> },
    #[serde(rename = "resources.waiters")]
    ResourcesWaiters { value: Page<LeaseRecord> },
    #[serde(rename = "browser.sessions")]
    BrowserSessions { value: Page<BrowserSession> },
    #[serde(rename = "browser.inspect")]
    BrowserInspect { value: BrowserSnapshot },
    #[serde(rename = "browser.navigate")]
    BrowserNavigate { value: BrowserSnapshot },
    #[serde(rename = "browser.input")]
    BrowserInput { value: BrowserObservation },
    #[serde(rename = "browser.upload")]
    BrowserUpload { value: BrowserObservation },
    #[serde(rename = "browser.screenshot")]
    BrowserScreenshot { value: ArtifactRef },
    #[serde(rename = "browser.console")]
    BrowserConsole { value: Page<BrowserObservation> },
    #[serde(rename = "browser.network")]
    BrowserNetwork { value: Page<BrowserObservation> },
    #[serde(rename = "desktop.inspect")]
    DesktopInspect { value: DesktopRecord },
    #[serde(rename = "desktop.capture")]
    DesktopCapture { value: DesktopCapture },
    #[serde(rename = "desktop.request_control")]
    DesktopRequestControl { value: DesktopRecord },
    #[serde(rename = "desktop.input")]
    DesktopInput { value: DesktopRecord },
    #[serde(rename = "desktop.release_control")]
    DesktopReleaseControl { value: DesktopRecord },
    #[serde(rename = "process.execute")]
    ProcessExecute { value: ProcessRecord },
    #[serde(rename = "process.output")]
    ProcessOutput { value: ProcessOutput },
    #[serde(rename = "process.cancel")]
    ProcessCancel { value: ProcessRecord },
    #[serde(rename = "process.install_project_dependencies")]
    ProcessInstallProjectDependencies { value: ProcessRecord },
    #[serde(rename = "process.request_system_install")]
    ProcessRequestSystemInstall { value: ProposalRecord },
    #[serde(rename = "web.search")]
    WebSearch { value: Page<WebResult> },
    #[serde(rename = "web.fetch")]
    WebFetch { value: WebDocument },
    #[serde(rename = "containers.capabilities")]
    ContainersCapabilities { value: Page<ContainerCapability> },
    #[serde(rename = "containers.build")]
    ContainersBuild { value: ContainerImage },
    #[serde(rename = "containers.pull")]
    ContainersPull { value: ContainerImage },
    #[serde(rename = "containers.create")]
    ContainersCreate { value: ContainerRecord },
    #[serde(rename = "containers.start")]
    ContainersStart { value: ContainerRecord },
    #[serde(rename = "containers.stop")]
    ContainersStop { value: ContainerRecord },
    #[serde(rename = "containers.exec")]
    ContainersExec { value: ProcessRecord },
    #[serde(rename = "containers.inspect")]
    ContainersInspect { value: ContainerRecord },
    #[serde(rename = "containers.logs")]
    ContainersLogs { value: ArtifactRef },
    #[serde(rename = "jobs.list")]
    JobsList { value: Page<JobRecord> },
    #[serde(rename = "jobs.create")]
    JobsCreate { value: JobRecord },
    #[serde(rename = "jobs.edit")]
    JobsEdit { value: JobRecord },
    #[serde(rename = "jobs.enable")]
    JobsEnable { value: JobRecord },
    #[serde(rename = "jobs.disable")]
    JobsDisable { value: JobRecord },
    #[serde(rename = "jobs.delete")]
    JobsDelete { value: JobRecord },
    #[serde(rename = "jobs.run_now")]
    JobsRunNow { value: JobRun },
    #[serde(rename = "jobs.inspect")]
    JobsInspect { value: JobInspection },
    #[serde(rename = "jobs.cancel")]
    JobsCancel { value: JobRun },
    #[serde(rename = "settings.read_effective")]
    SettingsReadEffective { value: Page<SettingEntry> },
    #[serde(rename = "settings.validate")]
    SettingsValidate { value: ValidationReport },
    #[serde(rename = "settings.save_revision")]
    SettingsSaveRevision { value: Page<SettingEntry> },
    #[serde(rename = "settings.explain_source")]
    SettingsExplainSource { value: Page<SettingEntry> },
    #[serde(rename = "router.catalog")]
    RouterCatalog { value: Page<RouteCatalogModel> },
    #[serde(rename = "router.connections")]
    RouterConnections { value: Page<ConnectionRecord> },
    #[serde(rename = "router.route")]
    RouterRoute { value: RouteChoice },
    #[serde(rename = "router.pin")]
    RouterPin { value: RouteChoice },
    #[serde(rename = "router.availability")]
    RouterAvailability { value: RouteAvailability },
    #[serde(rename = "router.usage")]
    RouterUsage { value: RouterUsageResult },
    #[serde(rename = "router.request_status")]
    RouterRequestStatus { value: OperationRef },
    #[serde(rename = "router.price_status")]
    RouterPriceStatus { value: PriceCatalogResult },
    #[serde(rename = "router.price_refresh")]
    RouterPriceRefresh { value: OperationRef },
    #[serde(rename = "router.price_override")]
    RouterPriceOverride { value: PriceOverrideResult },
    #[serde(rename = "router.configure")]
    RouterConfigure { value: MutationSummary },
    #[serde(rename = "router.decision_models")]
    RouterDecisionModels { value: Page<DecisionModel> },
    #[serde(rename = "router.decide")]
    RouterDecide { value: DecisionResult },
    #[serde(rename = "mcp.servers")]
    McpServers { value: Page<McpServer> },
    #[serde(rename = "mcp.configure")]
    McpConfigure { value: McpServer },
    #[serde(rename = "mcp.connect")]
    McpConnect { value: McpServer },
    #[serde(rename = "mcp.discover")]
    McpDiscover { value: Page<McpCapability> },
    #[serde(rename = "mcp.select_tools")]
    McpSelectTools { value: McpServer },
    #[serde(rename = "mcp.resources")]
    McpResources { value: Page<McpResource> },
    #[serde(rename = "mcp.prompts")]
    McpPrompts { value: Page<McpPrompt> },
    #[serde(rename = "mcp.capabilities")]
    McpCapabilities { value: Page<McpCapability> },
    #[serde(rename = "addons.catalog")]
    AddonsCatalog { value: Page<PackageRecord> },
    #[serde(rename = "addons.configure")]
    AddonsConfigure { value: PackageRecord },
    #[serde(rename = "addons.propose")]
    AddonsPropose { value: ProposalRecord },
    #[serde(rename = "addons.logs")]
    AddonsLogs { value: Page<Diagnostic> },
    #[serde(rename = "help.search")]
    HelpSearch { value: Page<HelpTopic> },
    #[serde(rename = "help.read")]
    HelpRead { value: HelpTopic },
    #[serde(rename = "help.topic_for_screen")]
    HelpTopicForScreen { value: Page<HelpTopic> },
    #[serde(rename = "help.package_guides")]
    HelpPackageGuides { value: Page<HelpTopic> },
    #[serde(rename = "help.generate_capability_docs")]
    HelpGenerateCapabilityDocs { value: OperationRef },
    #[serde(rename = "workspace.diagnostics")]
    WorkspaceDiagnostics { value: DiagnosticPage },
    #[serde(rename = "workspace.logs")]
    WorkspaceLogs { value: DiagnosticPage },
    #[serde(rename = "workspace.record_diagnostic")]
    WorkspaceRecordDiagnostic { value: DiagnosticEntry },
    #[serde(rename = "workspace.backup")]
    WorkspaceBackup { value: BackupRecord },
    #[serde(rename = "workspace.restore")]
    WorkspaceRestore { value: BackupRecord },
    #[serde(rename = "workspace.migrate")]
    WorkspaceMigrate { value: MutationSummary },
    #[serde(rename = "workspace.update_status")]
    WorkspaceUpdateStatus { value: Page<UpdateRecord> },
    #[serde(rename = "workspace.approve_install")]
    WorkspaceApproveInstall { value: UpdateRecord },
    #[serde(rename = "repositories.list")]
    RepositoriesList { value: Page<RepositoryRecord> },
    #[serde(rename = "repositories.get")]
    RepositoriesGet { value: RepositoryRecord },
    #[serde(rename = "repositories.nodes")]
    RepositoriesNodes { value: GraphPage },
    #[serde(rename = "repositories.remove_index")]
    RepositoriesRemoveIndex { value: MutationSummary },
    #[serde(rename = "repositories.readme")]
    RepositoriesReadme { value: RepositoryText },
    #[serde(rename = "repositories.summary")]
    RepositoriesSummary { value: RepositoryText },
    #[serde(rename = "repositories.search")]
    RepositoriesSearch { value: Page<RepositoryRecord> },
    #[serde(rename = "queue.inspect")]
    QueueInspect { value: Page<QueueDelivery> },
    #[serde(rename = "queue.resolve")]
    QueueResolve { value: QueueDelivery },
    #[serde(rename = "queue.retry")]
    QueueRetry { value: QueueDelivery },
    #[serde(rename = "queue.reconcile")]
    QueueReconcile { value: QueueDelivery },
    #[serde(rename = "queue.consumer_status")]
    QueueConsumerStatus { value: ConsumerStatus },
    #[serde(rename = "imports.preview")]
    ImportsPreview { value: ImportRun },
    #[serde(rename = "imports.configure")]
    ImportsConfigure { value: MutationSummary },
    #[serde(rename = "imports.apply")]
    ImportsApply { value: ImportRun },
    #[serde(rename = "imports.runs")]
    ImportsRuns { value: Page<ImportRun> },
    #[serde(rename = "imports.inspect")]
    ImportsInspect { value: ImportRun },
    #[serde(rename = "maintenance.prune_diagnostics")]
    MaintenancePruneDiagnostics { value: PrunePreview },
    #[serde(rename = "maintenance.prune_index_runs")]
    MaintenancePruneIndexRuns { value: PrunePreview },
    #[serde(rename = "maintenance.prune_messages")]
    MaintenancePruneMessages { value: PrunePreview },
    #[serde(rename = "maintenance.prune_sessions")]
    MaintenancePruneSessions { value: PrunePreview },
    #[serde(rename = "repository_docs.generate")]
    RepositoryDocsGenerate { value: OperationRef },
    #[serde(rename = "mcp_server.status")]
    McpServerStatus { value: McpInboundStatus },
    #[serde(rename = "mcp_server.configure")]
    McpServerConfigure { value: McpInboundStatus },
    #[serde(rename = "mcp_server.clients")]
    McpServerClients { value: Page<McpClientAccess> },
    #[serde(rename = "mcp_server.set_client_access")]
    McpServerSetClientAccess { value: McpClientAccess },
    #[serde(rename = "tasks.prioritize")]
    TasksPrioritize { value: MutationSummary },
    #[serde(rename = "tasks.set_feature")]
    TasksSetFeature { value: WorkRecord },
    #[serde(rename = "artifacts.tag")]
    ArtifactsTag { value: MutationSummary },
    #[serde(rename = "evals.grade")]
    EvalsGrade { value: EvalReport },
    #[serde(rename = "evals.results")]
    EvalsResults { value: EvalReportPage },
    #[schemars(extend("x-since" = 3))]
    #[serde(rename = "next_work.inspect_queue")]
    NextWorkInspectQueue { value: AllocationQueueSnapshot },
    #[schemars(extend("x-since" = 3))]
    #[serde(rename = "approvals.review_queue")]
    ApprovalsReviewQueue { value: KnowledgeReviewPage },
    #[schemars(extend("x-since" = 3))]
    #[serde(rename = "notifications.mark_read")]
    NotificationsMarkRead { value: NotificationRecord },
    #[serde(rename = "runs.start")]
    RunsStart { value: RootSnapshot },
    #[serde(rename = "runs.pause")]
    RunsPause { value: RootSnapshot },
    #[serde(rename = "runs.continue")]
    RunsContinue { value: RootSnapshot },
    #[serde(rename = "runs.inspect")]
    RunsInspect { value: RootSnapshot },
    #[serde(rename = "runs.eligibility")]
    RunsEligibility { value: EligibilitySnapshot },
    #[serde(rename = "history.operation_status")]
    HistoryOperationStatus { value: OperationRef },
    #[serde(rename = "history.command_receipt")]
    HistoryCommandReceipt { value: ReceiptObservation },
    #[serde(rename = "history.events")]
    HistoryEvents { value: HistoryPage<Event> },
    #[serde(rename = "sessions.append_message")]
    SessionsAppendMessage { value: SessionRecord },
    #[serde(rename = "sessions.transcript")]
    SessionsTranscript { value: TranscriptPage },
    #[serde(rename = "addons.install")]
    AddonsInstall { value: PackageRecord },
    #[serde(rename = "addons.update")]
    AddonsUpdate { value: PackageRecord },
    #[serde(rename = "addons.enable")]
    AddonsEnable { value: PackageRecord },
    #[serde(rename = "addons.disable")]
    AddonsDisable { value: PackageRecord },
    #[serde(rename = "addons.rollback")]
    AddonsRollback { value: PackageRecord },
    #[serde(rename = "addons.uninstall")]
    AddonsUninstall { value: PackageRecord },
    #[serde(rename = "code.set_trust")]
    CodeSetTrust { value: MutationSummary },
    #[serde(rename = "indexing.refresh_overlay")]
    IndexingRefreshOverlay { value: IndexRunRecord },
    #[serde(rename = "indexing.cancel")]
    IndexingCancel { value: IndexRunRecord },
    #[serde(rename = "documents.share")]
    DocumentsShare { value: DocumentRecord },
    #[serde(rename = "documents.move")]
    DocumentsMove { value: DocumentRecord },
    #[serde(rename = "documents.disable_retrieval")]
    DocumentsDisableRetrieval { value: DocumentRecord },
    #[serde(rename = "documents.remove_index")]
    DocumentsRemoveIndex { value: DocumentRecord },
    #[serde(rename = "artifacts.save")]
    ArtifactsSave { value: ArtifactRecord },
    #[serde(rename = "artifacts.reference")]
    ArtifactsReference { value: MutationSummary },
    #[serde(rename = "documents.read")]
    DocumentsRead { value: DocumentRecord },
    #[serde(rename = "artifacts.list")]
    ArtifactsList { value: Page<ArtifactRecord> },
    #[serde(rename = "artifacts.read")]
    ArtifactsRead { value: ArtifactReadContent },
    #[serde(rename = "artifacts.preview")]
    ArtifactsPreview { value: ArtifactPreview },
    #[serde(rename = "artifacts.export")]
    ArtifactsExport { value: ArtifactExport },
    #[serde(rename = "documents.search")]
    DocumentsSearch { value: RetrievalPage },
    #[serde(rename = "artifacts.delete")]
    ArtifactsDelete { value: MutationSummary },
    #[serde(rename = "git.commit")]
    GitCommit { value: GitInspection },
    #[serde(rename = "git.push")]
    GitPush { value: GitInspection },
    #[serde(rename = "git.pull_request")]
    GitPullRequest { value: GitLink },
    #[serde(rename = "git.integrate")]
    GitIntegrate { value: DeliverySnapshot },
    #[serde(rename = "git.cleanup")]
    GitCleanup { value: MutationSummary },
    #[serde(rename = "releases.prepare")]
    ReleasesPrepare { value: ReleaseRecord },
    #[serde(rename = "releases.publish")]
    ReleasesPublish { value: ReleaseRecord },
    #[serde(rename = "releases.deploy")]
    ReleasesDeploy { value: ReleaseRecord },
    #[serde(rename = "code.search")]
    CodeSearch { value: GraphPage },
    #[serde(rename = "code.architecture")]
    CodeArchitecture { value: GraphPage },
    #[serde(rename = "code.dependency")]
    CodeDependency { value: GraphPage },
    #[serde(rename = "code.impact")]
    CodeImpact { value: GraphPage },
    #[serde(rename = "code.source")]
    CodeSource { value: GraphPage },
    #[serde(rename = "code.traverse")]
    CodeTraverse { value: GraphPage },
    #[serde(rename = "code.graph_scope_status")]
    CodeGraphScopeStatus { value: GraphPage },
    #[serde(rename = "code.overview")]
    CodeOverview { value: GraphPage },
    #[serde(rename = "code.nodes_by_file")]
    CodeNodesByFile { value: GraphPage },
    #[serde(rename = "code.node_detail")]
    CodeNodeDetail { value: GraphPage },
    #[serde(rename = "code.file_impact")]
    CodeFileImpact { value: GraphPage },
    #[serde(rename = "code.graph_schema")]
    CodeGraphSchema { value: GraphPage },
    #[serde(rename = "code.call_path")]
    CodeCallPath { value: GraphPage },
    #[serde(rename = "code.data_lineage")]
    CodeDataLineage { value: GraphPage },
    #[serde(rename = "code.consumers")]
    CodeConsumers { value: GraphPage },
    #[serde(rename = "code.publishers")]
    CodePublishers { value: GraphPage },
    #[serde(rename = "code.snippet")]
    CodeSnippet { value: GraphPage },
    #[serde(rename = "code.repository_dependencies")]
    CodeRepositoryDependencies { value: GraphPage },
}
impl OperationResult {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::ProjectsList { .. } => "projects.list",
            Self::ProjectsGet { .. } => "projects.get",
            Self::ProjectsCreate { .. } => "projects.create",
            Self::ProjectsEdit { .. } => "projects.edit",
            Self::ProjectsMembership { .. } => "projects.membership",
            Self::ProjectsArchive { .. } => "projects.archive",
            Self::ProjectsRestore { .. } => "projects.restore",
            Self::ProjectsDelete { .. } => "projects.delete",
            Self::FeaturesList { .. } => "features.list",
            Self::FeaturesGet { .. } => "features.get",
            Self::FeaturesCreate { .. } => "features.create",
            Self::FeaturesEdit { .. } => "features.edit",
            Self::IssuesList { .. } => "issues.list",
            Self::IssuesGet { .. } => "issues.get",
            Self::IssuesCreate { .. } => "issues.create",
            Self::IssuesEdit { .. } => "issues.edit",
            Self::TasksList { .. } => "tasks.list",
            Self::TasksGet { .. } => "tasks.get",
            Self::TasksCreate { .. } => "tasks.create",
            Self::TasksEdit { .. } => "tasks.edit",
            Self::FeaturesPrioritize { .. } => "features.prioritize",
            Self::IssuesPrioritize { .. } => "issues.prioritize",
            Self::FeaturesEditPlan { .. } => "features.edit_plan",
            Self::FeaturesDependencies { .. } => "features.dependencies",
            Self::FeaturesRequestPlanApproval { .. } => "features.request_plan_approval",
            Self::IssuesPublish { .. } => "issues.publish",
            Self::IssuesLink { .. } => "issues.link",
            Self::TasksMove { .. } => "tasks.move",
            Self::TasksAssign { .. } => "tasks.assign",
            Self::TasksDependencyLinks { .. } => "tasks.dependency_links",
            Self::TasksAcceptanceEvidence { .. } => "tasks.acceptance_evidence",
            Self::TasksSyncPlan { .. } => "tasks.sync_plan",
            Self::WorkflowsList { .. } => "workflows.list",
            Self::WorkflowsCreateStatus { .. } => "workflows.create_status",
            Self::WorkflowsRenameStatus { .. } => "workflows.rename_status",
            Self::WorkflowsOrder { .. } => "workflows.order",
            Self::WorkflowsReassignAndRemove { .. } => "workflows.reassign_and_remove",
            Self::NextWorkClaimProjectQueue { .. } => "next_work.claim_project_queue",
            Self::NextWorkClaimFeatureTask { .. } => "next_work.claim_feature_task",
            Self::NextWorkInspectClaim { .. } => "next_work.inspect_claim",
            Self::NextWorkReleaseClaim { .. } => "next_work.release_claim",
            Self::RunsRedirect { .. } => "runs.redirect",
            Self::RunsStop { .. } => "runs.stop",
            Self::AgentsInspect { .. } => "agents.inspect",
            Self::AgentsDelegate { .. } => "agents.delegate",
            Self::AgentsMessages { .. } => "agents.messages",
            Self::AgentsInbox { .. } => "agents.inbox",
            Self::AgentsAcknowledge { .. } => "agents.acknowledge",
            Self::AgentsCheckpoint { .. } => "agents.checkpoint",
            Self::SessionsList { .. } => "sessions.list",
            Self::SessionsGet { .. } => "sessions.get",
            Self::SessionsBriefing { .. } => "sessions.briefing",
            Self::SessionsContextSources { .. } => "sessions.context_sources",
            Self::SessionsStart { .. } => "sessions.start",
            Self::SessionsEnd { .. } => "sessions.end",
            Self::UiCurrentScreen { .. } => "ui.current_screen",
            Self::HistoryWorkLineage { .. } => "history.work_lineage",
            Self::HistoryGitLinks { .. } => "history.git_links",
            Self::HistoryEvidenceLinks { .. } => "history.evidence_links",
            Self::HistoryToolUsage { .. } => "history.tool_usage",
            Self::IndexingDiscover { .. } => "indexing.discover",
            Self::IndexingIndex { .. } => "indexing.index",
            Self::IndexingReindex { .. } => "indexing.reindex",
            Self::IndexingRetireView { .. } => "indexing.retire_view",
            Self::IndexingStatus { .. } => "indexing.status",
            Self::IndexingRuns { .. } => "indexing.runs",
            Self::IndexingConfigureSources { .. } => "indexing.configure_sources",
            Self::AnalysisHealth { .. } => "analysis.health",
            Self::AnalysisCrossRepoLinks { .. } => "analysis.cross_repo_links",
            Self::AnalysisStatus { .. } => "analysis.status",
            Self::AnalysisClusters { .. } => "analysis.clusters",
            Self::AnalysisClusterDetail { .. } => "analysis.cluster_detail",
            Self::AnalysisSecurity { .. } => "analysis.security",
            Self::AnalysisMetrics { .. } => "analysis.metrics",
            Self::AnalysisHotspots { .. } => "analysis.hotspots",
            Self::AnalysisFleetHealth { .. } => "analysis.fleet_health",
            Self::AnalysisArchivalCandidates { .. } => "analysis.archival_candidates",
            Self::AnalysisFileHotspot { .. } => "analysis.file_hotspot",
            Self::AnalysisCommunityAnalysis { .. } => "analysis.community_analysis",
            Self::AnalysisLinkAndDetect { .. } => "analysis.link_and_detect",
            Self::AnalysisProcessBatch { .. } => "analysis.process_batch",
            Self::AnalysisReanalyze { .. } => "analysis.reanalyze",
            Self::AnalysisSynthesize { .. } => "analysis.synthesize",
            Self::SchemasConnections { .. } => "schemas.connections",
            Self::SchemasObjects { .. } => "schemas.objects",
            Self::SchemasRelationships { .. } => "schemas.relationships",
            Self::SchemasIndex { .. } => "schemas.index",
            Self::SchemasRefresh { .. } => "schemas.refresh",
            Self::SchemasRemoveIndex { .. } => "schemas.remove_index",
            Self::SchemasConfigure { .. } => "schemas.configure",
            Self::DatabaseRead { .. } => "database.read",
            Self::DatabaseExecuteWrite { .. } => "database.execute_write",
            Self::DatabaseDirectSchemaChange { .. } => "database.direct_schema_change",
            Self::DatabaseApplyMigration { .. } => "database.apply_migration",
            Self::DatabaseConfigureSensitiveColumns { .. } => {
                "database.configure_sensitive_columns"
            }
            Self::KnowledgeSearch { .. } => "knowledge.search",
            Self::KnowledgeRead { .. } => "knowledge.read",
            Self::KnowledgeRecord { .. } => "knowledge.record",
            Self::KnowledgeRevise { .. } => "knowledge.revise",
            Self::KnowledgeRelate { .. } => "knowledge.relate",
            Self::KnowledgeTag { .. } => "knowledge.tag",
            Self::KnowledgeVerify { .. } => "knowledge.verify",
            Self::KnowledgeRetract { .. } => "knowledge.retract",
            Self::KnowledgeChallenge { .. } => "knowledge.challenge",
            Self::KnowledgeScope { .. } => "knowledge.scope",
            Self::KnowledgeDiagnostics { .. } => "knowledge.diagnostics",
            Self::KnowledgeDeleteBySource { .. } => "knowledge.delete_by_source",
            Self::KnowledgeDeleteSelected { .. } => "knowledge.delete_selected",
            Self::KnowledgeWriteStatus { .. } => "knowledge.write_status",
            Self::KnowledgeSubgraph { .. } => "knowledge.subgraph",
            Self::KnowledgeEntityBundle { .. } => "knowledge.entity_bundle",
            Self::KnowledgeEntityGraph { .. } => "knowledge.entity_graph",
            Self::KnowledgeClaimBundle { .. } => "knowledge.claim_bundle",
            Self::KnowledgeExpandFrontier { .. } => "knowledge.expand_frontier",
            Self::KnowledgeQuery { .. } => "knowledge.query",
            Self::KnowledgeRenderSummary { .. } => "knowledge.render_summary",
            Self::KnowledgeImportUserData { .. } => "knowledge.import_user_data",
            Self::KnowledgePromote { .. } => "knowledge.promote",
            Self::KnowledgeTaggedLessons { .. } => "knowledge.tagged_lessons",
            Self::DreamingRun { .. } => "dreaming.run",
            Self::DreamingRuns { .. } => "dreaming.runs",
            Self::DreamingReport { .. } => "dreaming.report",
            Self::DreamingSettings { .. } => "dreaming.settings",
            Self::DreamingInstructions { .. } => "dreaming.instructions",
            Self::DreamingPropose { .. } => "dreaming.propose",
            Self::DreamingRevert { .. } => "dreaming.revert",
            Self::DreamingReview { .. } => "dreaming.review",
            Self::DocumentsImport { .. } => "documents.import",
            Self::DocumentsList { .. } => "documents.list",
            Self::DocumentsIndex { .. } => "documents.index",
            Self::DocumentsRetry { .. } => "documents.retry",
            Self::ConventionsList { .. } => "conventions.list",
            Self::ConventionsSearch { .. } => "conventions.search",
            Self::ConventionsResources { .. } => "conventions.resources",
            Self::ConventionsRead { .. } => "conventions.read",
            Self::ConventionsVersion { .. } => "conventions.version",
            Self::ConventionsEdit { .. } => "conventions.edit",
            Self::ConventionsApplicability { .. } => "conventions.applicability",
            Self::ConventionsRetire { .. } => "conventions.retire",
            Self::ConventionsIndex { .. } => "conventions.index",
            Self::ConventionsDelete { .. } => "conventions.delete",
            Self::ConventionsDeleteResource { .. } => "conventions.delete_resource",
            Self::ConventionsImport { .. } => "conventions.import",
            Self::ConventionsMove { .. } => "conventions.move",
            Self::SkillsRead { .. } => "skills.read",
            Self::SkillsDraft { .. } => "skills.draft",
            Self::SkillsPropose { .. } => "skills.propose",
            Self::SkillsHistory { .. } => "skills.history",
            Self::SkillsRollback { .. } => "skills.rollback",
            Self::RolesRead { .. } => "roles.read",
            Self::RolesDraft { .. } => "roles.draft",
            Self::RolesPropose { .. } => "roles.propose",
            Self::RolesHistory { .. } => "roles.history",
            Self::RolesRollback { .. } => "roles.rollback",
            Self::SkillsDiscover { .. } => "skills.discover",
            Self::SkillsResources { .. } => "skills.resources",
            Self::ApprovalsList { .. } => "approvals.list",
            Self::ApprovalsInspect { .. } => "approvals.inspect",
            Self::ApprovalsPropose { .. } => "approvals.propose",
            Self::ApprovalsRecordHumanDecision { .. } => "approvals.record_human_decision",
            Self::NotificationsList { .. } => "notifications.list",
            Self::NotificationsRead { .. } => "notifications.read",
            Self::NotificationsAcknowledge { .. } => "notifications.acknowledge",
            Self::NotificationsSettings { .. } => "notifications.settings",
            Self::GitWorktree { .. } => "git.worktree",
            Self::GitInspect { .. } => "git.inspect",
            Self::GitDiff { .. } => "git.diff",
            Self::ReviewsList { .. } => "reviews.list",
            Self::ReviewsInspect { .. } => "reviews.inspect",
            Self::ReviewsRequest { .. } => "reviews.request",
            Self::ReviewsFindings { .. } => "reviews.findings",
            Self::ReviewsRespond { .. } => "reviews.respond",
            Self::ReviewsAdjudicate { .. } => "reviews.adjudicate",
            Self::ReviewsReadiness { .. } => "reviews.readiness",
            Self::ReleasesInspect { .. } => "releases.inspect",
            Self::GithubAccount { .. } => "github.account",
            Self::GithubRepositories { .. } => "github.repositories",
            Self::GithubPollStatus { .. } => "github.poll_status",
            Self::GithubIssueSync { .. } => "github.issue_sync",
            Self::GithubChecks { .. } => "github.checks",
            Self::GithubReviews { .. } => "github.reviews",
            Self::ResourcesList { .. } => "resources.list",
            Self::ResourcesAcquire { .. } => "resources.acquire",
            Self::ResourcesRenew { .. } => "resources.renew",
            Self::ResourcesRelease { .. } => "resources.release",
            Self::ResourcesHolders { .. } => "resources.holders",
            Self::ResourcesWaiters { .. } => "resources.waiters",
            Self::BrowserSessions { .. } => "browser.sessions",
            Self::BrowserInspect { .. } => "browser.inspect",
            Self::BrowserNavigate { .. } => "browser.navigate",
            Self::BrowserInput { .. } => "browser.input",
            Self::BrowserUpload { .. } => "browser.upload",
            Self::BrowserScreenshot { .. } => "browser.screenshot",
            Self::BrowserConsole { .. } => "browser.console",
            Self::BrowserNetwork { .. } => "browser.network",
            Self::DesktopInspect { .. } => "desktop.inspect",
            Self::DesktopCapture { .. } => "desktop.capture",
            Self::DesktopRequestControl { .. } => "desktop.request_control",
            Self::DesktopInput { .. } => "desktop.input",
            Self::DesktopReleaseControl { .. } => "desktop.release_control",
            Self::ProcessExecute { .. } => "process.execute",
            Self::ProcessOutput { .. } => "process.output",
            Self::ProcessCancel { .. } => "process.cancel",
            Self::ProcessInstallProjectDependencies { .. } => {
                "process.install_project_dependencies"
            }
            Self::ProcessRequestSystemInstall { .. } => "process.request_system_install",
            Self::WebSearch { .. } => "web.search",
            Self::WebFetch { .. } => "web.fetch",
            Self::ContainersCapabilities { .. } => "containers.capabilities",
            Self::ContainersBuild { .. } => "containers.build",
            Self::ContainersPull { .. } => "containers.pull",
            Self::ContainersCreate { .. } => "containers.create",
            Self::ContainersStart { .. } => "containers.start",
            Self::ContainersStop { .. } => "containers.stop",
            Self::ContainersExec { .. } => "containers.exec",
            Self::ContainersInspect { .. } => "containers.inspect",
            Self::ContainersLogs { .. } => "containers.logs",
            Self::JobsList { .. } => "jobs.list",
            Self::JobsCreate { .. } => "jobs.create",
            Self::JobsEdit { .. } => "jobs.edit",
            Self::JobsEnable { .. } => "jobs.enable",
            Self::JobsDisable { .. } => "jobs.disable",
            Self::JobsDelete { .. } => "jobs.delete",
            Self::JobsRunNow { .. } => "jobs.run_now",
            Self::JobsInspect { .. } => "jobs.inspect",
            Self::JobsCancel { .. } => "jobs.cancel",
            Self::SettingsReadEffective { .. } => "settings.read_effective",
            Self::SettingsValidate { .. } => "settings.validate",
            Self::SettingsSaveRevision { .. } => "settings.save_revision",
            Self::SettingsExplainSource { .. } => "settings.explain_source",
            Self::RouterCatalog { .. } => "router.catalog",
            Self::RouterConnections { .. } => "router.connections",
            Self::RouterRoute { .. } => "router.route",
            Self::RouterPin { .. } => "router.pin",
            Self::RouterAvailability { .. } => "router.availability",
            Self::RouterUsage { .. } => "router.usage",
            Self::RouterRequestStatus { .. } => "router.request_status",
            Self::RouterPriceStatus { .. } => "router.price_status",
            Self::RouterPriceRefresh { .. } => "router.price_refresh",
            Self::RouterPriceOverride { .. } => "router.price_override",
            Self::RouterConfigure { .. } => "router.configure",
            Self::RouterDecisionModels { .. } => "router.decision_models",
            Self::RouterDecide { .. } => "router.decide",
            Self::McpServers { .. } => "mcp.servers",
            Self::McpConfigure { .. } => "mcp.configure",
            Self::McpConnect { .. } => "mcp.connect",
            Self::McpDiscover { .. } => "mcp.discover",
            Self::McpSelectTools { .. } => "mcp.select_tools",
            Self::McpResources { .. } => "mcp.resources",
            Self::McpPrompts { .. } => "mcp.prompts",
            Self::McpCapabilities { .. } => "mcp.capabilities",
            Self::AddonsCatalog { .. } => "addons.catalog",
            Self::AddonsConfigure { .. } => "addons.configure",
            Self::AddonsPropose { .. } => "addons.propose",
            Self::AddonsLogs { .. } => "addons.logs",
            Self::HelpSearch { .. } => "help.search",
            Self::HelpRead { .. } => "help.read",
            Self::HelpTopicForScreen { .. } => "help.topic_for_screen",
            Self::HelpPackageGuides { .. } => "help.package_guides",
            Self::HelpGenerateCapabilityDocs { .. } => "help.generate_capability_docs",
            Self::WorkspaceDiagnostics { .. } => "workspace.diagnostics",
            Self::WorkspaceLogs { .. } => "workspace.logs",
            Self::WorkspaceRecordDiagnostic { .. } => "workspace.record_diagnostic",
            Self::WorkspaceBackup { .. } => "workspace.backup",
            Self::WorkspaceRestore { .. } => "workspace.restore",
            Self::WorkspaceMigrate { .. } => "workspace.migrate",
            Self::WorkspaceUpdateStatus { .. } => "workspace.update_status",
            Self::WorkspaceApproveInstall { .. } => "workspace.approve_install",
            Self::RepositoriesList { .. } => "repositories.list",
            Self::RepositoriesGet { .. } => "repositories.get",
            Self::RepositoriesNodes { .. } => "repositories.nodes",
            Self::RepositoriesRemoveIndex { .. } => "repositories.remove_index",
            Self::RepositoriesReadme { .. } => "repositories.readme",
            Self::RepositoriesSummary { .. } => "repositories.summary",
            Self::RepositoriesSearch { .. } => "repositories.search",
            Self::QueueInspect { .. } => "queue.inspect",
            Self::QueueResolve { .. } => "queue.resolve",
            Self::QueueRetry { .. } => "queue.retry",
            Self::QueueReconcile { .. } => "queue.reconcile",
            Self::QueueConsumerStatus { .. } => "queue.consumer_status",
            Self::ImportsPreview { .. } => "imports.preview",
            Self::ImportsConfigure { .. } => "imports.configure",
            Self::ImportsApply { .. } => "imports.apply",
            Self::ImportsRuns { .. } => "imports.runs",
            Self::ImportsInspect { .. } => "imports.inspect",
            Self::MaintenancePruneDiagnostics { .. } => "maintenance.prune_diagnostics",
            Self::MaintenancePruneIndexRuns { .. } => "maintenance.prune_index_runs",
            Self::MaintenancePruneMessages { .. } => "maintenance.prune_messages",
            Self::MaintenancePruneSessions { .. } => "maintenance.prune_sessions",
            Self::RepositoryDocsGenerate { .. } => "repository_docs.generate",
            Self::McpServerStatus { .. } => "mcp_server.status",
            Self::McpServerConfigure { .. } => "mcp_server.configure",
            Self::McpServerClients { .. } => "mcp_server.clients",
            Self::McpServerSetClientAccess { .. } => "mcp_server.set_client_access",
            Self::TasksPrioritize { .. } => "tasks.prioritize",
            Self::TasksSetFeature { .. } => "tasks.set_feature",
            Self::ArtifactsTag { .. } => "artifacts.tag",
            Self::EvalsGrade { .. } => "evals.grade",
            Self::EvalsResults { .. } => "evals.results",
            Self::NextWorkInspectQueue { .. } => "next_work.inspect_queue",
            Self::ApprovalsReviewQueue { .. } => "approvals.review_queue",
            Self::NotificationsMarkRead { .. } => "notifications.mark_read",
            Self::RunsStart { .. } => "runs.start",
            Self::RunsPause { .. } => "runs.pause",
            Self::RunsContinue { .. } => "runs.continue",
            Self::RunsInspect { .. } => "runs.inspect",
            Self::RunsEligibility { .. } => "runs.eligibility",
            Self::HistoryOperationStatus { .. } => "history.operation_status",
            Self::HistoryCommandReceipt { .. } => "history.command_receipt",
            Self::HistoryEvents { .. } => "history.events",
            Self::SessionsAppendMessage { .. } => "sessions.append_message",
            Self::SessionsTranscript { .. } => "sessions.transcript",
            Self::AddonsInstall { .. } => "addons.install",
            Self::AddonsUpdate { .. } => "addons.update",
            Self::AddonsEnable { .. } => "addons.enable",
            Self::AddonsDisable { .. } => "addons.disable",
            Self::AddonsRollback { .. } => "addons.rollback",
            Self::AddonsUninstall { .. } => "addons.uninstall",
            Self::CodeSetTrust { .. } => "code.set_trust",
            Self::IndexingRefreshOverlay { .. } => "indexing.refresh_overlay",
            Self::IndexingCancel { .. } => "indexing.cancel",
            Self::DocumentsShare { .. } => "documents.share",
            Self::DocumentsMove { .. } => "documents.move",
            Self::DocumentsDisableRetrieval { .. } => "documents.disable_retrieval",
            Self::DocumentsRemoveIndex { .. } => "documents.remove_index",
            Self::ArtifactsSave { .. } => "artifacts.save",
            Self::ArtifactsReference { .. } => "artifacts.reference",
            Self::DocumentsRead { .. } => "documents.read",
            Self::ArtifactsList { .. } => "artifacts.list",
            Self::ArtifactsRead { .. } => "artifacts.read",
            Self::ArtifactsPreview { .. } => "artifacts.preview",
            Self::ArtifactsExport { .. } => "artifacts.export",
            Self::DocumentsSearch { .. } => "documents.search",
            Self::ArtifactsDelete { .. } => "artifacts.delete",
            Self::GitCommit { .. } => "git.commit",
            Self::GitPush { .. } => "git.push",
            Self::GitPullRequest { .. } => "git.pull_request",
            Self::GitIntegrate { .. } => "git.integrate",
            Self::GitCleanup { .. } => "git.cleanup",
            Self::ReleasesPrepare { .. } => "releases.prepare",
            Self::ReleasesPublish { .. } => "releases.publish",
            Self::ReleasesDeploy { .. } => "releases.deploy",
            Self::CodeSearch { .. } => "code.search",
            Self::CodeArchitecture { .. } => "code.architecture",
            Self::CodeDependency { .. } => "code.dependency",
            Self::CodeImpact { .. } => "code.impact",
            Self::CodeSource { .. } => "code.source",
            Self::CodeTraverse { .. } => "code.traverse",
            Self::CodeGraphScopeStatus { .. } => "code.graph_scope_status",
            Self::CodeOverview { .. } => "code.overview",
            Self::CodeNodesByFile { .. } => "code.nodes_by_file",
            Self::CodeNodeDetail { .. } => "code.node_detail",
            Self::CodeFileImpact { .. } => "code.file_impact",
            Self::CodeGraphSchema { .. } => "code.graph_schema",
            Self::CodeCallPath { .. } => "code.call_path",
            Self::CodeDataLineage { .. } => "code.data_lineage",
            Self::CodeConsumers { .. } => "code.consumers",
            Self::CodePublishers { .. } => "code.publishers",
            Self::CodeSnippet { .. } => "code.snippet",
            Self::CodeRepositoryDependencies { .. } => "code.repository_dependencies",
        }
    }
}
impl Check for OperationResult {
    fn check(&self) -> Result<(), String> {
        match self {
            Self::ProjectsList { value } => value.check()?,
            Self::ProjectsGet { value } => value.check()?,
            Self::ProjectsCreate { value } => value.check()?,
            Self::ProjectsEdit { value } => value.check()?,
            Self::ProjectsMembership { value } => value.check()?,
            Self::ProjectsArchive { value } => value.check()?,
            Self::ProjectsRestore { value } => value.check()?,
            Self::ProjectsDelete { value } => value.check()?,
            Self::FeaturesList { value } => value.check()?,
            Self::FeaturesGet { value } => value.check()?,
            Self::FeaturesCreate { value } => value.check()?,
            Self::FeaturesEdit { value } => value.check()?,
            Self::IssuesList { value } => value.check()?,
            Self::IssuesGet { value } => value.check()?,
            Self::IssuesCreate { value } => value.check()?,
            Self::IssuesEdit { value } => value.check()?,
            Self::TasksList { value } => value.check()?,
            Self::TasksGet { value } => value.check()?,
            Self::TasksCreate { value } => value.check()?,
            Self::TasksEdit { value } => value.check()?,
            Self::FeaturesPrioritize { value } => value.check()?,
            Self::IssuesPrioritize { value } => value.check()?,
            Self::FeaturesEditPlan { value } => value.check()?,
            Self::FeaturesDependencies { value } => value.check()?,
            Self::FeaturesRequestPlanApproval { value } => value.check()?,
            Self::IssuesPublish { value } => value.check()?,
            Self::IssuesLink { value } => value.check()?,
            Self::TasksMove { value } => value.check()?,
            Self::TasksAssign { value } => value.check()?,
            Self::TasksDependencyLinks { value } => value.check()?,
            Self::TasksAcceptanceEvidence { value } => value.check()?,
            Self::TasksSyncPlan { value } => value.check()?,
            Self::WorkflowsList { value } => value.check()?,
            Self::WorkflowsCreateStatus { value } => value.check()?,
            Self::WorkflowsRenameStatus { value } => value.check()?,
            Self::WorkflowsOrder { value } => value.check()?,
            Self::WorkflowsReassignAndRemove { value } => value.check()?,
            Self::NextWorkClaimProjectQueue { value } => value.check()?,
            Self::NextWorkClaimFeatureTask { value } => value.check()?,
            Self::NextWorkInspectClaim { value } => value.check()?,
            Self::NextWorkReleaseClaim { value } => value.check()?,
            Self::RunsRedirect { value } => value.check()?,
            Self::RunsStop { value } => value.check()?,
            Self::AgentsInspect { value } => value.check()?,
            Self::AgentsDelegate { value } => value.check()?,
            Self::AgentsMessages { value } => value.check()?,
            Self::AgentsInbox { value } => value.check()?,
            Self::AgentsAcknowledge { value } => value.check()?,
            Self::AgentsCheckpoint { value } => value.check()?,
            Self::SessionsList { value } => value.check()?,
            Self::SessionsGet { value } => value.check()?,
            Self::SessionsBriefing { value } => value.check()?,
            Self::SessionsContextSources { value } => value.check()?,
            Self::SessionsStart { value } => value.check()?,
            Self::SessionsEnd { value } => value.check()?,
            Self::UiCurrentScreen { value } => value.check()?,
            Self::HistoryWorkLineage { value } => value.check()?,
            Self::HistoryGitLinks { value } => value.check()?,
            Self::HistoryEvidenceLinks { value } => value.check()?,
            Self::HistoryToolUsage { value } => value.check()?,
            Self::IndexingDiscover { value } => value.check()?,
            Self::IndexingIndex { value } => value.check()?,
            Self::IndexingReindex { value } => value.check()?,
            Self::IndexingRetireView { value } => value.check()?,
            Self::IndexingStatus { value } => value.check()?,
            Self::IndexingRuns { value } => value.check()?,
            Self::IndexingConfigureSources { value } => value.check()?,
            Self::AnalysisHealth { value } => value.check()?,
            Self::AnalysisCrossRepoLinks { value } => value.check()?,
            Self::AnalysisStatus { value } => value.check()?,
            Self::AnalysisClusters { value } => value.check()?,
            Self::AnalysisClusterDetail { value } => value.check()?,
            Self::AnalysisSecurity { value } => value.check()?,
            Self::AnalysisMetrics { value } => value.check()?,
            Self::AnalysisHotspots { value } => value.check()?,
            Self::AnalysisFleetHealth { value } => value.check()?,
            Self::AnalysisArchivalCandidates { value } => value.check()?,
            Self::AnalysisFileHotspot { value } => value.check()?,
            Self::AnalysisCommunityAnalysis { value } => value.check()?,
            Self::AnalysisLinkAndDetect { value } => value.check()?,
            Self::AnalysisProcessBatch { value } => value.check()?,
            Self::AnalysisReanalyze { value } => value.check()?,
            Self::AnalysisSynthesize { value } => value.check()?,
            Self::SchemasConnections { value } => value.check()?,
            Self::SchemasObjects { value } => value.check()?,
            Self::SchemasRelationships { value } => value.check()?,
            Self::SchemasIndex { value } => value.check()?,
            Self::SchemasRefresh { value } => value.check()?,
            Self::SchemasRemoveIndex { value } => value.check()?,
            Self::SchemasConfigure { value } => value.check()?,
            Self::DatabaseRead { value } => value.check()?,
            Self::DatabaseExecuteWrite { value } => value.check()?,
            Self::DatabaseDirectSchemaChange { value } => value.check()?,
            Self::DatabaseApplyMigration { value } => value.check()?,
            Self::DatabaseConfigureSensitiveColumns { value } => value.check()?,
            Self::KnowledgeSearch { value } => value.check()?,
            Self::KnowledgeRead { value } => value.check()?,
            Self::KnowledgeRecord { value } => value.check()?,
            Self::KnowledgeRevise { value } => value.check()?,
            Self::KnowledgeRelate { value } => value.check()?,
            Self::KnowledgeTag { value } => value.check()?,
            Self::KnowledgeVerify { value } => value.check()?,
            Self::KnowledgeRetract { value } => value.check()?,
            Self::KnowledgeChallenge { value } => value.check()?,
            Self::KnowledgeScope { value } => value.check()?,
            Self::KnowledgeDiagnostics { value } => value.check()?,
            Self::KnowledgeDeleteBySource { value } => value.check()?,
            Self::KnowledgeDeleteSelected { value } => value.check()?,
            Self::KnowledgeWriteStatus { value } => value.check()?,
            Self::KnowledgeSubgraph { value } => value.check()?,
            Self::KnowledgeEntityBundle { value } => value.check()?,
            Self::KnowledgeEntityGraph { value } => value.check()?,
            Self::KnowledgeClaimBundle { value } => value.check()?,
            Self::KnowledgeExpandFrontier { value } => value.check()?,
            Self::KnowledgeQuery { value } => value.check()?,
            Self::KnowledgeRenderSummary { value } => value.check()?,
            Self::KnowledgeImportUserData { value } => value.check()?,
            Self::KnowledgePromote { value } => value.check()?,
            Self::KnowledgeTaggedLessons { value } => value.check()?,
            Self::DreamingRun { value } => value.check()?,
            Self::DreamingRuns { value } => value.check()?,
            Self::DreamingReport { value } => value.check()?,
            Self::DreamingSettings { value } => value.check()?,
            Self::DreamingInstructions { value } => value.check()?,
            Self::DreamingPropose { value } => value.check()?,
            Self::DreamingRevert { value } => value.check()?,
            Self::DreamingReview { value } => value.check()?,
            Self::DocumentsImport { value } => value.check()?,
            Self::DocumentsList { value } => value.check()?,
            Self::DocumentsIndex { value } => value.check()?,
            Self::DocumentsRetry { value } => value.check()?,
            Self::ConventionsList { value } => value.check()?,
            Self::ConventionsSearch { value } => value.check()?,
            Self::ConventionsResources { value } => value.check()?,
            Self::ConventionsRead { value } => value.check()?,
            Self::ConventionsVersion { value } => value.check()?,
            Self::ConventionsEdit { value } => value.check()?,
            Self::ConventionsApplicability { value } => value.check()?,
            Self::ConventionsRetire { value } => value.check()?,
            Self::ConventionsIndex { value } => value.check()?,
            Self::ConventionsDelete { value } => value.check()?,
            Self::ConventionsDeleteResource { value } => value.check()?,
            Self::ConventionsImport { value } => value.check()?,
            Self::ConventionsMove { value } => value.check()?,
            Self::SkillsRead { value } => value.check()?,
            Self::SkillsDraft { value } => value.check()?,
            Self::SkillsPropose { value } => value.check()?,
            Self::SkillsHistory { value } => value.check()?,
            Self::SkillsRollback { value } => value.check()?,
            Self::RolesRead { value } => value.check()?,
            Self::RolesDraft { value } => value.check()?,
            Self::RolesPropose { value } => value.check()?,
            Self::RolesHistory { value } => value.check()?,
            Self::RolesRollback { value } => value.check()?,
            Self::SkillsDiscover { value } => value.check()?,
            Self::SkillsResources { value } => value.check()?,
            Self::ApprovalsList { value } => value.check()?,
            Self::ApprovalsInspect { value } => value.check()?,
            Self::ApprovalsPropose { value } => value.check()?,
            Self::ApprovalsRecordHumanDecision { value } => value.check()?,
            Self::NotificationsList { value } => value.check()?,
            Self::NotificationsRead { value } => value.check()?,
            Self::NotificationsAcknowledge { value } => value.check()?,
            Self::NotificationsSettings { value } => value.check()?,
            Self::GitWorktree { value } => value.check()?,
            Self::GitInspect { value } => value.check()?,
            Self::GitDiff { value } => value.check()?,
            Self::ReviewsList { value } => value.check()?,
            Self::ReviewsInspect { value } => value.check()?,
            Self::ReviewsRequest { value } => value.check()?,
            Self::ReviewsFindings { value } => value.check()?,
            Self::ReviewsRespond { value } => value.check()?,
            Self::ReviewsAdjudicate { value } => value.check()?,
            Self::ReviewsReadiness { value } => value.check()?,
            Self::ReleasesInspect { value } => value.check()?,
            Self::GithubAccount { value } => value.check()?,
            Self::GithubRepositories { value } => value.check()?,
            Self::GithubPollStatus { value } => value.check()?,
            Self::GithubIssueSync { value } => value.check()?,
            Self::GithubChecks { value } => value.check()?,
            Self::GithubReviews { value } => value.check()?,
            Self::ResourcesList { value } => value.check()?,
            Self::ResourcesAcquire { value } => value.check()?,
            Self::ResourcesRenew { value } => value.check()?,
            Self::ResourcesRelease { value } => value.check()?,
            Self::ResourcesHolders { value } => value.check()?,
            Self::ResourcesWaiters { value } => value.check()?,
            Self::BrowserSessions { value } => value.check()?,
            Self::BrowserInspect { value } => value.check()?,
            Self::BrowserNavigate { value } => value.check()?,
            Self::BrowserInput { value } => value.check()?,
            Self::BrowserUpload { value } => value.check()?,
            Self::BrowserScreenshot { value } => value.check()?,
            Self::BrowserConsole { value } => value.check()?,
            Self::BrowserNetwork { value } => value.check()?,
            Self::DesktopInspect { value } => value.check()?,
            Self::DesktopCapture { value } => value.check()?,
            Self::DesktopRequestControl { value } => value.check()?,
            Self::DesktopInput { value } => value.check()?,
            Self::DesktopReleaseControl { value } => value.check()?,
            Self::ProcessExecute { value } => value.check()?,
            Self::ProcessOutput { value } => value.check()?,
            Self::ProcessCancel { value } => value.check()?,
            Self::ProcessInstallProjectDependencies { value } => value.check()?,
            Self::ProcessRequestSystemInstall { value } => value.check()?,
            Self::WebSearch { value } => value.check()?,
            Self::WebFetch { value } => value.check()?,
            Self::ContainersCapabilities { value } => value.check()?,
            Self::ContainersBuild { value } => value.check()?,
            Self::ContainersPull { value } => value.check()?,
            Self::ContainersCreate { value } => value.check()?,
            Self::ContainersStart { value } => value.check()?,
            Self::ContainersStop { value } => value.check()?,
            Self::ContainersExec { value } => value.check()?,
            Self::ContainersInspect { value } => value.check()?,
            Self::ContainersLogs { value } => value.check()?,
            Self::JobsList { value } => value.check()?,
            Self::JobsCreate { value } => value.check()?,
            Self::JobsEdit { value } => value.check()?,
            Self::JobsEnable { value } => value.check()?,
            Self::JobsDisable { value } => value.check()?,
            Self::JobsDelete { value } => value.check()?,
            Self::JobsRunNow { value } => value.check()?,
            Self::JobsInspect { value } => value.check()?,
            Self::JobsCancel { value } => value.check()?,
            Self::SettingsReadEffective { value } => value.check()?,
            Self::SettingsValidate { value } => value.check()?,
            Self::SettingsSaveRevision { value } => value.check()?,
            Self::SettingsExplainSource { value } => value.check()?,
            Self::RouterCatalog { value } => value.check()?,
            Self::RouterConnections { value } => value.check()?,
            Self::RouterRoute { value } => value.check()?,
            Self::RouterPin { value } => value.check()?,
            Self::RouterAvailability { value } => value.check()?,
            Self::RouterUsage { value } => value.check()?,
            Self::RouterRequestStatus { value } => value.check()?,
            Self::RouterPriceStatus { value } => value.check()?,
            Self::RouterPriceRefresh { value } => value.check()?,
            Self::RouterPriceOverride { value } => value.check()?,
            Self::RouterConfigure { value } => value.check()?,
            Self::RouterDecisionModels { value } => value.check()?,
            Self::RouterDecide { value } => value.check()?,
            Self::McpServers { value } => value.check()?,
            Self::McpConfigure { value } => value.check()?,
            Self::McpConnect { value } => value.check()?,
            Self::McpDiscover { value } => value.check()?,
            Self::McpSelectTools { value } => value.check()?,
            Self::McpResources { value } => value.check()?,
            Self::McpPrompts { value } => value.check()?,
            Self::McpCapabilities { value } => value.check()?,
            Self::AddonsCatalog { value } => value.check()?,
            Self::AddonsConfigure { value } => value.check()?,
            Self::AddonsPropose { value } => value.check()?,
            Self::AddonsLogs { value } => value.check()?,
            Self::HelpSearch { value } => value.check()?,
            Self::HelpRead { value } => value.check()?,
            Self::HelpTopicForScreen { value } => value.check()?,
            Self::HelpPackageGuides { value } => value.check()?,
            Self::HelpGenerateCapabilityDocs { value } => value.check()?,
            Self::WorkspaceDiagnostics { value } => value.check()?,
            Self::WorkspaceLogs { value } => value.check()?,
            Self::WorkspaceRecordDiagnostic { value } => value.check()?,
            Self::WorkspaceBackup { value } => value.check()?,
            Self::WorkspaceRestore { value } => value.check()?,
            Self::WorkspaceMigrate { value } => value.check()?,
            Self::WorkspaceUpdateStatus { value } => value.check()?,
            Self::WorkspaceApproveInstall { value } => value.check()?,
            Self::RepositoriesList { value } => value.check()?,
            Self::RepositoriesGet { value } => value.check()?,
            Self::RepositoriesNodes { value } => value.check()?,
            Self::RepositoriesRemoveIndex { value } => value.check()?,
            Self::RepositoriesReadme { value } => value.check()?,
            Self::RepositoriesSummary { value } => value.check()?,
            Self::RepositoriesSearch { value } => value.check()?,
            Self::QueueInspect { value } => value.check()?,
            Self::QueueResolve { value } => value.check()?,
            Self::QueueRetry { value } => value.check()?,
            Self::QueueReconcile { value } => value.check()?,
            Self::QueueConsumerStatus { value } => value.check()?,
            Self::ImportsPreview { value } => value.check()?,
            Self::ImportsConfigure { value } => value.check()?,
            Self::ImportsApply { value } => value.check()?,
            Self::ImportsRuns { value } => value.check()?,
            Self::ImportsInspect { value } => value.check()?,
            Self::MaintenancePruneDiagnostics { value } => value.check()?,
            Self::MaintenancePruneIndexRuns { value } => value.check()?,
            Self::MaintenancePruneMessages { value } => value.check()?,
            Self::MaintenancePruneSessions { value } => value.check()?,
            Self::RepositoryDocsGenerate { value } => value.check()?,
            Self::McpServerStatus { value } => value.check()?,
            Self::McpServerConfigure { value } => value.check()?,
            Self::McpServerClients { value } => value.check()?,
            Self::McpServerSetClientAccess { value } => value.check()?,
            Self::TasksPrioritize { value } => value.check()?,
            Self::TasksSetFeature { value } => value.check()?,
            Self::ArtifactsTag { value } => value.check()?,
            Self::EvalsGrade { value } => value.check()?,
            Self::EvalsResults { value } => value.check()?,
            Self::NextWorkInspectQueue { value } => value.check()?,
            Self::ApprovalsReviewQueue { value } => value.check()?,
            Self::NotificationsMarkRead { value } => value.check()?,
            Self::RunsStart { value } => value.check()?,
            Self::RunsPause { value } => value.check()?,
            Self::RunsContinue { value } => value.check()?,
            Self::RunsInspect { value } => value.check()?,
            Self::RunsEligibility { value } => value.check()?,
            Self::HistoryOperationStatus { value } => value.check()?,
            Self::HistoryCommandReceipt { value } => value.check()?,
            Self::HistoryEvents { value } => value.check()?,
            Self::SessionsAppendMessage { value } => value.check()?,
            Self::SessionsTranscript { value } => value.check()?,
            Self::AddonsInstall { value } => value.check()?,
            Self::AddonsUpdate { value } => value.check()?,
            Self::AddonsEnable { value } => value.check()?,
            Self::AddonsDisable { value } => value.check()?,
            Self::AddonsRollback { value } => value.check()?,
            Self::AddonsUninstall { value } => value.check()?,
            Self::CodeSetTrust { value } => value.check()?,
            Self::IndexingRefreshOverlay { value } => value.check()?,
            Self::IndexingCancel { value } => value.check()?,
            Self::DocumentsShare { value } => value.check()?,
            Self::DocumentsMove { value } => value.check()?,
            Self::DocumentsDisableRetrieval { value } => value.check()?,
            Self::DocumentsRemoveIndex { value } => value.check()?,
            Self::ArtifactsSave { value } => value.check()?,
            Self::ArtifactsReference { value } => value.check()?,
            Self::DocumentsRead { value } => value.check()?,
            Self::ArtifactsList { value } => value.check()?,
            Self::ArtifactsRead { value } => value.check()?,
            Self::ArtifactsPreview { value } => value.check()?,
            Self::ArtifactsExport { value } => value.check()?,
            Self::DocumentsSearch { value } => value.check()?,
            Self::ArtifactsDelete { value } => value.check()?,
            Self::GitCommit { value } => value.check()?,
            Self::GitPush { value } => value.check()?,
            Self::GitPullRequest { value } => value.check()?,
            Self::GitIntegrate { value } => value.check()?,
            Self::GitCleanup { value } => value.check()?,
            Self::ReleasesPrepare { value } => value.check()?,
            Self::ReleasesPublish { value } => value.check()?,
            Self::ReleasesDeploy { value } => value.check()?,
            Self::CodeSearch { value } => value.check()?,
            Self::CodeArchitecture { value } => value.check()?,
            Self::CodeDependency { value } => value.check()?,
            Self::CodeImpact { value } => value.check()?,
            Self::CodeSource { value } => value.check()?,
            Self::CodeTraverse { value } => value.check()?,
            Self::CodeGraphScopeStatus { value } => value.check()?,
            Self::CodeOverview { value } => value.check()?,
            Self::CodeNodesByFile { value } => value.check()?,
            Self::CodeNodeDetail { value } => value.check()?,
            Self::CodeFileImpact { value } => value.check()?,
            Self::CodeGraphSchema { value } => value.check()?,
            Self::CodeCallPath { value } => value.check()?,
            Self::CodeDataLineage { value } => value.check()?,
            Self::CodeConsumers { value } => value.check()?,
            Self::CodePublishers { value } => value.check()?,
            Self::CodeSnippet { value } => value.check()?,
            Self::CodeRepositoryDependencies { value } => value.check()?,
        }
        super::domain_rules::result(
            self.operation(),
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )
    }
}

impl Revisioned for OperationResult {
    fn retain_revision(&mut self, revision: u32) {
        match self {
            Self::FeaturesList { value } => value.retain_revision(revision),
            Self::FeaturesGet { value } => value.retain_revision(revision),
            Self::FeaturesCreate { value } => value.retain_revision(revision),
            Self::FeaturesEdit { value } => value.retain_revision(revision),
            Self::IssuesList { value } => value.retain_revision(revision),
            Self::IssuesGet { value } => value.retain_revision(revision),
            Self::IssuesCreate { value } => value.retain_revision(revision),
            Self::IssuesEdit { value } => value.retain_revision(revision),
            Self::TasksList { value } => value.retain_revision(revision),
            Self::TasksGet { value } => value.retain_revision(revision),
            Self::TasksCreate { value } => value.retain_revision(revision),
            Self::TasksEdit { value } => value.retain_revision(revision),
            Self::FeaturesEditPlan { value } => value.retain_revision(revision),
            Self::FeaturesDependencies { value } => value.retain_revision(revision),
            Self::TasksMove { value } => value.retain_revision(revision),
            Self::TasksAssign { value } => value.retain_revision(revision),
            Self::TasksDependencyLinks { value } => value.retain_revision(revision),
            Self::TasksAcceptanceEvidence { value } => value.retain_revision(revision),
            Self::TasksSyncPlan { value } => value.retain_revision(revision),
            Self::WorkflowsList { value } => value.retain_revision(revision),
            Self::WorkflowsCreateStatus { value } => value.retain_revision(revision),
            Self::WorkflowsRenameStatus { value } => value.retain_revision(revision),
            Self::WorkflowsOrder { value } => value.retain_revision(revision),
            Self::NextWorkClaimProjectQueue { value } => value.retain_revision(revision),
            Self::NextWorkClaimFeatureTask { value } => value.retain_revision(revision),
            Self::NextWorkInspectClaim { value } => value.retain_revision(revision),
            Self::NextWorkReleaseClaim { value } => value.retain_revision(revision),
            Self::SessionsList { value } => value.retain_revision(revision),
            Self::SessionsGet { value } => value.retain_revision(revision),
            Self::SessionsStart { value } => value.retain_revision(revision),
            Self::SessionsEnd { value } => value.retain_revision(revision),
            Self::HistoryWorkLineage { value } => value.retain_revision(revision),
            Self::HistoryGitLinks { value } => value.retain_revision(revision),
            Self::HistoryEvidenceLinks { value } => value.retain_revision(revision),
            Self::HistoryToolUsage { value } => value.retain_revision(revision),
            Self::IndexingStatus { value } => value.retain_revision(revision),
            Self::KnowledgeSearch { value } => value.retain_revision(revision),
            Self::KnowledgeRead { value } => value.retain_revision(revision),
            Self::KnowledgeRecord { value } => value.retain_revision(revision),
            Self::KnowledgeRevise { value } => value.retain_revision(revision),
            Self::KnowledgeRelate { value } => value.retain_revision(revision),
            Self::KnowledgeTag { value } => value.retain_revision(revision),
            Self::KnowledgeVerify { value } => value.retain_revision(revision),
            Self::KnowledgeRetract { value } => value.retain_revision(revision),
            Self::KnowledgeChallenge { value } => value.retain_revision(revision),
            Self::KnowledgeScope { value } => value.retain_revision(revision),
            Self::KnowledgeWriteStatus { value } => value.retain_revision(revision),
            Self::KnowledgeSubgraph { value } => value.retain_revision(revision),
            Self::KnowledgeEntityBundle { value } => value.retain_revision(revision),
            Self::KnowledgeEntityGraph { value } => value.retain_revision(revision),
            Self::KnowledgeClaimBundle { value } => value.retain_revision(revision),
            Self::KnowledgeExpandFrontier { value } => value.retain_revision(revision),
            Self::KnowledgeQuery { value } => value.retain_revision(revision),
            Self::KnowledgeImportUserData { value } => value.retain_revision(revision),
            Self::KnowledgePromote { value } => value.retain_revision(revision),
            Self::KnowledgeTaggedLessons { value } => value.retain_revision(revision),
            Self::DreamingRevert { value } => value.retain_revision(revision),
            Self::ApprovalsList { value } => value.retain_revision(revision),
            Self::ApprovalsInspect { value } => value.retain_revision(revision),
            Self::ApprovalsPropose { value } => value.retain_revision(revision),
            Self::ApprovalsRecordHumanDecision { value } => value.retain_revision(revision),
            Self::NotificationsList { value } => value.retain_revision(revision),
            Self::NotificationsRead { value } => value.retain_revision(revision),
            Self::NotificationsAcknowledge { value } => value.retain_revision(revision),
            Self::ResourcesList { value } => value.retain_revision(revision),
            Self::ResourcesAcquire { value } => value.retain_revision(revision),
            Self::ResourcesRenew { value } => value.retain_revision(revision),
            Self::ResourcesRelease { value } => value.retain_revision(revision),
            Self::ResourcesHolders { value } => value.retain_revision(revision),
            Self::ResourcesWaiters { value } => value.retain_revision(revision),
            Self::BrowserSessions { value } => value.retain_revision(revision),
            Self::BrowserInput { value } => value.retain_revision(revision),
            Self::BrowserUpload { value } => value.retain_revision(revision),
            Self::BrowserConsole { value } => value.retain_revision(revision),
            Self::BrowserNetwork { value } => value.retain_revision(revision),
            Self::WebSearch { value } => value.retain_revision(revision),
            Self::WebFetch { value } => value.retain_revision(revision),
            Self::JobsList { value } => value.retain_revision(revision),
            Self::JobsCreate { value } => value.retain_revision(revision),
            Self::JobsEdit { value } => value.retain_revision(revision),
            Self::JobsEnable { value } => value.retain_revision(revision),
            Self::JobsDisable { value } => value.retain_revision(revision),
            Self::JobsDelete { value } => value.retain_revision(revision),
            Self::JobsRunNow { value } => value.retain_revision(revision),
            Self::JobsInspect { value } => value.retain_revision(revision),
            Self::JobsCancel { value } => value.retain_revision(revision),
            Self::RouterCatalog { value } => value.retain_revision(revision),
            Self::RouterConnections { value } => value.retain_revision(revision),
            Self::RouterRoute { value } => value.retain_revision(revision),
            Self::RouterPin { value } => value.retain_revision(revision),
            Self::RouterAvailability { value } => value.retain_revision(revision),
            Self::RouterUsage { value } => value.retain_revision(revision),
            Self::RouterPriceStatus { value } => value.retain_revision(revision),
            Self::RouterPriceOverride { value } => value.retain_revision(revision),
            Self::AddonsCatalog { value } => value.retain_revision(revision),
            Self::AddonsConfigure { value } => value.retain_revision(revision),
            Self::WorkspaceDiagnostics { value } => value.retain_revision(revision),
            Self::WorkspaceLogs { value } => value.retain_revision(revision),
            Self::WorkspaceRecordDiagnostic { value } => value.retain_revision(revision),
            Self::TasksSetFeature { value } => value.retain_revision(revision),
            Self::ApprovalsReviewQueue { value } => value.retain_revision(revision),
            Self::NotificationsMarkRead { value } => value.retain_revision(revision),
            Self::HistoryEvents { value } => value.retain_revision(revision),
            Self::SessionsAppendMessage { value } => value.retain_revision(revision),
            Self::AddonsInstall { value } => value.retain_revision(revision),
            Self::AddonsUpdate { value } => value.retain_revision(revision),
            Self::AddonsEnable { value } => value.retain_revision(revision),
            Self::AddonsDisable { value } => value.retain_revision(revision),
            Self::AddonsRollback { value } => value.retain_revision(revision),
            Self::AddonsUninstall { value } => value.retain_revision(revision),
            Self::ArtifactsSave { value } => value.retain_revision(revision),
            Self::ArtifactsList { value } => value.retain_revision(revision),
            Self::ArtifactsRead { value } => value.retain_revision(revision),
            _ => {}
        }
    }
    fn check_revision(&self, revision: u32) -> Result<(), String> {
        match self {
            Self::NextWorkInspectQueue { .. } if revision < 3 => {
                Err("operation result predates its introduction".into())
            }
            Self::ApprovalsReviewQueue { .. } if revision < 3 => {
                Err("operation result predates its introduction".into())
            }
            Self::NotificationsMarkRead { .. } if revision < 3 => {
                Err("operation result predates its introduction".into())
            }
            Self::NextWorkClaimProjectQueue { value } => value.check_revision(revision),
            Self::NextWorkClaimFeatureTask { value } => value.check_revision(revision),
            Self::RouterUsage { value } => value.check_revision(revision),
            Self::RouterPriceStatus { value } => value.check_revision(revision),
            Self::RouterPriceOverride { value } => value.check_revision(revision),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DomainDecision {
    Applied { result: OperationResult },
    Accepted { operation_id: OperationId },
    NoChange { result: OperationResult },
    Refused { fault: DomainFault },
}
pub type DomainCommandReply = CommandReply<DomainDecision>;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum DomainQueryReply {
    Result {
        protocol: Protocol,
        query_id: QueryId,
        stamp: ReadStamp,
        result: OperationResult,
    },
    Rejected {
        protocol: Protocol,
        query_id: QueryId,
        fault: DomainFault,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum OperationCompletion {
    Succeeded {
        protocol: Protocol,
        operation_id: OperationId,
        revision: Revision,
        result: OperationResult,
    },
    Failed {
        protocol: Protocol,
        operation_id: OperationId,
        revision: Revision,
        fault: DomainFault,
    },
    OutcomeUnknown {
        protocol: Protocol,
        operation_id: OperationId,
        revision: Revision,
        correlation_id: CorrelationId,
    },
}
impl DomainQueryReply {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Result { result, .. } => result.check(),
            Self::Rejected { fault, .. } => fault.check(),
        }
    }
}
impl OperationCompletion {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Succeeded { result, .. } => result.check(),
            Self::Failed { fault, .. } => fault.check(),
            Self::OutcomeUnknown { .. } => Ok(()),
        }
    }
}
pub fn validate_command_reply(v: &DomainCommandReply) -> Result<(), String> {
    crate::validate_domain_reply(v).map_err(str::to_owned)?;
    if let CommandReply::Recorded { receipt, .. } = v {
        match &receipt.decision {
            DomainDecision::Applied { result } | DomainDecision::NoChange { result } => {
                result.check()?
            }
            DomainDecision::Refused { fault } => {
                fault.check()?;
                if matches!(
                    fault.code,
                    super::domain_support::DomainErrorCode::OutcomeUnknown
                ) && receipt.operation_id.is_none()
                {
                    return Err("unknown effect needs durable operation identity".into());
                }
            }
            _ => {}
        }
        if let DomainDecision::Accepted { operation_id } = &receipt.decision {
            if receipt.operation_id.as_ref() != Some(operation_id) {
                return Err("accepted operation does not match receipt".into());
            }
        }
    }
    Ok(())
}
