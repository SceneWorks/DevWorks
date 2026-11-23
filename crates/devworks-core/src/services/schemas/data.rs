//! Data access uses the stored target registration and an explicit data grant. Metadata
//! visibility and repository permissions do not grant access to data.
use super::certificates;
pub(in crate::services) use crate::authority::database::{source, transaction_source};
use crate::ports::storage::{
    SchemaObjectFilter, SchemaRead, SensitivePolicy, SensitiveRule, StorageError,
    StoredSchemaSource,
};
use crate::protocol::Id;
use crate::protocol::records::MetadataObjectKind;

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
