//! Private desktop administration and current authority for incoming MCP clients.

pub(crate) mod access;
mod credentials;
mod publication;
mod targets;

pub use access::{AccessFence, publication_allowed};

pub use credentials::{AuthenticatedClient, Binding, CredentialError, Credentials};
pub(crate) use publication::selected_operation;
pub use publication::{PublishedRequest, PublishedTool, published_tools};

use crate::authority::service::{AdminError, bind_principal};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, mcp_server as ops};
use crate::ports::storage::{
    ConfigScope, ConfigurationRead, DELETED, GitRead, IndexRead, NewEvent, NewPrincipalBinding,
    ProjectRead, ReadSnapshot, StorageError, WriteTransaction,
};
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, QueryAnswer, QueryHandler};
use crate::protocol::records::{
    ItemState, McpClientAccess, McpClientAccessAction, McpInboundSettings, McpInboundStatus,
    McpServerBinding,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{
    ActorId, CorrelationId, DomainErrorCode, DomainFault, EventId, Id, Revision, RevisionDimension,
};
use crate::registry::Mode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;

use super::cursor::{CursorBinding, slice};
use super::kernel::{Failure, Refusal, decided, not_found, query_fault, rev};

pub const OPERATIONS: [&str; 4] = [
    "mcp_server.status",
    "mcp_server.configure",
    "mcp_server.clients",
    "mcp_server.set_client_access",
];
const NAMESPACE: &str = "incoming_mcp";
const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Client {
    pub(crate) access: McpClientAccess,
    pub(crate) authorized_epoch: Option<i64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Configuration {
    pub(crate) settings: McpInboundSettings,
    pub(crate) authorized_epoch: Option<i64>,
    pub(crate) clients: Vec<Client>,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            settings: McpInboundSettings {
                revision: rev(1),
                enabled: false,
                bindings: BoundedList(Vec::new()),
                http_port: None,
            },
            authorized_epoch: None,
            clients: Vec::new(),
        }
    }
}

impl Configuration {
    pub(crate) fn client(&self, id: &Id) -> Option<&Client> {
        self.clients
            .iter()
            .find(|client| &client.access.client_id == id)
    }

    fn status(&self, epoch: i64) -> McpInboundStatus {
        let mut settings = self.settings.clone();
        let restore_requires_approval = settings.enabled && self.authorized_epoch != Some(epoch);
        if restore_requires_approval {
            settings.enabled = false;
        }
        let (state, diagnostic) = if restore_requires_approval {
            (
                ItemState::Disabled,
                Some("Enable incoming MCP again after restore."),
            )
        } else if settings.enabled {
            (
                ItemState::Unavailable,
                Some("The incoming MCP listener is unavailable in this build."),
            )
        } else {
            (ItemState::Disabled, None)
        };
        McpInboundStatus {
            settings,
            state,
            endpoint: None,
            protocol_versions: BoundedList(Vec::new()),
            publication_revision: rev(1),
            diagnostic: diagnostic.map(|value| BoundedText(value.into())),
        }
    }
}

pub(crate) fn current(read: &dyn ConfigurationRead) -> Result<(i64, Configuration), StorageError> {
    let Some(revision) = read.current(&ConfigScope::Workspace, NAMESPACE)? else {
        return Ok((1, Configuration::default()));
    };
    if revision.schema_version != SCHEMA_VERSION {
        return Err(StorageError::IncompatibleSchema(NAMESPACE.into()));
    }
    let body = serde_json::from_value(revision.body)
        .map_err(|_| StorageError::InvalidRecord(NAMESPACE.into()))?;
    Ok((revision.revision, body))
}

fn denied() -> Refusal {
    Refusal::new(DomainErrorCode::PermissionDenied, "mcp_server", None)
}

fn invalid(target: &'static str) -> Refusal {
    Refusal::new(DomainErrorCode::InvalidArgument, target, None)
}

fn check_revision(expected: &Revision, actual: &Revision, target: &Id) -> Result<(), Failure> {
    if expected == actual {
        return Ok(());
    }
    Err(StorageError::conflict(
        RevisionDimension::Configuration,
        "mcp_access",
        target.clone(),
        expected.clone(),
        actual.clone(),
    )
    .into())
}

