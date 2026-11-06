//! Source changes are human-owned, revision-checked metadata transactions. No source DB writes.
use super::SchemaService;
use crate::{
    context::{InvocationContext, PrincipalKind},
    gateway::AdmittedCommand,
    operations::{OperationInput, schemas as ops},
    ports::{CommandHandler, Decided, HandlerFailure, IdKind, storage::*},
    protocol::{
        DomainErrorCode, EventId, Id, RevisionDimension,
        data::DataScope,
        records::*,
        results::{DomainDecision, OperationResult},
    },
    services::{
        knowledge,
        sessions::{Failure, Refusal, decided, not_found, rev},
    },
};
use serde_json::json;
fn refuse(code: DomainErrorCode, kind: &'static str) -> Failure {
    Refusal::new(code, kind, None).into()
}
fn human(ctx: &InvocationContext) -> Result<(), Failure> {
    if ctx.principal_kind() == PrincipalKind::Human {
        Ok(())
    } else {
        Err(refuse(
            DomainErrorCode::PermissionDenied,
            "schema_configuration",
        ))
    }
}
fn owner(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    expected: &crate::protocol::Revision,
) -> Result<StoredSchemaSource, Failure> {
    human(ctx)?;
    let row = tx
        .schemas()
        .schema_source(id)?
        .filter(|r| !r.deleted && &r.owner_id == ctx.principal())
        .ok_or_else(|| not_found("schema_connection", id))?;
    if let Some(project) = ctx.project_id()
        && !row.connection.config.as_ref().is_some_and(|c| {
            c.project_access.as_ref().is_some_and(|a| {
                a.0.iter()
                    .any(|a| a.project_id == project.0 && a.metadata_enabled)
            })
        })
    {
        return Err(not_found("schema_connection", id).into());
    }
    if row.connection.revision != *expected {
        return Err(StorageError::conflict(
            RevisionDimension::Connection,
            "schema_connection",
            id.clone(),
            expected.clone(),
            row.connection.revision.clone(),
        )
        .into());
    }
    Ok(row)
}
fn event(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    row: &StoredSchemaSource,
    kind: &str,
) -> Result<(), StorageError> {
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id, actor: ctx.principal().clone(), project_id: ctx.project_id().cloned(), root_id: ctx.root_id().cloned(), operation_id: None,
        aggregate_kind: "schema_connection".into(), aggregate_id: row.connection.id.as_str().to_owned(), aggregate_revision: row.connection.revision.get(), kind: kind.into(),
        payload: json!({"connectionId":row.connection.id,"revision":row.connection.revision,"metadataOnly":true}),
    })?;
    Ok(())
}
impl SchemaService {
    fn configure(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::ConfigureInput,
    ) -> Result<DomainDecision, Failure> {
        human(ctx)?;
        let reach = knowledge::reach(tx.authority(), ctx)?;
        knowledge::resolve_scope_with(&reach, ctx, &input.scope, true, |p| {
            tx.artifacts().project_exists(p)
        })?;
        let old = match (&input.connection_id, &input.expected_revision) {
            (Some(id), Some(expected)) => Some(owner(tx, ctx, id, expected)?),
            (None, None) => None,
            _ => {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "schema_source_revision",
                ));
            }
        };
        let removing = matches!(input.action, Some(SchemaSourceAction::Remove));
        if removing && old.is_none() {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "schema_source_remove",
            ));
        }
        if let Some(old) = old.as_ref() {
            super::indexing::cancel_active(tx, old)?;
        }
        if !removing {
            // Explicit target configuration has no inline passwords or URL authority credentials.
            if !matches!(input.config.engine, DbEngine::Sqlite) {
                let authority = input
                    .config
                    .endpoint
                    .0
                    .split_once("://")
                    .map(|(_, p)| p.split('/').next().unwrap_or(p));
                if authority.is_none()
                    || authority.is_some_and(|a| a.contains('@'))
                    || input.config.endpoint.0.contains(['?', '#'])
                {
                    return Err(refuse(DomainErrorCode::InvalidArgument, "schema_endpoint"));
                }
            }
            let mut projects = std::collections::BTreeSet::new();
            for access in input
                .config
                .project_access
                .as_ref()
                .map(|p| p.0.as_slice())
                .unwrap_or(&[])
            {
                if !projects.insert(access.project_id.as_str())
                    || !tx
                        .artifacts()
                        .project_exists(&crate::protocol::ProjectId(access.project_id.clone()))?
                {
                    return Err(refuse(
                        DomainErrorCode::InvalidArgument,
                        "schema_project_access",
                    ));
                }
                if access.data_write_enabled
                    && (!access.data_read_enabled || !input.config.writes_enabled)
                {
                    return Err(refuse(
                        DomainErrorCode::InvalidArgument,
                        "schema_project_write_policy",
                    ));
                }
            }
            if let DataScope::Project { project_id } = &input.scope
                && !input.config.project_access.as_ref().is_some_and(|a| {
                    a.0.iter()
                        .any(|a| a.project_id == project_id.0 && a.metadata_enabled)
                })
            {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "schema_project_access",
                ));
            }
        }
        if !removing && let Some(handle) = input.config.credential_ref.as_ref() {
            let retained = old
                .as_ref()
                .is_some_and(|old| old.connection.credential_ref.as_ref() == Some(handle));
            if !retained
                && !self
                    .credentials
                    .as_ref()
                    .is_some_and(|owner| owner.valid(ctx, handle, self.clock.now_ms().get()))
            {
                return Err(refuse(
                    DomainErrorCode::CredentialsRequired,
                    "schema_staged_credential",
                ));
            }
        }
        let now = self.clock.now_ms().get();
        let mut row = old.clone().unwrap_or_else(|| StoredSchemaSource {
            connection: SchemaConnection {
                id: tx.new_id(IdKind::Connection),
                revision: rev(1),
                name: input.config.name.clone(),
                engine: input.config.engine.clone(),
                credential_ref: input.config.credential_ref.clone(),
                state: ItemState::Enabled,
                current_view_id: None,
                writes_enabled: input.config.writes_enabled,
                metadata_only: true,
                config: Some(input.config.clone()),
                owner_id: Some(ctx.principal().clone()),
                profile: None,
                coverage: None,
                last_synced_at_ms: None,
                last_error: None,
                active_operation_id: None,
            },
            owner_id: ctx.principal().clone(),
            deleted: false,
            created_at_ms: now,
            updated_at_ms: now,
        });
        if let Some(old) = old.as_ref() {
            row.connection.revision =
                old.connection.revision.checked_next().ok_or_else(|| {
                    StorageError::InvalidRecord("schema revision overflow".into())
                })?;
        }
        row.updated_at_ms = now;
        row.connection.active_operation_id = None;
        if removing {
            row.deleted = true;
            row.connection.state = ItemState::Disabled;
            row.connection.current_view_id = None;
            tx.schemas().remove_schema_views(&row.connection.id)?;
        } else {
            // An edited target never retains a healthy old target pointer or old coverage claim.
            row.connection.name = input.config.name.clone();
            row.connection.engine = input.config.engine.clone();
            row.connection.credential_ref = input.config.credential_ref.clone();
            row.connection.writes_enabled = input.config.writes_enabled;
            row.connection.config = Some(input.config.clone());
            row.connection.state = if input.config.enabled == Some(false) {
                ItemState::Disabled
            } else {
                ItemState::Enabled
            };
            row.connection.current_view_id = None;
            row.connection.profile = None;
            row.connection.coverage = None;
            row.connection.last_synced_at_ms = None;
            row.connection.last_error = None;
        }
        if let Some(old) = old {
            tx.schemas()
                .revise_schema_source(&row, old.connection.revision.get())?;
        } else {
            tx.schemas().create_schema_source(&row)?;
        }
        event(
            tx,
            ctx,
            &row,
            if removing {
                "SchemaSourceRemoved"
            } else {
                "SchemaSourceConfigured"
            },
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::SchemasConfigure {
                value: row.connection,
            },
        })
    }
    fn remove_index(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::RemoveIndexInput,
    ) -> Result<DomainDecision, Failure> {
        let mut row = owner(tx, ctx, &input.connection_id, &input.expected_revision)?;
        super::indexing::cancel_active(tx, &row)?;
        row.connection.active_operation_id = None;
        let changed = row.connection.current_view_id.is_some();
        tx.schemas().remove_schema_views(&input.connection_id)?;
        row.connection.current_view_id = None;
        row.connection.profile = None;
        row.connection.coverage = None;
        row.connection.last_synced_at_ms = None;
        row.connection.last_error = None;
        row.connection.state = if row
            .connection
            .config
            .as_ref()
            .is_some_and(|c| c.enabled == Some(false))
        {
            ItemState::Disabled
        } else {
            ItemState::Enabled
        };
        row.connection.revision = row
            .connection
            .revision
            .checked_next()
            .ok_or_else(|| StorageError::InvalidRecord("schema revision overflow".into()))?;
        row.updated_at_ms = self.clock.now_ms().get();
        tx.schemas()
            .revise_schema_source(&row, input.expected_revision.get())?;
        event(tx, ctx, &row, "SchemaIndexRemoved")?;
        Ok(DomainDecision::Applied {
            result: OperationResult::SchemasRemoveIndex {
                value: MutationSummary {
                    id: row.connection.id,
                    revision: row.connection.revision,
                    changed,
                },
            },
        })
    }
}
impl CommandHandler for SchemaService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = match command.input() {
            OperationInput::Schemas(ops::Input::Configure(input)) => self.configure(tx, ctx, input),
            OperationInput::Schemas(ops::Input::RemoveIndex(input)) => {
                self.remove_index(tx, ctx, input)
            }
            OperationInput::Schemas(ops::Input::Index(input)) => self.decide_index(
                tx,
                ctx,
                command,
                (
                    &input.connection_id,
                    &input.expected_revision,
                    input.test_only.unwrap_or(false),
                    input.scope.as_ref(),
                ),
            ),
            OperationInput::Schemas(ops::Input::Refresh(input)) => self.decide_index(
                tx,
                ctx,
                command,
                (
                    &input.connection_id,
                    &input.expected_revision,
                    input.test_only.unwrap_or(false),
                    input.scope.as_ref(),
                ),
            ),
            _ => Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "schema_command",
            )),
        };
        let mut result = decided(tx, result)?;
        if let DomainDecision::Accepted { operation_id } = &result.decision {
            result.operation_id = Some(operation_id.clone());
        }
        Ok(result)
    }
}
