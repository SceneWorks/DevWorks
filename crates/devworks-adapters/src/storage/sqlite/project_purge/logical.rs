//! Purpose-bound logical purge. Exact keys are materialized transiently from the fresh
//! native assessment. Foreign keys stay enabled and the ordinary collector gets no guard.
use super::super::conn::db;
use super::{candidates, effects, guard, invalid};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::project_purge::{PurgeEffectIntent, PurgeFileOwner};
use devworks_core::ports::storage::{
    ProjectPurgePlanning, ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason,
    ProjectPurgeRowWitness, StorageError, StoreInfo, project_purge_digest,
};
use devworks_core::protocol::{
    Counter, Id, OperationId, Revision,
    records::{ProjectPurgeManifest, PurgePhase},
};
use devworks_core::services::projects::purge::PurgePermit;
use rusqlite::{Connection, params, params_from_iter, types::Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Target {
    pub(super) row: ProjectPurgeRowWitness,
    pub(super) keys: Vec<Value>,
    pub(super) predicate: String,
}
fn targets(conn: &Connection, plan: &ProjectPurgePlanning) -> Result<Vec<Target>, StorageError> {
    let selection = candidates::selection(&plan.snapshot.project_id)?;
    let mut result = Vec::new();
    let tables = plan
        .rows
        .iter()
        .filter(|r| r.reason != Reason::PurgeControlAudit)
        .map(|r| r.table.as_str())
        .collect::<BTreeSet<_>>();
    for table in tables {
        let mut columns = candidates::primary_keys(conn, table)?;
        if columns.is_empty() {
            columns.push("rowid".into());
        }
        let projection = columns
            .iter()
            .map(|c| candidates::quote(c))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = db(conn.prepare(&selection.sql(table, &projection, "1=1")?))?;
        let mut rows = db(statement.query(selection.parameters()))?;
        let wanted = plan
            .rows
            .iter()
            .filter(|r| r.table == table && r.reason != Reason::PurgeControlAudit)
            .map(|r| (r.key_digest.as_str(), r))
            .collect::<BTreeMap<_, _>>();
        let mut seen = BTreeSet::new();
        while let Some(row) = db(rows.next())? {
            let key = (0..columns.len())
                .map(|i| candidates::key_value(db(row.get_ref(i))?))
                .collect::<Result<Vec<_>, _>>()?;
            let digest =
                guard::key_digest(&serde_json::to_string(&key).map_err(|_| invalid("purge key"))?)?;
            let Some(witness) = wanted.get(digest.as_str()) else {
                continue;
            };
            if !seen.insert(digest) {
                return Err(invalid("duplicate native purge key"));
            }
            let keys = key
                .into_iter()
                .map(|v| match v {
                    serde_json::Value::Null => Ok(Value::Null),
                    serde_json::Value::String(s) => Ok(Value::Text(s)),
                    serde_json::Value::Number(n) => n
                        .as_i64()
                        .map(Value::Integer)
                        .ok_or_else(|| invalid("unmapped numeric purge key")),
                    _ => Err(invalid("unmapped native purge key")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let predicate = columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{} IS ?{}", candidates::quote(c), i + 1))
                .collect::<Vec<_>>()
                .join(" AND ");
            result.push(Target {
                row: (*witness).clone(),
                keys,
                predicate,
            });
        }
        if seen.len() != wanted.len() {
            return Err(StorageError::ScopeDenied);
        }
    }
    Ok(result)
}
fn kept_head(
    conn: &Connection,
    target: &Target,
    plan: &ProjectPurgePlanning,
) -> Result<Id, StorageError> {
    let mut statement = db(conn.prepare("SELECT id FROM artifact_versions WHERE artifact_id=?1 AND deleted_at IS NULL ORDER BY version_no DESC,id"))?;
    let versions = db(
        statement.query_map(params_from_iter(target.keys.iter()), |r| {
            r.get::<_, String>(0)
        }),
    )?;
    for version in versions {
        let version = db(version)?;
        let key = guard::key_digest(&serde_json::to_string(&[&version]).expect("version key"))?;
        if plan
            .rows
            .iter()
            .find(|r| r.table == "artifact_versions" && r.key_digest == key)
            .is_none_or(|r| r.action == Action::Keep)
        {
            return Id::new(version).map_err(|_| invalid("kept artifact version"));
        }
    }
    Err(invalid("minimal artifact has no kept version"))
}
fn insert_effect(
    conn: &Connection,
    manifest: &Id,
    intent: PurgeEffectIntent,
) -> Result<(), StorageError> {
    let body = serde_json::to_string(&intent).map_err(|_| invalid("purge effect body"))?;
    let (kind, digest, bytes) = match &intent {
        PurgeEffectIntent::File { witness } => (
            "File",
            sha256_hex(
                &serde_json::to_vec(&witness.owner).map_err(|_| invalid("purge file owner"))?,
            ),
            i64::try_from(witness.byte_count).map_err(|_| invalid("purge file length"))?,
        ),
        PurgeEffectIntent::Client { scope, .. } => (
            "Client",
            sha256_hex(&serde_json::to_vec(scope).map_err(|_| invalid("purge client scope"))?),
            0,
        ),
    };
    db(conn.execute("INSERT INTO project_purge_effects(manifest_id,id,kind,owner_digest,body_json,byte_count,state) VALUES(?1,?2,?3,?4,?5,?6,'Pending')",params![manifest.as_str(),sha256_hex(body.as_bytes()),kind,digest,body,bytes]))?;
    Ok(())
}

pub(in crate::storage::sqlite) fn apply(
    conn: &Connection,
    info: &StoreInfo,
    now: i64,
    operation: OperationId,
    permit: &PurgePermit,
) -> Result<ProjectPurgeManifest, StorageError> {
    super::schema::verify(conn)?;
    let reviewed = permit.reviewed();
    let project = &reviewed.manifest.project_id;
    if reviewed.store_id != info.store_id
        || reviewed.restore_epoch != info.restore_epoch
        || conn.is_autocommit()
        || project_purge_digest(reviewed) != reviewed.manifest.digest.as_str()
    {
        return Err(StorageError::ScopeDenied);
    }
    let foreign_keys: bool = db(conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)))?;
    if !foreign_keys {
        return Err(StorageError::ScopeDenied);
    }
    let stored =
        super::manifest(conn, project, &reviewed.manifest.id)?.ok_or(StorageError::ScopeDenied)?;
    if !matches!(stored.manifest.phase, PurgePhase::Prepared)
        || stored.manifest.revision != reviewed.manifest.revision
        || stored.manifest.digest != reviewed.manifest.digest
    {
        return Err(StorageError::ScopeDenied);
    }
    let plan = super::classification::planning_with_client_keepers(
        conn,
        project,
        &permit.client().surviving_artifacts,
    )?
    .ok_or(StorageError::ScopeDenied)?;
    let business = plan
        .rows
        .iter()
        .filter(|r| r.reason != Reason::PurgeControlAudit)
        .cloned()
        .collect::<Vec<_>>();
    if !plan.is_resolved() || plan.digest != reviewed.policy_digest || business != reviewed.rows {
        return Err(StorageError::ScopeDenied);
    }
    let targets = targets(conn, &plan)?;
    let mut heads = BTreeMap::new();
    for target in targets
        .iter()
        .filter(|t| t.row.table == "artifacts" && t.row.action == Action::Scrub)
    {
        heads.insert(
            target.row.key_digest.clone(),
            kept_head(conn, target, &plan)?,
        );
    }
    for target in targets
        .iter()
        .filter(|t| t.row.table == "operations" && t.row.action != Action::Keep)
    {
        let state: String = db(conn.query_row(
            &format!("SELECT state FROM operations WHERE {}", target.predicate),
            params_from_iter(target.keys.iter()),
            |r| r.get(0),
        ))?;
        let state = serde_json::from_value(serde_json::Value::String(state))
            .map_err(|_| invalid("purge operation state"))?;
        if !devworks_core::ports::storage::is_terminal(state) {
            return Err(StorageError::Busy);
        }
    }
    let payload = serde_json::json!({"projectId":project,"manifestId":reviewed.manifest.id});
    db(conn.execute("INSERT INTO operations(id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES(?1,'projects.purge',1,?2,?3,'IntentRecorded',1,?4)",params![operation.0.as_str(),sha256_hex(payload.to_string().as_bytes()),payload.to_string(),now]))?;
    let mut manifest = reviewed.manifest.clone();
    manifest.operation_id = Some(operation);
    manifest.phase = PurgePhase::RemovingBytes;
    manifest.revision =
        Revision::new(manifest.revision.get() + 1).map_err(|_| invalid("purge revision"))?;
    manifest.pending_effects = Counter::new(
        i64::try_from(permit.files().files.len() + 1)
            .map_err(|_| invalid("purge effects count"))?,
    )
    .map_err(|_| invalid("purge effects count"))?;
    for witness in &permit.files().files {
        insert_effect(
            conn,
            &manifest.id,
            PurgeEffectIntent::File {
                witness: witness.clone(),
            },
        )?;
    }
    insert_effect(
        conn,
        &manifest.id,
        PurgeEffectIntent::Client {
            scope: permit.client_scope().clone(),
            digest: permit.client().digest.clone(),
        },
    )?;
    let allowed = targets
        .iter()
        .filter(|t| t.row.action != Action::Keep)
        .map(|t| guard::Permit {
            table: t.row.table.as_str().to_owned(),
            key_digest: t.row.key_digest.clone(),
            action: if t.row.action == Action::Delete {
                "Delete"
            } else {
                "Scrub"
            }
            .into(),
        })
        .collect();
    db(conn.execute_batch("PRAGMA defer_foreign_keys=ON"))?;
    guard::with_guard(conn, allowed, || {
        for target in targets.iter().filter(|t| t.row.action == Action::Delete) {
            if db(conn.execute(
                &format!(
                    "DELETE FROM {} WHERE {}",
                    candidates::quote(target.row.table.as_str()),
                    target.predicate
                ),
                params_from_iter(target.keys.iter()),
            ))? != 1
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        for target in targets.iter().filter(|t| t.row.action == Action::Scrub) {
            match target.row.table.as_str() {
                "command_receipts" => {
                    super::receipts::compact(conn, &target.predicate, &target.keys, &manifest.id)?
                }
                "blobs" => {
                    let digest = target
                        .keys
                        .first()
                        .and_then(|v| {
                            if let Value::Text(s) = v {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .ok_or_else(|| invalid("purge blob key"))?;
                    let pending = permit.files().files.iter().any(
                        |f| matches!(&f.owner,PurgeFileOwner::Blob {digest:d,..} if d == digest),
                    );
                    if db(conn.execute(
                        &format!(
                            "UPDATE blobs SET state=?2,claim_owner=?3,last_error=NULL WHERE {}",
                            target.predicate
                        ),
                        params![
                            digest,
                            if pending { "Unlinking" } else { "Unlinked" },
                            if pending {
                                Some(effects::blob_owner(&manifest.id))
                            } else {
                                None
                            }
                        ],
                    ))? != 1
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                }
                _ => super::redaction::scrub(
                    conn,
                    target,
                    project,
                    permit.confirmed_by(),
                    now,
                    heads.get(&target.row.key_digest),
                )?,
            }
            if target.row.reason == Reason::MinimalDeletedIdentity {
                db(conn.execute("INSERT INTO project_purged_subjects(table_name,key_digest,project_id,manifest_id,identity_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(table_name,key_digest) DO NOTHING",params![target.row.table.as_str(),target.row.key_digest,project.0.as_str(),manifest.id.as_str(),serde_json::json!({"label":"Deleted","table":target.row.table,"keyDigest":target.row.key_digest}).to_string()]))?;
            }
        }
        Ok(())
    })?;
    db(conn.execute("UPDATE project_purge_manifests SET revision=?2,phase='RemovingBytes',operation_id=?3,body_json=?4 WHERE id=?1 AND phase='Prepared'",params![manifest.id.as_str(),manifest.revision.get(),manifest.operation_id.as_ref().map(|o|o.0.as_str()),serde_json::to_string(&manifest).map_err(|_|invalid("purge manifest"))?]))?;
    // All cycles and immutable exceptions are checked before the encompassing unit commits.
    let broken: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
        [],
        |r| r.get(0),
    ))?;
    if broken {
        return Err(invalid("purge retained reference would be broken"));
    }
    Ok(manifest)
}
