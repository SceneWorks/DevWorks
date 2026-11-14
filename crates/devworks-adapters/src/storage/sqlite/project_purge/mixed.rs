//! Explicit mixed-owner metadata; source text and opaque names never grant scope.
use super::super::conn::db;
use super::classification::{Key, assign};
use devworks_core::ports::storage::{
    ProjectPurgeCandidates, ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason,
    ProjectPurgeRowWitness, ProjectPurgeUnresolved, StorageError,
};
use devworks_core::protocol::ProjectId;
use rusqlite::Connection;
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StagePayload {
    digest: String,
    byte_count: i64,
    #[serde(default)]
    request_digest: Option<String>,
    project_id: ProjectId,
    source: serde_json::Value,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VersionStageSource {
    source: serde_json::Value,
    operation_id: devworks_core::protocol::OperationId,
    produced_by: devworks_core::protocol::ActorId,
    principal_kind: String,
    #[serde(default)]
    session_id: Option<devworks_core::protocol::Id>,
    #[serde(default)]
    repository_id: Option<devworks_core::protocol::Id>,
    #[serde(default)]
    repository_revision: Option<i64>,
    #[serde(default)]
    original_name: Option<String>,
}

pub(super) fn stage_operations(
    conn: &Connection,
    project: &ProjectId,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::protocol::support::Check;
    let selection = super::candidates::selection(project)?;
    let candidates =
        selection.rows_where(conn, "operations", "capability='artifacts.stage_file'")?;
    let sql = selection.sql(
        "operations",
        "id,schema_version,args_digest,payload_json",
        "capability='artifacts.stage_file'",
    )?;
    let mut query = db(conn.prepare(&sql))?;
    let mut records = db(query.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let id: String = db(record.get(0))?;
        let revision: i64 = db(record.get(1))?;
        let fingerprint: String = db(record.get(2))?;
        let body: String = db(record.get(3))?;
        let key = super::guard::key_digest(&serde_json::to_string(&[&id]).expect("ID key"))?;
        let payload = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                let canonical_fingerprint =
                    devworks_core::fingerprint::sha256_hex(value.to_string().as_bytes());
                serde_json::from_value::<StagePayload>(value)
                    .ok()
                    .map(|p| (p, canonical_fingerprint))
            });
        let mut valid = payload
            .as_ref()
            .is_some_and(|(payload, canonical_fingerprint)| {
                revision == 1
                    && payload.project_id == *project
                    && payload.byte_count >= 0
                    && payload.source.is_object()
                    && super::digest(&payload.digest)
                    && payload
                        .request_digest
                        .as_ref()
                        .is_none_or(|d| super::digest(d))
                    && fingerprint == *canonical_fingerprint
            });
        if let Some((payload, _)) = payload.filter(|_| valid) {
            let mut versions = db(conn.prepare("SELECT blob_digest,source_json FROM artifact_versions WHERE json_type(source_json,'$.operationId')='text' AND json_extract(source_json,'$.operationId')=?1"))?;
            let mut versions = db(versions.query([&id]))?;
            while let Some(version) = db(versions.next())? {
                let digest: String = db(version.get(0))?;
                let source: String = db(version.get(1))?;
                valid &= serde_json::from_str::<VersionStageSource>(&source).is_ok_and(|source| {
                    source.operation_id.0.as_str() == id
                        && source.source == payload.source
                        && source.produced_by.check().is_ok()
                        && digest == payload.digest
                        && matches!(
                            source.principal_kind.as_str(),
                            "Human" | "Agent" | "Package" | "McpClient" | "Schedule" | "Host"
                        )
                        && source.session_id.as_ref().is_none_or(|s| s.check().is_ok())
                        && source
                            .repository_id
                            .as_ref()
                            .is_none_or(|s| s.check().is_ok())
                        && source.repository_revision.is_none_or(|r| r >= 1)
                        && source
                            .original_name
                            .as_ref()
                            .is_none_or(|n| !n.contains('\0'))
                });
            }
        }
        let candidate = candidates
            .iter()
            .find(|r| r.key_digest == key)
            .ok_or_else(|| {
                StorageError::InvalidRecord("missing native staging candidate".into())
            })?;
        if valid {
            assign(
                candidate,
                Action::Delete,
                Reason::OwnedProjectRecords,
                rows,
                unresolved,
            );
        } else {
            rows.remove(&(candidate.table, key.clone()));
            unresolved.insert((candidate.table, key.clone()), ProjectPurgeUnresolved {
                table: candidate.table, key_digest: key,
                reason: devworks_core::ports::storage::ProjectPurgeUnresolvedReason::AutomaticSnapshot,
            });
        }
    }
    Ok(())
}

