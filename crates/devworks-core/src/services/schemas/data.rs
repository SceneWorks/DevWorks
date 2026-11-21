//! Data access uses the stored target registration and an explicit data grant. Metadata
//! visibility and repository permissions do not grant access to data.
use super::certificates;
use crate::authority::reach::{Reach, reach};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    ReadSnapshot, RevisionConflict, SchemaObjectFilter, SchemaRead, SensitivePolicy, SensitiveRule,
    StorageError, StoredSchemaSource,
};
use crate::protocol::RevisionDimension;
use crate::protocol::records::{MetadataObjectKind, SchemaExposureMode};
use crate::protocol::{Id, Revision};
use crate::services::database::statement::DataMode;
use crate::settings::catalog::AgentRole;

pub(in crate::services) fn source(
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

pub(in crate::services) fn transaction_source(
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

pub(in crate::services) fn certificate(
    read: &dyn SchemaRead,
    source: &StoredSchemaSource,
) -> Result<Option<Vec<u8>>, StorageError> {
    certificates::for_source(read, source)
}

pub(in crate::services) fn sensitive_policy(
    read: &dyn SchemaRead,
    source: &StoredSchemaSource,
) -> Result<(SensitivePolicy, SensitivePolicy), StorageError> {
    let global = read.sensitive_policy(None)?;
    let local = read.sensitive_policy(Some(&source.connection.id))?;
    Ok((global, local))
}

/// Legacy IDs are translated from the sealed view explicitly selected by the source owner.
/// Store the physical names with the rule, so publishing another view cannot unset a rule.
pub(in crate::services) fn legacy_rules(
    read: &dyn SchemaRead,
    source: &StoredSchemaSource,
    ids: &[Id],
) -> Result<Vec<SensitiveRule>, StorageError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let view = source
        .connection
        .current_view_id
        .as_ref()
        .ok_or(StorageError::ScopeDenied)?;
    let retained = read.schema_view(view)?.filter(|r| {
        r.sealed
            && r.connection_id == source.connection.id
            && r.configuration_revision == source.configuration_revision
    });
    if retained.is_none() {
        return Err(StorageError::ScopeDenied);
    }
    ids.iter()
        .map(|id| {
            let get = |id: &Id| {
                read.schema_objects(
                    view,
                    &SchemaObjectFilter {
                        object_id: Some(id.clone()),
                        ..Default::default()
                    },
                    None,
                    1,
                )?
                .into_iter()
                .find(|object| &object.id == id)
                .ok_or(StorageError::ScopeDenied)
            };
            let column = get(id)?;
            if !matches!(column.kind, MetadataObjectKind::Column) {
                return Err(StorageError::ScopeDenied);
            }
            let table = get(column.parent_id.as_ref().ok_or(StorageError::ScopeDenied)?)?;
            if !matches!(table.kind, MetadataObjectKind::Table) {
                return Err(StorageError::ScopeDenied);
            }
            let column = column.metadata.ok_or(StorageError::ScopeDenied)?;
            let table = table.metadata.ok_or(StorageError::ScopeDenied)?;
            Ok(SensitiveRule {
                id: id.clone(),
                catalog: Some(table.catalog.0),
                schema: Some(table.schema.map(|value| value.0).unwrap_or_default()),
                table: Some(table.name.0),
                column: column.name.0,
                allow: false,
                reason: None,
            })
        })
        .collect()
}
