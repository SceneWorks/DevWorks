//! Outgoing MCP configuration. The scoped configuration repository holds the complete server
//! set at one revision, so every edit is durable, compare-and-set and project-purge aware.
use crate::context::InvocationContext;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, mcp as ops};
use crate::ports::storage::{
    ConfigScope, ProjectRead, ReadSnapshot, RevisionConflict, StorageError, WriteTransaction,
};
use crate::ports::{
    CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::data::DataScope;
use crate::protocol::records::{ItemState, McpCapability, McpServer, McpTransport, PageRequest};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Page};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, Id, RetryAdvice, Revision, RevisionDimension,
};
use crate::services::sessions::{Failure, Refusal, decided, query_fault, rev};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const NAMESPACE: &str = "outgoing_mcp";
const SCHEMA_VERSION: i64 = 1;
pub const OPERATIONS: [&str; 4] = [
    "mcp.servers",
    "mcp.configure",
    "mcp.select_tools",
    "mcp.capabilities",
];

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
    auto_select_read_only: bool,
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
    let config = serde_json::from_value(revision.body)
        .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
    Ok((revision.revision, config))
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
}
impl McpClientService {
    pub fn new(ids: Arc<dyn IdSource>) -> Self {
        Self { ids }
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
        let mut names = std::collections::HashSet::new();
        if capabilities.iter().any(|capability| {
            &capability.server_id != server_id
                || !names.insert(capability.id.as_str())
                || !matches!(capability.kind.0.as_str(), "tool" | "resource" | "prompt")
        }) {
            return Err(StorageError::InvalidRecord(
                "invalid MCP catalog identity".into(),
            ));
        }
        let selected = if server.auto_select_read_only {
            capabilities
                .iter()
                .filter(|cap| cap.kind.0 == "tool" && cap.read_only)
                .map(|cap| cap.id.clone())
                .collect::<Vec<_>>()
        } else {
            server
                .public
                .selected_tool_ids
                .0
                .iter()
                .filter(|id| {
                    let old = server.capabilities.iter().find(|cap| &cap.id == *id);
                    let new = capabilities.iter().find(|cap| &cap.id == *id);
                    matches!((old, new), (Some(old), Some(new))
                        if serde_json::to_value(old).ok() == serde_json::to_value(new).ok())
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        server.capabilities = capabilities;
        server.auto_select_read_only = false;
        server.public.selected_tool_ids = BoundedList(selected);
        server.public.catalog_revision = rev(server.public.catalog_revision.get() + 1);
        server.public.revision = rev(server.public.revision.get() + 1);
        server.public.state = ItemState::Healthy;
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
        let scope = scope(ctx, &input.scope, tx.projects())?;
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
            server.public.name = input.name.clone();
            server.public.transport = transport_label;
            server.public.revision = rev(server.public.revision.get() + 1);
            if changed_transport {
                server.public.state = ItemState::Disabled;
                server.public.catalog_revision = rev(server.public.catalog_revision.get() + 1);
                server.public.selected_tool_ids = BoundedList(Vec::new());
                server.capabilities.clear();
                server.auto_select_read_only = true;
            }
            server.transport = input.transport.clone();
            server.public.clone()
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
            };
            state.servers.push(StoredServer {
                public: public.clone(),
                transport: input.transport.clone(),
                capabilities: Vec::new(),
                auto_select_read_only: true,
            });
            public
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
        let scope = ctx.project_id().map_or(ConfigScope::Workspace, |id| {
            ConfigScope::Project(id.clone())
        });
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
                .any(|cap| cap.kind.0 == "tool" && cap.id == *id)
        }) {
            return Err(Refusal::new(DomainErrorCode::InvalidArgument, "mcp_tool", None).into());
        }
        let mut selected = input.tool_ids.0.clone();
        selected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        selected.dedup();
        server.public.selected_tool_ids = BoundedList(selected);
        server.public.revision = rev(server.public.revision.get() + 1);
        let result = server.public.clone();
        let body = serde_json::to_value(state)
            .map_err(|_| StorageError::InvalidRecord("outgoing MCP configuration".into()))?;
        tx.configuration()
            .append_revision(&scope, NAMESPACE, version, SCHEMA_VERSION, body)?;
        Ok(DomainDecision::Applied {
            result: OperationResult::McpSelectTools { value: result },
        })
    }
}

impl QueryHandler for McpClientService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = match query.input() {
            OperationInput::Mcp(ops::Input::Servers(input)) => {
                let scope =
                    scope(ctx, &input.scope, snapshot.projects()).map_err(|e| self.fault(e))?;
                let (version, mut state) =
                    current(snapshot.configuration(), &scope).map_err(|e| self.fault(e))?;
                state
                    .servers
                    .sort_by(|a, b| a.public.id.as_str().cmp(b.public.id.as_str()));
                let records: Vec<McpServer> = state.servers.into_iter().map(|s| s.public).collect();
                let value = page(&records, &input.page, version).map_err(|e| self.fault(e))?;
                (version, OperationResult::McpServers { value })
            }
            OperationInput::Mcp(ops::Input::Capabilities(input)) => {
                let scope = ctx.project_id().map_or(ConfigScope::Workspace, |id| {
                    ConfigScope::Project(id.clone())
                });
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