pub(super) fn session_scope(
    conn: &Connection,
    project: &ProjectId,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql("sessions", "id,project_id,root_id", "1=1")?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (id, owner, root) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, Option<String>>(1))?,
            db(record.get::<_, Option<String>>(2))?,
        );
        let root_matches = match root {
            None => true,
            Some(root) => db(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2)",
                [root.as_str(), project.0.as_str()],
                |r| r.get::<_, bool>(0),
            ))?,
        };
        if owner.as_deref() != Some(project.0.as_str()) || !root_matches {
            let key =
                super::guard::key_digest(&serde_json::to_string(&[id]).expect("session key"))?;
            rows.remove(&(
                devworks_core::ports::storage::ProjectRecordKind::Sessions,
                key.clone(),
            ));
            unresolved.insert(
                (
                    devworks_core::ports::storage::ProjectRecordKind::Sessions,
                    key.clone(),
                ),
                ProjectPurgeUnresolved {
                    table: devworks_core::ports::storage::ProjectRecordKind::Sessions,
                    key_digest: key,
                    reason:
                        devworks_core::ports::storage::ProjectPurgeUnresolvedReason::LegacyMapping,
                },
            );
        }
    }
    Ok(())
}

pub(super) fn source_access(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::ports::storage::StoredSchemaSource;
    use devworks_core::protocol::Check;
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql(
        "schema_sources",
        "id,owner_id,revision,configuration_revision,deleted,body_json",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (id, owner, revision, configuration, deleted, encoded) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, String>(1))?,
            db(record.get::<_, i64>(2))?,
            db(record.get::<_, i64>(3))?,
            db(record.get::<_, bool>(4))?,
            db(record.get::<_, String>(5))?,
        );
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(&encoded) else {
            continue;
        };
        let Ok(source) = serde_json::from_value::<StoredSchemaSource>(raw.clone()) else {
            continue;
        };
        if source.connection.check().is_err()
            || source.connection.id.as_str() != id
            || source.owner_id.0.as_str() != owner
            || source.connection.owner_id.as_ref() != Some(&source.owner_id)
            || source.connection.revision.get() != revision
            || source.configuration_revision != configuration
            || source.deleted != deleted
            || serde_json::to_value(&source).ok() != Some(raw)
            || !source.connection.config.as_ref().is_some_and(|c| {
                c.project_access
                    .as_ref()
                    .is_some_and(|a| a.0.iter().any(|a| a.project_id == project.0))
            })
        {
            continue;
        }
        let key =
            super::guard::key_digest(&serde_json::to_string(&[id]).expect("source access key"))?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "schema_sources" && r.key_digest == key)
        {
            assign(
                candidate,
                Action::Scrub,
                Reason::ProjectMembership,
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}

pub(super) fn managed_files(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::ports::project_purge::{PurgeFileOwner, PurgeFileWitness};
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql(
        "project_managed_files",
        "id,kind,relative_path,identity_json,byte_count,state",
        "1=1",
    )?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (id, kind, path, encoded, bytes, state) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, String>(1))?,
            db(record.get::<_, String>(2))?,
            db(record.get::<_, String>(3))?,
            db(record.get::<_, i64>(4))?,
            db(record.get::<_, String>(5))?,
        );
        if !matches!(
            kind.as_str(),
            "DiagnosticExport" | "ClientPaste" | "ClientStaging" | "Preview"
        ) {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<serde_json::Value>(&encoded) else {
            continue;
        };
        let action = if state == "Removed" && path.is_empty() && raw == serde_json::json!({}) {
            Action::Delete
        } else if matches!(state.as_str(), "Live" | "Removing") {
            let Ok(witness) = serde_json::from_value::<PurgeFileWitness>(raw.clone()) else {
                continue;
            };
            let owner = matches!((&*kind,&witness.owner),("DiagnosticExport",PurgeFileOwner::DiagnosticExport{file_id})|("ClientPaste",PurgeFileOwner::ClientPaste{file_id})|("ClientStaging",PurgeFileOwner::ClientStaging{file_id}) if file_id.as_str()==id);
            let normal = std::path::Path::new(&path);
            if !owner
                || witness.project_id != *project
                || witness.relative_entry != path
                || i64::try_from(witness.byte_count).ok() != Some(bytes)
                || witness.restore_epoch < 0
                || witness.directory
                || witness.link
                || serde_json::to_value(&witness).ok() != Some(raw)
                || devworks_core::fingerprint::Fingerprint::from_hex(&witness.content_digest)
                    .is_none()
                || path.contains('\\')
                || normal.components().count() != 1
                || !matches!(
                    normal.components().next(),
                    Some(std::path::Component::Normal(_))
                )
            {
                continue;
            }
            // This resolves only database attribution. Actual roots, contents,
            // store epoch and byte eligibility still require native inventory.
            Action::Scrub
        } else {
            continue;
        };
        let key =
            super::guard::key_digest(&serde_json::to_string(&[id]).expect("managed file key"))?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "project_managed_files" && r.key_digest == key)
        {
            assign(candidate, action, Reason::DerivedContent, rows, unresolved);
        }
    }
    Ok(())
}

