//! Transient native owner identities from an already verified read-only assessment.
use super::super::conn::db;
use super::candidates::{Selection, key_value, primary_keys, quote};
use devworks_core::ports::project_purge::{PurgeFileOwner, PurgeFileSelection};
use devworks_core::ports::storage::{
    ProjectPurgeClientScope, ProjectPurgeInventory, ProjectPurgePlanning,
    ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason, ProjectPurgeRowWitness,
    StorageError, is_terminal,
};
use devworks_core::protocol::{Id, OperationId, OperationState, ProjectId, StoreId};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;

fn invalid() -> StorageError {
    StorageError::InvalidRecord("native purge owner inventory is inconsistent".into())
}
type Materialized = (ProjectPurgeRowWitness, Vec<Value>);
fn materialize(
    conn: &Connection,
    selection: &Selection,
    plan: &ProjectPurgePlanning,
    table: &str,
    columns: &[&str],
) -> Result<Vec<Materialized>, StorageError> {
    let keys = primary_keys(conn, table)?;
    let projection = keys
        .iter()
        .map(String::as_str)
        .chain(columns.iter().copied())
        .map(quote)
        .collect::<Vec<_>>()
        .join(",");
    let sql = selection.sql(table, &projection, "1=1")?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    let mut result = Vec::new();
    let witnesses = plan
        .rows
        .iter()
        .filter(|r| r.table == table)
        .map(|r| (r.key_digest.as_str(), r))
        .collect::<BTreeMap<_, _>>();
    while let Some(record) = db(records.next())? {
        let key = (0..keys.len())
            .map(|i| key_value(db(record.get_ref(i))?))
            .collect::<Result<Vec<_>, _>>()?;
        let digest =
            super::guard::key_digest(&serde_json::to_string(&key).map_err(|_| invalid())?)?;
        let witness = witnesses.get(digest.as_str()).ok_or_else(invalid)?;
        let values = (keys.len()..keys.len() + columns.len())
            .map(|i| key_value(db(record.get_ref(i))?))
            .collect::<Result<Vec<_>, _>>()?;
        result.push(((*witness).clone(), values));
    }
    if result.len() != witnesses.len() {
        return Err(invalid());
    }
    Ok(result)
}
fn text(value: &Value) -> Result<&str, StorageError> {
    value.as_str().ok_or_else(invalid)
}
fn id(value: &Value) -> Result<Id, StorageError> {
    Id::new(text(value)?).map_err(|_| invalid())
}
fn owner(
    set: &mut BTreeMap<String, PurgeFileOwner>,
    owner: PurgeFileOwner,
) -> Result<(), StorageError> {
    set.insert(serde_json::to_string(&owner).map_err(|_| invalid())?, owner);
    Ok(())
}

fn kept_reference_versions(
    conn: &Connection,
    selection: &Selection,
    plan: &ProjectPurgePlanning,
    kept: &mut BTreeMap<String, PurgeFileOwner>,
) -> Result<(), StorageError> {
    // A workspace attachment linked to a P session is independent, but its exact content
    // still belongs in the review's kept set. Use the same typed targets as references.
    let targets = super::references::ARTIFACT_SUBJECT_TARGETS
        .iter()
        .map(|(kind, table)| {
            Ok(format!(
                "(subject_kind='{kind}' AND subject_id IN ({}))",
                selection.sql(table, "id", "1=1")?
            ))
        })
        .collect::<Result<Vec<_>, StorageError>>()?
        .join(" OR ");
    let keys = primary_keys(conn, "artifact_links")?;
    let projection = keys
        .iter()
        .map(String::as_str)
        .chain(["artifact_version_id"])
        .map(quote)
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT {projection} FROM artifact_links WHERE {targets}");
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    let actions = plan
        .rows
        .iter()
        .filter(|r| r.table == "artifact_links")
        .map(|r| (r.key_digest.as_str(), &r.action))
        .collect::<BTreeMap<_, _>>();
    let mut versions = BTreeMap::new();
    while let Some(record) = db(records.next())? {
        let key = (0..keys.len())
            .map(|i| key_value(db(record.get_ref(i))?))
            .collect::<Result<Vec<_>, _>>()?;
        let digest =
            super::guard::key_digest(&serde_json::to_string(&key).map_err(|_| invalid())?)?;
        if actions
            .get(digest.as_str())
            .is_some_and(|a| **a == Action::Delete)
        {
            continue;
        }
        let version: String = db(record.get(keys.len()))?;
        versions.insert(version.clone(), Id::new(version).map_err(|_| invalid())?);
    }
    for (key, version_id) in versions {
        let (digest, generation, deleted): (String,String,Option<i64>) = db(conn.query_row(
            "SELECT v.blob_digest,b.generation,v.deleted_at FROM artifact_versions v JOIN blobs b ON b.digest=v.blob_digest WHERE v.id=?1",
            [&key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))))?;
        owner(kept, PurgeFileOwner::Preview { version_id })?;
        if deleted.is_none() {
            owner(
                kept,
                PurgeFileOwner::Blob {
                    digest: digest.clone(),
                    generation: generation.clone(),
                },
            )?;
            owner(kept, PurgeFileOwner::Quarantine { digest, generation })?;
        }
    }
    Ok(())
}

