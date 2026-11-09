//! Router, session and transcript DTOs adopted from the recorded candidates
//! (tools/contracts-eval/src/provider.rs). These are wire/port messages only: no transport,
//! stream assembly or session store. Package-provided codec events live in
//! `devworks-extension-protocol` because packages emit them across the public boundary.
use super::shared::*;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(pub(crate) String);
impl TryFrom<String> for Digest {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() != 64
            || !s
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid SHA256 digest");
        }
        Ok(Self(s))
    }
}
impl From<Digest> for String {
    fn from(v: Digest) -> Self {
        v.0
    }
}
impl Digest {
    /// The lower-case hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl JsonSchema for Digest {
    fn schema_name() -> Cow<'static, str> {
        "Digest".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[a-f0-9]{64}$"})
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct QualifiedId(pub(crate) String);
impl TryFrom<String> for QualifiedId {
    type Error = &'static str;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let parts: Vec<_> = s.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|p| {
                p.is_empty()
                    || p.len() > 64
                    || !p.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.-".contains(&c)
                    })
            })
        {
            return Err("invalid qualified ID");
        }
        Ok(Self(s))
    }
}
impl QualifiedId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<QualifiedId> for String {
    fn from(v: QualifiedId) -> Self {
        v.0
    }
}
impl JsonSchema for QualifiedId {
    fn schema_name() -> Cow<'static, str> {
        "QualifiedId".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","pattern":"^[a-z0-9_.-]{1,64}/[a-z0-9_.-]{1,64}$"})
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactRef {
    pub artifact_id: Id,
    pub version_id: Id,
    pub sha256: Digest,
    pub byte_length: Counter,
    #[schemars(length(min = 1, max = 128))]
    pub media_type: String,
}
impl ArtifactRef {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.media_type.is_empty() || self.media_type.len() > 128 {
            return Err("invalid media type");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaRef {
    pub schema_id: QualifiedId,
    pub revision: Revision,
    pub sha256: Digest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EndpointProfile {
    OpenAiResponses,
    CodexSubscriptionResponses,
    AnthropicMessages,
    ClaudeSubscriptionMessages,
    OpenRouterChat,
    CompatibleChat,
    Embedding,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeBinding {
    pub connection_id: Id,
    pub account_revision: Revision,
    pub endpoint_profile: EndpointProfile,
    pub model_id: Id,
    pub context_epoch: Counter,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum InferencePurpose {
    Conversation,
    Planning,
    Implementation,
    Review,
    Dreaming,
    Compaction,
    Title,
    Vision,
    Embedding,
    Semantic,
    Package,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Attribution {
    Workspace {
        session_id: Id,
    },
    Feature {
        project_id: ProjectId,
        root_id: RootId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignment_id: Option<AssignmentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_run_id: Option<Id>,
    },
    Issue {
        project_id: ProjectId,
        root_id: RootId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        assignment_id: Option<AssignmentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_run_id: Option<Id>,
    },
    Job {
        run_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
    },
    Service {
        operation_id: OperationId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolSchema {
    pub capability_id: QualifiedId,
    #[schemars(length(min = 1, max = 128))]
    pub wire_name: String,
    pub input_schema: SchemaRef,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum InferenceInput {
    Generation {
        session_id: Id,
        context: ArtifactRef,
        context_epoch: Counter,
        #[schemars(length(max = 256))]
        tools: Vec<ToolSchema>,
        response_headroom_tokens: Revision,
        requires_images: bool,
        requires_parallel_tools: bool,
    },
    Embedding {
        embedding_space_id: Id,
        #[schemars(range(min = 1, max = 65536))]
        dimensions: u32,
        #[schemars(length(min = 1, max = 128))]
        inputs: Vec<ArtifactRef>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ModelSelection {
    Auto,
    Model { model_id: Id },
    Pool { pool_id: Id },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffortOption {
    pub value: Id,
    pub required: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InferenceOptions {
    pub selection: ModelSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<SchemaRef>,
    #[schemars(range(min = 1, max = 86400000))]
    pub deadline_ms: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InferenceRequest {
    pub options: InferenceOptions,
    pub authority_revision: Revision,
    pub control_generation: Revision,
    pub instruction_revision: Revision,
    pub cancellation_revision: Revision,
    pub protocol: Protocol,
    pub request_id: Id,
    pub purpose: InferencePurpose,
    pub attribution: Attribution,
    pub input: InferenceInput,
}
impl InferenceRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.options.deadline_ms == 0 || self.options.deadline_ms > 86400000 {
            return Err("invalid inference deadline");
        }
        match &self.input {
            InferenceInput::Generation { tools, context, .. } => {
                context.validate()?;
                let mut names = HashSet::new();
                let mut caps = HashSet::new();
                if tools.len() > 256
                    || tools.iter().any(|t| {
                        t.wire_name.is_empty()
                            || t.wire_name.len() > 128
                            || !names.insert(&t.wire_name)
                            || !caps.insert(&t.capability_id.0)
                    })
                {
                    return Err("ambiguous tool aliases");
                }
            }
            InferenceInput::Embedding {
                dimensions, inputs, ..
            } => {
                if !matches!(self.purpose, InferencePurpose::Embedding)
                    || *dimensions == 0
                    || *dimensions > 65536
                    || inputs.is_empty()
                    || inputs.len() > 128
                {
                    return Err("invalid embedding request");
                }
                for a in inputs {
                    a.validate()?;
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCallKey {
    pub attempt_id: AttemptId,
    #[schemars(length(min = 1, max = 1024))]
    pub native_call_id: String,
    pub ordinal: Counter,
}
impl ToolCallKey {
    pub fn key(&self) -> String {
        format!("{}:{}", self.attempt_id.0.0, self.native_call_id)
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.native_call_id.is_empty() || self.native_call_id.len() > 1024 {
            return Err("invalid native call ID");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteToolCall {
    pub call: ToolCallKey,
    pub capability_id: QualifiedId,
    pub input_schema: SchemaRef,
    pub arguments: ArtifactRef,
    pub native_item: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum TurnOutcome {
    Completed,
    ToolCallsReady,
    Incomplete,
    Cancelled,
    Failed,
    OutcomeUnknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PriceObservation {
    Reported {
        #[schemars(regex(pattern = "^[0-9]+(\\.[0-9]+)?$"))]
        amount: String,
        currency: Id,
    },
    Estimated {
        #[schemars(regex(pattern = "^[0-9]+(\\.[0-9]+)?$"))]
        amount: String,
        currency: Id,
        price_snapshot_id: Id,
        subscription_equivalent: bool,
    },
    Free {
        basis: FreeBasis,
    },
    Untracked {
        reason: Id,
    },
    Unknown {
        reason: Id,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum FreeBasis {
    LocalProvider,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<Counter>,
    pub cost: PriceObservation,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ProviderBody {
    TextDelta {
        block_index: Counter,
        #[schemars(length(max = 65536))]
        text: String,
    },
    ToolCallComplete {
        tool_call: CompleteToolCall,
    },
    NativeContinuation {
        binding: NativeBinding,
        artifact: ArtifactRef,
    },
    Usage {
        observation: UsageObservation,
    },
    Terminal {
        outcome: TurnOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_stop_reason: Option<String>,
    },
    Heartbeat,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderEvent {
    pub protocol: Protocol,
    pub request_id: Id,
    pub attempt_id: AttemptId,
    pub binding: NativeBinding,
    pub sequence: Revision,
    pub event: ProviderBody,
}
impl ProviderEvent {
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.event {
            ProviderBody::TextDelta { text, .. } if text.len() > 65536 => {
                return Err("oversized text delta");
            }
            ProviderBody::ToolCallComplete { tool_call } => {
                tool_call.call.validate()?;
                tool_call.arguments.validate()?;
                tool_call.native_item.validate()?;
                if tool_call.call.attempt_id != self.attempt_id {
                    return Err("foreign tool attempt");
                }
            }
            ProviderBody::NativeContinuation { binding, artifact } => {
                artifact.validate()?;
                if binding != &self.binding {
                    return Err("foreign opaque continuation");
                }
            }
            ProviderBody::Usage { observation } => {
                if let PriceObservation::Reported { amount, .. }
                | PriceObservation::Estimated { amount, .. } = &observation.cost
                {
                    let parts: Vec<_> = amount.split('.').collect();
                    if parts.len() > 2
                        || parts
                            .iter()
                            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
                    {
                        return Err("invalid monetary decimal");
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ToolOutcome {
    Completed {
        #[schemars(length(max = 128))]
        artifacts: Vec<ArtifactRef>,
    },
    /// The command receipt is final, while the accepted operation may complete later.
    OperationAccepted {
        operation_id: OperationId,
        #[schemars(length(max = 128))]
        artifacts: Vec<ArtifactRef>,
    },
    Failed {
        code: Id,
    },
    ApprovalRequired {
        approval_id: Id,
        operation_id: OperationId,
    },
    DependencyBlocked {
        #[schemars(length(min = 1, max = 100))]
        blocker_ids: Vec<HoldId>,
    },
    CancelledBeforeDispatch,
    OutcomeUnknown {
        operation_id: OperationId,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum UserContent {
    Text {
        #[schemars(length(min = 1, max = 65536))]
        text: String,
    },
    Attachment {
        artifact: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum TranscriptBody {
    UserMessage {
        #[schemars(length(min = 1, max = 128))]
        content: Vec<UserContent>,
    },
    AssistantAttempt {
        request_id: Id,
        attempt_id: AttemptId,
        #[schemars(length(max = 65536))]
        visible_text: String,
        interrupted: bool,
    },
    ToolCallRecorded {
        tool_call: CompleteToolCall,
    },
    ToolResponseRecorded {
        call: ToolCallKey,
        outcome: ToolOutcome,
    },
    HostNotice {
        source: Id,
        artifact: ArtifactRef,
    },
    NativeItem {
        binding: NativeBinding,
        artifact: ArtifactRef,
    },
    ContextEpochChanged {
        previous_epoch: Counter,
        next_epoch: Counter,
        reason: ContextChangeReason,
        context: ArtifactRef,
    },
    OperationUpdate {
        operation_id: OperationId,
        revision: Revision,
        artifact: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ContextChangeReason {
    Compaction,
    ProviderChange,
    InstructionAdoption,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TranscriptEntry {
    pub entry_id: Id,
    pub sequence: Revision,
    pub body: TranscriptBody,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TranscriptBatch {
    pub protocol: Protocol,
    pub batch_id: Id,
    pub session_id: Id,
    pub context_epoch: Counter,
    pub expected_next_sequence: Revision,
    #[schemars(length(min = 1, max = 128))]
    pub entries: Vec<TranscriptEntry>,
}
impl TranscriptBatch {
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut expected = self.expected_next_sequence.get();
        let mut ids = HashSet::new();
        if self.entries.is_empty() || self.entries.len() > 128 {
            return Err("invalid transcript batch size");
        }
        for entry in &self.entries {
            if entry.sequence.get() != expected || !ids.insert(&entry.entry_id.0) {
                return Err("transcript sequence or identity mismatch");
            }
            expected = expected.checked_add(1).ok_or("sequence exhausted")?;
            match &entry.body {
                TranscriptBody::ToolCallRecorded { tool_call } => tool_call.call.validate()?,
                TranscriptBody::ToolResponseRecorded { call, .. } => call.validate()?,
                _ => {}
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub session_id: Id,
    pub expected_session_revision: Revision,
    #[schemars(length(min = 1, max = 128))]
    pub content: Vec<UserContent>,
}
impl SessionCommandRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.content.is_empty() || self.content.len() > 128 {
            return Err("invalid user content count");
        }
        for c in &self.content {
            match c {
                UserContent::Text { text } if text.is_empty() || text.len() > 65536 => {
                    return Err("invalid message text");
                }
                UserContent::Attachment { artifact } => artifact.validate()?,
                _ => {}
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum SessionDecision {
    MessageSaved {
        session_id: Id,
        session_revision: Revision,
        entry_id: Id,
        sequence: Revision,
    },
    RevisionConflict {
        expected: Revision,
        current: Revision,
    },
    Blocked {
        blockers: Blockers,
    },
}
pub type SessionCommandReply = CommandReply<SessionDecision>;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionQueryRequest {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub session_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub after: Option<String>,
    #[schemars(range(min = 1, max = 500))]
    pub limit: u16,
}
impl SessionQueryRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.limit == 0
            || self.limit > 500
            || self
                .after
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 4096)
        {
            return Err("invalid transcript page");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TranscriptPage {
    pub protocol: Protocol,
    pub query_id: QueryId,
    pub stamp: ReadStamp,
    pub session_id: Id,
    #[schemars(length(max = 500))]
    pub entries: Vec<TranscriptEntry>,
    #[schemars(length(min = 1, max = 4096))]
    pub next_cursor: String,
    pub has_more: bool,
}
impl TranscriptPage {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.entries.len() > 500 || self.next_cursor.is_empty() || self.next_cursor.len() > 4096
        {
            return Err("invalid transcript page");
        }
        let mut prior = 0;
        for e in &self.entries {
            let n = e.sequence.get();
            if n <= prior {
                return Err("unordered transcript page");
            }
            prior = n;
        }
        Ok(())
    }
}
/// Untrusted inference input; host fills purpose, attribution and request identity separately.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InferenceSpec {
    pub options: InferenceOptions,
    pub input: InferenceInput,
}
impl InferenceSpec {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.options.deadline_ms == 0 || self.options.deadline_ms > 86400000 {
            return Err("invalid inference deadline");
        }
        match &self.input {
            InferenceInput::Embedding {
                dimensions, inputs, ..
            } if *dimensions == 0
                || *dimensions > 65536
                || inputs.is_empty()
                || inputs.len() > 128 =>
            {
                return Err("invalid embedding spec");
            }
            InferenceInput::Generation { tools, .. } if tools.len() > 256 => {
                return Err("too many tools");
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum AccessMode {
    ApiKey,
    Subscription,
    Local,
    Custom,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum SelectionReason {
    Explicit,
    SessionPin,
    RoundRobin,
    Failover,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PriceBasis {
    ReportedPreferred,
    RateSnapshot {
        snapshot_id: Id,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        override_revision: Option<Revision>,
    },
    LocalFree,
    Untracked,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RouteDecision {
    pub request_id: Id,
    pub attempt_id: AttemptId,
    pub attempt_number: Revision,
    pub binding: NativeBinding,
    pub access_mode: AccessMode,
    pub connection_revision: Revision,
    pub credential_revision: Revision,
    pub capability_revision: Revision,
    pub catalog_revision: Revision,
    pub policy_revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_revision: Option<Revision>,
    pub reason: SelectionReason,
    pub capacity_grant_id: Id,
    pub price_basis: PriceBasis,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoundNativeItem {
    pub binding: NativeBinding,
    pub artifact: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptEnvelope {
    pub protocol: Protocol,
    pub route: RouteDecision,
    pub options: InferenceOptions,
    pub request_artifact: ArtifactRef,
    pub context_digest: Digest,
    pub cache_epoch: Counter,
    #[schemars(length(max = 256))]
    pub native_continuation: Vec<BoundNativeItem>,
}
impl AttemptEnvelope {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.options.deadline_ms == 0
            || self.options.deadline_ms > 86400000
            || self.native_continuation.len() > 256
        {
            return Err("invalid attempt envelope bounds");
        }
        for item in &self.native_continuation {
            if item.binding != self.route.binding {
                return Err("attempt includes foreign native continuation");
            }
            item.artifact.validate()?;
        }
        self.request_artifact.validate()?;
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilitySnapshot {
    pub binding: NativeBinding,
    pub revision: Revision,
    pub tools: Support,
    pub parallel_tools: Support,
    pub input_images: Support,
    pub structured_output: Support,
    pub embeddings: Support,
    pub cancellation: Support,
    pub request_observation: Support,
    pub cache_metrics: Support,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<Revision>,
}
