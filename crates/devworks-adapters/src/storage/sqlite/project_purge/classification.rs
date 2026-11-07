//! Native ownership assessment. Explicit unresolved rows keep incomplete or
//! unknown partitions out of any reviewed preparation or execution permit.
use super::super::conn::db;
use devworks_core::ports::storage::{
    ProjectPurgeCandidate, ProjectPurgePlanning, ProjectPurgeReferenceKind,
    ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason, ProjectPurgeRowWitness,
    ProjectPurgeUnresolved, ProjectPurgeUnresolvedReason as Unresolved, StorageError,
};
use devworks_core::protocol::ProjectId;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const POLICIES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/architecture/validation/project-purge/table-selectors.json"
));
const EXTENSION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/architecture/validation/project-purge/schema36-extension.json"
));
type Key = (String, String);
fn invalid() -> StorageError {
    StorageError::IncompatibleSchema("unmapped project purge ownership policy".into())
}
fn policies(conn: &Connection) -> Result<BTreeMap<String, (String, String)>, StorageError> {
    let policy: Value = serde_json::from_str(POLICIES).map_err(|_| invalid())?;
    let extension: Value = serde_json::from_str(EXTENSION).map_err(|_| invalid())?;
    let mut known = BTreeMap::new();
    for row in policy["policies"].as_array().ok_or_else(invalid)? {
        let (table, class, disposition) = (
            row["table"].as_str().ok_or_else(invalid)?,
            row["class"].as_str().ok_or_else(invalid)?,
            row["disposition"].as_str().ok_or_else(invalid)?,
        );
        if known
            .insert(table.into(), (class.into(), disposition.into()))
            .is_some()
        {
            return Err(invalid());
        }
    }
    for row in extension["additionalTables"]
        .as_array()
        .ok_or_else(invalid)?
    {
        let table = row["table"].as_str().ok_or_else(invalid)?;
        if known
            .insert(
                table.into(),
                ("purge_control".into(), "purge_control".into()),
            )
            .is_some()
        {
            return Err(invalid());
        }
    }
    let mut statement = db(conn.prepare("SELECT name FROM main.sqlite_schema WHERE type='table'"))?;
    let actual = db(statement.query_map([], |row| row.get::<_, String>(0)))?
        .map(db)
        .collect::<Result<BTreeSet<_>, _>>()?;
    if actual.iter().any(|name| !known.contains_key(name))
        || known
            .keys()
            .any(|name| name != "sqlite_schema" && !actual.contains(name))
    {
        return Err(invalid());
    }
    Ok(known)
}
fn initial(
    row: &ProjectPurgeCandidate,
    class: &str,
    disposition: &str,
) -> Result<(Action, Reason), Unresolved> {
    match row.table.as_str() {
        "projects" | "project_details" => {
            return Ok((Action::Scrub, Reason::MinimalDeletedIdentity));
        }
        "principal_authority" => return Ok((Action::Scrub, Reason::RevokedAuthority)),
        "events" | "command_receipts" | "receipt_replies" => {
            return Err(Unresolved::ReceiptControl);
        }
        "graph_read_pins" | "graph_read_pin_views" | "schema_read_pins" => {
            return Err(Unresolved::TypedLease);
        }
        "project_managed_files" => return Err(Unresolved::ManagedBytes),
        "knowledge_entries"
        | "knowledge_revisions"
        | "knowledge_tags"
        | "knowledge_shares"
        | "knowledge_links" => return Err(Unresolved::LegacySharing),
        "work_dependencies" => return Err(Unresolved::MixedOwnership),
        "operations" => return Ok((Action::Delete, Reason::OwnedLineage)),
        "browser_observations" | "os_notification_deliveries" => {
            return Ok((Action::Delete, Reason::OwnedLineage));
        }
        "resources" => return Ok((Action::Delete, Reason::OwnedProjectRecords)),
        "package_handler_commits" => return Ok((Action::Keep, Reason::IndependentPublication)),
        _ => {}
    }
    if class == "artifact_and_derived_lineage"
        || matches!(
            row.table.as_str(),
            "artifacts" | "artifact_shares" | "artifact_blob_pins" | "blobs"
        )
    {
        return Err(Unresolved::MixedOwnership);
    }
    match disposition {
        "project_owner" => Ok((Action::Delete, Reason::OwnedProjectRecords)),
        "owned_lineage" => Ok((Action::Delete, Reason::OwnedLineage)),
        "preserve_independent" => Ok((Action::Keep, Reason::IndependentPublication)),
        "mixed_explicit" => Err(Unresolved::MixedOwnership),
        _ => Err(Unresolved::UnmappedTable),
    }
}
fn scrub_reference(table: &str, columns: &[String]) -> Option<bool> {
    let (retained, cleared): (&[&str], &[&str]) = match table {
        "projects" => (&[], &[]),
        "project_details" => (&["project_id"], &[]),
        "work_items" => (
            &["project_id", "status_id"],
            &["parent_id", "feature_id", "active_plan_id"],
        ),
        "workflow_statuses" | "index_runs" => (&["project_id"], &[]),
        "events" => (&["actor_id", "project_id"], &["root_id", "operation_id"]),
        "operations" => (&[], &["root_id", "assignment_id"]),
        "principal_authority" => (&["actor_id", "project_id", "bound_by"], &[]),
        _ => return None,
    };
    if columns
        .iter()
        .all(|column| retained.contains(&column.as_str()))
    {
        Some(true)
    } else if columns
        .iter()
        .any(|column| cleared.contains(&column.as_str()))
    {
        Some(false)
    } else {
        None
    }
}

