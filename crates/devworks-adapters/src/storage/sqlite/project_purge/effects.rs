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
    let mut statement = db(conn.prepare("SELECT e.manifest_id,e.id,e.body_json,e.lease_owner FROM project_purge_effects e JOIN project_purge_manifests m ON m.id=e.manifest_id WHERE e.state IN('Pending','Held') AND m.phase IN('RemovingBytes','Held') ORDER BY COALESCE(e.lease_until,0),e.manifest_id,e.id LIMIT ?1"))?;
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
                    return Err(StorageError::Busy);
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
        }
    }
    Ok(Some(result))
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
    let n = db(conn.execute("UPDATE project_purge_effects SET state=?6,body_json=CASE WHEN ?7 IS NULL THEN NULL ELSE body_json END,error_code=?7,lease_owner=NULL,lease_boot_id=NULL,lease_until=?8 WHERE manifest_id=?1 AND id=?2 AND lease_owner=?3 AND lease_boot_id=?4 AND lease_until>?5",params![manifest.as_str(),id,owner.as_str(),boot.as_str(),now,if code.is_none(){"Removed"}else{"Held"},code,code.map(|_|now.saturating_add(1000))]))?;
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
