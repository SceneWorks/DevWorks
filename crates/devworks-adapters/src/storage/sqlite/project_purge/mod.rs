//! Durable immutable preparation. Execution and its physical effects share this owner.
mod blobs;
pub(super) mod candidates;
pub(super) mod classification;
pub(super) mod client_keepers;
pub(super) mod effects;
pub(super) mod guard;
pub(super) mod inventory;
mod leases;
pub(super) mod logical;
mod mixed;
mod receipts;
mod redaction;
mod references;
mod schema;
#[cfg(test)]
mod selectors;
use super::conn::db;
use devworks_core::ports::storage::{
    ProjectManagedFileRecovery, ProjectManagedFileReservation, StorageError, StoreInfo,
    StoredProjectManagedFile, StoredProjectPurge, project_purge_digest,
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
    schema::verify(conn)?;
    let m = &stored.manifest;
    if pending_managed_writes(conn, &m.project_id)? {
        return Err(StorageError::Busy);
    }
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
    let mut selected = std::collections::BTreeSet::new();
    for row in &stored.rows {
        let known: bool = db(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1 AND type='table')",
            [&row.table],
            |r| r.get(0),
        ))?;
        if !known
            || !digest(&row.key_digest)
            || !digest(&row.row_digest)
            || !selected.insert((&row.table, &row.key_digest))
        {
            return Err(invalid("invalid or duplicate purge row witness"));
        }
    }
    // A preparation never replaces a reviewed ID, even with identical content.
    db(conn.execute("INSERT INTO project_purge_manifests(id,project_id,revision,phase,project_revision,scope_revision,store_id,restore_epoch,actor_id,content_digest,policy_digest,witness_digest,body_json,created_at) VALUES(?1,?2,1,'Prepared',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![m.id.as_str(),m.project_id.0.as_str(),m.project_revision.get(),stored.scope_revision,stored.store_id.0.as_str(),stored.restore_epoch,stored.actor_id.0.as_str(),m.digest.as_str(),stored.policy_digest,stored.witness_digest,serde_json::to_string(m).map_err(|_|invalid("purge manifest JSON"))?,m.created_at_ms.get()]))?;
    for (ordinal, entry) in stored.entries.iter().enumerate() {
        let ordinal = i64::try_from(ordinal).map_err(|_| invalid("purge entry count overflow"))?;
        db(conn.execute("INSERT INTO project_purge_entries(manifest_id,ordinal,entry_digest,body_json) VALUES(?1,?2,?3,?4)",params![m.id.as_str(),ordinal,entry.id.as_str(),serde_json::to_string(entry).map_err(|_|invalid("purge entry JSON"))?]))?;
    }
    for row in &stored.rows {
        let action = serde_json::to_value(&row.action).map_err(|_| invalid("purge row action"))?;
        let reason = serde_json::to_value(&row.reason).map_err(|_| invalid("purge row reason"))?;
        db(conn.execute("INSERT INTO project_purge_rows(manifest_id,table_name,key_digest,row_digest,action,reason) VALUES(?1,?2,?3,?4,?5,?6)",params![m.id.as_str(),row.table,row.key_digest,row.row_digest,action.as_str().ok_or_else(||invalid("purge row action"))?,reason.as_str().ok_or_else(||invalid("purge row reason"))?]))?;
    }
    Ok(())
}

pub(super) fn manifest(
    conn: &Connection,
    project: &ProjectId,
    id: &Id,
) -> Result<Option<StoredProjectPurge>, StorageError> {
    type ManifestRaw = (
        String,
        String,
        String,
        i64,
        i64,
        String,
        String,
        Option<String>,
    );
    let raw:Option<ManifestRaw>=db(conn.query_row(
        "SELECT body_json,actor_id,store_id,restore_epoch,scope_revision,policy_digest,witness_digest,operation_id FROM project_purge_manifests WHERE id=?1 AND project_id=?2",
        params![id.as_str(),project.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional())?;
    let Some((
        body,
        actor,
        store,
        restore_epoch,
        scope_revision,
        policy_digest,
        witness_digest,
        operation,
    )) = raw
    else {
        return Ok(None);
    };
    let manifest: ProjectPurgeManifest =
        serde_json::from_str(&body).map_err(|_| invalid("stored purge manifest JSON"))?;
    manifest
        .check()
        .map_err(|_| invalid("stored purge manifest shape"))?;
    if manifest.id != *id
        || manifest.project_id != *project
        || manifest.operation_id.as_ref().map(|o| o.0.as_str()) != operation.as_deref()
    {
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
        rows: row_witnesses(conn, id)?,
    };
    if matches!(stored.manifest.phase, PurgePhase::Prepared)
        && stored.manifest.digest.as_str() != project_purge_digest(&stored)
    {
        return Err(invalid("stored purge preparation content changed"));
    }
    Ok(Some(stored))
}

fn row_witnesses(
    conn: &Connection,
    id: &Id,
) -> Result<Vec<devworks_core::ports::storage::ProjectPurgeRowWitness>, StorageError> {
    let mut statement=db(conn.prepare("SELECT table_name,key_digest,row_digest,action,reason FROM project_purge_rows WHERE manifest_id=?1 ORDER BY table_name,key_digest"))?;
    db(statement.query_map([id.as_str()], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    }))?
    .map(|row| {
        let (table, key_digest, row_digest, action, reason) = db(row)?;
        if !digest(&key_digest) || !digest(&row_digest) {
            return Err(invalid("stored purge row digest"));
        }
        Ok(devworks_core::ports::storage::ProjectPurgeRowWitness {
            table,
            key_digest,
            row_digest,
            action: serde_json::from_value(serde_json::Value::String(action))
                .map_err(|_| invalid("stored purge row action"))?,
            reason: serde_json::from_value(serde_json::Value::String(reason))
                .map_err(|_| invalid("stored purge row reason"))?,
        })
    })
    .collect()
}

pub(super) fn register_managed_file(
    conn: &Connection,
    store: &StoreInfo,
    file: &StoredProjectManagedFile,
) -> Result<(), StorageError> {
    use devworks_core::ports::project_purge::PurgeFileOwner;
    let owner_matches = match (&*file.kind, &file.witness.owner) {
        ("DiagnosticExport", PurgeFileOwner::DiagnosticExport { file_id })
        | ("ClientPaste", PurgeFileOwner::ClientPaste { file_id })
        | ("ClientStaging", PurgeFileOwner::ClientStaging { file_id }) => file_id == &file.id,
        _ => false,
    };
    let path = std::path::Path::new(&file.relative_path);
    if file.witness.project_id != file.project_id
        || file.witness.store_id != store.store_id
        || file.witness.restore_epoch != store.restore_epoch
    {
        return Err(StorageError::ScopeDenied);
    }
    if !owner_matches
        || file.relative_path != file.witness.relative_entry
        || path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, std::path::Component::Normal(_)))
        || file.witness.directory
        || file.witness.link
        || !digest(&file.witness.content_digest)
    {
        return Err(invalid("invalid managed purge file attribution"));
    }
    let bytes =
        i64::try_from(file.witness.byte_count).map_err(|_| invalid("managed purge file size"))?;
    let witness =
        serde_json::to_string(&file.witness).map_err(|_| invalid("managed purge file witness"))?;
    managed_scope_open(conn, &file.project_id)?;
    let reservation:Option<String>=db(conn.query_row("SELECT identity_json FROM project_managed_files WHERE id=?1 AND project_id=?2 AND state='Writing'",params![file.id.as_str(),file.project_id.0.as_str()],|r|r.get(0)).optional())?;
    let reserved: ProjectManagedFileReservation =
        serde_json::from_str(&reservation.ok_or(StorageError::ScopeDenied)?)
            .map_err(|_| invalid("managed file reservation"))?;
    if reserved
        .created_identity
        .is_some_and(|id| id != file.witness.identity)
    {
        return Err(StorageError::ScopeDenied);
    }
    if reserved.id != file.id
        || reserved.project_id != file.project_id
        || reserved.kind != file.kind
        || reserved.relative_path != file.relative_path
        || reserved.root != file.witness.root
        || reserved.store_id != store.store_id
        || reserved.restore_epoch != store.restore_epoch
        || reserved.boot_id != store.boot_id
        || reserved.created_at_ms != file.created_at_ms
    {
        return Err(StorageError::ScopeDenied);
    }
    db(conn.execute("UPDATE project_managed_files SET state='Live',identity_json=?3,byte_count=?4 WHERE id=?1 AND project_id=?2 AND state='Writing'",params![file.id.as_str(),file.project_id.0.as_str(),witness,bytes]))?;
    Ok(())
}

