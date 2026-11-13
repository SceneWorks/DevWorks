//! Native ownership assessment. Explicit unresolved rows keep incomplete or
//! unknown partitions out of any reviewed preparation or execution permit.
use super::super::conn::db;
use devworks_core::ports::storage::{
    ProjectPurgeCandidate, ProjectPurgePlanning, ProjectPurgeReference, ProjectPurgeReferenceKind,
    ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason, ProjectPurgeRowWitness,
    ProjectPurgeUnresolved, ProjectPurgeUnresolvedReason as Unresolved, StorageError,
};
use devworks_core::protocol::ProjectId;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// How `policies()` reads one ownership policy document.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `policies` rows, each naming its table, class and disposition.
    Baseline,
    /// `additionalTables` rows naming purge control tables.
    PurgeControl,
    /// `additionalTables` rows, each naming its table, class and disposition.
    Tables,
    /// `tables` changed by added columns or indexes; the document adds no table.
    Columns,
}
struct PolicyDocument {
    /// The document's key in the assessment digest.
    key: &'static str,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a test matches the list to the directory")
    )]
    file: &'static str,
    shape: Shape,
    text: &'static str,
}
macro_rules! policy_document {
    ($key:literal, $shape:ident, $file:literal) => {
        PolicyDocument {
            key: $key,
            file: $file,
            shape: Shape::$shape,
            text: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/architecture/validation/project-purge/",
                $file
            )),
        }
    };
}
/// Every reviewed ownership policy document. `policies()` loads exactly this list and the
/// assessment digest hashes exactly this list, so a policy change invalidates prepared purges.
const POLICY_DOCUMENTS: &[PolicyDocument] = &[
    policy_document!("policy", Baseline, "table-selectors.json"),
    policy_document!("extension", PurgeControl, "schema36-extension.json"),
    policy_document!("stagingExtension", Tables, "schema37-extension.json"),
    policy_document!("recoveryExtension", Columns, "schema38-extension.json"),
    policy_document!("containerExtension", Tables, "schema39-extension.json"),
    policy_document!("assignmentExtension", Columns, "schema40-extension.json"),
    policy_document!("messageExtension", Tables, "schema41-extension.json"),
    policy_document!(
        "assignmentContextExtension",
        Tables,
        "schema42-extension.json"
    ),
    policy_document!(
        "providerContextExtension",
        Tables,
        "schema46-extension.json"
    ),
    policy_document!("messageHeadExtension", Tables, "schema47-extension.json"),
    policy_document!("instructionExtension", Tables, "schema49-extension.json"),
    policy_document!("watcherExtension", Tables, "schema50-extension.json"),
];
/// The SHA-256 of each policy document, by its digest key.
fn document_digests() -> BTreeMap<&'static str, String> {
    POLICY_DOCUMENTS
        .iter()
        .map(|document| {
            (
                document.key,
                devworks_core::fingerprint::sha256_hex(document.text.as_bytes()),
            )
        })
        .collect()
}
pub(super) type Key = (String, String);
fn invalid() -> StorageError {
    StorageError::IncompatibleSchema("unmapped project purge ownership policy".into())
}
fn policies(conn: &Connection) -> Result<BTreeMap<String, (String, String)>, StorageError> {
    let mut known = BTreeMap::new();
    for document in POLICY_DOCUMENTS {
        let value: Value = serde_json::from_str(document.text).map_err(|_| invalid())?;
        let rows = match document.shape {
            Shape::Baseline => value["policies"].as_array().ok_or_else(invalid)?,
            Shape::PurgeControl | Shape::Tables => {
                value["additionalTables"].as_array().ok_or_else(invalid)?
            }
            Shape::Columns => {
                value["tables"].as_object().ok_or_else(invalid)?;
                continue;
            }
        };
        for row in rows {
            let table = row["table"].as_str().ok_or_else(invalid)?;
            let (class, disposition) = match document.shape {
                Shape::PurgeControl => ("purge_control", "purge_control"),
                _ => (
                    row["class"].as_str().ok_or_else(invalid)?,
                    row["disposition"].as_str().ok_or_else(invalid)?,
                ),
            };
            if known
                .insert(table.into(), (class.into(), disposition.into()))
                .is_some()
            {
                return Err(invalid());
            }
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
        "work_dependencies" | "search_fts" => return Err(Unresolved::MixedOwnership),
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
type ScrubColumns = (&'static [&'static str], &'static [&'static str]);
fn scrub_columns(table: &str) -> Option<ScrubColumns> {
    Some(match table {
        "projects" => (&[], &[]),
        "project_details" => (&["project_id"], &[]),
        "artifacts" => (
            &[
                "owner_actor_id",
                "current_project_id",
                "origin_project_id",
                "origin_work_id",
            ],
            &["current_version_id"],
        ),
        "work_items" => (
            &["project_id", "status_id"],
            &["parent_feature_id", "associated_feature_id"],
        ),
        "workflow_statuses" | "index_runs" => (&["project_id"], &[]),
        "events" => (&["actor_id", "project_id"], &["root_id", "operation_id"]),
        "operations" => (&[], &["root_id", "assignment_id"]),
        "sessions" => (
            &[
                "conversation_id",
                "agent_id",
                "project_id",
                "created_by",
                "closed_by",
            ],
            &["root_id"],
        ),
        "conversations" => (&[], &["root_id"]),
        "assignments" => (
            &["root_id", "work_id", "session_id"],
            &["parent_id", "execution_id"],
        ),
        "resources" => (&["project_id"], &[]),
        "blockers" => (
            &["cleared_by"],
            &["root_id", "assignment_id", "operation_id"],
        ),
        "command_receipts" => (&["actor_id", "operation_id"], &[]),
        "project_managed_files" => (&["project_id"], &[]),
        "schema_sources" => (&["owner_id"], &[]),
        "authority_grants" => (&["actor_id", "issued_by", "revoked_by"], &[]),
        "github_issue_intake" => (
            &["repository_id", "project_id"],
            &["work_id", "operation_id"],
        ),
        "principal_authority" => (&["actor_id", "project_id", "bound_by"], &[]),
        _ => return None,
    })
}
fn minimal_identity(table: &str) -> bool {
    matches!(
        table,
        "projects"
            | "artifacts"
            | "work_items"
            | "workflow_statuses"
            | "events"
            | "operations"
            | "index_runs"
            | "sessions"
            | "conversations"
            | "assignments"
            | "resources"
            | "blockers"
    )
}
fn scrub_reference(table: &str, columns: &[String]) -> Option<bool> {
    let (retained, cleared) = scrub_columns(table)?;
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

pub(super) fn assign(
    candidate: &ProjectPurgeCandidate,
    action: Action,
    reason: Reason,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> bool {
    let key = (candidate.table.clone(), candidate.key_digest.clone());
    if rows.get(&key).is_some_and(|row| {
        row.action == action && row.reason == reason && row.row_digest == candidate.row_digest
    }) && !unresolved.contains_key(&key)
    {
        return false;
    }
    unresolved.remove(&key);
    rows.insert(
        key,
        ProjectPurgeRowWitness {
            table: candidate.table.clone(),
            key_digest: candidate.key_digest.clone(),
            row_digest: candidate.row_digest.clone(),
            action,
            reason,
        },
    );
    true
}
fn content_lineage(edge: &ProjectPurgeReference) -> bool {
    if edge.kind == ProjectPurgeReferenceKind::SearchChunk {
        return edge.from_table == "search_fts" && edge.to_table == "search_chunks";
    }
    if edge.kind != ProjectPurgeReferenceKind::ForeignKey {
        return false;
    }
    let column = match (edge.from_table.as_str(), edge.to_table.as_str()) {
        ("artifact_versions" | "artifact_tags" | "documents", "artifacts") => "artifact_id",
        ("artifact_links" | "extractions", "artifact_versions") => "artifact_version_id",
        ("search_chunks", "artifact_versions") => "source_version_id",
        ("chunk_vectors", "search_chunks") => "chunk_id",
        ("knowledge_revisions" | "knowledge_tags", "knowledge_entries") => "entry_id",
        _ => return false,
    };
    edge.from_columns.len() == 1 && edge.from_columns[0] == column
}
fn content_partitions(
    conn: &Connection,
    project: &ProjectId,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    for (table, condition, action, reason) in [
        (
            "artifacts",
            "retention='Explicit' AND scope_kind='Project' AND current_project_id=?1 AND NOT EXISTS(SELECT 1 FROM artifact_shares s WHERE s.artifact_id=artifacts.id AND s.project_id<>?1)",
            Action::Delete,
            Reason::OwnedProjectRecords,
        ),
        (
            "artifacts",
            "retention='Explicit' AND scope_kind='Project' AND current_project_id=?1 AND EXISTS(SELECT 1 FROM artifact_shares s WHERE s.artifact_id=artifacts.id AND s.project_id<>?1)",
            Action::Keep,
            Reason::SharedContent,
        ),
        (
            "artifacts",
            "retention='Explicit' AND (scope_kind='Workspace' OR current_project_id<>?1)",
            Action::Keep,
            Reason::IndependentPublication,
        ),
        (
            "knowledge_entries",
            "scope_kind='Project' AND project_id=?1 AND NOT EXISTS(SELECT 1 FROM knowledge_shares s WHERE s.entry_id=knowledge_entries.id AND s.project_id<>?1)",
            Action::Delete,
            Reason::OwnedProjectRecords,
        ),
        (
            "knowledge_entries",
            "scope_kind='Project' AND project_id=?1 AND EXISTS(SELECT 1 FROM knowledge_shares s WHERE s.entry_id=knowledge_entries.id AND s.project_id<>?1)",
            Action::Keep,
            Reason::SharedContent,
        ),
        (
            "knowledge_entries",
            "scope_kind='Workspace' OR project_id<>?1",
            Action::Keep,
            Reason::IndependentPublication,
        ),
        (
            "artifact_shares",
            "project_id=?1",
            Action::Delete,
            Reason::ProjectMembership,
        ),
        (
            "artifact_shares",
            "project_id<>?1",
            Action::Keep,
            Reason::SharedContent,
        ),
        (
            "knowledge_shares",
            "project_id=?1",
            Action::Delete,
            Reason::ProjectMembership,
        ),
        (
            "knowledge_shares",
            "project_id<>?1",
            Action::Keep,
            Reason::SharedContent,
        ),
        (
            "work_dependencies",
            "work_id IN(SELECT id FROM ownedWork)",
            Action::Delete,
            Reason::OwnedLineage,
        ),
        (
            "work_dependencies",
            "work_id NOT IN(SELECT id FROM ownedWork)",
            Action::Keep,
            Reason::IndependentPublication,
        ),
    ] {
        for row in selection.rows_where(conn, table, condition)? {
            assign(&row, action.clone(), reason.clone(), rows, unresolved);
        }
    }
    let removed_knowledge = "SELECT k.id FROM knowledge_entries k WHERE k.scope_kind='Project' AND k.project_id=?1 AND NOT EXISTS(SELECT 1 FROM knowledge_shares s WHERE s.entry_id=k.id AND s.project_id<>?1)";
    let removed_link = format!("from_id IN({removed_knowledge}) OR to_id IN({removed_knowledge})");
    for (condition, action, reason) in [
        (removed_link.clone(), Action::Delete, Reason::OwnedLineage),
        (
            format!("NOT({removed_link})"),
            Action::Keep,
            Reason::SharedContent,
        ),
    ] {
        for row in selection.rows_where(conn, "knowledge_links", &condition)? {
            assign(&row, action.clone(), reason.clone(), rows, unresolved);
        }
    }
    Ok(())
}
fn propagate_content(
    snapshot: &devworks_core::ports::storage::ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    // Follow only declared content ownership edges. Memberships and independent
    // referencing records never inherit a deletion from a referenced owner.
    let candidates = snapshot
        .rows
        .iter()
        .map(|row| ((row.table.clone(), row.key_digest.clone()), row))
        .collect::<BTreeMap<_, _>>();
    loop {
        let mut changed = false;
        for edge in snapshot
            .references
            .iter()
            .filter(|edge| content_lineage(edge))
        {
            let Some(parent) = rows
                .get(&(edge.to_table.clone(), edge.to_key_digest.clone()))
                .cloned()
            else {
                continue;
            };
            let reason = match parent.action {
                Action::Keep => parent.reason,
                Action::Delete => Reason::OwnedLineage,
                Action::Scrub => continue,
            };
            let Some(child) = candidates
                .get(&(edge.from_table.clone(), edge.from_key_digest.clone()))
                .copied()
            else {
                continue;
            };
            changed |= assign(child, parent.action, reason, rows, unresolved);
        }
        if !changed {
            break;
        }
    }
    Ok(())
}

pub(in crate::storage::sqlite) fn planning(
    conn: &Connection,
    project: &ProjectId,
) -> Result<Option<ProjectPurgePlanning>, StorageError> {
    planning_with_client_keepers(conn, project, &[])
}
pub(in crate::storage::sqlite) fn planning_with_client_keepers(
    conn: &Connection,
    project: &ProjectId,
    keepers: &[devworks_core::protocol::provider::ArtifactRef],
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
    content_partitions(conn, project, &mut rows, &mut unresolved)?;
    super::mixed::session_scope(conn, project, &mut rows, &mut unresolved)?;
    super::mixed::stage_operations(conn, project, &mut rows, &mut unresolved)?;
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
    super::receipts::partition(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    super::leases::partition(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    super::mixed::grants(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    super::mixed::intake(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    super::mixed::managed_files(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    super::mixed::source_access(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    let mut container_states =
        db(conn.prepare("SELECT id,state FROM owned_containers WHERE project_id=?1"))?;
    let mut active_containers = db(container_states.query([project.0.as_str()]))?;
    while let Some(row) = db(active_containers.next())? {
        let id: String = db(row.get(0))?;
        let state: String = db(row.get(1))?;
        if !matches!(state.as_str(), "Stopped" | "Failed") {
            let digest =
                super::guard::key_digest(&serde_json::to_string(&[id]).map_err(|_| invalid())?)?;
            let key = ("owned_containers".into(), digest.clone());
            rows.remove(&key);
            unresolved.insert(
                key,
                ProjectPurgeUnresolved {
                    table: "owned_containers".into(),
                    key_digest: digest,
                    reason: Unresolved::ActiveContainer,
                },
            );
        }
    }
    propagate_content(&snapshot, &mut rows, &mut unresolved)?;
    super::client_keepers::retain(conn, keepers, &snapshot, &mut rows, &mut unresolved)?;
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
                Some(_)
                    if edge.from_table == "schema_sources"
                        && matches!(
                            edge.kind,
                            ProjectPurgeReferenceKind::SchemaCertificate
                                | ProjectPurgeReferenceKind::SchemaSourceOperation
                        ) =>
                {
                    Some(true)
                }
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
            if minimal_identity(&row.table) {
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
    super::mixed::source_heads(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    // Keeper closure can retain a version that was initially owned; byte
    // eligibility must use those final decisions, never the earlier partition.
    super::blobs::partition(conn, project, &snapshot, &mut rows, &mut unresolved)?;
    let rows = rows.into_values().collect::<Vec<_>>();
    let unresolved = unresolved.into_values().collect::<Vec<_>>();
    let control_keys = rows
        .iter()
        .filter(|row| row.reason == Reason::PurgeControlAudit)
        .map(|row| (row.table.as_str(), row.key_digest.as_str()))
        .collect::<BTreeSet<_>>();
    let business_rows = rows
        .iter()
        .filter(|row| row.reason != Reason::PurgeControlAudit)
        .collect::<Vec<_>>();
    let references = snapshot
        .references
        .iter()
        .filter(|edge| {
            !control_keys.contains(&(edge.from_table.as_str(), edge.from_key_digest.as_str()))
                && !matches!(
                    edge.from_table.as_str(),
                    "project_purge_manifests"
                        | "project_purge_entries"
                        | "project_purge_rows"
                        | "project_purge_effects"
                        | "project_purged_subjects"
                )
        })
        .collect::<Vec<_>>();
    // Bind the actual compiled minimal-header/FK policy, not just prose describing it.
    let scrub_policy = known
        .keys()
        .filter_map(|table| {
            scrub_columns(table).map(|columns| (table, minimal_identity(table), columns))
        })
        .collect::<Vec<_>>();
    let digest=devworks_core::fingerprint::sha256_hex(serde_json::to_string(&serde_json::json!({
        "format":"dw83-ownership-assessment-v2","project":project,
        "policyDocuments":document_digests(),
        "scrubPolicy":scrub_policy,"clientKeepers":super::client_keepers::digest(keepers),"selection":snapshot.policy_digest,"rows":business_rows,"references":references,"unresolved":unresolved
    })).map_err(|_|invalid())?.as_bytes());
    Ok(Some(ProjectPurgePlanning {
        snapshot,
        digest,
        rows,
        unresolved,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assessment_digest_covers_every_policy_document_in_the_directory() {
        let directory = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/architecture/validation/project-purge"
        );
        let mut on_disk = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with("schema") && name.ends_with("-extension.json"))
            .collect::<BTreeSet<_>>();
        on_disk.insert("table-selectors.json".into());
        let listed = POLICY_DOCUMENTS
            .iter()
            .map(|document| document.file.to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            listed, on_disk,
            "every purge policy document must be listed in POLICY_DOCUMENTS \
             (project_purge/classification.rs), which both policies() and the assessment digest read"
        );
        assert_eq!(
            document_digests().len(),
            POLICY_DOCUMENTS.len(),
            "each policy document needs its own digest key"
        );
    }
}
