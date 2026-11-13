//! Outgoing MCP configuration. The scoped configuration repository holds the complete server
//! set at one revision, so every edit is durable, compare-and-set and project-purge aware.
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, mcp as ops};
use crate::ports::mcp::{
    McpCallOrigin, McpEndpoint, McpRemote, McpRemoteFault, McpSampler, RemoteCatalog,
    SamplingRefusal,
};
use crate::ports::packages::SchemaValidator;
use crate::ports::storage::{
    ApplicationStore, ConfigScope, NewEvent, NewOperation, NewQueueEntry, OperationRecord,
    ProjectRead, ReadOptions, ReadSnapshot, RevisionConflict, StorageError, WriteTransaction,
    read_snapshot,
};
use crate::ports::{
    AuthorityFence, Authorization, CommandHandler, Decided, HandlerFailure, IdKind, IdSource,
    QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::provider::{Digest, QualifiedId, SchemaRef, ToolCallKey, ToolSchema};
use crate::protocol::records::{ItemState, McpCapability, McpServer, McpTransport, PageRequest};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Check, Limit, Page};
use crate::protocol::{
    ActorId, CorrelationId, Counter, DomainErrorCode, DomainFault, EventId, Id, OperationId,
    OperationState, ProjectId, RetryAdvice, Revision, RevisionDimension,
};
use crate::services::artifacts::ArtifactService;
use crate::services::sessions::{Failure, Refusal, decided, query_fault, rev};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
mod content;

const NAMESPACE: &str = "outgoing_mcp";
const SCHEMA_VERSION: i64 = 1;
pub const OPERATIONS: [&str; 11] = [
    "mcp.servers",
    "mcp.configure",
    "mcp.connect",
    "mcp.disable",
    "mcp.remove",
    "mcp.discover",
    "mcp.select_tools",
    "mcp.classify_tool",
    "mcp.capabilities",
    "mcp.resources",
    "mcp.prompts",
];
pub const EFFECT_CONSUMER: &str = "mcp_effects";
/// Host-synthesized catalog entry a human selects to let a server request sampling. It is never
/// a provider tool: tool exposure and tool admission only consider `kind == "tool"`.
const SAMPLING_KIND: &str = "sampling";
const SAMPLING_NAME: &str = "createMessage";

#[derive(Clone, Serialize, Deserialize, Default)]
struct Configuration {
    servers: Vec<StoredServer>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredServer {
    public: McpServer,
    transport: McpTransport,
    capabilities: Vec<McpCapability>,
    #[serde(default)]
    schemas: BTreeMap<String, Value>,
    #[serde(default)]
    resource_uris: BTreeMap<String, String>,
    /// The last available version of each selected tool that a later discovery no longer
    /// listed. It stays in the catalog without an input schema, so it is shown unavailable and
    /// never offered; it is selected again when the server lists it again unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    retired: BTreeMap<String, McpCapability>,
    /// A person's read-only classification of a tool, by capability ID, in place of the
    /// server's hint. It is kept apart from the catalog so rediscovery and a transport reset
    /// keep it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    read_only_overrides: BTreeMap<String, bool>,
}

impl StoredServer {
    /// The record a caller sees. A human or the host also gets the saved transport so Settings
    /// can show and resend it; it holds credential handles, never secret values. Agents do not
    /// get it.
    fn view(&self, ctx: &InvocationContext) -> McpServer {
        let mut public = self.public.clone();
        public.configuration = matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        )
        .then(|| self.transport.clone());
        public
    }
    fn samples(&self) -> bool {
        capability_id(&self.public.id, SAMPLING_KIND, SAMPLING_NAME)
            .is_ok_and(|id| self.public.selected_tool_ids.0.contains(&id))
    }
    fn endpoint(&self, scope: &ConfigScope) -> McpEndpoint {
        McpEndpoint {
            server_id: self.public.id.clone(),
            scope: scope.clone(),
            transport: self.transport.clone(),
            sampling: self.samples(),
        }
    }
}

/// The agent invocation a sampling request runs under, rebuilt from its `mcp.tool` journal.
pub struct SamplingGrant {
    pub context: InvocationContext,
    pub operation_id: OperationId,
    pub attempt_id: Id,
    pub call: ToolCallKey,
    pub fences: Vec<AuthorityFence>,
}

/// A server-targeted input names its scope from revision 8; an earlier caller addresses the
/// workspace, which was the only outgoing MCP scope before then.
pub(crate) fn requested_scope(scope: &Option<DataScope>) -> DataScope {
    scope.clone().unwrap_or(DataScope::Workspace)
}

fn scope(
    ctx: &InvocationContext,
    requested: &DataScope,
    projects: &dyn ProjectRead,
) -> Result<ConfigScope, Failure> {
    match (ctx.project_id(), requested) {
        (None, DataScope::Workspace) => Ok(ConfigScope::Workspace),
        (_, DataScope::Project { project_id }) => {
            crate::services::projects::visible_project(projects, ctx, project_id)?;
            Ok(ConfigScope::Project(project_id.clone()))
        }
        _ => Err(Refusal::new(DomainErrorCode::PermissionDenied, "mcp_scope", None).into()),
    }
}

/// A project agent may use a workspace server inherited into its tool catalog. The operation
/// still names the exact configuration scope so a server ID never triggers an unbounded search.
fn server_scope(
    ctx: &InvocationContext,
    requested: &DataScope,
    projects: &dyn ProjectRead,
) -> Result<ConfigScope, Failure> {
    match requested {
        DataScope::Workspace => Ok(ConfigScope::Workspace),
        DataScope::Project { project_id } => {
            crate::services::projects::visible_project(projects, ctx, project_id)?;
            Ok(ConfigScope::Project(project_id.clone()))
        }
    }
}

fn active_server_scope(
    ctx: &InvocationContext,
    requested: &DataScope,
    projects: &dyn ProjectRead,
) -> Result<ConfigScope, Failure> {
    let resolved = server_scope(ctx, requested, projects)?;
    let project = resolved.project_id().or_else(|| ctx.project_id());
    if let Some(project) = project
        && crate::services::projects::visible_project(projects, ctx, project)?.lifecycle
            != crate::ports::storage::ACTIVE
    {
        return Err(Refusal::new(
            DomainErrorCode::Conflict,
            "mcp_project_lifecycle",
            Some(project.0.clone()),
        )
        .into());
    }
    Ok(resolved)
}

fn current(
    read: &dyn crate::ports::storage::ConfigurationRead,
    scope: &ConfigScope,
) -> Result<(i64, Configuration), StorageError> {
    let Some(revision) = read.current(scope, NAMESPACE)? else {
        return Ok((1, Configuration::default()));
    };
    if revision.schema_version != SCHEMA_VERSION {
        return Err(StorageError::IncompatibleSchema(
            "outgoing MCP configuration version".into(),
        ));
    }
    let mut config: Configuration = serde_json::from_value(revision.body)
        .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
    // A catalog saved before capabilities carried their URI keeps it only in `resource_uris`.
    for server in &mut config.servers {
        for cap in &mut server.capabilities {
            if cap.kind.0 == "resource" && cap.uri.is_none() {
                cap.uri = server
                    .resource_uris
                    .get(cap.id.as_str())
                    .cloned()
                    .map(BoundedText);
            }
        }
    }
    Ok((revision.revision, config))
}

/// Credential handles retained by current outgoing MCP server configurations. The connection
/// owner's secret collector shares this namespace and must include these references.
pub fn credential_references(read: &dyn ReadSnapshot) -> Result<BTreeSet<String>, StorageError> {
    use crate::ports::storage::{ACTIVE, ARCHIVED, DELETED, ProjectFilter, ProjectPosition};
    let mut references = BTreeSet::new();
    let mut scopes = vec![ConfigScope::Workspace];
    let mut after: Option<ProjectPosition> = None;
    loop {
        let projects = read.projects().projects(
            &ProjectFilter {
                only: None,
                lifecycles: vec![ACTIVE.into(), ARCHIVED.into(), DELETED.into()],
            },
            after.as_ref(),
            500,
        )?;
        scopes.extend(
            projects
                .iter()
                .map(|project| ConfigScope::Project(project.id.clone())),
        );
        if projects.len() < 500 {
            break;
        }
        after = projects.last().map(|project| ProjectPosition {
            created_at_ms: project.created_at_ms,
            id: project.id.clone(),
        });
    }
    for scope in scopes {
        let (_, configuration) = current(read.configuration(), &scope)?;
        for server in configuration.servers {
            if let McpTransport::Http {
                credential_ref: Some(handle),
                ..
            } = server.transport
            {
                references.insert(handle.as_str().to_owned());
            }
        }
    }
    Ok(references)
}

fn page<T: Clone>(
    items: &[T],
    request: &PageRequest,
    revision: i64,
) -> Result<Page<T>, StorageError> {
    let offset = match &request.cursor {
        None => 0,
        Some(cursor) => {
            let (version, position) = cursor
                .0
                .split_once(':')
                .ok_or(StorageError::CursorExpired)?;
            if version.parse::<i64>().ok() != Some(revision) {
                return Err(StorageError::CursorExpired);
            }
            position
                .parse::<usize>()
                .map_err(|_| StorageError::CursorExpired)?
        }
    };
    if offset > items.len() {
        return Err(StorageError::CursorExpired);
    }
    let end = offset
        .saturating_add(request.limit.get() as usize)
        .min(items.len());
    Ok(Page {
        items: BoundedList(items[offset..end].to_vec()),
        next_cursor: (end < items.len()).then(|| BoundedText(format!("{revision}:{end}"))),
        has_more: end < items.len(),
    })
}

pub struct McpClientService {
    ids: Arc<dyn IdSource>,
    native: OnceLock<Native>,
}

