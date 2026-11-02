//! Candidate router/session DTOs and deterministic invariants, not a transport or session service.
use crate::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);
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
pub struct QualifiedId(String);
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
        let mut expected = self.expected_next_sequence.0.parse::<i64>().unwrap();
        let mut ids = HashSet::new();
        if self.entries.is_empty() || self.entries.len() > 128 {
            return Err("invalid transcript batch size");
        }
        for entry in &self.entries {
            if entry.sequence.0.parse::<i64>().unwrap() != expected
                || !ids.insert(&entry.entry_id.0)
            {
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
            let n = e.sequence.0.parse::<i64>().unwrap();
            if n <= prior {
                return Err("unordered transcript page");
            }
            prior = n;
        }
        Ok(())
    }
}
#[derive(Default, Clone)]
pub struct StreamModel {
    request: Option<Id>,
    attempt: Option<AttemptId>,
    binding: Option<NativeBinding>,
    next: i64,
    calls: HashSet<String>,
    ordinals: HashSet<String>,
    terminal: bool,
    ready: bool,
}
impl StreamModel {
    pub fn accept(&mut self, e: &ProviderEvent) -> Result<(), &'static str> {
        e.validate()?;
        if self.terminal {
            return Err("event after terminal");
        }
        if let Some(r) = &self.request {
            if r != &e.request_id
                || self.attempt.as_ref() != Some(&e.attempt_id)
                || self.binding.as_ref() != Some(&e.binding)
            {
                return Err("attempt identity changed");
            }
        }
        if e.sequence.0.parse::<i64>().unwrap() != self.next + 1 {
            return Err("stream sequence mismatch");
        }
        match &e.event {
            ProviderBody::ToolCallComplete { tool_call } => {
                if self.calls.contains(&tool_call.call.native_call_id)
                    || self.ordinals.contains(&tool_call.call.ordinal.0)
                {
                    return Err("duplicate call identity");
                }
            }
            ProviderBody::Terminal { outcome, .. } => {
                if matches!(outcome, TurnOutcome::ToolCallsReady) && self.calls.is_empty()
                    || matches!(outcome, TurnOutcome::Completed) && !self.calls.is_empty()
                {
                    return Err("terminal/tool-set mismatch");
                }
            }
            _ => {}
        }
        self.request = Some(e.request_id.clone());
        self.attempt = Some(e.attempt_id.clone());
        self.binding = Some(e.binding.clone());
        self.next += 1;
        if let ProviderBody::ToolCallComplete { tool_call } = &e.event {
            self.calls.insert(tool_call.call.native_call_id.clone());
            self.ordinals.insert(tool_call.call.ordinal.0.clone());
        }
        if let ProviderBody::Terminal { outcome, .. } = &e.event {
            self.terminal = true;
            self.ready = matches!(outcome, TurnOutcome::ToolCallsReady);
        }
        Ok(())
    }
    pub fn tools_ready(&self) -> bool {
        self.ready
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

#[derive(Clone)]
pub struct TranscriptModel {
    next: i64,
    context_epoch: i64,
    session: Option<Id>,
    batches: std::collections::HashMap<String, String>,
    entries: HashSet<String>,
    calls: std::collections::HashMap<String, String>,
    responses: HashSet<String>,
}
impl Default for TranscriptModel {
    fn default() -> Self {
        Self {
            next: 1,
            context_epoch: 0,
            session: None,
            batches: Default::default(),
            entries: Default::default(),
            calls: Default::default(),
            responses: Default::default(),
        }
    }
}
impl TranscriptModel {
    pub fn next_sequence(&self) -> i64 {
        self.next
    }
    pub fn accept(&mut self, b: &TranscriptBatch) -> Result<&'static str, &'static str> {
        b.validate()?;
        let key = b.batch_id.0.clone();
        let payload = serde_json::to_string(b).map_err(|_| "invalid batch")?;
        if let Some(old) = self.batches.get(&key) {
            return if old == &payload {
                Ok("Replayed")
            } else {
                Err("batch identity changed payload")
            };
        }
        if self.session.as_ref().is_some_and(|s| s != &b.session_id)
            || b.expected_next_sequence.0.parse::<i64>().unwrap() != self.next
        {
            return Err("wrong session or expected sequence");
        }
        let epoch = b.context_epoch.0.parse::<i64>().unwrap();
        if epoch != self.context_epoch {
            match &b.entries[0].body {
                TranscriptBody::ContextEpochChanged {
                    previous_epoch,
                    next_epoch,
                    ..
                } if previous_epoch.0.parse::<i64>().unwrap() == self.context_epoch
                    && next_epoch.0.parse::<i64>().unwrap() == epoch
                    && self.context_epoch.checked_add(1) == Some(epoch) => {}
                _ => return Err("context epoch change requires explicit boundary"),
            }
        } else if b
            .entries
            .iter()
            .any(|e| matches!(e.body, TranscriptBody::ContextEpochChanged { .. }))
        {
            return Err("duplicate context boundary");
        }
        let mut next = self.clone();
        next.context_epoch = epoch;
        for e in &b.entries {
            if !next.entries.insert(e.entry_id.0.clone()) {
                return Err("duplicate transcript entry");
            }
            match &e.body {
                TranscriptBody::ToolCallRecorded { tool_call } => {
                    if next
                        .calls
                        .insert(tool_call.call.key(), tool_call.call.ordinal.0.clone())
                        .is_some()
                    {
                        return Err("duplicate durable tool call");
                    }
                }
                TranscriptBody::ToolResponseRecorded { call, .. } => {
                    if next.calls.get(&call.key()) != Some(&call.ordinal.0)
                        || !next.responses.insert(call.key())
                    {
                        return Err("missing call or second native response");
                    }
                }
                _ => {}
            }
            next.next = next.next.checked_add(1).ok_or("sequence exhausted")?;
        }
        next.session = Some(b.session_id.clone());
        next.batches.insert(key, payload);
        *self = next;
        Ok("Accepted")
    }
}

/// Codec observations are untrusted; the host adds price classification, route binding and attribution.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawUsageObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_cost: Option<ReportedCost>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReportedCost {
    #[schemars(regex(pattern = "^[0-9]+(\\.[0-9]+)?$"))]
    pub amount: String,
    pub currency: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum CodecBody {
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
        observation: RawUsageObservation,
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
pub struct ProviderCodecEvent {
    pub request_id: Id,
    pub attempt_id: AttemptId,
    pub sequence: Revision,
    pub event: CodecBody,
}
impl ProviderCodecEvent {
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.event {
            CodecBody::TextDelta { text, .. } if text.len() > 65536 => {
                return Err("codec text too large");
            }
            CodecBody::ToolCallComplete { tool_call } => {
                tool_call.call.validate()?;
                if tool_call.call.attempt_id != self.attempt_id {
                    return Err("codec foreign call");
                }
            }
            CodecBody::Usage { observation } => {
                if let Some(cost) = &observation.reported_cost {
                    let parts: Vec<_> = cost.amount.split('.').collect();
                    if parts.len() > 2
                        || parts
                            .iter()
                            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
                    {
                        return Err("invalid reported decimal");
                    }
                }
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
