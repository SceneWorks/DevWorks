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