fn next_revision(revision: &Revision) -> Result<Revision, StorageError> {
    revision
        .get()
        .checked_add(1)
        .and_then(|n| Revision::new(n).ok())
        .ok_or_else(|| StorageError::InvalidRecord("incoming MCP revision exhausted".into()))
}

fn edit_configuration(
    ctx: &InvocationContext,
    tx: &mut dyn WriteTransaction,
    configuration: &mut Configuration,
    input: &ops::ConfigureInput,
) -> Result<DomainDecision, Failure> {
    check_revision(
        &input.expected_revision,
        &configuration.settings.revision,
        &Id::new("workspace").expect("static id"),
    )?;
    let stdio = input
        .bindings
        .0
        .iter()
        .filter(|binding| matches!(binding, McpServerBinding::Stdio))
        .count();
    let http = input
        .bindings
        .0
        .iter()
        .filter(|binding| matches!(binding, McpServerBinding::LoopbackHttp))
        .count();
    if stdio > 1
        || http > 1
        || input.enabled && stdio + http == 0
        || input.http_port.is_some() != (http == 1)
    {
        return Err(invalid("mcp_bindings").into());
    }
    let epoch = tx.store().restore_epoch;
    configuration.settings = McpInboundSettings {
        revision: next_revision(&configuration.settings.revision)?,
        enabled: input.enabled,
        bindings: input.bindings.clone(),
        http_port: input.http_port.clone(),
    };
    configuration.authorized_epoch = input.enabled.then_some(epoch);
    let status = configuration.status(epoch);
    audit(
        tx,
        ctx,
        "McpInboundSettingsChanged",
        "workspace",
        &status.settings.revision,
        json!({"enabled": status.settings.enabled, "bindings": status.settings.bindings}),
    )?;
    Ok(DomainDecision::Applied {
        result: OperationResult::McpServerConfigure { value: status },
    })
}

fn unique<T: Eq + std::hash::Hash>(values: &[T]) -> bool {
    let mut seen = HashSet::new();
    values.iter().all(|value| seen.insert(value))
}

fn validate_operations(access: &McpClientAccess) -> Result<(), Failure> {
    for (mode, operations) in [
        (Mode::Query, &access.read_operations.0),
        (Mode::Command, &access.command_operations.0),
    ] {
        let selected: Vec<_> = operations
            .iter()
            .map(|operation| operation.0.as_str())
            .collect();
        if !unique(&selected)
            || selected
                .iter()
                .any(|op| !selected_operation(op, mode) || !access::supported_target(op, mode))
        {
            return Err(invalid("mcp_selected_operations").into());
        }
    }
    Ok(())
}

fn validate_project_membership(
    read: &dyn ProjectRead,
    access: &McpClientAccess,
) -> Result<(), Failure> {
    for project in &access.project_ids.0 {
        if !read
            .project(project)?
            .is_some_and(|p| p.lifecycle != DELETED)
        {
            return Err(invalid("mcp_project_grant").into());
        }
    }
    for repository in &access.repository_ids.0 {
        let projects = read.repository_projects(repository)?;
        if !projects
            .iter()
            .any(|project| access.project_ids.0.contains(project))
        {
            return Err(invalid("mcp_repository_grant").into());
        }
    }
    Ok(())
}

fn registered_worktrees(
    read: &dyn IndexRead,
    repositories: &[Id],
) -> Result<HashSet<Id>, StorageError> {
    let mut registered = HashSet::new();
    for repository in repositories {
        registered.extend(
            read.worktrees(repository)?
                .into_iter()
                .map(|tree| tree.worktree_id),
        );
    }
    Ok(registered)
}

fn validate_worktree_membership(
    read: &dyn GitRead,
    access: &McpClientAccess,
    registered: &HashSet<Id>,
) -> Result<(), Failure> {
    for worktree in &access.worktree_ids.0 {
        if !registered.contains(worktree)
            && !read.git_worktree(worktree)?.is_some_and(|tree| {
                !tree.removed && access.repository_ids.0.contains(&tree.repository_id)
            })
        {
            return Err(invalid("mcp_worktree_grant").into());
        }
    }
    Ok(())
}