pub(super) fn managed_file(
    conn: &Connection,
    project: &ProjectId,
    id: &Id,
) -> Result<Option<StoredProjectManagedFile>, StorageError> {
    let raw:Option<(String,String,String,i64,i64)>=db(conn.query_row("SELECT kind,relative_path,identity_json,byte_count,created_at FROM project_managed_files WHERE id=?1 AND project_id=?2 AND state IN('Live','Removing')",params![id.as_str(),project.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional())?;
    let Some((kind, relative_path, body, bytes, created_at_ms)) = raw else {
        return Ok(None);
    };
    let witness: devworks_core::ports::project_purge::PurgeFileWitness =
        serde_json::from_str(&body).map_err(|_| invalid("stored managed file witness"))?;
    if i64::try_from(witness.byte_count).ok() != Some(bytes)
        || witness.relative_entry != relative_path
    {
        return Err(invalid("stored managed file identity"));
    }
    Ok(Some(StoredProjectManagedFile {
        id: id.clone(),
        project_id: project.clone(),
        kind,
        relative_path,
        witness,
        created_at_ms,
    }))
}

pub(super) fn managed_files(
    conn: &Connection,
    project: &ProjectId,
    after: Option<&Id>,
    n: u16,
) -> Result<Vec<StoredProjectManagedFile>, StorageError> {
    if n == 0 || n > 500 {
        return Err(invalid("managed file page limit"));
    }
    let mut statement=db(conn.prepare("SELECT id FROM project_managed_files WHERE project_id=?1 AND state IN('Live','Removing') AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3"))?;
    let ids = db(statement.query_map(
        params![project.0.as_str(), after.map(Id::as_str), i64::from(n)],
        |r| r.get::<_, String>(0),
    ))?
    .collect::<Result<Vec<_>, _>>()
    .map_err(super::conn::db_err)?;
    ids.into_iter()
        .map(|id| {
            let id = Id::new(id).map_err(|_| invalid("stored managed file ID"))?;
            managed_file(conn, project, &id)?
                .ok_or_else(|| invalid("managed file disappeared within snapshot"))
        })
        .collect()
}

pub(super) fn finish_managed_file(
    conn: &Connection,
    project: &ProjectId,
    id: &Id,
) -> Result<(), StorageError> {
    db(conn.execute("UPDATE project_managed_files SET state='Removed',relative_path='',identity_json='{}' WHERE id=?1 AND project_id=?2",params![id.as_str(),project.0.as_str()]))?;
    Ok(())
}

fn managed_scope_open(conn: &Connection, project: &ProjectId) -> Result<(), StorageError> {
    let open:bool=db(conn.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1) AND NOT EXISTS(SELECT 1 FROM project_purge_manifests WHERE project_id=?1 AND phase IN('RemovingBytes','Held','Complete'))",[project.0.as_str()],|r|r.get(0)))?;
    if open {
        Ok(())
    } else {
        Err(StorageError::ScopeDenied)
    }
}

pub(super) fn reserve_managed_file(
    conn: &Connection,
    store: &StoreInfo,
    file: &ProjectManagedFileReservation,
) -> Result<(), StorageError> {
    if file.created_identity.is_some()
        || file.store_id != store.store_id
        || file.boot_id != store.boot_id
        || file.boot_id.is_none()
        || file.restore_epoch != store.restore_epoch
        || !matches!(
            &*file.kind,
            "DiagnosticExport" | "ClientPaste" | "ClientStaging" | "Preview"
        )
        || file.created_at_ms < 0
        || file.relative_path.is_empty()
        || file.relative_path.contains('\\')
        || std::path::Path::new(&file.relative_path)
            .components()
            .count()
            != 1
        || !matches!(
            std::path::Path::new(&file.relative_path)
                .components()
                .next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(invalid("managed file reservation identity"));
    }
    managed_scope_open(conn, &file.project_id)?;
    let body = serde_json::to_string(file).map_err(|_| invalid("managed file reservation JSON"))?;
    db(conn.execute("INSERT INTO project_managed_files(id,project_id,kind,relative_path,identity_json,byte_count,state,created_at) VALUES(?1,?2,?3,?4,?5,0,'Writing',?6)",params![file.id.as_str(),file.project_id.0.as_str(),file.kind,file.relative_path,body,file.created_at_ms]))?;
    Ok(())
}

pub(super) fn begin_managed_write(
    conn: &Connection,
    store: &StoreInfo,
    project: &ProjectId,
    id: &Id,
    identity: devworks_core::authority::destructive::FileIdentity,
) -> Result<(), StorageError> {
    managed_scope_open(conn, project)?;
    let raw:Option<String>=db(conn.query_row("SELECT identity_json FROM project_managed_files WHERE id=?1 AND project_id=?2 AND state='Writing'",params![id.as_str(),project.0.as_str()],|r|r.get(0)).optional())?;
    let Some(raw) = raw else {
        return Err(StorageError::ScopeDenied);
    };
    let mut reservation: ProjectManagedFileReservation =
        serde_json::from_str(&raw).map_err(|_| invalid("managed write reservation"))?;
    if reservation.id != *id
        || reservation.project_id != *project
        || reservation.store_id != store.store_id
        || reservation.restore_epoch != store.restore_epoch
        || reservation.boot_id != store.boot_id
        || store.boot_id.is_none()
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(existing) = reservation.created_identity {
        return if existing == identity {
            Ok(())
        } else {
            Err(StorageError::ScopeDenied)
        };
    }
    reservation.created_identity = Some(identity);
    let encoded =
        serde_json::to_string(&reservation).map_err(|_| invalid("managed write identity"))?;
    let changed=db(conn.execute("UPDATE project_managed_files SET identity_json=?3 WHERE id=?1 AND project_id=?2 AND state='Writing' AND identity_json=?4",params![id.as_str(),project.0.as_str(),encoded,raw]))?;
    if changed != 1 {
        return Err(StorageError::Busy);
    }
    Ok(())
}

pub(super) fn pending_managed_writes(
    conn: &Connection,
    project: &ProjectId,
) -> Result<bool, StorageError> {
    db(conn.query_row("SELECT EXISTS(SELECT 1 FROM project_managed_files WHERE project_id=?1 AND state='Writing')",[project.0.as_str()],|r|r.get(0)))
}

pub(super) fn managed_recovery(
    conn: &Connection,
    kind: &str,
    after: Option<&Id>,
    n: u16,
) -> Result<Vec<ProjectManagedFileRecovery>, StorageError> {
    if !matches!(
        kind,
        "DiagnosticExport" | "ClientPaste" | "ClientStaging" | "Preview"
    ) || n == 0
        || n > 500
    {
        return Err(invalid("managed recovery page"));
    }
    let mut query=db(conn.prepare("SELECT id,project_id,relative_path,identity_json,state FROM project_managed_files WHERE kind=?1 AND state<>'Removed' AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3"))?;
    let rows = db(
        query.query_map(params![kind, after.map(Id::as_str), i64::from(n)], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        }),
    )?;
    rows.map(|row| {
        let (id, project, path, body, state) = db(row)?;
        let id = Id::new(id).map_err(|_| invalid("managed recovery ID"))?;
        let project_id =
            ProjectId(Id::new(project).map_err(|_| invalid("managed recovery project"))?);
        let writing = state == "Writing";
        let mut boot_id = None;
        let file_identity;
        let store_id;
        let restore_epoch;
        let root = if writing {
            let reserved: ProjectManagedFileReservation =
                serde_json::from_str(&body).map_err(|_| invalid("managed recovery reservation"))?;
            if reserved.id != id
                || reserved.project_id != project_id
                || reserved.relative_path != path
                || reserved.kind != kind
            {
                return Err(invalid("managed recovery binding"));
            }
            boot_id = reserved.boot_id;
            file_identity = reserved.created_identity;
            store_id = reserved.store_id;
            restore_epoch = reserved.restore_epoch;
            reserved.root
        } else {
            let witness: devworks_core::ports::project_purge::PurgeFileWitness =
                serde_json::from_str(&body).map_err(|_| invalid("managed recovery witness"))?;
            if witness.relative_entry != path {
                return Err(invalid("managed recovery path"));
            }
            file_identity = Some(witness.identity);
            store_id = witness.store_id;
            restore_epoch = witness.restore_epoch;
            witness.root
        };
        Ok(ProjectManagedFileRecovery {
            id,
            project_id,
            relative_path: path,
            root,
            file_identity,
            store_id,
            restore_epoch,
            writing,
            boot_id,
        })
    })
    .collect()
}