#[derive(Clone)]
pub struct McpToolMaterial {
    pub alias: ToolSchema,
    pub schema: Value,
    pub description: String,
    pub server_id: Id,
    pub server_scope: ConfigScope,
    pub server_revision: Revision,
    pub catalog_revision: Revision,
    pub capability_id: Id,
    pub wire_name: String,
}
struct Native {
    store: Arc<dyn ApplicationStore>,
    remote: Arc<dyn McpRemote>,
    validator: Arc<dyn SchemaValidator>,
    artifacts: Arc<ArtifactService>,
}
impl McpClientService {
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self {
            ids,
            native: OnceLock::new(),
        }
    }

    /// Attach the one native connection owner after boot creates ProcessSupervisor. The gateway
    /// service already registered at the desktop composition root then uses this same owner.
    pub fn install_remote(
        &self,
        store: Arc<dyn ApplicationStore>,
        remote: Arc<dyn McpRemote>,
        validator: Arc<dyn SchemaValidator>,
        artifacts: Arc<ArtifactService>,
    ) -> Result<(), &'static str> {
        self.native
            .set(Native {
                store,
                remote,
                validator,
                artifacts,
            })
            .map_err(|_| "outgoing MCP remote already installed")
    }

    /// The same saved selection qualifies both provider exposure and later effect admission.
    /// Workspace servers are inherited by project agents; project-owned servers stay scoped.
    pub fn selected_tool_materials(
        &self,
        ctx: &InvocationContext,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<Vec<McpToolMaterial>, StorageError> {
        if snapshot.stamp().store_id != *ctx.store_id()
            || snapshot.stamp().restore_epoch != ctx.restore_epoch().get()
            || snapshot.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
        {
            return Err(StorageError::ScopeDenied);
        }
        let binding = match ctx.principal_kind() {
            PrincipalKind::Human | PrincipalKind::Host => None,
            _ => snapshot.authority().principal(ctx.principal())?,
        };
        let policy =
            crate::authority::policy::resolve_policy(snapshot.configuration(), ctx.project_id())?;
        let scopes = std::iter::once(ConfigScope::Workspace)
            .chain(ctx.project_id().cloned().map(ConfigScope::Project));
        let mut selected = Vec::new();
        for scope in scopes {
            let (_, state) = current(snapshot.configuration(), &scope)?;
            for server in state.servers {
                if !matches!(server.public.state, ItemState::Healthy) {
                    continue;
                }
                for cap in &server.capabilities {
                    if cap.kind.0 != "tool" || !server.public.selected_tool_ids.0.contains(&cap.id)
                    {
                        continue;
                    }
                    let Some(reference) = &cap.input_schema else {
                        continue;
                    };
                    let digest =
                        cap.id.as_str().strip_prefix("mcp-").ok_or_else(|| {
                            StorageError::InvalidRecord("invalid MCP tool ID".into())
                        })?;
                    let capability =
                        QualifiedId::try_from(format!("mcp/{digest}")).map_err(|_| {
                            StorageError::InvalidRecord("invalid MCP capability ID".into())
                        })?;
                    if !matches!(
                        crate::authority::policy::outgoing_mcp_tool_visibility(
                            ctx,
                            binding.as_ref(),
                            &policy,
                            capability.as_str(),
                        ),
                        crate::authority::policy::Verdict::Allow
                    ) {
                        continue;
                    }
                    let schema = server
                        .schemas
                        .get(reference.schema_id.as_str())
                        .ok_or_else(|| {
                            StorageError::InvalidRecord("selected MCP schema missing".into())
                        })?;
                    let canonical = crate::jcs::canonicalize(schema).map_err(|_| {
                        StorageError::InvalidRecord("selected MCP schema invalid".into())
                    })?;
                    if sha256_hex(canonical.as_bytes()) != reference.sha256.as_str() {
                        return Err(StorageError::InvalidRecord(
                            "selected MCP schema changed".into(),
                        ));
                    }
                    selected.push(McpToolMaterial {
                        alias: ToolSchema {
                            capability_id: capability,
                            wire_name: format!("mcp_{}", &digest[..60]),
                            input_schema: reference.clone(),
                        },
                        schema: schema.clone(),
                        description: format!(
                            "MCP {}: {}. {}",
                            server.public.name.0, cap.name.0, cap.description.0
                        ),
                        server_id: server.public.id.clone(),
                        server_scope: scope.clone(),
                        server_revision: server.public.revision.clone(),
                        catalog_revision: server.public.catalog_revision.clone(),
                        capability_id: cap.id.clone(),
                        wire_name: cap.name.0.clone(),
                    });
                }
            }
        }
        selected.sort_by(|a, b| {
            a.alias
                .capability_id
                .as_str()
                .cmp(b.alias.capability_id.as_str())
        });
        Ok(selected)
    }

    /// Queue one provider-selected external tool under a stable call identity. A retry returns
    /// the existing operation; only the dispatcher may send its journaled effect.
    pub fn queue_selected_tool(
        &self,
        ctx: &InvocationContext,
        call: &ToolCallKey,
        alias: &ToolSchema,
        arguments: Value,
    ) -> Result<OperationId, StorageError> {
        let Some(native) = self.native.get() else {
            return Err(StorageError::ScopeDenied);
        };
        let Some(session_id) = ctx.session_id() else {
            return Err(StorageError::ScopeDenied);
        };
        if !matches!(
            ctx.principal_kind(),
            PrincipalKind::Agent | PrincipalKind::Host
        ) {
            return Err(StorageError::ScopeDenied);
        }
        let Value::Object(_) = &arguments else {
            return Err(StorageError::InvalidRecord(
                "MCP tool arguments must be an object".into(),
            ));
        };
        let canonical = crate::jcs::canonicalize(&arguments)
            .map_err(|_| StorageError::InvalidRecord("invalid MCP tool arguments".into()))?;
        if canonical.len() > 8 * 1024 * 1024 {
            return Err(StorageError::InvalidRecord(
                "MCP tool arguments exceed bound".into(),
            ));
        }
        let identity = sha256_hex(
            format!(
                "{}\0{}\0{}\0{}",
                session_id.as_str(),
                call.attempt_id.0.as_str(),
                call.native_call_id,
                call.ordinal.as_str()
            )
            .as_bytes(),
        );
        let operation_id =
            OperationId(Id::new(format!("mcp-{identity}")).expect("bounded digest ID"));
        let digest = sha256_hex(
            format!(
                "{}\0{}\0{}",
                alias.capability_id.as_str(),
                alias.input_schema.sha256.as_str(),
                canonical
            )
            .as_bytes(),
        );
        if let Some(existing) = read_snapshot(&*native.store, &ReadOptions::default(), |s| {
            s.operations().operation(&operation_id)
        })? {
            return if existing.capability == "mcp.tool" && existing.args_digest == digest {
                Ok(operation_id)
            } else {
                Err(StorageError::ScopeDenied)
            };
        }
        let (material, fences) =
            read_snapshot(&*native.store, &ReadOptions::default(), |snapshot| {
                let material = self
                    .selected_tool_materials(ctx, snapshot)?
                    .into_iter()
                    .find(|material| {
                        material.alias.capability_id.as_str() == alias.capability_id.as_str()
                            && material.alias.wire_name == alias.wire_name
                            && material.alias.input_schema.schema_id == alias.input_schema.schema_id
                            && material.alias.input_schema.revision == alias.input_schema.revision
                            && material.alias.input_schema.sha256 == alias.input_schema.sha256
                    })
                    .ok_or(StorageError::ScopeDenied)?;
                let policy = crate::authority::policy::resolve_policy(
                    snapshot.configuration(),
                    ctx.project_id(),
                )?;
                let mut fences = vec![AuthorityFence::Policy {
                    project_id: ctx.project_id().cloned(),
                    digest: policy.digest(),
                }];
                if ctx.principal_kind() == PrincipalKind::Agent {
                    let binding = snapshot
                        .authority()
                        .principal(ctx.principal())?
                        .ok_or(StorageError::ScopeDenied)?;
                    fences.push(AuthorityFence::Principal {
                        actor: ctx.principal().clone(),
                        revision: binding.revision,
                    });
                }
                Ok((material, fences))
            })?;
        native
            .validator
            .validate(&material.schema, &arguments)
            .map_err(|_| {
                StorageError::InvalidRecord("MCP tool arguments failed the selected schema".into())
            })?;
        let ctx = ctx.clone();
        let call = call.clone();
        let operation_for_write = operation_id.clone();
        let digest_for_write = digest.clone();
        crate::ports::storage::run_write(
            &*native.store,
            crate::ports::storage::WriteOptions::default(),
            move |tx| {
                if let Some(existing) = tx.operations().operation(&operation_for_write)? {
                    return if existing.capability == "mcp.tool"
                        && existing.args_digest == digest_for_write
                    {
                        Ok(operation_for_write)
                    } else {
                        Err(StorageError::ScopeDenied)
                    };
                }
                if tx.store().store_id != *ctx.store_id()
                    || tx.store().restore_epoch != ctx.restore_epoch().get()
                    || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                    || !crate::authority::fences_hold(tx, &fences)?
                {
                    return Err(StorageError::ScopeDenied);
                }
                let session = tx
                    .sessions()
                    .session(ctx.session_id().ok_or(StorageError::ScopeDenied)?)?
                    .ok_or(StorageError::ScopeDenied)?;
                if session.closed() || session.agent_id != *ctx.principal() {
                    return Err(StorageError::ScopeDenied);
                }
                let (_, state) = current(tx.configuration(), &material.server_scope)?;
                let server = state
                    .servers
                    .iter()
                    .find(|server| {
                        server.public.id == material.server_id
                            && server.public.revision == material.server_revision
                            && server.public.catalog_revision == material.catalog_revision
                    })
                    .ok_or(StorageError::ScopeDenied)?;
                if !server
                    .public
                    .selected_tool_ids
                    .0
                    .contains(&material.capability_id)
                    || !server.capabilities.iter().any(|cap| {
                        cap.id == material.capability_id
                            && cap.input_schema.as_ref().is_some_and(|schema| {
                                schema.sha256 == material.alias.input_schema.sha256
                            })
                    })
                {
                    return Err(StorageError::ScopeDenied);
                }
                let event_id = EventId(tx.new_id(IdKind::Event));
                tx.operations().record_intent(NewOperation {
                id: operation_for_write.clone(), root_id: ctx.root_id().cloned(), assignment_id: ctx.assignment_id().cloned(),
                capability: "mcp.tool".into(), schema_version: 1, args_digest: digest_for_write,
                payload: json!({
                    "scope":material.server_scope.key(),"serverId":material.server_id,
                    "serverRevision":material.server_revision.get(),"catalogRevision":material.catalog_revision.get(),
                    "capabilityId":material.capability_id,"schema":material.alias.input_schema,
                    "wireName":material.wire_name,"arguments":arguments,
                    "principal":ctx.principal(),"principalKind":format!("{:?}",ctx.principal_kind()),
                    "sessionId":ctx.session_id(),"projectId":ctx.project_id(),"fences":fences,"call":call,
                }),
            })?;
                tx.history().append(NewEvent {
                id: event_id.clone(), actor: ctx.principal().clone(), project_id: ctx.project_id().cloned(),
                root_id: ctx.root_id().cloned(), operation_id: Some(operation_for_write.clone()),
                aggregate_kind: "mcp_tool_call".into(), aggregate_id: operation_for_write.0.as_str().into(),
                aggregate_revision: 1, kind: "McpToolRequested".into(),
                payload: json!({"serverId":material.server_id,"capabilityId":material.capability_id,"call":call}),
            })?;
                let queue_id = tx.new_id(IdKind::QueueEntry);
                let now = tx.now_ms();
                tx.queue().enqueue(NewQueueEntry {
                    id: queue_id,
                    consumer: EFFECT_CONSUMER.into(),
                    event_id,
                    root_id: ctx.root_id().cloned(),
                    operation_id: Some(operation_for_write.clone()),
                    partition: None,
                    priority: 0,
                    ready_at_ms: now,
                    payload: json!({"serverId":material.server_id,"call":call}),
                })?;
                Ok(operation_for_write)
            },
        )
    }

    /// Run only after the dispatch journal has committed `Dispatching`. The saved scope and
    /// revision fence prevent a queued connect from starting a server after its configuration
    /// changes; authority fences are checked again immediately before the native call.
    pub fn execute(self: &Arc<Self>, op: &OperationRecord) -> McpEffectOutcome {
        if op.state != OperationState::Dispatching {
            return McpEffectOutcome::Failed("MCP effect was not dispatched".into());
        }
        if op.capability == "mcp.tool" {
            return self.execute_tool(op);
        }
        if matches!(op.capability.as_str(), "mcp.disable" | "mcp.remove") {
            return self.execute_disconnect(op);
        }
        if !matches!(op.capability.as_str(), "mcp.connect" | "mcp.discover") {
            return McpEffectOutcome::Failed("unsupported MCP effect".into());
        }
        let Some(native) = self.native.get() else {
            return McpEffectOutcome::NotApplied("native MCP client is unavailable".into());
        };
        let Some(scope) = op
            .payload
            .get("scope")
            .and_then(Value::as_str)
            .and_then(ConfigScope::from_key)
        else {
            return McpEffectOutcome::Failed("invalid MCP effect scope".into());
        };
        let Some(server_id) = op
            .payload
            .get("serverId")
            .cloned()
            .and_then(|v| serde_json::from_value::<Id>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP effect server".into());
        };
        let Some(pending_revision) = op.payload.get("pendingRevision").and_then(Value::as_i64)
        else {
            return McpEffectOutcome::Failed("invalid MCP effect revision".into());
        };
        let Some(principal) = op
            .payload
            .get("principal")
            .cloned()
            .and_then(|v| serde_json::from_value::<ActorId>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP effect principal".into());
        };
        let kind = match op.payload.get("principalKind").and_then(Value::as_str) {
            Some("Human") => PrincipalKind::Human,
            Some("Agent") => PrincipalKind::Agent,
            Some("Package") => PrincipalKind::Package,
            Some("McpClient") => PrincipalKind::McpClient,
            Some("Schedule") => PrincipalKind::Schedule,
            Some("Host") => PrincipalKind::Host,
            _ => return McpEffectOutcome::Failed("invalid MCP principal kind".into()),
        };
        let Some(fences) = op
            .payload
            .get("fences")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<AuthorityFence>>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP authority fence".into());
        };
        let server = match read_snapshot(&*native.store, &ReadOptions::default(), |snapshot| {
            if snapshot.operations().operation(&op.id)?.as_ref() != Some(op) {
                return Err(StorageError::InvalidRecord(
                    "MCP effect journal fence changed".into(),
                ));
            }
            if !crate::authority::authorization_holds(
                snapshot,
                &Authorization::direct().fenced_by(fences),
            )? {
                return Err(StorageError::InvalidRecord(
                    "MCP authority changed before dispatch".into(),
                ));
            }
            if let Some(project) = scope.project_id()
                && snapshot
                    .projects()
                    .project(project)?
                    .as_ref()
                    .is_none_or(|p| p.lifecycle != crate::ports::storage::ACTIVE)
            {
                return Err(StorageError::ScopeDenied);
            }
            let (_, state) = current(snapshot.configuration(), &scope)?;
            state
                .servers
                .into_iter()
                .find(|server| {
                    server.public.id == server_id
                        && server.public.revision.get() == pending_revision
                })
                .ok_or_else(|| {
                    StorageError::InvalidRecord("MCP configuration changed before dispatch".into())
                })
        }) {
            Ok(server) => server,
            Err(e) => {
                self.settle_server_failure(
                    native,
                    op,
                    &scope,
                    &server_id,
                    pending_revision,
                    "MCP authority or configuration changed before dispatch".into(),
                    "McpDispatchRefused",
                );
                return McpEffectOutcome::Failed(e.to_string());
            }
        };
        let info = native.store.info();
        let mut context = InvocationContext::new(
            principal,
            kind,
            op.id.0.clone(),
            Id::new("mcp-effects").expect("static MCP ingress"),
            info.store_id,
            Counter::new(info.restore_epoch).expect("valid store epoch"),
            rev(1),
        )
        .with_lineage(
            scope.project_id().cloned(),
            op.root_id.clone(),
            op.assignment_id.clone(),
        );
        if let Some(session) = op
            .payload
            .get("sessionId")
            .cloned()
            .and_then(|v| serde_json::from_value::<Option<Id>>(v).ok())
            .flatten()
        {
            context = context.with_session(session);
        }
        if op.capability == "mcp.discover" {
            let Some(catalog_revision) = op.payload.get("catalogRevision").and_then(Value::as_i64)
            else {
                return McpEffectOutcome::Failed("invalid MCP discovery catalog revision".into());
            };
            if server.public.catalog_revision.get() != catalog_revision {
                return McpEffectOutcome::Failed("MCP catalog changed before discovery".into());
            }
            return self.execute_discover(
                native,
                op,
                scope,
                server_id,
                pending_revision,
                server,
                context,
            );
        }
        let endpoint = server.endpoint(&scope);
        let outcome = native.remote.connect(&context, &endpoint);
        let successful = outcome.is_ok();
        let failure = outcome
            .as_ref()
            .err()
            .map(|fault| BoundedText(connection_failure(&native.remote, &server.transport, fault)));
        let new_state = if successful {
            ItemState::Healthy
        } else {
            ItemState::Failed
        };
        let scope_for_write = scope.clone();
        let server_for_write = server_id.clone();
        let root_for_write = op.root_id.clone();
        let operation_for_write = op.id.clone();
        let settled = crate::ports::storage::run_write(
            &*native.store,
            crate::ports::storage::WriteOptions::default(),
            move |tx| {
                let (version, mut state) = current(tx.configuration(), &scope_for_write)?;
                let server = state
                    .servers
                    .iter_mut()
                    .find(|server| {
                        server.public.id == server_for_write
                            && server.public.revision.get() == pending_revision
                    })
                    .ok_or_else(|| {
                        StorageError::InvalidRecord(
                            "MCP configuration changed during connect".into(),
                        )
                    })?;
                server.public.revision = rev(pending_revision + 1);
                server.public.state = new_state;
                server.public.last_failure = failure;
                let result = server.public.clone();
                let body = serde_json::to_value(state).map_err(|_| {
                    StorageError::InvalidRecord("outgoing MCP configuration".into())
                })?;
                tx.configuration().append_revision(
                    &scope_for_write,
                    NAMESPACE,
                    version,
                    SCHEMA_VERSION,
                    body,
                )?;
                let actor = tx.store().host_actor.clone();
                let event_id = EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent {
                    id: event_id,
                    actor,
                    project_id: scope_for_write.project_id().cloned(),
                    root_id: root_for_write,
                    operation_id: Some(operation_for_write),
                    aggregate_kind: "mcp_server".into(),
                    aggregate_id: server_for_write.as_str().into(),
                    aggregate_revision: pending_revision + 1,
                    kind: if successful {
                        "McpConnected"
                    } else {
                        "McpConnectFailed"
                    }
                    .into(),
                    payload: json!({"serverId":server_for_write,"scope":scope_for_write.key()}),
                })?;
                Ok(result)
            },
        );
        match settled {
            Ok(server) => match outcome {
                Ok(()) => {
                    McpEffectOutcome::Completed(serde_json::to_value(server).unwrap_or(Value::Null))
                }
                Err(fault) => McpEffectOutcome::Failed(format!("MCP connection failed: {fault:?}")),
            },
            Err(e) => {
                if outcome.is_ok() {
                    let _ = native.remote.disconnect(&server_id);
                }
                McpEffectOutcome::Unknown(format!(
                    "MCP connection outcome could not be recorded: {e}"
                ))
            }
        }
    }

    /// Close the live session of a server a human disabled or removed. The disconnect happens
    /// only while the server is still disabled or still gone, so a reconnect the human asked for
    /// after this operation keeps its session. Repeating it is harmless.
    fn execute_disconnect(&self, op: &OperationRecord) -> McpEffectOutcome {
        let Some(native) = self.native.get() else {
            return McpEffectOutcome::NotApplied("native MCP client is unavailable".into());
        };
        let (Some(scope), Some(server_id), Some(server)) = (
            op.payload
                .get("scope")
                .and_then(Value::as_str)
                .and_then(ConfigScope::from_key),
            op.payload
                .get("serverId")
                .cloned()
                .and_then(|v| serde_json::from_value::<Id>(v).ok()),
            op.payload.get("server").cloned(),
        ) else {
            return McpEffectOutcome::Failed("invalid MCP disconnect payload".into());
        };
        let removing = op.capability == "mcp.remove";
        // The check runs while the remote holds the server's session lock, so an enable that
        // lands first keeps its session and one that lands later reconnects after the close.
        let mut read_error = None;
        let mut still_off =
            || match read_snapshot(&*native.store, &ReadOptions::default(), |snapshot| {
                let (_, state) = current(snapshot.configuration(), &scope)?;
                Ok(
                    match state.servers.iter().find(|s| s.public.id == server_id) {
                        None => true,
                        Some(saved) => !removing && saved.public.disabled == Some(true),
                    },
                )
            }) {
                Ok(off) => off,
                Err(error) => {
                    read_error = Some(error);
                    false
                }
            };
        let closed = native.remote.disconnect_if(&server_id, &mut still_off);
        if let Some(error) = read_error {
            return McpEffectOutcome::NotApplied(format!(
                "MCP configuration could not be read before disconnect: {error}"
            ));
        }
        match closed {
            Ok(()) => McpEffectOutcome::Completed(server),
            Err(fault) => {
                McpEffectOutcome::Unknown(format!("MCP session did not close: {fault:?}"))
            }
        }
    }

    fn execute_tool(&self, op: &OperationRecord) -> McpEffectOutcome {
        let Some(native) = self.native.get() else {
            return McpEffectOutcome::NotApplied("native MCP client is unavailable".into());
        };
        let Some(scope) = op
            .payload
            .get("scope")
            .and_then(Value::as_str)
            .and_then(ConfigScope::from_key)
        else {
            return McpEffectOutcome::Failed("invalid MCP tool scope".into());
        };
        let Some(server_id) = op
            .payload
            .get("serverId")
            .cloned()
            .and_then(|v| serde_json::from_value::<Id>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP tool server".into());
        };
        let Some(capability_id) = op
            .payload
            .get("capabilityId")
            .cloned()
            .and_then(|v| serde_json::from_value::<Id>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP tool capability".into());
        };
        let Some(reference) = op
            .payload
            .get("schema")
            .cloned()
            .and_then(|v| serde_json::from_value::<SchemaRef>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP tool schema".into());
        };
        let Some(wire_name) = op.payload.get("wireName").and_then(Value::as_str) else {
            return McpEffectOutcome::Failed("invalid MCP tool name".into());
        };
        let Some(arguments @ Value::Object(_)) = op.payload.get("arguments").cloned() else {
            return McpEffectOutcome::Failed("invalid MCP tool arguments".into());
        };
        let Some(server_revision) = op.payload.get("serverRevision").and_then(Value::as_i64) else {
            return McpEffectOutcome::Failed("invalid MCP server revision".into());
        };
        let Some(catalog_revision) = op.payload.get("catalogRevision").and_then(Value::as_i64)
        else {
            return McpEffectOutcome::Failed("invalid MCP catalog revision".into());
        };
        let Some(principal) = op
            .payload
            .get("principal")
            .cloned()
            .and_then(|v| serde_json::from_value::<ActorId>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP tool principal".into());
        };
        let principal_kind = match op.payload.get("principalKind").and_then(Value::as_str) {
            Some("Agent") => PrincipalKind::Agent,
            Some("Host") => PrincipalKind::Host,
            _ => return McpEffectOutcome::Failed("invalid MCP tool principal kind".into()),
        };
        let Some(session_id) = op
            .payload
            .get("sessionId")
            .cloned()
            .and_then(|v| serde_json::from_value::<Id>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP tool session".into());
        };
        let Some(project_id) = op
            .payload
            .get("projectId")
            .cloned()
            .and_then(|v| serde_json::from_value::<Option<ProjectId>>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP tool project".into());
        };
        let Some(fences) = op
            .payload
            .get("fences")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<AuthorityFence>>(v).ok())
        else {
            return McpEffectOutcome::Failed("invalid MCP tool authority fences".into());
        };
        let info = native.store.info();
        let context = InvocationContext::new(
            principal,
            principal_kind,
            op.id.0.clone(),
            Id::new("mcp-effects").expect("static ingress"),
            info.store_id,
            Counter::new(info.restore_epoch).expect("valid store epoch"),
            rev(1),
        )
        .with_lineage(project_id, op.root_id.clone(), op.assignment_id.clone())
        .with_session(session_id.clone());
        let selected = read_snapshot(&*native.store, &ReadOptions::default(), |snapshot| {
            if snapshot.operations().operation(&op.id)?.as_ref() != Some(op)
                || !crate::authority::authorization_holds(
                    snapshot,
                    &Authorization::direct().fenced_by(fences.clone()),
                )?
            {
                return Err(StorageError::ScopeDenied);
            }
            let session = snapshot
                .sessions()
                .session(&session_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if session.closed()
                || session.agent_id != *context.principal()
                || session.project_id.as_ref() != context.project_id()
            {
                return Err(StorageError::ScopeDenied);
            }
            self.selected_tool_materials(&context, snapshot)?
                .into_iter()
                .find(|material| {
                    material.server_scope == scope
                        && material.server_id == server_id
                        && material.server_revision.get() == server_revision
                        && material.catalog_revision.get() == catalog_revision
                        && material.capability_id == capability_id
                        && material.wire_name == wire_name
                        && material.alias.input_schema.schema_id == reference.schema_id
                        && material.alias.input_schema.revision == reference.revision
                        && material.alias.input_schema.sha256 == reference.sha256
                })
                .ok_or(StorageError::ScopeDenied)
        });
        let material = match selected {
            Ok(material) => material,
            Err(error) => {
                return McpEffectOutcome::Failed(format!("MCP tool authority changed: {error}"));
            }
        };
        if native
            .validator
            .validate(&material.schema, &arguments)
            .is_err()
        {
            return McpEffectOutcome::Failed(
                "MCP tool arguments no longer match the selected schema".into(),
            );
        }
        let Some(attempt) = op.latest_attempt() else {
            return McpEffectOutcome::Failed("MCP tool was not dispatched".into());
        };
        let origin = McpCallOrigin {
            server_id: server_id.clone(),
            operation_id: op.id.clone(),
            attempt_id: attempt.id.0.clone(),
        };
        let mut result = native
            .remote
            .call_tool_with_origin(&origin, wire_name, arguments.clone());
        if matches!(result, Err(McpRemoteFault::Unreachable)) {
            let endpoint =
                match read_snapshot(&*native.store, &ReadOptions::default(), |snapshot| {
                    let (_, state) = current(snapshot.configuration(), &scope)?;
                    state
                        .servers
                        .into_iter()
                        .find(|server| {
                            server.public.id == server_id
                                && server.public.revision.get() == server_revision
                        })
                        .map(|server| server.endpoint(&scope))
                        .ok_or(StorageError::ScopeDenied)
                }) {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        return McpEffectOutcome::Failed(format!(
                            "MCP server changed before reconnect: {error}"
                        ));
                    }
                };
            result = match native.remote.connect(&context, &endpoint) {
                Ok(()) => {
                    let still_authorized =
                        read_snapshot(&*native.store, &ReadOptions::default(), |snapshot| {
                            if snapshot.operations().operation(&op.id)?.as_ref() != Some(op)
                                || !crate::authority::authorization_holds(
                                    snapshot,
                                    &Authorization::direct().fenced_by(fences.clone()),
                                )?
                            {
                                return Err(StorageError::ScopeDenied);
                            }
                            self.selected_tool_materials(&context, snapshot)?
                                .into_iter()
                                .find(|candidate| {
                                    candidate.server_id == server_id
                                        && candidate.server_revision.get() == server_revision
                                        && candidate.catalog_revision.get() == catalog_revision
                                        && candidate.capability_id == capability_id
                                        && candidate.alias.input_schema.sha256 == reference.sha256
                                })
                                .ok_or(StorageError::ScopeDenied)
                        });
                    if still_authorized.is_err() {
                        // A server disabled or removed while this call reconnected must not keep
                        // the session this call opened.
                        let serving =
                            read_snapshot(&*native.store, &ReadOptions::default(), |snapshot| {
                                let (_, state) = current(snapshot.configuration(), &scope)?;
                                Ok(state.servers.iter().any(|server| {
                                    server.public.id == server_id
                                        && matches!(server.public.state, ItemState::Healthy)
                                }))
                            });
                        if !matches!(serving, Ok(true)) {
                            let _ = native.remote.disconnect(&server_id);
                        }
                        return McpEffectOutcome::Failed(
                            "MCP tool authority changed during reconnect".into(),
                        );
                    }
                    native
                        .remote
                        .call_tool_with_origin(&origin, wire_name, arguments)
                }
                Err(fault) => Err(fault),
            };
        }
        match result {
            Ok(value) => McpEffectOutcome::Completed(value),
            Err(
                McpRemoteFault::Deadline
                | McpRemoteFault::Protocol
                | McpRemoteFault::TooLarge
                | McpRemoteFault::InputRequired,
            ) => McpEffectOutcome::Unknown(
                "MCP tool may have run; reconcile its result before retry".into(),
            ),
            Err(McpRemoteFault::Rejected) => {
                McpEffectOutcome::Failed("MCP server answered the tool call with an error".into())
            }
            Err(fault) => McpEffectOutcome::Failed(format!("MCP tool was not sent: {fault:?}")),
        }
    }

    /// Attach the router-backed sampler to the one native connection owner.
    pub fn install_sampler(&self, sampler: Arc<dyn McpSampler>) -> Result<(), &'static str> {
        let native = self
            .native
            .get()
            .ok_or("outgoing MCP remote is not installed")?;
        native.remote.install_sampler(sampler);
        Ok(())
    }

    /// Re-authorize one sampling request against its dispatching `mcp.tool` journal and the
    /// server's current human selection. The decision is recorded on the tool call without
    /// any prompt text.
    pub fn authorize_sampling(
        &self,
        origin: &McpCallOrigin,
        ordinal: u8,
    ) -> Result<SamplingGrant, SamplingRefusal> {
        let native = self
            .native
            .get()
            .ok_or(SamplingRefusal::Unauthorized("mcp_runtime"))?;
        let info = native.store.info();
        let origin = origin.clone();
        let decided = crate::ports::storage::run_write(
            &*native.store,
            crate::ports::storage::WriteOptions::default(),
            move |tx| {
                let Some(op) = tx.operations().operation(&origin.operation_id)? else {
                    return Ok(Err("mcp_sampling_call"));
                };
                let decision = sampling_decision(tx, &op, &origin, &info)?;
                let actor = tx.store().host_actor.clone();
                let event_id = EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent {
                    id: event_id,
                    actor,
                    project_id: op
                        .payload
                        .get("projectId")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .flatten(),
                    root_id: op.root_id.clone(),
                    operation_id: Some(op.id.clone()),
                    aggregate_kind: "mcp_tool_call".into(),
                    aggregate_id: op.id.0.as_str().into(),
                    aggregate_revision: 1 + i64::from(ordinal),
                    kind: if decision.is_ok() {
                        "McpSamplingAccepted"
                    } else {
                        "McpSamplingRefused"
                    }
                    .into(),
                    payload: json!({"serverId":origin.server_id,"attemptId":origin.attempt_id,
                        "ordinal":ordinal,"reason":decision.as_ref().err()}),
                })?;
                Ok(decision)
            },
        );
        match decided {
            Ok(Ok(grant)) => Ok(grant),
            Ok(Err(reason)) => Err(SamplingRefusal::Unauthorized(reason)),
            Err(_) => Err(SamplingRefusal::Unauthorized("mcp_sampling_unavailable")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_discover(
        self: &Arc<Self>,
        native: &Native,
        op: &OperationRecord,
        scope: ConfigScope,
        server_id: Id,
        pending_revision: i64,
        server: StoredServer,
        context: InvocationContext,
    ) -> McpEffectOutcome {
        // A saved transport may have changed while the native session stayed alive. Reconnect
        // under the current revision fence before reading a catalog from that server ID.
        let endpoint = server.endpoint(&scope);
        let discovery = native
            .remote
            .connect(&context, &endpoint)
            .and_then(|_| native.remote.discover(&server_id));
        let catalog = match discovery {
            Ok(catalog) => catalog,
            Err(fault) => {
                self.settle_server_failure(
                    native,
                    op,
                    &scope,
                    &server_id,
                    pending_revision,
                    format!("Discovery failed: {fault:?}"),
                    "McpDiscoveryFailed",
                );
                return McpEffectOutcome::Failed(format!("MCP discovery failed: {fault:?}"));
            }
        };
        let service = self.clone();
        let scope_for_write = scope.clone();
        let server_for_write = server_id.clone();
        let root_for_write = op.root_id.clone();
        let operation_for_write = op.id.clone();
        let expected_catalog = server.public.catalog_revision;
        let published = crate::ports::storage::run_write(
            &*native.store,
            crate::ports::storage::WriteOptions::default(),
            move |tx| {
                let (_, state) = current(tx.configuration(), &scope_for_write)?;
                if !state.servers.iter().any(|server| {
                    server.public.id == server_for_write
                        && server.public.revision.get() == pending_revision
                        && server.public.catalog_revision == expected_catalog
                        && server.public.disabled != Some(true)
                }) {
                    return Err(StorageError::InvalidRecord(
                        "MCP configuration changed during discovery".into(),
                    ));
                }
                let result = service.publish_remote_catalog(
                    tx,
                    &scope_for_write,
                    &server_for_write,
                    &expected_catalog,
                    catalog,
                )?;
                let actor = tx.store().host_actor.clone();
                let event_id = EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent {
                    id: event_id,
                    actor,
                    project_id: scope_for_write.project_id().cloned(),
                    root_id: root_for_write,
                    operation_id: Some(operation_for_write),
                    aggregate_kind: "mcp_server".into(),
                    aggregate_id: server_for_write.as_str().into(),
                    aggregate_revision: result.revision.get(),
                    kind: "McpCatalogDiscovered".into(),
                    payload: json!({"serverId":server_for_write,"scope":scope_for_write.key(),"catalogRevision":result.catalog_revision.get()}),
                })?;
                Ok(result)
            },
        );
        match published {
            Ok(server) => {
                McpEffectOutcome::Completed(serde_json::to_value(server).unwrap_or(Value::Null))
            }
            Err(error) => {
                self.settle_server_failure(
                    native,
                    op,
                    &scope,
                    &server_id,
                    pending_revision,
                    "Discovery could not publish its catalog".into(),
                    "McpDiscoveryFailed",
                );
                McpEffectOutcome::Unknown(format!(
                    "MCP discovery publication could not be confirmed: {error}"
                ))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn settle_server_failure(
        &self,
        native: &Native,
        op: &OperationRecord,
        scope: &ConfigScope,
        server_id: &Id,
        pending_revision: i64,
        failure: String,
        event_kind: &'static str,
    ) {
        let scope = scope.clone();
        let server_id = server_id.clone();
        let root = op.root_id.clone();
        let operation = op.id.clone();
        let _ = crate::ports::storage::run_write(
            &*native.store,
            crate::ports::storage::WriteOptions::default(),
            move |tx| {
                let (version, mut state) = current(tx.configuration(), &scope)?;
                let server = state
                    .servers
                    .iter_mut()
                    .find(|s| {
                        s.public.id == server_id && s.public.revision.get() == pending_revision
                    })
                    .ok_or_else(|| {
                        StorageError::InvalidRecord(
                            "MCP server changed during failed discovery".into(),
                        )
                    })?;
                let same_failure = matches!(server.public.state, ItemState::Failed)
                    && server
                        .public
                        .last_failure
                        .as_ref()
                        .is_some_and(|last| last.0 == failure);
                let result_revision = if same_failure {
                    pending_revision
                } else {
                    server.public.revision = rev(pending_revision + 1);
                    server.public.state = ItemState::Failed;
                    server.public.last_failure = Some(BoundedText(failure));
                    let body = serde_json::to_value(state).map_err(|_| {
                        StorageError::InvalidRecord("outgoing MCP configuration".into())
                    })?;
                    tx.configuration().append_revision(
                        &scope,
                        NAMESPACE,
                        version,
                        SCHEMA_VERSION,
                        body,
                    )?;
                    pending_revision + 1
                };
                let actor = tx.store().host_actor.clone();
                let event_id = EventId(tx.new_id(IdKind::Event));
                tx.history().append(NewEvent {
                    id: event_id,
                    actor,
                    project_id: scope.project_id().cloned(),
                    root_id: root,
                    operation_id: Some(operation),
                    aggregate_kind: "mcp_server".into(),
                    aggregate_id: server_id.as_str().into(),
                    aggregate_revision: result_revision,
                    kind: event_kind.into(),
                    payload: json!({"serverId":server_id,"scope":scope.key()}),
                })?;
                Ok(())
            },
        );
    }

    pub fn observe(&self, op: &OperationRecord) -> McpEffectOutcome {
        if matches!(op.capability.as_str(), "mcp.disable" | "mcp.remove") {
            return self.execute_disconnect(op);
        }
        if matches!(op.capability.as_str(), "mcp.connect" | "mcp.discover")
            && let Some(native) = self.native.get()
            && let (Some(scope), Some(server_id), Some(pending_revision)) = (
                op.payload
                    .get("scope")
                    .and_then(Value::as_str)
                    .and_then(ConfigScope::from_key),
                op.payload
                    .get("serverId")
                    .cloned()
                    .and_then(|v| serde_json::from_value::<Id>(v).ok()),
                op.payload.get("pendingRevision").and_then(Value::as_i64),
            )
        {
            self.settle_server_failure(
                native,
                op,
                &scope,
                &server_id,
                pending_revision,
                "Previous MCP operation outcome could not be confirmed; reconnect before use"
                    .into(),
                "McpOutcomeUnconfirmed",
            );
        }
        McpEffectOutcome::Unknown(
            "MCP connection outcome cannot be established after interruption".into(),
        )
    }

    fn fault(&self, error: impl Into<Failure>) -> DomainFault {
        query_fault(
            error.into(),
            "mcp_server",
            CorrelationId(self.ids.next_id(IdKind::Correlation)),
        )
    }

    /// Publish one discovered catalog after the external handshake has completed. An effect
    /// worker calls this in its own write unit, fenced by the catalog revision it read before
    /// contacting the server. A new read-only hint sets the initial user selection only; it
    /// never marks a tool as a verified native read or grants authority to invoke it.
    pub fn publish_catalog(
        &self,
        tx: &mut dyn WriteTransaction,
        scope: &ConfigScope,
        server_id: &Id,
        expected_catalog_revision: &Revision,
        capabilities: Vec<McpCapability>,
    ) -> Result<McpServer, StorageError> {
        self.write_catalog(
            tx,
            scope,
            server_id,
            expected_catalog_revision,
            capabilities,
            BTreeMap::new(),
            BTreeMap::new(),
        )
    }

    /// Convert an untrusted wire catalog into stable, server-qualified identities and immutable
    /// schema digests before publishing it. Raw schemas stay in the scoped configuration so a
    /// later selected invocation can validate against the exact catalog revision it named.
    pub fn publish_remote_catalog(
        &self,
        tx: &mut dyn WriteTransaction,
        scope: &ConfigScope,
        server_id: &Id,
        expected_catalog_revision: &Revision,
        catalog: RemoteCatalog,
    ) -> Result<McpServer, StorageError> {
        let (_, state) = current(tx.configuration(), scope)?;
        let previous = state
            .servers
            .iter()
            .find(|server| &server.public.id == server_id)
            .ok_or_else(|| StorageError::NotFound {
                kind: "mcp_server",
                id: server_id.as_str().into(),
            })?;
        let mut capabilities = Vec::new();
        let mut schemas = BTreeMap::new();
        let mut resource_uris = BTreeMap::new();
        for tool in catalog.tools {
            let id = capability_id(server_id, "tool", &tool.name)?;
            let input_schema =
                schema_ref(server_id, "tool", &tool.name, "input", &tool.input_schema)?;
            schemas.insert(input_schema.schema_id.as_str().into(), tool.input_schema);
            let output_schema = tool
                .output_schema
                .map(|schema| {
                    let reference = schema_ref(server_id, "tool", &tool.name, "output", &schema)?;
                    schemas.insert(reference.schema_id.as_str().into(), schema);
                    Ok(reference)
                })
                .transpose()?;
            capabilities.push(capability(
                previous,
                id,
                server_id,
                "tool",
                tool.name,
                tool.description,
                Some(input_schema),
                output_schema,
                tool.read_only_hint,
                None,
                tool.title.filter(|title| {
                    !title.is_empty() && title.chars().count() <= 256 && !title.contains('\0')
                }),
                tool.destructive_hint,
            )?);
        }
        for resource in catalog.resources {
            let id = capability_id(server_id, "resource", &resource.uri)?;
            if resource.uri.is_empty() || resource.uri.len() > 4096 {
                return Err(StorageError::InvalidRecord(
                    "invalid MCP resource URI".into(),
                ));
            }
            resource_uris.insert(id.as_str().to_owned(), resource.uri.clone());
            capabilities.push(capability(
                previous,
                id,
                server_id,
                "resource",
                resource.name,
                resource.description,
                None,
                None,
                false,
                Some(resource.uri),
                None,
                None,
            )?);
        }
        for prompt in catalog.prompts {
            let id = capability_id(server_id, "prompt", &prompt.name)?;
            if prompt.arguments.len() > 100 {
                return Err(StorageError::InvalidRecord(
                    "MCP prompt has too many arguments".into(),
                ));
            }
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for argument in prompt.arguments {
                if argument.name.is_empty()
                    || argument.name.len() > 128
                    || properties
                        .insert(argument.name.clone(), json!({"type":"string"}))
                        .is_some()
                {
                    return Err(StorageError::InvalidRecord(
                        "invalid MCP prompt argument".into(),
                    ));
                }
                if argument.required {
                    required.push(argument.name);
                }
            }
            let schema = json!({"type":"object","properties":properties,
                "required":required,"additionalProperties":false});
            let input_schema = schema_ref(server_id, "prompt", &prompt.name, "input", &schema)?;
            schemas.insert(input_schema.schema_id.as_str().into(), schema);
            capabilities.push(capability(
                previous,
                id,
                server_id,
                "prompt",
                prompt.name,
                prompt.description,
                Some(input_schema),
                None,
                false,
                None,
                None,
                None,
            )?);
        }
        capabilities.push(capability(
            previous,
            capability_id(server_id, SAMPLING_KIND, SAMPLING_NAME)?,
            server_id,
            SAMPLING_KIND,
            SAMPLING_NAME.into(),
            "Let this server ask DevWorks for a text completion during a selected tool call. \
             DevWorks chooses the model and charges the calling agent's work."
                .into(),
            None,
            None,
            false,
            None,
            None,
            None,
        )?);
        self.write_catalog(
            tx,
            scope,
            server_id,
            expected_catalog_revision,
            capabilities,
            schemas,
            resource_uris,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_catalog(
        &self,
        tx: &mut dyn WriteTransaction,
        scope: &ConfigScope,
        server_id: &Id,
        expected_catalog_revision: &Revision,
        capabilities: Vec<McpCapability>,
        schemas: BTreeMap<String, Value>,
        resource_uris: BTreeMap<String, String>,
    ) -> Result<McpServer, StorageError> {
        let (version, mut state) = current(tx.configuration(), scope)?;
        let server = state
            .servers
            .iter_mut()
            .find(|s| &s.public.id == server_id)
            .ok_or_else(|| StorageError::NotFound {
                kind: "mcp_server",
                id: server_id.as_str().into(),
            })?;
        if &server.public.catalog_revision != expected_catalog_revision {
            return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
                dimension: RevisionDimension::Configuration,
                target_kind: "mcp_catalog",
                target_id: server_id.clone(),
                expected: expected_catalog_revision.clone(),
                current: server.public.catalog_revision.clone(),
            })));
        }
        // The 500-entry bound applies to remote entries; the one host sampling entry is extra.
        let sampling_id = capability_id(server_id, SAMPLING_KIND, SAMPLING_NAME)?;
        if capabilities
            .iter()
            .filter(|capability| capability.id != sampling_id)
            .count()
            > 500
        {
            return Err(StorageError::InvalidRecord(
                "MCP catalog exceeds page bound".into(),
            ));
        }
        let catalog_bytes = serde_json::to_vec(&(&capabilities, &schemas, &resource_uris))
            .map_err(|_| StorageError::InvalidRecord("MCP catalog serialization".into()))?;
        if catalog_bytes.len() > 1024 * 1024 {
            return Err(StorageError::InvalidRecord(
                "MCP catalog exceeds byte bound".into(),
            ));
        }
        let mut names = std::collections::HashSet::new();
        let mut wire_names = std::collections::HashSet::new();
        if capabilities.iter().any(|capability| {
            &capability.server_id != server_id
                || !names.insert(capability.id.as_str())
                || !matches!(
                    capability.kind.0.as_str(),
                    "tool" | "resource" | "prompt" | SAMPLING_KIND
                )
                || (capability.kind.0 != "resource"
                    && !wire_names.insert((capability.kind.0.as_str(), capability.name.0.as_str())))
                || ((capability.kind.0 == SAMPLING_KIND) != (capability.id == sampling_id))
                || capability.check().is_err()
        }) {
            return Err(StorageError::InvalidRecord(
                "invalid MCP catalog identity".into(),
            ));
        }
        // A read-only hint is untrusted: it only chooses the default selection of a newly added
        // server (MCP-06) and never counts as a verified read. Later discoveries keep a human
        // selection while the selected capability is unchanged and never add to it.
        let first_discovery = server.capabilities.is_empty();
        let previously_retired = std::mem::take(&mut server.retired);
        let mut last_available = previously_retired.clone();
        for old in &server.capabilities {
            last_available
                .entry(old.id.as_str().to_owned())
                .or_insert_with(|| old.clone());
        }
        // A title is display only, and a record saved before tools carried their destructive
        // hint takes the new one; any other change is a different tool.
        let annotated = |old: &McpCapability, new: &McpCapability| {
            let mut old = old.clone();
            old.title = new.title.clone();
            if old.destructive.is_none() {
                old.destructive = new.destructive;
            }
            old
        };
        let same = |a: &McpCapability, b: &McpCapability| {
            serde_json::to_value(annotated(a, b)).ok() == serde_json::to_value(b).ok()
        };
        // A returning tool carries a newer revision than its last available version; compare
        // what it is, not when it was numbered.
        let same_content = |a: &McpCapability, b: &McpCapability| {
            let unnumbered = |cap: &McpCapability| {
                let mut cap = cap.clone();
                cap.revision = rev(1);
                for schema in [&mut cap.input_schema, &mut cap.output_schema]
                    .into_iter()
                    .flatten()
                {
                    schema.revision = rev(1);
                }
                serde_json::to_value(cap).ok()
            };
            unnumbered(&annotated(a, b)) == unnumbered(b)
        };
        let mut capabilities = capabilities;
        let mut retired = BTreeMap::new();
        let mut selected = Vec::new();
        if first_discovery {
            // A person's classification decides in place of the server's hint.
            let overrides = &server.read_only_overrides;
            selected.extend(
                capabilities
                    .iter()
                    .filter(|cap| {
                        cap.kind.0 == "tool"
                            && overrides
                                .get(cap.id.as_str())
                                .copied()
                                .unwrap_or(cap.read_only)
                    })
                    .take(128)
                    .map(|cap| cap.id.clone()),
            );
        }
        for id in &server.public.selected_tool_ids.0 {
            let Some(old) = last_available.get(id.as_str()) else {
                continue;
            };
            match capabilities.iter().find(|cap| &cap.id == id) {
                Some(new)
                    if if previously_retired.contains_key(id.as_str()) {
                        same_content(old, new)
                    } else {
                        same(old, new)
                    } =>
                {
                    selected.push(id.clone())
                }
                Some(_) => {}
                None if old.kind.0 == "tool" => {
                    let mut unavailable = old.clone();
                    unavailable.input_schema = None;
                    unavailable.output_schema = None;
                    // An identical placeholder keeps the revision already shown, so a repeated
                    // discovery with the tool still absent changes nothing.
                    if let Some(shown) = server.capabilities.iter().find(|cap| &cap.id == id) {
                        unavailable.revision = shown.revision.clone();
                        if !same(shown, &unavailable) {
                            unavailable.revision = rev(shown.revision.get() + 1);
                        }
                    }
                    retired.insert(id.as_str().to_owned(), old.clone());
                    capabilities.push(unavailable);
                    selected.push(id.clone());
                }
                None => {}
            }
        }
        if selected.len() > 128 {
            return Err(StorageError::InvalidRecord(
                "MCP tool selection exceeds contract bound".into(),
            ));
        }
        if matches!(server.public.state, ItemState::Healthy)
            && server.public.last_failure.is_none()
            && server.public.selected_tool_ids.0 == selected
            && serde_json::to_value(&server.capabilities).ok()
                == serde_json::to_value(&capabilities).ok()
            && server.schemas == schemas
            && server.resource_uris == resource_uris
        {
            server.retired = retired;
            return Ok(server.public.clone());
        }
        server.retired = retired;
        server.capabilities = capabilities;
        server.schemas = schemas;
        server.resource_uris = resource_uris;
        server.public.selected_tool_ids = BoundedList(selected);
        server.public.catalog_revision = rev(server.public.catalog_revision.get() + 1);
        server.public.revision = rev(server.public.revision.get() + 1);
        server.public.state = ItemState::Healthy;
        server.public.last_failure = None;
        let result = server.public.clone();
        let body = serde_json::to_value(state)
            .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
        tx.configuration()
            .append_revision(scope, NAMESPACE, version, SCHEMA_VERSION, body)?;
        Ok(result)
    }

    fn configure(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        input: &ops::ConfigureInput,
    ) -> Result<DomainDecision, Failure> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(
                Refusal::new(DomainErrorCode::PermissionDenied, "mcp_configuration", None).into(),
            );
        }
        if let McpTransport::Http {
            credential_ref: Some(handle),
            ..
        } = &input.transport
            && !handle.as_str().starts_with("mcp-")
        {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "mcp_credential_ref",
                Some(handle.clone()),
            )
            .into());
        }
        scope(ctx, &input.scope, tx.projects())?;
        let scope = active_server_scope(ctx, &input.scope, tx.projects())?;
        let (version, mut state) = current(tx.configuration(), &scope)?;
        let transport_label = BoundedText(match &input.transport {
            McpTransport::Stdio { .. } => "stdio".into(),
            McpTransport::Http { .. } => "http".into(),
        });
        let result = if let Some(id) = &input.server_id {
            let expected = input.expected_revision.as_ref().ok_or_else(|| {
                Refusal::new(DomainErrorCode::InvalidArgument, "expected_revision", None)
            })?;
            let server = state
                .servers
                .iter_mut()
                .find(|s| &s.public.id == id)
                .ok_or_else(|| {
                    Refusal::new(DomainErrorCode::NotFound, "mcp_server", Some(id.clone()))
                })?;
            if &server.public.revision != expected {
                return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
                    dimension: RevisionDimension::Configuration,
                    target_kind: "mcp_server",
                    target_id: id.clone(),
                    expected: expected.clone(),
                    current: server.public.revision.clone(),
                }))
                .into());
            }
            let changed_transport = serde_json::to_value(&server.transport).ok()
                != serde_json::to_value(&input.transport).ok();
            // The same endpoint or process with another credential is the same server: it keeps
            // its catalog and selection, but stops serving until it reconnects with the new one.
            let changed_server =
                without_credentials(&server.transport) != without_credentials(&input.transport);
            server.public.name = input.name.clone();
            server.public.transport = transport_label;
            server.public.revision = rev(server.public.revision.get() + 1);
            if changed_transport && !changed_server {
                server.public.state = ItemState::Disabled;
                server.public.last_failure = None;
            }
            if changed_server {
                server.public.state = ItemState::Disabled;
                server.public.last_failure = None;
                server.public.catalog_revision = rev(server.public.catalog_revision.get() + 1);
                server.public.selected_tool_ids = BoundedList(Vec::new());
                server.capabilities.clear();
                server.schemas.clear();
                server.retired.clear();
            }
            server.transport = input.transport.clone();
            server.view(ctx)
        } else {
            if input.expected_revision.is_some() {
                return Err(Refusal::new(
                    DomainErrorCode::InvalidArgument,
                    "expected_revision",
                    None,
                )
                .into());
            }
            let public = McpServer {
                id: tx.new_id(IdKind::Connection),
                revision: rev(1),
                name: input.name.clone(),
                state: ItemState::Disabled,
                transport: transport_label,
                selected_tool_ids: BoundedList(Vec::new()),
                catalog_revision: rev(1),
                last_failure: None,
                configuration: None,
                disabled: None,
                missing_runtime: None,
            };
            let stored = StoredServer {
                public,
                transport: input.transport.clone(),
                capabilities: Vec::new(),
                schemas: BTreeMap::new(),
                resource_uris: BTreeMap::new(),
                retired: BTreeMap::new(),
                read_only_overrides: BTreeMap::new(),
            };
            let view = stored.view(ctx);
            state.servers.push(stored);
            view
        };
        let body = serde_json::to_value(state)
            .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
        tx.configuration()
            .append_revision(&scope, NAMESPACE, version, SCHEMA_VERSION, body)?;
        Ok(DomainDecision::Applied {
            result: OperationResult::McpConfigure { value: result },
        })
    }

    fn select_tools(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        input: &ops::SelectToolsInput,
    ) -> Result<DomainDecision, Failure> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(
                Refusal::new(DomainErrorCode::PermissionDenied, "mcp_selection", None).into(),
            );
        }
        scope(ctx, &requested_scope(&input.scope), tx.projects())?;
        let scope = active_server_scope(ctx, &requested_scope(&input.scope), tx.projects())?;
        let (version, mut state) = current(tx.configuration(), &scope)?;
        let server = state
            .servers
            .iter_mut()
            .find(|s| s.public.id == input.server_id)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::NotFound,
                    "mcp_server",
                    Some(input.server_id.clone()),
                )
            })?;
        for (expected, actual) in [
            (&input.expected_revision, &server.public.revision),
            (
                &input.expected_catalog_revision,
                &server.public.catalog_revision,
            ),
        ] {
            if expected != actual {
                return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
                    dimension: RevisionDimension::Configuration,
                    target_kind: "mcp_server",
                    target_id: input.server_id.clone(),
                    expected: expected.clone(),
                    current: actual.clone(),
                }))
                .into());
            }
        }
        if input.tool_ids.0.iter().any(|id| {
            !server
                .capabilities
                .iter()
                .any(|cap| matches!(cap.kind.0.as_str(), "tool" | SAMPLING_KIND) && cap.id == *id)
        }) {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "mcp_tool", None).into());
        }
        let mut selected = input.tool_ids.0.clone();
        selected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        selected.dedup();
        server.public.selected_tool_ids = BoundedList(selected);
        server.public.revision = rev(server.public.revision.get() + 1);
        let result = server.view(ctx);
        let body = serde_json::to_value(state)
            .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
        tx.configuration()
            .append_revision(&scope, NAMESPACE, version, SCHEMA_VERSION, body)?;
        Ok(DomainDecision::Applied {
            result: OperationResult::McpSelectTools { value: result },
        })
    }

    /// `mcp.classify_tool`: a person decides whether a tool counts as read-only, or returns it
    /// to the server's hint. It changes no saved selection.
    fn classify_tool(
        &self,
        ctx: &InvocationContext,
        tx: &mut dyn WriteTransaction,
        input: &ops::ClassifyToolInput,
    ) -> Result<DomainDecision, Failure> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "mcp_classification",
                None,
            )
            .into());
        }
        let scope = active_server_scope(ctx, &requested_scope(&input.scope), tx.projects())?;
        let (version, mut state) = current(tx.configuration(), &scope)?;
        let server = state
            .servers
            .iter_mut()
            .find(|s| s.public.id == input.server_id)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::NotFound,
                    "mcp_server",
                    Some(input.server_id.clone()),
                )
            })?;
        if server.public.revision != input.expected_revision {
            return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
                dimension: RevisionDimension::Configuration,
                target_kind: "mcp_server",
                target_id: input.server_id.clone(),
                expected: input.expected_revision.clone(),
                current: server.public.revision.clone(),
            }))
            .into());
        }
        if !server
            .capabilities
            .iter()
            .any(|cap| cap.kind.0 == "tool" && cap.id == input.tool_id)
        {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "mcp_tool",
                Some(input.tool_id.clone()),
            )
            .into());
        }
        let key = input.tool_id.as_str().to_owned();
        if server.read_only_overrides.get(&key).copied() == input.read_only {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::McpClassifyTool {
                    value: server.view(ctx),
                },
            });
        }
        match input.read_only {
            Some(read_only) => server.read_only_overrides.insert(key, read_only),
            None => server.read_only_overrides.remove(&key),
        };
        server.public.revision = rev(server.public.revision.get() + 1);
        let result = server.view(ctx);
        let body = serde_json::to_value(state)
            .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
        tx.configuration()
            .append_revision(&scope, NAMESPACE, version, SCHEMA_VERSION, body)?;
        Ok(DomainDecision::Applied {
            result: OperationResult::McpClassifyTool { value: result },
        })
    }

    fn connect(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::ConnectInput,
    ) -> Result<Decided, Failure> {
        if self.native.get().is_none() {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedTarget, "mcp_runtime", None).into(),
            );
        }
        let scope = active_server_scope(ctx, &requested_scope(&input.scope), tx.projects())?;
        let (version, mut state) = current(tx.configuration(), &scope)?;
        let server = state
            .servers
            .iter_mut()
            .find(|s| s.public.id == input.server_id)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::NotFound,
                    "mcp_server",
                    Some(input.server_id.clone()),
                )
            })?;
        if server.public.revision != input.expected_revision {
            return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
                dimension: RevisionDimension::Configuration,
                target_kind: "mcp_server",
                target_id: input.server_id.clone(),
                expected: input.expected_revision.clone(),
                current: server.public.revision.clone(),
            }))
            .into());
        }
        // Connecting a server a human disabled enables it again, which only a human may do.
        if server.public.disabled == Some(true) {
            if ctx.principal_kind() != PrincipalKind::Human {
                return Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "mcp_server_disabled",
                    Some(input.server_id.clone()),
                )
                .into());
            }
            server.public.disabled = None;
        }
        server.public.revision = rev(server.public.revision.get() + 1);
        server.public.state = ItemState::Unavailable;
        server.public.last_failure = None;
        let pending_revision = server.public.revision.get();
        let body = serde_json::to_value(state)
            .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
        tx.configuration()
            .append_revision(&scope, NAMESPACE, version, SCHEMA_VERSION, body)?;

        let operation = OperationId(tx.new_id(IdKind::Operation));
        let event = EventId(tx.new_id(IdKind::Event));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "mcp.connect".into(),
            schema_version: 1,
            args_digest: sha256_hex(
                format!(
                    "{}|{}|{}",
                    scope.key(),
                    input.server_id.as_str(),
                    input.expected_revision.get()
                )
                .as_bytes(),
            ),
            payload: json!({
                "scope": scope.key(),
                "serverId": input.server_id,
                "pendingRevision": pending_revision,
                "principal": ctx.principal(),
                "principalKind": format!("{:?}", ctx.principal_kind()),
                "sessionId": ctx.session_id(),
                "fences": command.authorization().fences(),
            }),
        })?;
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "mcp_server".into(),
            aggregate_id: input.server_id.as_str().into(),
            aggregate_revision: pending_revision,
            kind: "McpConnectRequested".into(),
            payload: json!({"serverId":input.server_id,"scope":scope.key()}),
        })?;
        let now = tx.now_ms();
        let queue_id = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: queue_id,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"serverId":input.server_id}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }

    /// `mcp.disable` and `mcp.remove`: save the change, then close the live session through the
    /// effect queue. Without a native client there is no session, so the change is final.
    fn switch_off(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, Failure> {
        let (operation, requested, server_id, expected_revision) = match command.input() {
            OperationInput::Mcp(ops::Input::Disable(input)) => (
                "mcp.disable",
                &input.scope,
                &input.server_id,
                &input.expected_revision,
            ),
            OperationInput::Mcp(ops::Input::Remove(input)) => (
                "mcp.remove",
                &input.scope,
                &input.server_id,
                &input.expected_revision,
            ),
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )
                .into());
            }
        };
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(
                Refusal::new(DomainErrorCode::PermissionDenied, "mcp_configuration", None).into(),
            );
        }
        let requested = requested_scope(requested);
        scope(ctx, &requested, tx.projects())?;
        let scope = active_server_scope(ctx, &requested, tx.projects())?;
        let (version, mut state) = current(tx.configuration(), &scope)?;
        let index = state
            .servers
            .iter()
            .position(|s| &s.public.id == server_id)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::NotFound,
                    "mcp_server",
                    Some(server_id.clone()),
                )
            })?;
        let server = &mut state.servers[index];
        if &server.public.revision != expected_revision {
            return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
                dimension: RevisionDimension::Configuration,
                target_kind: "mcp_server",
                target_id: server_id.clone(),
                expected: expected_revision.clone(),
                current: server.public.revision.clone(),
            }))
            .into());
        }
        server.public.revision = rev(server.public.revision.get() + 1);
        server.public.state = ItemState::Disabled;
        server.public.disabled = Some(true);
        server.public.last_failure = None;
        let result = server.view(ctx);
        let removing = operation == "mcp.remove";
        if removing {
            state.servers.remove(index);
        }
        let body = serde_json::to_value(state)
            .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
        tx.configuration()
            .append_revision(&scope, NAMESPACE, version, SCHEMA_VERSION, body)?;
        let value = || {
            if removing {
                OperationResult::McpRemove {
                    value: result.clone(),
                }
            } else {
                OperationResult::McpDisable {
                    value: result.clone(),
                }
            }
        };
        let kind = if removing {
            "McpRemoved"
        } else {
            "McpDisabled"
        };
        if self.native.get().is_none() {
            let event = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id: event,
                actor: ctx.principal().clone(),
                project_id: ctx.project_id().cloned(),
                root_id: ctx.root_id().cloned(),
                operation_id: None,
                aggregate_kind: "mcp_server".into(),
                aggregate_id: server_id.as_str().into(),
                aggregate_revision: result.revision.get(),
                kind: kind.into(),
                payload: json!({"serverId":server_id,"scope":scope.key()}),
            })?;
            return Ok(Decided {
                decision: DomainDecision::Applied { result: value() },
                operation_id: None,
            });
        }
        let operation_id = OperationId(tx.new_id(IdKind::Operation));
        let event = EventId(tx.new_id(IdKind::Event));
        tx.operations().record_intent(NewOperation {
            id: operation_id.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: operation.into(),
            schema_version: 1,
            args_digest: sha256_hex(
                format!(
                    "{}|{}|{}",
                    scope.key(),
                    server_id.as_str(),
                    expected_revision.get()
                )
                .as_bytes(),
            ),
            payload: json!({
                "scope": scope.key(),
                "serverId": server_id,
                "server": result,
                "principal": ctx.principal(),
                "fences": command.authorization().fences(),
            }),
        })?;
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation_id.clone()),
            aggregate_kind: "mcp_server".into(),
            aggregate_id: server_id.as_str().into(),
            aggregate_revision: result.revision.get(),
            kind: kind.into(),
            payload: json!({"serverId":server_id,"scope":scope.key()}),
        })?;
        let now = tx.now_ms();
        let queue_id = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: queue_id,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation_id.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"serverId":server_id}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation_id.clone(),
            },
            operation_id: Some(operation_id),
        })
    }

    fn discover(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::DiscoverInput,
    ) -> Result<Decided, Failure> {
        if self.native.get().is_none() {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedTarget, "mcp_runtime", None).into(),
            );
        }
        let scope = active_server_scope(ctx, &requested_scope(&input.scope), tx.projects())?;
        let (_, state) = current(tx.configuration(), &scope)?;
        let server = state
            .servers
            .iter()
            .find(|server| server.public.id == input.server_id)
            .ok_or_else(|| {
                Refusal::new(
                    DomainErrorCode::NotFound,
                    "mcp_server",
                    Some(input.server_id.clone()),
                )
            })?;
        if server.public.catalog_revision != input.expected_catalog_revision {
            return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
                dimension: RevisionDimension::Configuration,
                target_kind: "mcp_catalog",
                target_id: input.server_id.clone(),
                expected: input.expected_catalog_revision.clone(),
                current: server.public.catalog_revision.clone(),
            }))
            .into());
        }
        // Discovery reconnects, so a disabled server is discovered only after a human enables it.
        if server.public.disabled == Some(true) {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "mcp_server_disabled",
                Some(input.server_id.clone()),
            )
            .into());
        }
        let pending_revision = server.public.revision.get();
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let event = EventId(tx.new_id(IdKind::Event));
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: ctx.root_id().cloned(),
            assignment_id: ctx.assignment_id().cloned(),
            capability: "mcp.discover".into(),
            schema_version: 1,
            args_digest: sha256_hex(
                format!(
                    "{}|{}|{}",
                    scope.key(),
                    input.server_id.as_str(),
                    input.expected_catalog_revision.get()
                )
                .as_bytes(),
            ),
            payload: json!({
                "scope":scope.key(),"serverId":input.server_id,"pendingRevision":pending_revision,
                "catalogRevision":input.expected_catalog_revision.get(),
                "principal":ctx.principal(),"principalKind":format!("{:?}",ctx.principal_kind()),
                "sessionId":ctx.session_id(),"fences":command.authorization().fences(),
            }),
        })?;
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            aggregate_kind: "mcp_server".into(),
            aggregate_id: input.server_id.as_str().into(),
            aggregate_revision: pending_revision,
            kind: "McpDiscoveryRequested".into(),
            payload: json!({"serverId":input.server_id,"scope":scope.key()}),
        })?;
        let now = tx.now_ms();
        let queue_id = tx.new_id(IdKind::QueueEntry);
        tx.queue().enqueue(NewQueueEntry {
            id: queue_id,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"serverId":input.server_id}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: operation.clone(),
            },
            operation_id: Some(operation),
        })
    }
}

