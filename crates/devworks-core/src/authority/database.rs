//! Scoped source-registration authority shared by database admission and execution.
use super::reach::{Reach, reach};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::database::statement::DataMode;
use crate::ports::storage::{ReadSnapshot, RevisionConflict, StorageError, StoredSchemaSource};
use crate::protocol::records::SchemaExposureMode;
use crate::protocol::{Id, Revision, RevisionDimension};
use crate::settings::catalog::AgentRole;

pub(crate) fn scope_context(
    context: &InvocationContext,
    scope: Option<&crate::protocol::data::DataScope>,
) -> Result<InvocationContext, StorageError> {
    use crate::protocol::data::DataScope;
    match scope {
        Some(DataScope::Workspace) if context.project_id().is_some() => {
            Err(StorageError::ScopeDenied)
        }
        Some(DataScope::Project { project_id })
            if context
                .project_id()
                .is_some_and(|current| current != project_id) =>
        {
            Err(StorageError::ScopeDenied)
        }
        Some(DataScope::Project { project_id }) => Ok(context.in_project(project_id.clone())),
        _ => Ok(context.clone()),
    }
}

/// Applies the selected data scope before the generic authority policy is evaluated.
pub(crate) fn authority_scope(
    context: &crate::context::InvocationContext,
    input: &crate::operations::OperationInput,
) -> Result<Option<crate::context::InvocationContext>, StorageError> {
    use crate::operations::{OperationInput, database::Input};
    let scope = match input {
        OperationInput::Database(Input::Read(input)) => input.scope.as_ref(),
        OperationInput::Database(Input::MysqlReadonlyQuery(input)) => input.scope.as_ref(),
        OperationInput::Database(Input::ExecuteWrite(input)) => input.scope.as_ref(),
        OperationInput::Database(Input::ApplyMigration(input)) => input.scope.as_ref(),
        OperationInput::Database(Input::DirectSchemaChange(input)) => input.scope.as_ref(),
        _ => None,
    };
    scope
        .map(|scope| scope_context(context, Some(scope)))
        .transpose()
}

/// Source permission is checked before a command's receipt lookup, including replay-only grants.
pub(crate) fn mutation_fence(
    snapshot: &dyn crate::ports::storage::ReadSnapshot,
    ctx: &crate::context::InvocationContext,
    input: &crate::operations::OperationInput,
) -> Result<Option<crate::ports::AuthorityFence>, StorageError> {
    use crate::operations::{OperationInput, database::Input};
    let (id, mode, scope) = match input {
        OperationInput::Database(Input::ExecuteWrite(input)) => {
            (&input.connection_id, DataMode::Write, input.scope.as_ref())
        }
        OperationInput::Database(Input::ApplyMigration(input)) => (
            &input.connection_id,
            DataMode::Migration,
            input.scope.as_ref(),
        ),
        OperationInput::Database(Input::DirectSchemaChange(input)) => {
            (&input.connection_id, DataMode::Schema, input.scope.as_ref())
        }
        _ => return Ok(None),
    };
    let ctx = scope_context(ctx, scope)?;
    let revision = snapshot
        .schemas()
        .schema_source(id)?
        .ok_or(StorageError::ScopeDenied)?
        .connection
        .revision;
    source(snapshot, &ctx, id, &revision, mode)?;
    Ok(Some(crate::ports::AuthorityFence::DatabaseSource {
        id: id.clone(),
        revision: revision.get(),
    }))
}
pub(crate) fn source(
    read: &dyn ReadSnapshot,
    context: &InvocationContext,
    id: &Id,
    expected: &Revision,
    mode: DataMode,
) -> Result<StoredSchemaSource, StorageError> {
    let (project, human) = principal(read.authority(), context, mode)?;
    let exists = project
        .as_ref()
        .map(|project| read.artifacts().project_exists(project))
        .transpose()?
        .unwrap_or(false);
    let row = read.schemas().schema_source(id)?;
    validate(
        row,
        context,
        id,
        expected,
        mode,
        project.as_ref(),
        human,
        exists,
    )
}

