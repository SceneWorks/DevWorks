//! Durable immutable preparation. Execution and its physical effects share this owner.
pub(super) mod guard;
#[cfg(test)]
mod selectors;
use super::conn::db;
use devworks_core::ports::storage::{
    StorageError, StoreInfo, StoredProjectPurge, project_purge_digest,
};
use devworks_core::protocol::{
    Id, ProjectId,
    records::{ProjectPurgeEntry, ProjectPurgeManifest, PurgePhase},
    support::Check,
};
use rusqlite::{Connection, OptionalExtension, params};

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub(super) fn insert_manifest(
    conn: &Connection,
    store: &StoreInfo,
    stored: &StoredProjectPurge,
) -> Result<(), StorageError> {
    let m = &stored.manifest;
    m.check().map_err(|_| invalid("invalid purge manifest"))?;
    if !matches!(m.phase, PurgePhase::Prepared)
        || m.revision.get() != 1
        || m.operation_id.is_some()
        || m.error_code.is_some()
        || m.pending_effects.get() != 0
        || stored.scope_revision < 1
        || stored.store_id != store.store_id
        || stored.restore_epoch != store.restore_epoch
        || m.digest.as_str() != project_purge_digest(stored)
        || !digest(&stored.policy_digest)
        || !digest(&stored.witness_digest)
    {
        return Err(invalid("invalid purge preparation fences"));
    }
    let current: Option<(String,i64,i64,i64)>=db(conn.query_row(
        "SELECT p.lifecycle,p.revision,p.scope_revision,COALESCE(d.archive_stop_pending,0) FROM projects p LEFT JOIN project_details d ON d.project_id=p.id WHERE p.id=?1",
        [m.project_id.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
    if current
        != Some((
            "Deleted".into(),
            m.project_revision.get(),
            stored.scope_revision,
            0,
        ))
    {
        return Err(StorageError::ScopeDenied);
    }
    let mut seen = std::collections::BTreeSet::new();
    for entry in &stored.entries {
        entry
            .check()
            .map_err(|_| invalid("invalid purge manifest entry"))?;
        if !seen.insert(entry.id.as_str()) {
            return Err(invalid("duplicate purge manifest entry"));
        }
    }
    // A preparation never replaces a reviewed ID, even with identical content.
    db(conn.execute("INSERT INTO project_purge_manifests(id,project_id,revision,phase,project_revision,scope_revision,store_id,restore_epoch,actor_id,content_digest,policy_digest,witness_digest,body_json,created_at) VALUES(?1,?2,1,'Prepared',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![m.id.as_str(),m.project_id.0.as_str(),m.project_revision.get(),stored.scope_revision,stored.store_id.0.as_str(),stored.restore_epoch,stored.actor_id.0.as_str(),m.digest.as_str(),stored.policy_digest,stored.witness_digest,serde_json::to_string(m).map_err(|_|invalid("purge manifest JSON"))?,m.created_at_ms.get()]))?;
    for (ordinal, entry) in stored.entries.iter().enumerate() {
        let ordinal = i64::try_from(ordinal).map_err(|_| invalid("purge entry count overflow"))?;
        db(conn.execute("INSERT INTO project_purge_entries(manifest_id,ordinal,entry_digest,body_json) VALUES(?1,?2,?3,?4)",params![m.id.as_str(),ordinal,entry.id.as_str(),serde_json::to_string(entry).map_err(|_|invalid("purge entry JSON"))?]))?;
    }
    Ok(())
}

pub(super) fn manifest(
    conn: &Connection,
    project: &ProjectId,
    id: &Id,
) -> Result<Option<StoredProjectPurge>, StorageError> {
    let raw:Option<(String,String,String,i64,i64,String,String)>=db(conn.query_row(
        "SELECT body_json,actor_id,store_id,restore_epoch,scope_revision,policy_digest,witness_digest FROM project_purge_manifests WHERE id=?1 AND project_id=?2",
        params![id.as_str(),project.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional())?;
    let Some((body, actor, store, restore_epoch, scope_revision, policy_digest, witness_digest)) =
        raw
    else {
        return Ok(None);
    };
    let manifest: ProjectPurgeManifest =
        serde_json::from_str(&body).map_err(|_| invalid("stored purge manifest JSON"))?;
    manifest
        .check()
        .map_err(|_| invalid("stored purge manifest shape"))?;
    if manifest.id != *id || manifest.project_id != *project {
        return Err(invalid("stored purge manifest identity"));
    }
    let mut stmt = db(conn.prepare(
        "SELECT body_json FROM project_purge_entries WHERE manifest_id=?1 ORDER BY ordinal",
    ))?;
    let entries = db(stmt.query_map([id.as_str()], |r| r.get::<_, String>(0)))?
        .map(|row| {
            let entry: ProjectPurgeEntry =
                serde_json::from_str(&db(row)?).map_err(|_| invalid("stored purge entry JSON"))?;
            entry
                .check()
                .map_err(|_| invalid("stored purge entry shape"))?;
            Ok(entry)
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    let stored = StoredProjectPurge {
        manifest,
        actor_id: devworks_core::protocol::ActorId(
            Id::new(actor).map_err(|_| invalid("stored purge actor"))?,
        ),
        store_id: devworks_core::protocol::StoreId(
            Id::new(store).map_err(|_| invalid("stored purge store"))?,
        ),
        restore_epoch,
        scope_revision,
        policy_digest,
        witness_digest,
        entries,
    };
    if matches!(stored.manifest.phase, PurgePhase::Prepared)
        && stored.manifest.digest.as_str() != project_purge_digest(&stored)
    {
        return Err(invalid("stored purge preparation content changed"));
    }
    Ok(Some(stored))
}
