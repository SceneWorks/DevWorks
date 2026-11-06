//! Resolve host-bound scope before source discovery, then project the shared definition.
use crate::{
    context::{InvocationContext, PrincipalKind},
    ports::storage::{ReadSnapshot, SchemaVisibility, StoredSchemaSource},
    protocol::{
        Id,
        data::DataScope,
        records::{SchemaConnection, SchemaExposureMode},
    },
    services::{
        knowledge,
        sessions::{Failure, not_found},
    },
};

pub(super) fn visibility(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    scope: &DataScope,
) -> Result<SchemaVisibility, Failure> {
    knowledge::resolve_scope(read.artifacts(), read.authority(), ctx, scope, false)?;
    let human = matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    );
    // A workspace request from a project principal cannot turn project access into a
    // workspace-wide discovery grant, even for a source exposed elsewhere.
    let project = match scope {
        DataScope::Project { project_id } => Some(project_id.0.clone()),
        DataScope::Workspace => match knowledge::reach(read.authority(), ctx)? {
            knowledge::Reach::Project(project) => Some(project.0),
            knowledge::Reach::Workspace => ctx.project_id().map(|p| p.0.clone()),
        },
    };
    Ok(SchemaVisibility {
        actor_id: ctx.principal().clone(),
        workspace: project.is_none(),
        project_id: project,
        exposed_only: !human,
    })
}

pub(super) fn sees(row: &StoredSchemaSource, v: &SchemaVisibility) -> bool {
    let Some(config) = row.connection.config.as_ref() else {
        return false;
    };
    !row.deleted
        && ((v.workspace && (row.owner_id == v.actor_id || config.workspace_exposed == Some(true)))
            || v.project_id.as_ref().is_some_and(|p| {
                config
                    .project_access
                    .as_ref()
                    .is_some_and(|a| a.0.iter().any(|a| &a.project_id == p && a.metadata_enabled))
            }))
        && (!v.exposed_only
            || (config.enabled != Some(false)
                && config
                    .exposure_mode
                    .as_ref()
                    .is_some_and(|m| !matches!(m, SchemaExposureMode::Hidden))))
}

pub(super) fn source(
    read: &dyn ReadSnapshot,
    id: &Id,
    visibility: &SchemaVisibility,
) -> Result<StoredSchemaSource, Failure> {
    read.schemas()
        .schema_source(id)?
        .filter(|r| sees(r, visibility))
        .ok_or_else(|| not_found("schema_connection", id).into())
}

pub(super) fn project(row: &StoredSchemaSource, v: &SchemaVisibility) -> SchemaConnection {
    let mut connection = row.connection.clone();
    if row.owner_id != v.actor_id || v.exposed_only {
        // Shared access does not disclose opaque credentials, certificate artifact references,
        // login identity or another project's associations/permissions.
        connection.credential_ref = None;
        if let Some(config) = connection.config.as_mut() {
            config.credential_ref = None;
            config.ca_certificate = None;
            config.username = None;
            if let Some(access) = config.project_access.as_mut() {
                access
                    .0
                    .retain(|a| v.project_id.as_ref() == Some(&a.project_id));
            }
            config.workspace_exposed = None;
        }
    }
    connection
}
