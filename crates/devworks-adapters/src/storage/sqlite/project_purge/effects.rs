//! Current-boot leases for exact durable purge effects. No effect body can be supplied
//! through a claim or settlement call, and claims never create deletion authority.
use super::super::conn::db;
use super::invalid;
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::project_purge::{
    PurgeEffect, PurgeEffectIntent, PurgeFileFault, PurgeFileOwner,
};
use devworks_core::ports::storage::{StorageError, StoreInfo};
use devworks_core::protocol::{
    Counter, Id, Revision,
    records::{ProjectPurgeManifest, PurgePhase},
};
use rusqlite::{Connection, OptionalExtension, params};

pub(super) fn blob_owner(manifest: &Id) -> String {
    format!("project-purge:{}", manifest.as_str())
}
fn effect(
    manifest: String,
    id: String,
    body: String,
    owner: Option<String>,
) -> Result<PurgeEffect, StorageError> {
    Ok(PurgeEffect {
        manifest_id: Id::new(manifest).map_err(|_| invalid("purge effect manifest"))?,
        id,
        intent: serde_json::from_str(&body).map_err(|_| invalid("purge effect body"))?,
        lease_owner: owner
            .map(Id::new)
            .transpose()
            .map_err(|_| invalid("purge effect lease"))?,
    })
}
pub(in crate::storage::sqlite) fn pending(
    conn: &Connection,
    limit: u16,
) -> Result<Vec<PurgeEffect>, StorageError> {
    if limit == 0 || limit > 500 {
        return Err(invalid("purge effect page limit"));
    }
    let mut statement = db(conn.prepare("SELECT e.manifest_id,e.id,e.body_json,e.lease_owner FROM project_purge_effects e JOIN project_purge_manifests m ON m.id=e.manifest_id WHERE e.state IN('Pending','Held') AND m.phase IN('RemovingBytes','Held') ORDER BY CASE json_extract(e.body_json,'$.kind') WHEN 'Client' THEN 0 ELSE 1 END,COALESCE(e.lease_until,0),e.manifest_id,e.id LIMIT ?1"))?;
    db(statement.query_map([limit], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    }))?
    .map(|r| {
        let (m, i, b, o) = db(r)?;
        effect(m, i, b, o)
    })
    .collect()
}
pub(in crate::storage::sqlite) fn reserved(
    conn: &Connection,
    digest: &str,
    generation: &str,
    owner: &str,
) -> Result<bool, StorageError> {
    db(conn.query_row("SELECT EXISTS(SELECT 1 FROM project_purge_effects e JOIN project_purge_manifests m ON m.id=e.manifest_id WHERE m.phase IN('RemovingBytes','Held') AND e.state IN('Pending','Held') AND ?3='project-purge:'||m.id AND json_extract(e.body_json,'$.kind')='File' AND json_extract(e.body_json,'$.witness.owner.kind')='Blob' AND json_extract(e.body_json,'$.witness.owner.digest')=?1 AND json_extract(e.body_json,'$.witness.owner.generation')=?2)", params![digest,generation,owner], |r| r.get(0)))
}
pub(in crate::storage::sqlite) fn claim(
    conn: &Connection,
    info: &StoreInfo,
    now: i64,
    manifest: &Id,
    id: &str,
    owner: &Id,
    until: i64,
) -> Result<Option<PurgeEffect>, StorageError> {
    super::schema::verify(conn)?;
    let boot = info.boot_id.as_ref().ok_or(StorageError::ScopeDenied)?;
    if until <= now || until > now.saturating_add(120_000) {
        return Err(invalid("purge effect lease bound"));
    }
    let n = db(conn.execute("UPDATE project_purge_effects SET lease_owner=?3,lease_boot_id=?4,lease_until=?5 WHERE manifest_id=?1 AND id=?2 AND state IN('Pending','Held') AND ((lease_owner IS NULL AND COALESCE(lease_until,0)<=?6) OR lease_until<=?6 OR lease_boot_id<>?4 OR lease_owner=?3) AND EXISTS(SELECT 1 FROM project_purge_manifests m WHERE m.id=?1 AND m.phase IN('RemovingBytes','Held') AND m.store_id=?7 AND m.restore_epoch=?8)", params![manifest.as_str(),id,owner.as_str(),boot.as_str(),until,now,info.store_id.0.as_str(),info.restore_epoch]))?;
    if n == 0 {
        return Ok(None);
    }
    let (body, bytes, digest): (String,i64,String) = db(conn.query_row("SELECT body_json,byte_count,owner_digest FROM project_purge_effects WHERE manifest_id=?1 AND id=?2", params![manifest.as_str(),id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))))?;
    let result = effect(
        manifest.as_str().into(),
        id.into(),
        body.clone(),
        Some(owner.as_str().into()),
    )?;
    if id != sha256_hex(body.as_bytes()) || digest.len() != 64 {
        return Err(invalid("purge effect identity changed"));
    }
    let project: String = db(conn.query_row(
        "SELECT project_id FROM project_purge_manifests WHERE id=?1",
        [manifest.as_str()],
        |r| r.get(0),
    ))?;
    let project_id = devworks_core::protocol::ProjectId(
        Id::new(&project).map_err(|_| invalid("purge effect project"))?,
    );
    let reviewed =
        super::manifest(conn, &project_id, manifest)?.ok_or(StorageError::ScopeDenied)?;
    let entry = devworks_core::ports::project_purge::reviewed_effect_entry(&result.intent);
    if !reviewed.entries.iter().any(|e| {
        e.id.as_str() == entry
            && matches!(
                e.disposition,
                devworks_core::protocol::records::PurgeDisposition::Remove
            )
    }) {
        return Err(invalid("recovered effect was not in the reviewed manifest"));
    }
    match &result.intent {
        PurgeEffectIntent::File { witness } => {
            if witness.store_id != info.store_id
                || witness.project_id.0.as_str() != project
                || witness.restore_epoch != info.restore_epoch
                || i64::try_from(witness.byte_count).ok() != Some(bytes)
                || sha256_hex(
                    &serde_json::to_vec(&witness.owner).map_err(|_| invalid("purge owner"))?,
                ) != digest
            {
                return Err(StorageError::ScopeDenied);
            }
            if let PurgeFileOwner::Blob { digest, generation }
            | PurgeFileOwner::Quarantine { digest, generation } = &witness.owner
            {
                let blocked: bool = db(conn.query_row("SELECT EXISTS(SELECT 1 FROM blobs WHERE digest=?1 AND generation=?2) AND (EXISTS(SELECT 1 FROM artifact_blob_pins WHERE blob_digest=?1) OR EXISTS(SELECT 1 FROM artifact_versions WHERE blob_digest=?1 AND deleted_at IS NULL) OR EXISTS(SELECT 1 FROM graph_source_captures WHERE blob_digest=?1) OR EXISTS(SELECT 1 FROM convention_import_artifacts a JOIN convention_imports d ON d.operation_id=a.operation_id WHERE a.blob_digest=?1 AND d.phase!='Published'))", params![digest,generation], |r| r.get(0)))?;
                if blocked {
                    db(conn.execute("UPDATE project_purge_effects SET state='Held',error_code='OwnerInUse',lease_owner=NULL,lease_boot_id=NULL,lease_until=?3 WHERE manifest_id=?1 AND id=?2",params![manifest.as_str(),id,now.saturating_add(1000)]))?;
                    refresh(conn, manifest)?;
                    return Ok(None);
                }
            }
        }
        PurgeEffectIntent::Client {
            scope,
            digest: client,
        } => {
            if bytes != 0
                || !super::digest(client)
                || scope.project_id.0.as_str() != project
                || sha256_hex(
                    &serde_json::to_vec(scope).map_err(|_| invalid("purge client scope"))?,
                ) != digest
            {
                return Err(invalid("purge client effect changed"));
            }
            validate_scope(conn, &reviewed, scope)?;
        }
    }
    Ok(Some(result))
}
fn validate_scope(
    conn: &Connection,
    reviewed: &devworks_core::ports::storage::StoredProjectPurge,
    scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
) -> Result<(), StorageError> {
    for (table, ids) in [
        ("sessions", &scope.session_ids),
        ("conversations", &scope.conversation_ids),
        ("work_items", &scope.work_ids),
        ("operations", &scope.operation_ids),
        ("artifacts", &scope.deleted_artifact_ids),
    ] {
        let expected = reviewed
            .rows
            .iter()
            .filter(|r| {
                r.table == table
                    && (table != "artifacts"
                        || r.action == devworks_core::ports::storage::ProjectPurgeRowAction::Delete)
            })
            .map(|r| r.key_digest.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut observed = std::collections::BTreeSet::new();
        for id in ids {
            let key = super::guard::key_digest(
                &serde_json::to_string(&[id.as_str()]).expect("client scope key"),
            )?;
            if !observed.insert(key) {
                return Err(invalid("client scope was not in the reviewed manifest"));
            }
        }
        if observed != expected {
            return Err(invalid("client scope differs from the reviewed manifest"));
        }
    }
    let mut expected_records =
        std::collections::BTreeMap::<String, std::collections::BTreeSet<String>>::new();
    for row in reviewed.rows.iter().filter(|r| {
        matches!(
            r.action,
            devworks_core::ports::storage::ProjectPurgeRowAction::Delete
                | devworks_core::ports::storage::ProjectPurgeRowAction::Scrub
        ) && !matches!(
            r.reason,
            devworks_core::ports::storage::ProjectPurgeRowReason::ProjectMembership
                | devworks_core::ports::storage::ProjectPurgeRowReason::PurgeControlAudit
        )
    }) {
        if super::candidates::primary_keys(conn, &row.table)? == ["id"] {
            expected_records
                .entry(row.table.clone())
                .or_default()
                .insert(row.key_digest.clone());
        }
    }
    let mut records = std::collections::BTreeMap::new();
    for (table, ids) in &scope.record_ids {
        let mut keys = std::collections::BTreeSet::new();
        for id in ids {
            let key = super::guard::key_digest(
                &serde_json::to_string(&[id.as_str()]).expect("record key"),
            )?;
            if !keys.insert(key) {
                return Err(invalid("duplicate client record scope"));
            }
        }
        records.insert(table.clone(), keys);
    }
    if records != expected_records {
        return Err(invalid("client record scope differs from review"));
    }
    Ok(())
}
pub(in crate::storage::sqlite) fn client_fence(
    conn: &Connection,
    project: &devworks_core::protocol::ProjectId,
    digest: &str,
) -> Result<Option<devworks_core::ports::storage::ProjectPurgeClientScope>, StorageError> {
    let mut query = db(conn.prepare("SELECT e.body_json FROM project_purge_effects e JOIN project_purge_manifests m ON m.id=e.manifest_id WHERE m.project_id=?1 AND m.phase IN('RemovingBytes','Held','Complete') AND json_extract(e.body_json,'$.kind')='Client'"))?;
    let bodies = db(query.query_map([project.0.as_str()], |r| r.get::<_, String>(0)))?;
    for body in bodies {
        let intent: PurgeEffectIntent =
            serde_json::from_str(&db(body)?).map_err(|_| invalid("client fence effect"))?;
        if let PurgeEffectIntent::Client {
            scope,
            digest: committed,
        } = intent
            && scope.project_id == *project
            && committed == digest
            && validate_client_fence(conn, &scope, digest)?
        {
            return Ok(Some(scope));
        }
    }
    Ok(None)
}
pub(in crate::storage::sqlite) fn validate_client_fence(
    conn: &Connection,
    scope: &devworks_core::ports::storage::ProjectPurgeClientScope,
    digest: &str,
) -> Result<bool, StorageError> {
    let id:Option<String>=db(conn.query_row("SELECT id FROM project_purge_manifests WHERE project_id=?1 AND phase IN('RemovingBytes','Held','Complete')",[scope.project_id.0.as_str()],|r|r.get(0)).optional())?;
    let Some(id) = id else {
        return Ok(false);
    };
    let reviewed = super::manifest(
        conn,
        &scope.project_id,
        &Id::new(id).map_err(|_| invalid("purge fence"))?,
    )?
    .ok_or(StorageError::ScopeDenied)?;
    let entry =
        devworks_core::ports::project_purge::reviewed_effect_entry(&PurgeEffectIntent::Client {
            scope: scope.clone(),
            digest: digest.into(),
        });
    Ok(reviewed.entries.iter().any(|e| {
        e.id.as_str() == entry
            && matches!(
                e.disposition,
                devworks_core::protocol::records::PurgeDisposition::Remove
            )
    }) && validate_scope(conn, &reviewed, scope).is_ok())
}
pub(in crate::storage::sqlite) fn file_reviewed(
    conn: &Connection,
    witness: &devworks_core::ports::project_purge::PurgeFileWitness,
) -> Result<bool, StorageError> {
    let intent = PurgeEffectIntent::File {
        witness: witness.clone(),
    };
    let body = serde_json::to_string(&intent).map_err(|_| invalid("purge file effect"))?;
    let id = sha256_hex(body.as_bytes());
    let manifest:Option<String>=db(conn.query_row("SELECT m.id FROM project_purge_effects e JOIN project_purge_manifests m ON m.id=e.manifest_id WHERE e.id=?1 AND e.body_json=?2 AND e.state IN('Pending','Held') AND m.phase IN('RemovingBytes','Held') AND m.project_id=?3 AND m.store_id=?4 AND m.restore_epoch=?5",params![id,body,witness.project_id.0.as_str(),witness.store_id.0.as_str(),witness.restore_epoch],|r|r.get(0)).optional())?;
    let Some(manifest) = manifest else {
        return Ok(false);
    };
    let reviewed = super::manifest(
        conn,
        &witness.project_id,
        &Id::new(manifest).map_err(|_| invalid("purge file manifest"))?,
    )?
    .ok_or(StorageError::ScopeDenied)?;
    let entry = devworks_core::ports::project_purge::reviewed_effect_entry(&intent);
    Ok(reviewed.entries.iter().any(|e| {
        e.id.as_str() == entry
            && matches!(
                e.disposition,
                devworks_core::protocol::records::PurgeDisposition::Remove
            )
    }))
}
fn fault_code(f: PurgeFileFault) -> &'static str {
    match f {
        PurgeFileFault::InUse => "OwnerInUse",
        PurgeFileFault::Changed => "WitnessChanged",
        PurgeFileFault::Unattributed => "UnattributedOwner",
        PurgeFileFault::Unsupported => "UnsupportedOwner",
        PurgeFileFault::Cancelled => "Cancelled",
        PurgeFileFault::Io => "FileUnavailable",
    }
}
pub(in crate::storage::sqlite) fn settle(
    conn: &Connection,
    info: &StoreInfo,
    now: i64,
    manifest: &Id,
    id: &str,
    owner: &Id,
    outcome: Result<(), PurgeFileFault>,
) -> Result<(), StorageError> {
    let boot = info.boot_id.as_ref().ok_or(StorageError::ScopeDenied)?;
    let body: Option<String> = db(conn.query_row("SELECT body_json FROM project_purge_effects WHERE manifest_id=?1 AND id=?2 AND state IN('Pending','Held') AND lease_owner=?3 AND lease_boot_id=?4 AND lease_until>?5", params![manifest.as_str(),id,owner.as_str(),boot.as_str(),now], |r| r.get(0)).optional())?;
    let Some(body) = body else {
        return Err(StorageError::ScopeDenied);
    };
    let intent: PurgeEffectIntent =
        serde_json::from_str(&body).map_err(|_| invalid("purge effect body"))?;
    let code = outcome.err().map(fault_code);
    if let PurgeEffectIntent::File { witness } = &intent
        && let PurgeFileOwner::Blob { digest, generation } = &witness.owner
    {
        let n = db(conn.execute("UPDATE blobs SET state=CASE WHEN ?4 IS NULL THEN 'Unlinked' ELSE 'Unlinking' END,claim_owner=CASE WHEN ?4 IS NULL THEN NULL ELSE claim_owner END,last_error=?4 WHERE digest=?1 AND generation=?2 AND state='Unlinking' AND claim_owner=?3",params![digest,generation,blob_owner(manifest),code]))?;
        if n != 1 {
            return Err(StorageError::ScopeDenied);
        }
    }
    let n = db(conn.execute("UPDATE project_purge_effects SET state=?6,body_json=CASE WHEN ?7 IS NULL AND kind<>'Client' THEN NULL ELSE body_json END,error_code=?7,lease_owner=NULL,lease_boot_id=NULL,lease_until=?8 WHERE manifest_id=?1 AND id=?2 AND lease_owner=?3 AND lease_boot_id=?4 AND lease_until>?5",params![manifest.as_str(),id,owner.as_str(),boot.as_str(),now,if code.is_none(){"Removed"}else{"Held"},code,code.map(|_|now.saturating_add(1000))]))?;
    if n != 1 {
        return Err(StorageError::ScopeDenied);
    }
    refresh(conn, manifest)
}
pub(super) fn refresh(conn: &Connection, manifest: &Id) -> Result<(), StorageError> {
    let (pending, held): (i64,i64) = db(conn.query_row("SELECT count(*),COALESCE(sum(state='Held'),0) FROM project_purge_effects WHERE manifest_id=?1 AND state IN('Pending','Held')",[manifest.as_str()],|r|Ok((r.get(0)?,r.get(1)?))))?;
    let raw: String = db(conn.query_row(
        "SELECT body_json FROM project_purge_manifests WHERE id=?1",
        [manifest.as_str()],
        |r| r.get(0),
    ))?;
    let mut m: ProjectPurgeManifest =
        serde_json::from_str(&raw).map_err(|_| invalid("purge manifest body"))?;
    m.revision = Revision::new(
        m.revision
            .get()
            .checked_add(1)
            .ok_or_else(|| invalid("purge revision overflow"))?,
    )
    .map_err(|_| invalid("purge revision"))?;
    m.pending_effects = Counter::new(pending).map_err(|_| invalid("purge pending count"))?;
    m.phase = if pending == 0 {
        PurgePhase::Complete
    } else if held > 0 {
        PurgePhase::Held
    } else {
        PurgePhase::RemovingBytes
    };
    m.error_code = if held > 0 {
        Some(serde_json::from_value(serde_json::json!("PurgeEffectsHeld")).expect("static label"))
    } else {
        None
    };
    let phase = serde_json::to_value(&m.phase).expect("phase");
    db(conn.execute(
        "UPDATE project_purge_manifests SET revision=?2,phase=?3,body_json=?4 WHERE id=?1",
        params![
            manifest.as_str(),
            m.revision.get(),
            phase.as_str(),
            serde_json::to_string(&m).map_err(|_| invalid("purge manifest"))?
        ],
    ))?;
    if pending == 0
        && let Some(operation) = &m.operation_id
    {
        db(conn.execute("UPDATE operations SET state='ObservedComplete',revision=revision+1 WHERE id=?1 AND capability='projects.purge' AND state='IntentRecorded' AND json_extract(payload_json,'$.manifestId')=?2",params![operation.0.as_str(),manifest.as_str()]))?;
    }
    Ok(())
}