pub(crate) fn transaction_source(
    tx: &mut dyn crate::ports::storage::WriteTransaction,
    context: &InvocationContext,
    id: &Id,
    expected: &Revision,
    mode: DataMode,
) -> Result<StoredSchemaSource, StorageError> {
    let (project, human) = principal(tx.authority(), context, mode)?;
    let exists = project
        .as_ref()
        .map(|project| tx.artifacts().project_exists(project))
        .transpose()?
        .unwrap_or(false);
    let row = tx.schemas().schema_source(id)?;
    validate(
        row,
        context,
        id,
        expected,
        mode,
        project.as_ref(),
        human,
        exists,
    )
}

fn principal(
    authority: &dyn crate::ports::storage::AuthorityRead,
    context: &InvocationContext,
    mode: DataMode,
) -> Result<(Option<crate::protocol::ProjectId>, bool), StorageError> {
    let denied = || StorageError::ScopeDenied;
    let human = context.principal_kind() == PrincipalKind::Human;
    let write = mode != DataMode::Read;
    let scope = reach(authority, context)?.ok_or_else(denied)?;
    let project = match &scope {
        Reach::Workspace => context.project_id(),
        Reach::Project(project) if context.project_id() == Some(project) => Some(project),
        Reach::Project(_) => return Err(denied()),
    };

    if !human {
        let binding = authority
            .principal(context.principal())?
            .filter(|binding| binding.active && binding.kind == context.principal_kind())
            .ok_or_else(denied)?;
        if context.principal_kind() == PrincipalKind::Host
            || (write
                && (context.principal_kind() != PrincipalKind::Agent
                    || matches!(
                        binding.role,
                        None | Some(
                            AgentRole::CodeReviewer | AgentRole::Dreaming | AgentRole::Scheduled
                        )
                    )))
        {
            return Err(denied());
        }
    }
    Ok((project.cloned(), human))
}

#[allow(clippy::too_many_arguments)]
fn validate(
    row: Option<StoredSchemaSource>,
    context: &InvocationContext,
    id: &Id,
    expected: &Revision,
    mode: DataMode,
    project: Option<&crate::protocol::ProjectId>,
    human: bool,
    project_exists: bool,
) -> Result<StoredSchemaSource, StorageError> {
    let denied = || StorageError::ScopeDenied;
    let row = row.filter(|row| !row.deleted).ok_or_else(denied)?;
    let config = row.connection.config.as_ref().ok_or_else(denied)?;
    let write = mode != DataMode::Read;
    let exposed = matches!(
        config.exposure_mode,
        Some(SchemaExposureMode::ReadOnly | SchemaExposureMode::ReadWrite)
    );
    if config.enabled == Some(false)
        || !exposed
        || (write
            && (!config.writes_enabled
                || !matches!(config.exposure_mode, Some(SchemaExposureMode::ReadWrite))))
    {
        return Err(denied());
    }
    let allowed = match project {
        Some(project) => {
            let existing = project_exists;
            existing
                && config.project_access.as_ref().is_some_and(|access| {
                    access.0.iter().any(|grant| {
                        grant.project_id == project.0
                            && if write {
                                grant.data_write_enabled
                            } else {
                                grant.data_read_enabled
                            }
                    })
                })
        }
        None if human && &row.owner_id == context.principal() => true,
        None => !write && config.workspace_exposed == Some(true),
    };
    if !allowed {
        return Err(denied());
    }
    if &row.connection.revision != expected {
        return Err(StorageError::RevisionConflict(Box::new(RevisionConflict {
            dimension: RevisionDimension::Source,
            target_kind: "schema_source",
            target_id: id.clone(),
            expected: expected.clone(),
            current: row.connection.revision,
        })));
    }
    Ok(row)
}