pub(super) fn intake(
    conn: &Connection,
    project: &ProjectId,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    // These are association edges with no independent body. The frozen policy
    // removes an edge when its explicitly owned work or integration record goes.
    for candidate in selection.rows_where(conn, "integration_links", "1=1")? {
        assign(
            &candidate,
            Action::Delete,
            Reason::OwnedLineage,
            rows,
            unresolved,
        );
    }
    Ok(())
}

pub(super) fn grants(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    use devworks_core::authority::service::GrantBinding;
    use devworks_core::fingerprint::Fingerprint;
    use devworks_core::registry::Mode;
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql("authority_grants", "id,kind,binding_json", "1=1")?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (id, kind, encoded) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, String>(1))?,
            db(record.get::<_, String>(2))?,
        );
        if !matches!(kind.as_str(), "Operation" | "NativeExecution") {
            continue;
        }
        let Ok(binding) = serde_json::from_str::<GrantBinding>(&encoded) else {
            continue;
        };
        if binding.project_id.as_ref().is_some_and(|p| p != project)
            || !devworks_core::registry::find(&binding.operation)
                .is_some_and(|d| d.binding(Mode::Command).is_some())
            || [&binding.request_digest, &binding.policy_digest]
                .into_iter()
                .flatten()
                .any(|d| Fingerprint::from_hex(d).is_none())
        {
            continue;
        }
        if let Some(root) = &binding.root_id {
            let owned: bool = db(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2)",
                [root.0.as_str(), project.0.as_str()],
                |r| r.get(0),
            ))?;
            if !owned {
                continue;
            }
        } else if binding.project_id.as_ref() != Some(project) {
            continue;
        }
        let key = super::guard::key_digest(&serde_json::to_string(&[id]).expect("grant key"))?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "authority_grants" && r.key_digest == key)
        {
            assign(
                candidate,
                Action::Scrub,
                Reason::RevokedAuthority,
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}

pub(super) fn source_heads(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    let sql = selection.sql("knowledge_source_heads", "source_ref", "1=1")?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let source = db(record.get::<_, String>(0))?;
        let key_digest =
            super::guard::key_digest(&serde_json::to_string(&[&source]).expect("source head key"))?;
        let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "knowledge_source_heads" && r.key_digest == key_digest)
        else {
            continue;
        };
        let mut keeper = false;
        let mut known = false;
        for (table, projection) in [
            ("knowledge_claim_basis", "claim_id,source_ref"),
            ("knowledge_evidence", "id"),
        ] {
            let mut references = db(conn.prepare(&format!(
                "SELECT {projection} FROM {table} WHERE source_ref=?1"
            )))?;
            let mut references = db(references.query([&source]))?;
            while let Some(reference) = db(references.next())? {
                known = true;
                let mut tuple = vec![db(reference.get::<_, String>(0))?];
                if table == "knowledge_claim_basis" {
                    tuple.push(db(reference.get::<_, String>(1))?);
                }
                let key = (
                    super::record_kind(table)?,
                    super::guard::key_digest(
                        &serde_json::to_string(&tuple).expect("source reference key"),
                    )?,
                );
                keeper |= rows
                    .get(&key)
                    .is_none_or(|row| row.action != Action::Delete)
                    || unresolved.contains_key(&key);
            }
        }
        if known {
            assign(
                candidate,
                if keeper { Action::Keep } else { Action::Delete },
                if keeper {
                    Reason::SharedContent
                } else {
                    Reason::OwnedLineage
                },
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}