fn unresolved_keys(
    conn: &Connection,
    project: &ProjectId,
    sql: &str,
    table: &str,
    reason: Unresolved,
    output: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let mut statement = db(conn.prepare(sql))?;
    let mut records = db(statement.query([project.0.as_str()]))?;
    while let Some(row) = db(records.next())? {
        let key = [
            db(row.get::<_, String>(0))?,
            db(row.get::<_, String>(1))?,
            db(row.get::<_, String>(2))?,
        ];
        let digest =
            super::guard::key_digest(&serde_json::to_string(&key).map_err(|_| invalid())?)?;
        output.insert(
            (table.into(), digest.clone()),
            ProjectPurgeUnresolved {
                table: table.into(),
                key_digest: digest,
                reason: reason.clone(),
            },
        );
    }
    Ok(())
}

pub(in crate::storage::sqlite) fn planning(
    conn: &Connection,
    project: &ProjectId,
) -> Result<Option<ProjectPurgePlanning>, StorageError> {
    let Some(snapshot) = super::candidates::snapshot(conn, project)? else {
        return Ok(None);
    };
    let known = policies(conn)?;
    let mut rows = BTreeMap::<Key, ProjectPurgeRowWitness>::new();
    let mut unresolved = BTreeMap::<Key, ProjectPurgeUnresolved>::new();
    for row in &snapshot.rows {
        let key = (row.table.clone(), row.key_digest.clone());
        let (class, disposition) = known.get(&row.table).ok_or_else(invalid)?;
        match initial(row, class, disposition) {
            Ok((action, reason)) => {
                rows.insert(
                    key,
                    ProjectPurgeRowWitness {
                        table: row.table.clone(),
                        key_digest: row.key_digest.clone(),
                        row_digest: row.row_digest.clone(),
                        action,
                        reason,
                    },
                );
            }
            Err(reason) => {
                unresolved.insert(
                    key,
                    ProjectPurgeUnresolved {
                        table: row.table.clone(),
                        key_digest: row.key_digest.clone(),
                        reason,
                    },
                );
            }
        }
    }
    // Workspace-created project snapshots are independently attributed receipts.
    // Their recognized result needs explicit compaction; do not relabel the
    // CommandDecided event or silently omit private copied project content.
    unresolved_keys(
        conn,
        project,
        "SELECT actor_id,scope_key,idempotency_key FROM command_receipts WHERE command_kind='projects.create' AND json_extract(result_json,'$.receipt.decision.result.value.id')=?1",
        "command_receipts",
        Unresolved::AutomaticSnapshot,
        &mut unresolved,
    )?;
    // There is no native mapping writer with a closed typed ownership format.
    // Unmapped legacy records stay visible as a hold, never as string-matched owners.
    unresolved_keys(
        conn,
        project,
        "SELECT import_id,source_kind,source_id FROM import_mappings WHERE ?1 IS NOT NULL",
        "import_mappings",
        Unresolved::LegacyMapping,
        &mut unresolved,
    )?;
    for key in unresolved.keys() {
        rows.remove(key);
    }
    // Only retention decisions propagate. A reference can never add a deletion.
    loop {
        let mut changed = false;
        for edge in &snapshot.references {
            let source = (edge.from_table.clone(), edge.from_key_digest.clone());
            let target = (edge.to_table.clone(), edge.to_key_digest.clone());
            if unresolved.contains_key(&source) {
                continue;
            }
            let surviving = match rows.get(&source) {
                None => Some(true),
                Some(row) if row.action == Action::Delete => Some(false),
                Some(row) if row.action == Action::Keep => Some(true),
                Some(_) if edge.kind == ProjectPurgeReferenceKind::ForeignKey => {
                    scrub_reference(&edge.from_table, &edge.from_columns)
                }
                Some(_) => None,
            };
            if surviving == Some(false) {
                continue;
            }
            if surviving.is_none() {
                if rows.remove(&source).is_some() {
                    unresolved.insert(
                        source,
                        ProjectPurgeUnresolved {
                            table: edge.from_table.clone(),
                            key_digest: edge.from_key_digest.clone(),
                            reason: Unresolved::ScrubReference,
                        },
                    );
                    changed = true;
                }
                continue;
            }
            let Some(row) = rows.get_mut(&target) else {
                continue;
            };
            if row.action != Action::Delete {
                continue;
            }
            if matches!(
                row.table.as_str(),
                "projects"
                    | "work_items"
                    | "workflow_statuses"
                    | "events"
                    | "operations"
                    | "index_runs"
            ) {
                row.action = Action::Scrub;
                row.reason = Reason::MinimalDeletedIdentity;
            } else {
                let row = rows.remove(&target).expect("selected target");
                unresolved.insert(
                    target,
                    ProjectPurgeUnresolved {
                        table: row.table,
                        key_digest: row.key_digest,
                        reason: Unresolved::SurvivingReference,
                    },
                );
            }
            changed = true;
        }
        if !changed {
            break;
        }
    }
    Ok(Some(ProjectPurgePlanning {
        snapshot,
        rows: rows.into_values().collect(),
        unresolved: unresolved.into_values().collect(),
    }))
}
