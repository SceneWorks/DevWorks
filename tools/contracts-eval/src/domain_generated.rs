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
        super::domain_rules::record(
            "WorkRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
    }
}

impl Revisioned for WorkRecord {
    fn retain_revision(&mut self, revision: u32) {
        if revision < 2 {
            self.readiness = None;
        }
        if revision < 2 {
            self.queue_revision = None;
        }
        if revision < 2 {
            self.workflow_revision = None;
        }
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
}
impl Check for StatusRecord {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.label.check()?;
        self.category.check()?;
        self.rank.check()?;
        super::domain_rules::record(
            "StatusRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for SourceEvidence {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.claim_id.check()?;
        self.source.check()?;
        self.snippet.check()?;
        self.metadata.check()?;
        super::domain_rules::record(
            "SourceEvidence",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for KnowledgeEntity {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.label.check()?;
        self.entity_type.check()?;
        self.aliases.check()?;
        self.scope.check()?;
        super::domain_rules::record(
            "KnowledgeEntity",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "KnowledgeClaim",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "ClaimDraft",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "KnowledgeBundle",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for KnowledgeWriteReceipt {
    fn check(&self) -> Result<(), String> {
        self.write_id.check()?;
        self.state.check()?;
        self.entity_ids.check()?;
        self.claim_ids.check()?;
        self.revision.check()?;
        self.diagnostics.check()?;
        super::domain_rules::record(
            "KnowledgeWriteReceipt",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "NotificationRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for BrowserSession {
    fn check(&self) -> Result<(), String> {
        self.id.check()?;
        self.revision.check()?;
        self.state.check()?;
        self.target_ids.check()?;
        self.process_owner_id.check()?;
        super::domain_rules::record(
            "BrowserSession",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for BrowserObservation {
    fn check(&self) -> Result<(), String> {
        self.session_id.check()?;
        self.sequence.check()?;
        self.evidence.check()?;
        self.truncated.check()?;
        super::domain_rules::record(
            "BrowserObservation",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for WebResult {
    fn check(&self) -> Result<(), String> {
        self.title.check()?;
        self.url.check()?;
        self.snippet.check()?;
        self.observed_at_ms.check()?;
        super::domain_rules::record(
            "WebResult",
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
}
impl Check for WebDocument {
    fn check(&self) -> Result<(), String> {
        self.url.check()?;
        self.final_url.check()?;
        self.content.check()?;
        self.media_type.check()?;
        self.observed_at_ms.check()?;
        self.truncated.check()?;
        super::domain_rules::record(
            "WebDocument",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "JobRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "JobRun",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "RouteModel",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "ConnectionRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "RouteChoice",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "PackageRecord",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "TaskPlanEntry",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
            } => {
                connection_id.check()?;
                expected_revision.check()?;
                name.check()?;
                profile.check()?;
                endpoint.check()?;
                credential_ref.check()?;
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
}
impl Check for JobInspection {
    fn check(&self) -> Result<(), String> {
        self.job.check()?;
        self.run.check()?;
        super::domain_rules::record(
            "JobInspection",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for RepositoryIndexStatus {
    fn check(&self) -> Result<(), String> {
        self.repository_id.check()?;
        self.current_view_ids.check()?;
        self.coverage.check()?;
        self.last_run.check()?;
        super::domain_rules::record(
            "RepositoryIndexStatus",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "FeaturePlanDraft",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
            } => {
                title.check()?;
                description.check()?;
                repository_ids.check()?;
                status_id.check()?;
                priority.check()?;
                plan.check()?;
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
}
impl Check for ArtifactRecord {
    fn check(&self) -> Result<(), String> {
        self.artifact.check()?;
        self.name.check()?;
        self.created_at_ms.check()?;
        self.scope.check()?;
        self.reference_count.check()?;
        self.tags.check()?;
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
    },
    Binary {
        artifact: ArtifactRef,
        stream_id: Id,
        first_byte: Counter,
        byte_length: Counter,
        expires_at_ms: Counter,
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
            } => {
                artifact.check()?;
                first_byte.check()?;
                next_byte.check()?;
                content.check()?;
                truncated.check()?;
            }
            Self::Binary {
                artifact,
                stream_id,
                first_byte,
                byte_length,
                expires_at_ms,
            } => {
                artifact.check()?;
                stream_id.check()?;
                first_byte.check()?;
                byte_length.check()?;
                expires_at_ms.check()?;
            }
        }
        Ok(())
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
        super::domain_rules::record(
            "IssuePatch",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
        super::domain_rules::record(
            "TaskPatch",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
}
impl Check for PendingFeaturePlan {
    fn check(&self) -> Result<(), String> {
        self.revision.check()?;
        self.draft.check()?;
        self.approval.check()?;
        super::domain_rules::record(
            "PendingFeaturePlan",
            &serde_json::to_value(self).map_err(|e| e.to_string())?,
        )?;
        Ok(())
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
    IndexingStatus { repository_id: Id },
    #[serde(rename = "indexing.runs")]
    IndexingRuns {
        scope: DataScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repository_id: Option<Id>,
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
    },
    #[serde(rename = "knowledge.read")]
    KnowledgeRead {
        scope: DataScope,
        claim_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<Revision>,
    },
    #[serde(rename = "knowledge.diagnostics")]
    KnowledgeDiagnostics { scope: DataScope },
    #[serde(rename = "knowledge.write_status")]
    KnowledgeWriteStatus { write_id: Id },
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
    },
    #[serde(rename = "router.request_status")]
    RouterRequestStatus { request_id: Id },
    #[serde(rename = "router.price_status")]
    RouterPriceStatus {
        model_ids: Items<Id>,
        page: PageRequest,
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
            Self::IndexingStatus { repository_id } => {
                repository_id.check()?;
            }
            Self::IndexingRuns {
                scope,
                repository_id,
                page,
            } => {
                scope.check()?;
                repository_id.check()?;
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
            } => {
                scope.check()?;
                text.check()?;
                tags.check()?;
                kinds.check()?;
                page.check()?;
                entity_limit.check()?;
                claim_limit.check()?;
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
            Self::KnowledgeDiagnostics { scope } => {
                scope.check()?;
            }
            Self::KnowledgeWriteStatus { write_id } => {
                write_id.check()?;
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
            Self::KnowledgeTaggedLessons { scope, tags, page } => {
                scope.check()?;
                tags.check()?;
                page.check()?;
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
            } => {
                scope.check()?;
                page.check()?;
                unacknowledged_only.check()?;
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
            Self::JobsInspect { job_id, run_id } => {
                job_id.check()?;
                run_id.check()?;
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
            } => {
                root_id.check()?;
                session_id.check()?;
                job_run_id.check()?;
                page.check()?;
            }
            Self::RouterRequestStatus { request_id } => {
                request_id.check()?;
            }
            Self::RouterPriceStatus { model_ids, page } => {
                model_ids.check()?;
                page.check()?;
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
            Self::RouterAvailability { session_id, .. } => {
                if revision < 2 {
                    *session_id = None;
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
    },
    #[serde(rename = "tasks.edit")]
    TasksEdit {
        work_id: Id,
        expected_revision: Revision,
        changes: TaskPatch,
    },
    #[serde(rename = "features.prioritize")]
    FeaturesPrioritize {
        project_id: ProjectId,
        expected_queue_revision: Revision,
        ordered_work_ids: NonEmpty<Id>,
    },
    #[serde(rename = "issues.prioritize")]
    IssuesPrioritize {
        project_id: ProjectId,
        expected_queue_revision: Revision,
        ordered_work_ids: NonEmpty<Id>,
    },
    #[serde(rename = "features.edit_plan")]
    FeaturesEditPlan {
        feature_id: RootId,
        expected_revision: Revision,
        plan: PlanDraft,
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
    },
    #[serde(rename = "next_work.claim_feature_task")]
    NextWorkClaimFeatureTask {
        root_id: RootId,
        expected_task_set_revision: Revision,
        lease_ms: PositiveCounter,
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
    },
    #[serde(rename = "knowledge.scope")]
    KnowledgeScope {
        claim_ids: NonEmpty<RevisionedRef>,
        destination: DataScope,
    },
    #[serde(rename = "knowledge.delete_by_source")]
    KnowledgeDeleteBySource {
        scope: DataScope,
        source_id: Id,
        expected_scope_revision: Revision,
        preview_digest: Digest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proposal_id: Option<Id>,
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
    RouterPriceOverride {
        model_id: Id,
        expected_revision: Revision,
        price: PriceInput,
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
    TasksPrioritize {
        project_id: ProjectId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feature_id: Option<RootId>,
        expected_queue_revision: Revision,
        priority: i32,
        ordered_task_ids: NonEmpty<Id>,
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
            } => {
                project_id.check()?;
                title.check()?;
                description.check()?;
                repository_ids.check()?;
                status_id.check()?;
                priority.check()?;
                feature_id.check()?;
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
            } => {
                project_id.check()?;
                title.check()?;
                description.check()?;
                repository_ids.check()?;
                parent_id.check()?;
                status_id.check()?;
                priority.check()?;
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
            } => {
                project_id.check()?;
                expected_queue_revision.check()?;
                ordered_work_ids.check()?;
            }
            Self::IssuesPrioritize {
                project_id,
                expected_queue_revision,
                ordered_work_ids,
            } => {
                project_id.check()?;
                expected_queue_revision.check()?;
                ordered_work_ids.check()?;
            }
            Self::FeaturesEditPlan {
                feature_id,
                expected_revision,
                plan,
            } => {
                feature_id.check()?;
                expected_revision.check()?;
                plan.check()?;
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
            } => {
                project_id.check()?;
                expected_queue_revision.check()?;
                lease_ms.check()?;
            }
            Self::NextWorkClaimFeatureTask {
                root_id,
                expected_task_set_revision,
                lease_ms,
            } => {
                root_id.check()?;
                expected_task_set_revision.check()?;
                lease_ms.check()?;
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
            } => {
                scope.check()?;
                claim_id.check()?;
                expected_revision.check()?;
                reason.check()?;
                evidence_ids.check()?;
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
            } => {
                scope.check()?;
                claim_id.check()?;
                expected_revision.check()?;
                reason.check()?;
                evidence_ids.check()?;
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
            } => {
                scope.check()?;
                source_id.check()?;
                expected_scope_revision.check()?;
                preview_digest.check()?;
                proposal_id.check()?;
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
            } => {
                claim_id.check()?;
                expected_revision.check()?;
                destination.check()?;
                proposal_id.check()?;
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
            } => {
                scope.check()?;
                operation_kind.check()?;
                target.check()?;
                content_digest.check()?;
                summary.check()?;
                artifact.check()?;
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
            } => {
                model_id.check()?;
                expected_revision.check()?;
                price.check()?;
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
            } => {
                project_id.check()?;
                feature_id.check()?;
                expected_queue_revision.check()?;
                priority.check()?;
                ordered_task_ids.check()?;
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
            Self::FeaturesEdit { changes, .. } => {
                changes.retain_revision(revision);
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
            _ => {}
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
    NextWorkClaimProjectQueue { value: ClaimRecord },
    #[serde(rename = "next_work.claim_feature_task")]
    NextWorkClaimFeatureTask { value: ClaimRecord },
    #[serde(rename = "next_work.inspect_claim")]
    NextWorkInspectClaim { value: ClaimRecord },
    #[serde(rename = "next_work.release_claim")]
    NextWorkReleaseClaim { value: ClaimRecord },
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
    ApprovalsList { value: Page<ProposalRecord> },
    #[serde(rename = "approvals.inspect")]
    ApprovalsInspect { value: ProposalRecord },
    #[serde(rename = "approvals.propose")]
    ApprovalsPropose { value: ProposalRecord },
    #[serde(rename = "approvals.record_human_decision")]
    ApprovalsRecordHumanDecision { value: ProposalRecord },
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
    RouterCatalog { value: Page<RouteModel> },
    #[serde(rename = "router.connections")]
    RouterConnections { value: Page<ConnectionRecord> },
    #[serde(rename = "router.route")]
    RouterRoute { value: RouteChoice },
    #[serde(rename = "router.pin")]
    RouterPin { value: RouteChoice },
    #[serde(rename = "router.availability")]
    RouterAvailability { value: RouteAvailability },
    #[serde(rename = "router.usage")]
    RouterUsage { value: UsageReport },
    #[serde(rename = "router.request_status")]
    RouterRequestStatus { value: OperationRef },
    #[serde(rename = "router.price_status")]
    RouterPriceStatus { value: Page<ModelPriceStatus> },
    #[serde(rename = "router.price_refresh")]
    RouterPriceRefresh { value: OperationRef },
    #[serde(rename = "router.price_override")]
    RouterPriceOverride { value: PriceRecord },
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
            Self::SessionsList { value } => value.retain_revision(revision),
            Self::SessionsGet { value } => value.retain_revision(revision),
            Self::SessionsStart { value } => value.retain_revision(revision),
            Self::SessionsEnd { value } => value.retain_revision(revision),
            Self::HistoryWorkLineage { value } => value.retain_revision(revision),
            Self::HistoryGitLinks { value } => value.retain_revision(revision),
            Self::HistoryEvidenceLinks { value } => value.retain_revision(revision),
            Self::HistoryToolUsage { value } => value.retain_revision(revision),
            Self::ResourcesList { value } => value.retain_revision(revision),
            Self::ResourcesAcquire { value } => value.retain_revision(revision),
            Self::ResourcesRenew { value } => value.retain_revision(revision),
            Self::ResourcesRelease { value } => value.retain_revision(revision),
            Self::ResourcesHolders { value } => value.retain_revision(revision),
            Self::ResourcesWaiters { value } => value.retain_revision(revision),
            Self::RouterAvailability { value } => value.retain_revision(revision),
            Self::WorkspaceDiagnostics { value } => value.retain_revision(revision),
            Self::WorkspaceLogs { value } => value.retain_revision(revision),
            Self::WorkspaceRecordDiagnostic { value } => value.retain_revision(revision),
            Self::TasksSetFeature { value } => value.retain_revision(revision),
            Self::HistoryEvents { value } => value.retain_revision(revision),
            Self::SessionsAppendMessage { value } => value.retain_revision(revision),
            Self::ArtifactsSave { value } => value.retain_revision(revision),
            Self::ArtifactsList { value } => value.retain_revision(revision),
            _ => {}
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