pub(in crate::storage::sqlite) fn read(
    conn: &Connection,
    project: &ProjectId,
) -> Result<Option<ProjectPurgeInventory>, StorageError> {
    read_with_client_keepers(conn, project, &[])
}
pub(in crate::storage::sqlite) fn read_with_client_keepers(
    conn: &Connection,
    project: &ProjectId,
    keepers: &[devworks_core::protocol::provider::ArtifactRef],
) -> Result<Option<ProjectPurgeInventory>, StorageError> {
    let Some(planning) =
        super::classification::planning_with_client_keepers(conn, project, keepers)?
    else {
        return Ok(None);
    };
    if !planning.is_resolved() {
        return Err(StorageError::ScopeDenied);
    }
    let selection = super::candidates::selection(project)?;
    let (store, restore_epoch): (String, i64) = db(conn.query_row(
        "SELECT store_id,restore_epoch FROM store_meta WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))?;
    let store_id = StoreId(Id::new(store).map_err(|_| invalid())?);
    let mut removing = BTreeMap::new();
    let mut kept = BTreeMap::new();
    let mut unsettled_operations = Vec::new();
    let mut unsettled_deliveries = Vec::new();
    for (row, values) in materialize(
        conn,
        &selection,
        &planning,
        "queue_entries",
        &["id", "state"],
    )? {
        if row.reason != Reason::PurgeControlAudit {
            match text(&values[1])? {
                "Ready" | "Claimed" | "Reconciling" => unsettled_deliveries.push(id(&values[0])?),
                "Completed" | "Failed" | "Cancelled" | "Dismissed" => {}
                _ => return Err(invalid()),
            }
        }
    }

    let mut client = ProjectPurgeClientScope {
        project_id: project.clone(),
        session_ids: Vec::new(),
        conversation_ids: Vec::new(),
        work_ids: Vec::new(),
        operation_ids: Vec::new(),
        deleted_artifact_ids: Vec::new(),
        record_ids: BTreeMap::new(),
    };
    for table in planning
        .rows
        .iter()
        .filter(|r| {
            matches!(r.action, Action::Delete | Action::Scrub)
                && !matches!(
                    r.reason,
                    Reason::ProjectMembership | Reason::PurgeControlAudit
                )
        })
        .map(|r| r.table.as_str())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if primary_keys(conn, table)? != ["id"] {
            continue;
        }
        let mut ids = Vec::new();
        for (row, values) in materialize(conn, &selection, &planning, table, &["id"])? {
            if matches!(row.action, Action::Delete | Action::Scrub)
                && !matches!(
                    row.reason,
                    Reason::ProjectMembership | Reason::PurgeControlAudit
                )
            {
                ids.push(id(&values[0])?);
            }
        }
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.dedup();
        if !ids.is_empty() {
            client.record_ids.insert(table.into(), ids);
        }
    }
    for (table, destination) in [
        ("sessions", &mut client.session_ids),
        ("conversations", &mut client.conversation_ids),
        ("work_items", &mut client.work_ids),
    ] {
        for (_, values) in materialize(conn, &selection, &planning, table, &["id"])? {
            destination.push(id(&values[0])?);
        }
        destination.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        destination.dedup();
    }
    for (row, values) in materialize(conn, &selection, &planning, "artifacts", &["id"])? {
        if row.action == Action::Delete {
            client.deleted_artifact_ids.push(id(&values[0])?)
        }
    }
    for (row, values) in materialize(
        conn,
        &selection,
        &planning,
        "operations",
        &["id", "capability", "state"],
    )? {
        let operation = id(&values[0])?;
        if row.reason != Reason::PurgeControlAudit {
            client.operation_ids.push(operation.clone());
            let state: OperationState =
                serde_json::from_value(values[2].clone()).map_err(|_| invalid())?;
            if !is_terminal(state) {
                unsettled_operations.push(OperationId(operation.clone()));
            }
        }
        if row.reason != Reason::PurgeControlAudit
            && text(&values[1])? == devworks_core::ports::storage::STAGE_CAPABILITY
        {
            // A replay fence/kept version can require the operation header while its
            // unshared temporary bytes still belong to P and must be removed.
            let set = if row.reason == Reason::PurgeControlAudit {
                &mut kept
            } else {
                &mut removing
            };
            owner(
                set,
                PurgeFileOwner::ArtifactStaging {
                    operation_id: operation,
                },
            )?;
        }
    }
    for (row, values) in materialize(conn, &selection, &planning, "artifact_versions", &["id"])? {
        if row.reason == Reason::PurgeControlAudit {
            continue;
        }
        let set = match row.action {
            Action::Delete => &mut removing,
            Action::Keep => &mut kept,
            _ => return Err(invalid()),
        };
        owner(
            set,
            PurgeFileOwner::Preview {
                version_id: id(&values[0])?,
            },
        )?;
    }
    for (row, values) in materialize(
        conn,
        &selection,
        &planning,
        "blobs",
        &["digest", "generation"],
    )? {
        if row.reason == Reason::PurgeControlAudit {
            continue;
        }
        let set = match (&row.action, &row.reason) {
            (Action::Scrub, Reason::DerivedContent) => &mut removing,
            (
                Action::Keep,
                Reason::SharedContent | Reason::IndependentPublication | Reason::PurgeControlAudit,
            ) => &mut kept,
            _ => return Err(invalid()),
        };
        let digest = text(&values[0])?.to_owned();
        let generation = text(&values[1])?.to_owned();
        owner(
            set,
            PurgeFileOwner::Blob {
                digest: digest.clone(),
                generation: generation.clone(),
            },
        )?;
        owner(set, PurgeFileOwner::Quarantine { digest, generation })?;
    }
    for (_, values) in materialize(conn, &selection, &planning, "browser_sessions", &["id"])? {
        owner(
            &mut removing,
            PurgeFileOwner::BrowserProfile {
                session_id: id(&values[0])?,
            },
        )?;
    }
    for (_, values) in materialize(
        conn,
        &selection,
        &planning,
        "project_managed_files",
        &["id", "kind", "state"],
    )? {
        if text(&values[2])? == "Removed" {
            continue;
        }
        let file_id = id(&values[0])?;
        let file = match text(&values[1])? {
            "DiagnosticExport" => PurgeFileOwner::DiagnosticExport { file_id },
            "ClientPaste" => PurgeFileOwner::ClientPaste { file_id },
            "ClientStaging" => PurgeFileOwner::ClientStaging { file_id },
            // Preview attribution comes from the exact version, not a reservation ID.
            "Preview" => continue,
            _ => return Err(invalid()),
        };
        owner(&mut removing, file)?;
    }
    client
        .operation_ids
        .sort_by(|a, b| a.as_str().cmp(b.as_str()));
    client.operation_ids.dedup();
    client
        .deleted_artifact_ids
        .sort_by(|a, b| a.as_str().cmp(b.as_str()));
    client.deleted_artifact_ids.dedup();
    unsettled_operations.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    kept_reference_versions(conn, &selection, &planning, &mut kept)?;
    if removing.keys().any(|key| kept.contains_key(key)) {
        return Err(invalid());
    }
    let removing_files = PurgeFileSelection {
        project_id: project.clone(),
        store_id: store_id.clone(),
        restore_epoch,
        owners: removing.into_values().collect(),
    };
    let kept_files = PurgeFileSelection {
        project_id: project.clone(),
        store_id,
        restore_epoch,
        owners: kept.into_values().collect(),
    };
    Ok(Some(ProjectPurgeInventory {
        planning,
        removing_files,
        kept_files,
        client,
        unsettled_operations,
        unsettled_deliveries,
    }))
}