/// A sampling request is accepted only from an Agent's in-flight call whose authority fences,
/// session and the server's current sampling selection all still hold. The server revision is
/// not compared: selection and connection edits bump it while the catalog stays the same.
fn sampling_decision(
    tx: &mut dyn WriteTransaction,
    op: &OperationRecord,
    origin: &McpCallOrigin,
    info: &crate::ports::storage::StoreInfo,
) -> Result<Result<SamplingGrant, &'static str>, StorageError> {
    let payload = &op.payload;
    if op.capability != "mcp.tool"
        || op.state != OperationState::Dispatching
        || op.latest_attempt().map(|attempt| &attempt.id.0) != Some(&origin.attempt_id)
        || payload.get("serverId") != Some(&json!(origin.server_id))
    {
        return Ok(Err("mcp_sampling_call"));
    }
    if payload.get("principalKind").and_then(Value::as_str) != Some("Agent") {
        return Ok(Err("mcp_sampling_principal"));
    }
    let field = |name: &str| payload.get(name).cloned().unwrap_or(Value::Null);
    let parsed = (
        serde_json::from_value::<ActorId>(field("principal")),
        serde_json::from_value::<Id>(field("sessionId")),
        serde_json::from_value::<Option<ProjectId>>(field("projectId")),
        serde_json::from_value::<Vec<AuthorityFence>>(field("fences")),
        serde_json::from_value::<ToolCallKey>(field("call")),
        payload
            .get("scope")
            .and_then(Value::as_str)
            .and_then(ConfigScope::from_key),
        payload.get("catalogRevision").and_then(Value::as_i64),
    );
    let (
        Ok(principal),
        Ok(session_id),
        Ok(project_id),
        Ok(fences),
        Ok(call),
        Some(scope),
        Some(catalog),
    ) = parsed
    else {
        return Ok(Err("mcp_sampling_call"));
    };
    if tx.actors().actor_kind(&principal)? != Some(PrincipalKind::Agent)
        || !crate::authority::fences_hold(tx, &fences)?
    {
        return Ok(Err("mcp_sampling_authority"));
    }
    if tx.sessions().session(&session_id)?.is_none_or(|session| {
        session.closed() || session.agent_id != principal || session.project_id != project_id
    }) {
        return Ok(Err("mcp_sampling_session"));
    }
    let (_, state) = current(tx.configuration(), &scope)?;
    if !state.servers.iter().any(|server| {
        server.public.id == origin.server_id
            && matches!(server.public.state, ItemState::Healthy)
            && server.public.catalog_revision.get() == catalog
            && server.samples()
    }) {
        return Ok(Err("mcp_sampling_selection"));
    }
    let context = InvocationContext::new(
        principal,
        PrincipalKind::Agent,
        op.id.0.clone(),
        Id::new("mcp-effects").expect("static ingress"),
        info.store_id.clone(),
        Counter::new(info.restore_epoch).expect("valid store epoch"),
        rev(1),
    )
    .with_lineage(project_id, op.root_id.clone(), op.assignment_id.clone())
    .with_session(session_id);
    Ok(Ok(SamplingGrant {
        context,
        operation_id: op.id.clone(),
        attempt_id: origin.attempt_id.clone(),
        call,
        fences,
    }))
}

