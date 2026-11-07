//! An independent client draft retains only the exact referenced version. It does not
//! retain the owning project's other versions or turn a path/prose mention into ownership.
use super::super::conn::db;
use super::classification::{Key, assign};
use devworks_core::ports::storage::{
    ProjectPurgeCandidates, ProjectPurgeRowAction as Action, ProjectPurgeRowReason as Reason,
    ProjectPurgeRowWitness, ProjectPurgeUnresolved, StorageError,
};
use devworks_core::protocol::{provider::ArtifactRef, support::Check};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn digest(keepers: &[ArtifactRef]) -> String {
    let values = keepers
        .iter()
        .map(|r| serde_json::to_string(r).expect("artifact reference"))
        .collect::<BTreeSet<_>>();
    devworks_core::fingerprint::sha256_hex(&serde_json::to_vec(&values).expect("client keeper set"))
}
pub(super) fn retain(
    conn: &Connection,
    keepers: &[ArtifactRef],
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    for reference in keepers {
        reference.check().map_err(|_| StorageError::ScopeDenied)?;
        let actual: Option<(String,String,i64,String)> = db(conn.query_row(
            "SELECT v.artifact_id,v.blob_digest,b.byte_count,v.media_type FROM artifact_versions v JOIN blobs b ON b.digest=v.blob_digest JOIN artifacts a ON a.id=v.artifact_id WHERE v.id=?1 AND v.deleted_at IS NULL AND a.state='Live'",
            params![reference.version_id.as_str()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional())?;
        if actual
            != Some((
                reference.artifact_id.as_str().into(),
                reference.sha256.as_str().into(),
                reference.byte_length.get(),
                reference.media_type.clone(),
            ))
        {
            return Err(StorageError::ScopeDenied);
        }
        let key = super::guard::key_digest(
            &serde_json::to_string(&[reference.version_id.as_str()]).expect("version key"),
        )?;
        if let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "artifact_versions" && r.key_digest == key)
        {
            assign(
                candidate,
                Action::Keep,
                Reason::SharedContent,
                rows,
                unresolved,
            );
        }
    }
    Ok(())
}

/// A native managed path independently picked by a surviving draft retains its exact
/// content versions; their private headers are still projected to minimal identities.
pub(in crate::storage::sqlite) fn file_references(
    conn: &Connection,
    project: &devworks_core::protocol::ProjectId,
    owners: &[devworks_core::ports::project_purge::PurgeFileOwner],
) -> Result<Vec<ArtifactRef>, StorageError> {
    use devworks_core::ports::project_purge::PurgeFileOwner;
    let inventory = super::inventory::read(conn, project)?.ok_or(StorageError::ScopeDenied)?;
    let mut result = BTreeMap::new();
    for owner in owners {
        if !inventory.removing_files.owners.contains(owner)
            && !inventory.kept_files.owners.contains(owner)
        {
            return Err(StorageError::ScopeDenied);
        }
        let (digest, version) = match owner {
            PurgeFileOwner::Blob { digest, .. } | PurgeFileOwner::Quarantine { digest, .. } => {
                (Some(digest.as_str()), None)
            }
            PurgeFileOwner::Preview { version_id } => (None, Some(version_id.as_str())),
            _ => continue,
        };
        let mut stmt=db(conn.prepare("SELECT v.id,v.artifact_id,v.blob_digest,b.byte_count,v.media_type FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id JOIN blobs b ON b.digest=v.blob_digest WHERE a.state='Live' AND v.deleted_at IS NULL AND ((?1 IS NOT NULL AND v.blob_digest=?1) OR (?2 IS NOT NULL AND v.id=?2)) ORDER BY v.id"))?;
        let rows = db(stmt.query_map(params![digest, version], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        }))?;
        for row in rows {
            let (v, a, d, b, m) = db(row)?;
            let reference:ArtifactRef=serde_json::from_value(serde_json::json!({"artifactId":a,"versionId":v,"sha256":d,"byteLength":b.to_string(),"mediaType":m})).map_err(|_|StorageError::ScopeDenied)?;
            result.insert(v, reference);
        }
    }
    Ok(result.into_values().collect())
}
