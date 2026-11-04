//! Candidate package control/contribution DTOs. Native executables are not OS-sandboxed by this protocol.
use crate::provider::*;
use crate::*;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum RpcVersion {
    #[serde(rename = "2.0")]
    V2,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WireVersion {
    pub major: u16,
    pub minor: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageFile {
    #[schemars(length(min = 1, max = 240))]
    pub path: String,
    pub sha256: Digest,
    pub byte_length: Counter,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetBinary {
    pub target_triple: Id,
    #[schemars(length(min = 1, max = 240))]
    pub entrypoint: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum EffectRequest {
    ReadCandidate,
    Mutation,
    Opaque,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum CancelContract {
    Cooperative,
    Checkpoint,
    ReconcileOnly,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContributionMeta {
    pub id: QualifiedId,
    pub input_schema: SchemaRef,
    pub output_schema: SchemaRef,
    pub help_topic: Id,
    pub required: bool,
    pub cancellation: CancelContract,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum IndexFormat {
    Scip,
    Native,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum WorkflowRole {
    Step,
    VerificationGate,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum HookPhase {
    Assemble,
    Compact,
    Decision,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ContentKind {
    Skill,
    Role,
    Prompt,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum FieldKind {
    Text,
    Toggle,
    Select,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum UiNode {
    Text {
        #[schemars(length(max = 4096))]
        text: String,
    },
    Group {
        #[schemars(length(max = 64))]
        children: Vec<UiNode>,
    },
    Field {
        field_id: Id,
        label: String,
        field_kind: FieldKind,
        read_only: bool,
    },
    Action {
        action_id: QualifiedId,
        label: String,
    },
    Table {
        #[schemars(length(min = 1, max = 64))]
        columns: Vec<Id>,
        data_schema: SchemaRef,
    },
    Image {
        artifact: ArtifactRef,
    },
}
impl UiNode {
    fn check(&self, depth: usize, nodes: &mut usize) -> Result<(), &'static str> {
        *nodes += 1;
        if depth > 16 || *nodes > 4096 {
            return Err("UI tree bounds exceeded");
        }
        match self {
            Self::Group { children } => {
                if children.len() > 64 {
                    return Err("too many UI children");
                }
                for c in children {
                    c.check(depth + 1, nodes)?;
                }
            }
            Self::Text { text } if text.len() > 4096 => return Err("UI text too long"),
            Self::Field { label, .. } | Self::Action { label, .. }
                if label.is_empty() || label.len() > 1024 =>
            {
                return Err("invalid UI label");
            }
            Self::Table { columns, .. } if columns.is_empty() || columns.len() > 64 => {
                return Err("invalid columns");
            }
            Self::Image { artifact } => artifact.validate()?,
            _ => {}
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Contribution {
    Semantic {
        meta: ContributionMeta,
        #[schemars(length(min = 1, max = 32))]
        languages: Vec<Id>,
        format: IndexFormat,
    },
    Tool {
        meta: ContributionMeta,
        requested_effect: EffectRequest,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reconciler: Option<QualifiedId>,
    },
    Workflow {
        meta: ContributionMeta,
        role: WorkflowRole,
        evidence_schema: SchemaRef,
    },
    EventHandler {
        meta: ContributionMeta,
        #[schemars(length(min = 1, max = 32))]
        event_schemas: Vec<SchemaRef>,
    },
    ContextHook {
        meta: ContributionMeta,
        phase: HookPhase,
        priority: i32,
    },
    Content {
        meta: ContributionMeta,
        content_kind: ContentKind,
        artifact: ArtifactRef,
    },
    Provider {
        meta: ContributionMeta,
        #[schemars(length(min = 1, max = 16))]
        profiles: Vec<EndpointProfile>,
        #[schemars(length(max = 32))]
        endpoint_ids: Vec<Id>,
    },
    NativeUi {
        meta: ContributionMeta,
        view: UiNode,
    },
}
impl Contribution {
    pub fn meta(&self) -> &ContributionMeta {
        match self {
            Self::Semantic { meta, .. }
            | Self::Tool { meta, .. }
            | Self::Workflow { meta, .. }
            | Self::EventHandler { meta, .. }
            | Self::ContextHook { meta, .. }
            | Self::Content { meta, .. }
            | Self::Provider { meta, .. }
            | Self::NativeUi { meta, .. } => meta,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PermissionRequest {
    WorktreeRead,
    WorktreeWrite,
    Network { endpoint_id: Id },
    Process { runtime_id: Id },
    Inference,
    StateWrite,
    UiContribution,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageDependency {
    pub package_id: Id,
    pub version_requirement: String,
    pub manifest_digest: Digest,
    pub required: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeRequirement {
    pub runtime_id: Id,
    pub version_requirement: String,
    pub required: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaAsset {
    pub reference: SchemaRef,
    pub path: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestedLimits {
    #[schemars(range(min = 1, max = 256))]
    pub max_concurrent: u16,
    #[schemars(range(min = 1, max = 1024))]
    pub queue_capacity: u16,
    pub max_memory_bytes: Revision,
    #[schemars(range(min = 1, max = 262144))]
    pub stream_credit_bytes: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageManifest {
    pub package_id: Id,
    #[schemars(length(min = 1, max = 128))]
    pub version: String,
    pub publisher_id: Id,
    pub protocol_min: WireVersion,
    pub protocol_max: WireVersion,
    pub minimum_host_version: String,
    #[schemars(length(max = 64))]
    pub dependencies: Vec<PackageDependency>,
    #[schemars(length(max = 64))]
    pub runtimes: Vec<RuntimeRequirement>,
    #[schemars(length(max = 512))]
    pub schema_assets: Vec<SchemaAsset>,
    pub requested_limits: RequestedLimits,
    #[schemars(length(min = 1, max = 64))]
    pub targets: Vec<TargetBinary>,
    #[schemars(length(min = 1, max = 2048))]
    pub files: Vec<PackageFile>,
    pub state_schema: SchemaRef,
    pub configuration_schema: SchemaRef,
    pub help_file: String,
    #[schemars(length(max = 64))]
    pub requested_permissions: Vec<PermissionRequest>,
    #[schemars(length(min = 1, max = 256))]
    pub contributions: Vec<Contribution>,
}
fn safe_relative(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 240
        && !p.contains(['\\', ':'])
        && !p.starts_with('/')
        && p.split('/').all(|x| !x.is_empty() && x != "." && x != "..")
}
impl PackageManifest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if semver::Version::parse(&self.version).is_err()
            || semver::Version::parse(&self.minimum_host_version).is_err()
        {
            return Err("invalid semantic version");
        }
        if self.protocol_min.major != 1
            || self.protocol_max.major != 1
            || self.protocol_min.minor > self.protocol_max.minor
        {
            return Err("unsupported protocol range");
        }
        if self.targets.is_empty()
            || self.targets.len() > 64
            || self.files.is_empty()
            || self.files.len() > 2048
            || self.contributions.is_empty()
            || self.contributions.len() > 256
            || self.requested_permissions.len() > 64
        {
            return Err("manifest bounds");
        }
        if self.dependencies.len() > 64
            || self.runtimes.len() > 64
            || self.schema_assets.len() > 512
            || self.requested_limits.max_concurrent == 0
            || self.requested_limits.max_concurrent > 256
            || self.requested_limits.queue_capacity == 0
            || self.requested_limits.queue_capacity > 1024
            || self.requested_limits.stream_credit_bytes == 0
            || self.requested_limits.stream_credit_bytes > 262144
        {
            return Err("dependency/runtime/resource bounds");
        }
        let mut dependency_ids = HashSet::new();
        for d in &self.dependencies {
            if d.package_id == self.package_id
                || !dependency_ids.insert(&d.package_id.0)
                || semver::VersionReq::parse(&d.version_requirement).is_err()
            {
                return Err("invalid dependency");
            }
        }
        for r in &self.runtimes {
            if semver::VersionReq::parse(&r.version_requirement).is_err() {
                return Err("invalid runtime range");
            }
        }
        let mut files = HashSet::new();
        let mut exact_files = HashSet::new();
        for f in &self.files {
            exact_files.insert(f.path.clone());
            if !safe_relative(&f.path) || !files.insert(f.path.to_ascii_lowercase()) {
                return Err("unsafe or aliased package path");
            }
        }
        if !safe_relative(&self.help_file) || !exact_files.contains(&self.help_file) {
            return Err("missing Help file");
        }
        for asset in &self.schema_assets {
            if !safe_relative(&asset.path)
                || !self
                    .files
                    .iter()
                    .any(|f| f.path == asset.path && f.sha256 == asset.reference.sha256)
            {
                return Err("schema artifact missing or digest mismatch");
            }
        }
        let mut targets = HashSet::new();
        for t in &self.targets {
            if !safe_relative(&t.entrypoint)
                || !exact_files.contains(&t.entrypoint)
                || !targets.insert(&t.target_triple.0)
            {
                return Err("invalid target entrypoint");
            }
        }
        let mut contributions = HashSet::new();
        for c in &self.contributions {
            let name = c.meta().id.as_str();
            if !name.starts_with(&(self.package_id.0.clone() + "/")) || !contributions.insert(name)
            {
                return Err("contribution namespace conflict");
            }
            match c {
                Contribution::NativeUi { view, .. } => view.check(0, &mut 0)?,
                Contribution::Semantic { languages, .. }
                    if languages.is_empty() || languages.len() > 32 =>
                {
                    return Err("invalid language set");
                }
                Contribution::EventHandler { event_schemas, .. }
                    if event_schemas.is_empty() || event_schemas.len() > 32 =>
                {
                    return Err("invalid event schemas");
                }
                Contribution::Provider {
                    profiles,
                    endpoint_ids,
                    ..
                } if profiles.is_empty() || profiles.len() > 16 || endpoint_ids.len() > 32 => {
                    return Err("invalid provider profiles");
                }
                Contribution::Content { artifact, .. } => artifact.validate()?,
                _ => {}
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Initialize {
    pub process_session_id: Id,
    pub offered_min: WireVersion,
    pub offered_max: WireVersion,
    pub expected_manifest_digest: Digest,
    #[schemars(length(max = 64))]
    pub required_features: Vec<Id>,
    #[schemars(range(min = 1, max = 1048576))]
    pub max_frame_bytes: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Activate {
    pub process_session_id: Id,
    pub registry_revision: Revision,
    pub state_generation: Revision,
    pub configuration_revision: Revision,
    /// The package's committed configuration (absent when none was saved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<ActivateConfiguration>,
}
/// A package's committed configuration: the value and the schema it was validated against.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivateConfiguration {
    pub schema: SchemaRef,
    pub value: serde_json::Value,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum InvocationScope {
    Workspace,
    Project {
        project_id: ProjectId,
    },
    Root {
        project_id: ProjectId,
        root_id: RootId,
    },
    Assignment {
        project_id: ProjectId,
        root_id: RootId,
        assignment_id: AssignmentId,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Invoke {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub invocation_token: Id,
    pub contribution_id: QualifiedId,
    pub scope: InvocationScope,
    #[schemars(length(max = 128))]
    pub allowed_operations: Vec<QualifiedId>,
    pub registry_revision: Revision,
    pub state_generation: Revision,
    pub authority_revision: Revision,
    pub control_generation: Revision,
    pub operation_id: OperationId,
    #[schemars(range(min = 1, max = 86400000))]
    pub deadline_ms: u32,
    pub input: ArtifactRef,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cancel {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub cancellation_revision: Revision,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Shutdown {
    pub process_session_id: Id,
    #[schemars(range(min = 1, max = 60000))]
    pub deadline_ms: u32,
}
macro_rules! requests {($name:ident; $($variant:ident => $method:literal : $params:ty),+ $(,)?)=>{
 #[derive(Clone,Debug,Serialize,Deserialize,JsonSchema)]
 #[serde(tag="method",deny_unknown_fields)]
 pub enum $name {$ (#[serde(rename=$method)]$variant{jsonrpc:RpcVersion,#[schemars(length(min=1,max=256))]id:String,params:$params}),+}
};}
requests!(HostRequest;Initialize=>"host.initialize":Initialize,Activate=>"host.activate":Activate,Invoke=>"host.invoke":Invoke,Cancel=>"host.cancel":Cancel,Shutdown=>"host.shutdown":Shutdown);
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum BrokerCall {
    Application {
        operation: QualifiedId,
        input_schema: SchemaRef,
        arguments: ArtifactRef,
    },
    Http {
        connection_handle: Id,
        endpoint_id: Id,
        http_method: HttpMethod,
        relative_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<ArtifactRef>,
    },
    Inference {
        input_schema: SchemaRef,
        request: ArtifactRef,
    },
    ArtifactRead {
        artifact: ArtifactRef,
    },
    ArtifactWrite {
        proposal: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerInvoke {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub invocation_token: Id,
    pub action_key: Id,
    pub call: BrokerCall,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum StateAction {
    Read,
    CompareAndSet {
        expected_revision: Revision,
        proposal: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateRequest {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub invocation_token: Id,
    pub action_key: Id,
    pub state_generation: Revision,
    pub action: StateAction,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventIntent {
    pub action_key: Id,
    pub call: BrokerCall,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventCommit {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub invocation_token: Id,
    pub subscription_id: Id,
    pub event_id: EventId,
    pub expected_checkpoint: Counter,
    pub state_generation: Revision,
    pub expected_state_revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_proposal: Option<ArtifactRef>,
    #[schemars(length(max = 64))]
    pub intents: Vec<EventIntent>,
}
impl BrokerCall {
    fn validate(&self) -> Result<(), &'static str> {
        if let Self::Http { relative_path, .. } = self {
            if !relative_path.starts_with('/') || relative_path.starts_with("//")
                || relative_path.contains(['\\', '\r', '\n']) || relative_path.contains("://")
                || relative_path.split('/').any(|x| x == ".." || x == ".") {
                return Err("nonrelative broker HTTP target");
            }
        }
        Ok(())
    }
}
requests!(PackageRequest;Call=>"broker.call":BrokerInvoke,State=>"broker.state":StateRequest,CommitEvent=>"broker.commit_event":EventCommit);
impl PackageRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Call { id, params, .. } => {
                if id.is_empty() || id.len() > 256 {
                    return Err("invalid request ID");
                }
                params.call.validate()?;
            }
            Self::CommitEvent { id, params, .. } => {
                if id.is_empty() || id.len() > 256 || params.intents.len() > 64 {
                    return Err("event commit bounds");
                }
                let mut keys = std::collections::HashSet::new();
                for intent in &params.intents {
                    if !keys.insert(intent.action_key.0.as_str()) {
                        return Err("duplicate event intent action key");
                    }
                    intent.call.validate()?;
                }
            }
            Self::State { id, .. } if id.is_empty() || id.len() > 256 => {
                return Err("invalid request ID");
            }
            _ => {}
        }
        Ok(())
    }
}
impl HostRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        let (id, good) = match self {
            Self::Initialize { id, params, .. } => (
                id,
                params.offered_min.major == 1
                    && params.offered_max.major == 1
                    && params.offered_min.minor <= params.offered_max.minor
                    && params.max_frame_bytes > 0
                    && params.max_frame_bytes <= 1048576
                    && params.required_features.len() <= 64,
            ),
            Self::Activate { id, .. } => (id, true),
            Self::Invoke { id, params, .. } => (
                id,
                params.deadline_ms > 0
                    && params.deadline_ms <= 86400000
                    && params.allowed_operations.len() <= 128,
            ),
            Self::Cancel { id, .. } => (id, true),
            Self::Shutdown { id, params, .. } => {
                (id, params.deadline_ms > 0 && params.deadline_ms <= 60000)
            }
        };
        if id.is_empty() || id.len() > 256 || !good {
            return Err("invalid host request");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", deny_unknown_fields)]
pub enum PackageNotification {
    #[serde(rename = "package.progress")]
    Progress {
        jsonrpc: RpcVersion,
        params: Progress,
    },
    #[serde(rename = "package.event")]
    Event {
        jsonrpc: RpcVersion,
        params: InvocationEvent,
    },
    #[serde(rename = "broker.streamCredit")]
    Credit {
        jsonrpc: RpcVersion,
        params: StreamCredit,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Progress {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub invocation_token: Id,
    pub sequence: Revision,
    #[schemars(length(max = 4096))]
    pub text: String,
    pub dropped_count: Counter,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamCredit {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub invocation_token: Id,
    pub stream_id: Id,
    pub sequence: Revision,
    #[schemars(range(min = 1, max = 262144))]
    pub grant_bytes: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationEvent {
    pub process_session_id: Id,
    pub invocation_id: Id,
    pub invocation_token: Id,
    pub sequence: Revision,
    pub event: ContributionEvent,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ContributionEvent {
    Provider {
        event: ProviderCodecEvent,
    },
    ArtifactProduced {
        artifact: ArtifactRef,
    },
    SemanticIndex {
        source_snapshot: ArtifactRef,
        index: ArtifactRef,
        format: IndexFormat,
    },
    ContextProposal {
        input_digest: Digest,
        proposal: ArtifactRef,
    },
    Verification {
        passed: bool,
        #[schemars(length(max = 128))]
        evidence: Vec<ArtifactRef>,
    },
    ViewUpdate {
        revision: Revision,
        data: ArtifactRef,
    },
    EventProcessed {
        subscription_id: Id,
        event_id: EventId,
        processing_receipt: Id,
    },
}
impl PackageNotification {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Progress { params, .. } if params.text.len() > 4096 => {
                return Err("progress too large");
            }
            Self::Credit { params, .. }
                if params.grant_bytes == 0 || params.grant_bytes > 262144 =>
            {
                return Err("credit bounds");
            }
            Self::Event { params, .. } => {
                if let ContributionEvent::Provider { event } = &params.event {
                    event.validate()?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum InvocationOutcome {
    Completed {
        #[schemars(length(max = 128))]
        artifacts: Vec<ArtifactRef>,
    },
    Failed {
        code: Id,
    },
    CancelledBeforeEffect,
    OutcomeUnknown {
        operation_id: OperationId,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum RpcResult {
    Initialized {
        selected: WireVersion,
        manifest_digest: Digest,
        #[schemars(length(max = 256))]
        contribution_ids: Vec<QualifiedId>,
        #[schemars(length(max = 64))]
        features: Vec<Id>,
    },
    Activated {
        registry_revision: Revision,
        state_generation: Revision,
    },
    InvocationFinished {
        invocation_id: Id,
        outcome: InvocationOutcome,
    },
    CancelAcknowledged {
        invocation_id: Id,
    },
    ShutdownAcknowledged,
    BrokerCompleted {
        artifact: ArtifactRef,
    },
    StateSnapshot {
        state_generation: Revision,
        revision: Revision,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact: Option<ArtifactRef>,
    },
    EventCommitAccepted {
        processing_receipt: Id,
        checkpoint: Counter,
        state_revision: Revision,
        #[schemars(length(max = 64))]
        operation_ids: Vec<OperationId>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcSuccess {
    pub jsonrpc: RpcVersion,
    #[schemars(length(min = 1, max = 256))]
    pub id: String,
    pub result: RpcResult,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcError {
    pub code: i32,
    #[schemars(length(min = 1, max = 4096))]
    pub message: String,
    pub data: RpcErrorData,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcErrorData {
    pub class: RpcErrorClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum RpcErrorClass {
    Busy,
    PermissionDenied,
    OutcomeUnknown,
    ProtocolViolation,
    Unavailable,
    Conflict,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RpcFailureId {
    Known(#[schemars(length(min = 1, max = 256))] String),
    Unknown(()),
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcFailure {
    pub jsonrpc: RpcVersion,
    #[schemars(length(min = 1, max = 256))]
    pub id: RpcFailureId,
    pub error: RpcError,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RpcResponse {
    Success(RpcSuccess),
    Failure(RpcFailure),
}
impl RpcResponse {
    pub fn validate(&self) -> Result<(), &'static str> {
        let id = match self {
            Self::Success(s) => &s.id,
            Self::Failure(f) => {
                if f.error.message.is_empty() || f.error.message.len() > 4096 {
                    return Err("invalid error message");
                }
                match &f.id {
                    RpcFailureId::Known(id) => id,
                    RpcFailureId::Unknown(_) => {
                        if ![-32700, -32600].contains(&f.error.code) {
                            return Err("null error ID without parse/invalid-request error");
                        }
                        return Ok(());
                    }
                }
            }
        };
        if id.is_empty() || id.len() > 256 {
            return Err("invalid response ID");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum AddonCommand {
    Install {
        bundle: ArtifactRef,
        expected_registry_revision: Revision,
    },
    Update {
        package_id: Id,
        bundle: ArtifactRef,
        expected_registry_revision: Revision,
    },
    Enable {
        package_id: Id,
        expected_registry_revision: Revision,
    },
    Disable {
        package_id: Id,
        expected_registry_revision: Revision,
    },
    Rollback {
        package_id: Id,
        target_manifest_digest: Digest,
        expected_registry_revision: Revision,
        expected_state_generation: Revision,
    },
    Uninstall {
        package_id: Id,
        expected_registry_revision: Revision,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddonCommandRequest {
    pub protocol: Protocol,
    pub request_key: RequestKey,
    pub command: AddonCommand,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum PackageWait {
    TrustApproval,
    ExactVersionApproval,
    AccessApproval,
    Compatibility,
    Quiescence,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum PackageDecision {
    Staged {
        package_id: Id,
        manifest_digest: Digest,
        waits: Vec<PackageWait>,
    },
    Activated {
        package_id: Id,
        manifest_digest: Digest,
        registry_revision: Revision,
        state_generation: Revision,
    },
    Disabled {
        package_id: Id,
        registry_revision: Revision,
    },
    Uninstalled {
        package_id: Id,
        registry_revision: Revision,
    },
    RevisionConflict {
        revision_kind: RevisionKind,
        expected: Revision,
        current: Revision,
    },
    Rejected {
        code: Id,
    },
}
pub type AddonCommandReply = CommandReply<PackageDecision>;