/// The diagnostic a failed connection leaves on its server. A launch whose executable this
/// host cannot resolve names that executable, so a person knows which runtime to install.
fn connection_failure(
    remote: &Arc<dyn McpRemote>,
    transport: &McpTransport,
    fault: &McpRemoteFault,
) -> String {
    match (fault, remote.missing_runtime(transport)) {
        (McpRemoteFault::UnavailableRuntime, Some(executable)) => format!(
            "Connection failed: the executable `{executable}` is not available on this \
             computer. Install its runtime, make it available on PATH, then reconnect."
        ),
        _ => format!("Connection failed: {fault:?}"),
    }
}

fn capability_id(server: &Id, kind: &str, wire_name: &str) -> Result<Id, StorageError> {
    Id::new(format!(
        "mcp-{}",
        sha256_hex(format!("{}:{kind}:{wire_name}", server.as_str()).as_bytes())
    ))
    .map_err(|_| StorageError::InvalidRecord("invalid MCP capability identity".into()))
}

fn schema_ref(
    server: &Id,
    kind: &str,
    wire_name: &str,
    direction: &str,
    value: &Value,
) -> Result<SchemaRef, StorageError> {
    if !value.is_object() {
        return Err(StorageError::InvalidRecord(
            "MCP tool schema must be an object".into(),
        ));
    }
    let canonical = crate::jcs::canonicalize(value)
        .map_err(|_| StorageError::InvalidRecord("invalid MCP tool schema".into()))?;
    if canonical.len() > 65_536 {
        return Err(StorageError::InvalidRecord(
            "MCP tool schema exceeds bound".into(),
        ));
    }
    let identity =
        sha256_hex(format!("{}:{kind}:{wire_name}:{direction}", server.as_str()).as_bytes());
    Ok(SchemaRef {
        schema_id: QualifiedId::try_from(format!("mcp/{identity}"))
            .map_err(|_| StorageError::InvalidRecord("invalid MCP schema identity".into()))?,
        revision: rev(1),
        sha256: Digest::try_from(sha256_hex(canonical.as_bytes()))
            .map_err(|_| StorageError::InvalidRecord("invalid MCP schema digest".into()))?,
    })
}