fn validate_grants(tx: &mut dyn WriteTransaction, access: &McpClientAccess) -> Result<(), Failure> {
    if !unique(&access.project_ids.0)
        || !unique(&access.repository_ids.0)
        || !unique(&access.worktree_ids.0)
    {
        return Err(invalid("mcp_scope_grants").into());
    }
    validate_operations(access)?;
    validate_project_membership(tx.projects(), access)?;
    let registered = registered_worktrees(tx.indexing(), &access.repository_ids.0)?;
    validate_worktree_membership(tx.git(), access, &registered)
}

fn audit(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    kind: &'static str,
    id: &str,
    revision: &Revision,
    payload: serde_json::Value,
) -> Result<(), StorageError> {
    let id_event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: id_event,
        actor: ctx.principal().clone(),
        project_id: None,
        root_id: None,
        operation_id: None,
        aggregate_kind: "incoming_mcp".into(),
        aggregate_id: id.into(),
        aggregate_revision: revision.get(),
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

fn register_client(
    ctx: &InvocationContext,
    tx: &mut dyn WriteTransaction,
    name: &crate::protocol::support::Label,
) -> Result<Client, Failure> {
    let client_id = tx.new_id(IdKind::Connection);
    let actor = ActorId(client_id.clone());
    tx.actors()
        .register(&actor, PrincipalKind::McpClient, &name.0)?;
    bind_principal(
        tx,
        ctx,
        NewPrincipalBinding {
            actor,
            kind: PrincipalKind::McpClient,
            role: None,
            project_id: None,
            tool_families: None,
            bound_by: ctx.principal().clone(),
        },
        None,
    )
    .map_err(|error| match error {
        AdminError::Storage(error) => error,
        AdminError::Invalid(problem) => StorageError::InvalidRecord(problem.into()),
        AdminError::Denied(_) => StorageError::ScopeDenied,
    })?;
    Ok(Client {
        access: McpClientAccess {
            client_id,
            revision: rev(1),
            name: name.clone(),
            enabled: false,
            credential_generation: rev(1),
            project_ids: BoundedList(Vec::new()),
            repository_ids: BoundedList(Vec::new()),
            worktree_ids: BoundedList(Vec::new()),
            read_operations: BoundedList(Vec::new()),
            command_operations: BoundedList(Vec::new()),
        },
        authorized_epoch: None,
    })
}

fn edit_client(
    ctx: &InvocationContext,
    tx: &mut dyn WriteTransaction,
    configuration: &mut Configuration,
    action: &McpClientAccessAction,
) -> Result<DomainDecision, Failure> {
    use McpClientAccessAction as Action;
    let epoch = tx.store().restore_epoch;
    let (client_id, expected_revision, event) = match action {
        Action::Register { name } => {
            let client = register_client(ctx, tx, name)?;
            record_client_event(ctx, tx, "McpClientRegistered", &client.access)?;
            let access = client.access.clone();
            configuration.clients.push(client);
            return Ok(client_applied(access));
        }
        Action::Replace {
            client_id,
            expected_revision,
            ..
        } => (client_id, expected_revision, "McpClientAccessChanged"),
        Action::Revoke {
            client_id,
            expected_revision,
        } => (client_id, expected_revision, "McpClientRevoked"),
        Action::RotateCredential {
            client_id,
            expected_revision,
        } => (client_id, expected_revision, "McpClientCredentialRotated"),
    };
    let stored = configuration
        .clients
        .iter_mut()
        .find(|client| &client.access.client_id == client_id)
        .ok_or_else(|| not_found("mcp_client", client_id))?;
    check_revision(expected_revision, &stored.access.revision, client_id)?;
    let mut client = stored.clone();
    client.access.revision = next_revision(&client.access.revision)?;
    match action {
        Action::Replace {
            name,
            enabled,
            project_ids,
            repository_ids,
            worktree_ids,
            read_operations,
            command_operations,
            ..
        } => {
            if client.access.enabled && !enabled {
                client.access.credential_generation =
                    next_revision(&client.access.credential_generation)?;
            }
            client.access.name = name.clone();
            client.access.enabled = *enabled;
            client.access.project_ids = project_ids.clone();
            client.access.repository_ids = repository_ids.clone();
            client.access.worktree_ids = worktree_ids.clone();
            client.access.read_operations = read_operations.clone();
            client.access.command_operations = command_operations.clone();
            validate_grants(tx, &client.access)?;
            client.authorized_epoch = enabled.then_some(epoch);
        }
        Action::Revoke { .. } => {
            client.access.enabled = false;
            client.authorized_epoch = None;
            client.access.credential_generation =
                next_revision(&client.access.credential_generation)?;
        }
        Action::RotateCredential { .. } => {
            client.access.credential_generation =
                next_revision(&client.access.credential_generation)?;
        }
        Action::Register { .. } => unreachable!("handled registration"),
    };
    record_client_event(ctx, tx, event, &client.access)?;
    let access = client.access.clone();
    *stored = client;
    Ok(client_applied(access))
}

fn record_client_event(
    ctx: &InvocationContext,
    tx: &mut dyn WriteTransaction,
    event: &'static str,
    access: &McpClientAccess,
) -> Result<(), StorageError> {
    audit(
        tx,
        ctx,
        event,
        access.client_id.as_str(),
        &access.revision,
        json!({
            "enabled": access.enabled,
            "credentialGeneration": access.credential_generation,
        }),
    )
}

fn client_applied(value: McpClientAccess) -> DomainDecision {
    DomainDecision::Applied {
        result: OperationResult::McpServerSetClientAccess { value },
    }
}

/// Internal administration. The gateway and handler both require registered human ingress.
pub struct Administration;

impl QueryHandler for Administration {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let fail = |error: Failure| {
            query_fault(
                error,
                "mcp_server",
                CorrelationId(ctx.invocation_id().clone()),
            )
        };
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(fail(denied().into()));
        }
        let (revision, configuration) =
            current(snapshot.configuration()).map_err(|e| fail(e.into()))?;
        let epoch = snapshot.stamp().restore_epoch;
        let result = match query.input() {
            OperationInput::McpServer(ops::Input::Status(_)) => OperationResult::McpServerStatus {
                value: configuration.status(epoch),
            },
            OperationInput::McpServer(ops::Input::Clients(input)) => {
                let binding = CursorBinding::new(ctx, query.descriptor().name, &[], revision);
                let mut clients: Vec<_> = configuration
                    .clients
                    .into_iter()
                    .map(|mut client| {
                        client.access.enabled &= client.authorized_epoch == Some(epoch);
                        client.access
                    })
                    .collect();
                clients.sort_unstable_by(|a, b| a.client_id.as_str().cmp(b.client_id.as_str()));
                OperationResult::McpServerClients {
                    value: slice(&binding, clients, &input.page).map_err(|e| fail(e.into()))?,
                }
            }
            _ => return Err(fail(invalid("mcp_operation").into())),
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
            result,
        })
    }
}

impl CommandHandler for Administration {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return decided(tx, Err(denied().into()));
        }
        let (revision, mut configuration) = current(tx.configuration())?;
        let result = match command.input() {
            OperationInput::McpServer(ops::Input::Configure(input)) => {
                edit_configuration(ctx, tx, &mut configuration, input)
            }
            OperationInput::McpServer(ops::Input::SetClientAccess(input)) => {
                edit_client(ctx, tx, &mut configuration, &input.action)
            }
            _ => Err(invalid("mcp_operation").into()),
        };
        if result.is_ok() {
            let body = serde_json::to_value(&configuration)
                .map_err(|_| StorageError::InvalidRecord(NAMESPACE.into()))?;
            tx.configuration().append_revision(
                &ConfigScope::Workspace,
                NAMESPACE,
                revision,
                SCHEMA_VERSION,
                body,
            )?;
        }
        decided(tx, result)
    }
}