/// A transport with every credential handle removed: what identifies the server itself.
fn without_credentials(transport: &McpTransport) -> Option<Value> {
    let mut value = serde_json::to_value(transport).ok()?;
    if let Some(object) = value.as_object_mut() {
        object.remove("credentialRef");
    }
    if let Some(entries) = value
        .pointer_mut("/process/environment")
        .and_then(Value::as_array_mut)
    {
        for entry in entries.iter_mut().filter_map(Value::as_object_mut) {
            entry.remove("credentialRef");
        }
    }
    Some(value)
}

#[allow(clippy::too_many_arguments)]
fn capability(
    previous: &StoredServer,
    id: Id,
    server_id: &Id,
    kind: &str,
    name: String,
    description: String,
    input_schema: Option<SchemaRef>,
    output_schema: Option<SchemaRef>,
    read_only: bool,
    uri: Option<String>,
    title: Option<String>,
    destructive: Option<bool>,
) -> Result<McpCapability, StorageError> {
    if name.is_empty()
        || name.chars().count() > 256
        || name.contains('\0')
        || description.chars().count() > 65_536
        || description.contains('\0')
    {
        return Err(StorageError::InvalidRecord(
            "MCP capability text exceeds contract".into(),
        ));
    }
    let mut cap = McpCapability {
        id,
        server_id: server_id.clone(),
        kind: BoundedText(kind.into()),
        name: BoundedText(name),
        description: BoundedText(description),
        input_schema,
        output_schema,
        read_only,
        revision: rev(1),
        // A resource names the URI a reader passes to `mcp.resources`.
        uri: uri.map(BoundedText),
        title: title.map(BoundedText),
        destructive,
        read_only_override: None,
    };
    if let Some(old) = previous.capabilities.iter().find(|old| old.id == cap.id) {
        cap.revision = old.revision.clone();
        if let Some(schema) = &mut cap.input_schema {
            schema.revision = old
                .input_schema
                .as_ref()
                .map_or_else(|| old.revision.clone(), |s| s.revision.clone());
        }
        if let Some(schema) = &mut cap.output_schema {
            schema.revision = old
                .output_schema
                .as_ref()
                .map_or_else(|| old.revision.clone(), |s| s.revision.clone());
        }
        // A title is display only, and a hint the saved record never had is not a change.
        let mut compared = old.clone();
        compared.title = cap.title.clone();
        if compared.destructive.is_none() {
            compared.destructive = cap.destructive;
        }
        if serde_json::to_value(&compared).ok() != serde_json::to_value(&cap).ok() {
            cap.revision = rev(old.revision.get() + 1);
            if let Some(schema) = &mut cap.input_schema {
                schema.revision = cap.revision.clone();
            }
            if let Some(schema) = &mut cap.output_schema {
                schema.revision = cap.revision.clone();
            }
        }
    }
    Ok(cap)
}

pub enum McpEffectOutcome {
    Completed(Value),
    Failed(String),
    NotApplied(String),
    Unknown(String),
}

impl QueryHandler for McpClientService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &crate::ports::Preparation<'_>,
    ) -> Result<Option<crate::ports::Prepared>, DomainFault> {
        self.prepare_content(ctx, query, env)
    }

    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        if let Some(answer) = self.content_answer(ctx, query, snapshot)? {
            return Ok(answer);
        }
        let result = match query.input() {
            OperationInput::Mcp(ops::Input::Servers(input)) => {
                let scope =
                    scope(ctx, &input.scope, snapshot.projects()).map_err(|e| self.fault(e))?;
                let (version, mut state) =
                    current(snapshot.configuration(), &scope).map_err(|e| self.fault(e))?;
                state
                    .servers
                    .sort_by(|a, b| a.public.id.as_str().cmp(b.public.id.as_str()));
                // Runtime availability is a fact about this host now, so it is read, not stored.
                let records: Vec<McpServer> = state
                    .servers
                    .iter()
                    .map(|s| {
                        let mut view = s.view(ctx);
                        // It names the saved executable, so it goes only where the saved
                        // configuration does.
                        if view.configuration.is_some() {
                            view.missing_runtime = self
                                .native
                                .get()
                                .and_then(|native| native.remote.missing_runtime(&s.transport))
                                .map(BoundedText);
                        }
                        view
                    })
                    .collect();
                let value = page(&records, &input.page, version).map_err(|e| self.fault(e))?;
                (version, OperationResult::McpServers { value })
            }
            OperationInput::Mcp(ops::Input::Capabilities(input)) => {
                let scope = server_scope(ctx, &requested_scope(&input.scope), snapshot.projects())
                    .map_err(|e| self.fault(e))?;
                let (version, state) =
                    current(snapshot.configuration(), &scope).map_err(|e| self.fault(e))?;
                let server = state
                    .servers
                    .into_iter()
                    .find(|s| s.public.id == input.server_id)
                    .ok_or_else(|| {
                        self.fault(StorageError::NotFound {
                            kind: "mcp_server",
                            id: input.server_id.as_str().into(),
                        })
                    })?;
                if server.public.catalog_revision != input.catalog_revision {
                    return Err(self.fault(StorageError::RevisionConflict(Box::new(
                        RevisionConflict {
                            dimension: RevisionDimension::Configuration,
                            target_kind: "mcp_catalog",
                            target_id: input.server_id.clone(),
                            expected: input.catalog_revision.clone(),
                            current: server.public.catalog_revision,
                        },
                    ))));
                }
                let mut capabilities = server.capabilities;
                for cap in &mut capabilities {
                    cap.read_only_override =
                        server.read_only_overrides.get(cap.id.as_str()).copied();
                }
                capabilities.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
                let value = page(&capabilities, &input.page, version).map_err(|e| self.fault(e))?;
                (version, OperationResult::McpCapabilities { value })
            }
            _ => {
                return Err(crate::services::fault(
                    DomainErrorCode::UnsupportedCapability,
                    RetryAdvice::Never,
                    &CorrelationId(self.ids.next_id(IdKind::Correlation)),
                ));
            }
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(result.0)),
            result: result.1,
        })
    }
}

impl CommandHandler for McpClientService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        match command.input() {
            OperationInput::Mcp(ops::Input::Configure(input)) => {
                let result = self.configure(ctx, tx, input);
                decided(tx, result)
            }
            OperationInput::Mcp(ops::Input::SelectTools(input)) => {
                let result = self.select_tools(ctx, tx, input);
                decided(tx, result)
            }
            OperationInput::Mcp(ops::Input::ClassifyTool(input)) => {
                let result = self.classify_tool(ctx, tx, input);
                decided(tx, result)
            }
            OperationInput::Mcp(ops::Input::Connect(input)) => {
                match self.connect(ctx, command, tx, input) {
                    Ok(decision) => Ok(decision),
                    Err(error) => decided(tx, Err(error)),
                }
            }
            OperationInput::Mcp(ops::Input::Discover(input)) => {
                match self.discover(ctx, command, tx, input) {
                    Ok(decision) => Ok(decision),
                    Err(error) => decided(tx, Err(error)),
                }
            }
            OperationInput::Mcp(ops::Input::Disable(_) | ops::Input::Remove(_)) => {
                match self.switch_off(ctx, command, tx) {
                    Ok(decision) => Ok(decision),
                    Err(error) => decided(tx, Err(error)),
                }
            }
            _ => decided(
                tx,
                Err(crate::services::sessions::Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )
                .into()),
            ),
        }
    }
}
