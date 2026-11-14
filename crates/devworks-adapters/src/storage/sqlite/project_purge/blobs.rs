//! Selected blob metadata and surviving byte keepers. This assessment neither
//! claims a generation nor invokes the global collector.
use super::super::conn::db;
use super::classification::{Key, assign};
use devworks_core::ports::storage::{
    BlobState, ProjectPurgeCandidates, ProjectPurgeRowAction as Action,
    ProjectPurgeRowReason as Reason, ProjectPurgeRowWitness, ProjectPurgeUnresolved, StorageError,
};
use devworks_core::protocol::ProjectId;
use rusqlite::Connection;
use std::collections::BTreeMap;

pub(super) fn partition(
    conn: &Connection,
    project: &ProjectId,
    snapshot: &ProjectPurgeCandidates,
    rows: &mut BTreeMap<Key, ProjectPurgeRowWitness>,
    unresolved: &mut BTreeMap<Key, ProjectPurgeUnresolved>,
) -> Result<(), StorageError> {
    let selection = super::candidates::selection(project)?;
    // A holder name is not an ownership proof. Its owner must release the pin;
    // preparation never sweeps backup, reader or unknown global holders.
    for pin in selection.rows_where(conn, "artifact_blob_pins", "1=1")? {
        assign(
            &pin,
            Action::Keep,
            Reason::IndependentPublication,
            rows,
            unresolved,
        );
    }
    let sql = selection.sql("blobs", "digest,generation,state,claim_owner", "1=1")?;
    let mut statement = db(conn.prepare(&sql))?;
    let mut records = db(statement.query(selection.parameters()))?;
    while let Some(record) = db(records.next())? {
        let (digest, generation, state, claim) = (
            db(record.get::<_, String>(0))?,
            db(record.get::<_, String>(1))?,
            db(record.get::<_, String>(2))?,
            db(record.get::<_, Option<String>>(3))?,
        );
        let key_digest =
            super::guard::key_digest(&serde_json::to_string(&[&digest]).expect("blob key"))?;
        let Some(candidate) = snapshot
            .rows
            .iter()
            .find(|r| r.table == "blobs" && r.key_digest == key_digest)
        else {
            continue;
        };
        let Some(state) = BlobState::parse(&state) else {
            continue;
        };
        // A collector's active generation claim must settle through its actual
        // owner. The purge cannot adopt a speculative or unreviewed claim.
        if generation.is_empty() || claim.is_some() || state == BlobState::Unlinking {
            continue;
        }
        let mut keeper = false;
        let mut control_only = rows
            .get(&(
                devworks_core::ports::storage::ProjectRecordKind::Blobs,
                key_digest.clone(),
            ))
            .is_some_and(|r| r.reason == Reason::PurgeControlAudit);
        let mut versions = db(conn.prepare(
            "SELECT id FROM artifact_versions WHERE blob_digest=?1 AND deleted_at IS NULL",
        ))?;
        let mut versions = db(versions.query([&digest]))?;
        while let Some(version) = db(versions.next())? {
            let id = db(version.get::<_, String>(0))?;
            let key = (
                devworks_core::ports::storage::ProjectRecordKind::ArtifactVersions,
                super::guard::key_digest(&serde_json::to_string(&[id]).expect("version key"))?,
            );
            keeper |= rows.get(&key).is_none_or(|r| r.action != Action::Delete)
                || unresolved.contains_key(&key);
            control_only &= rows
                .get(&key)
                .is_some_and(|r| r.reason == Reason::PurgeControlAudit);
        }
        for reference in snapshot.references.iter().filter(|edge| {
            edge.to_table == "blobs"
                && edge.to_key_digest == key_digest
                && edge.from_table != "artifact_versions"
        }) {
            let key = (reference.from_table, reference.from_key_digest.clone());
            keeper |= rows.get(&key).is_none_or(|r| r.action != Action::Delete)
                || unresolved.contains_key(&key);
            control_only &= reference.from_table == "artifact_blob_pins"
                || rows
                    .get(&key)
                    .is_some_and(|r| r.reason == Reason::PurgeControlAudit);
        }
        assign(
            candidate,
            if keeper { Action::Keep } else { Action::Scrub },
            if control_only {
                Reason::PurgeControlAudit
            } else if keeper {
                Reason::SharedContent
            } else {
                Reason::DerivedContent
            },
            rows,
            unresolved,
        );
        if control_only {
            for reference in snapshot.references.iter().filter(|edge| {
                edge.to_table == "blobs"
                    && edge.to_key_digest == key_digest
                    && edge.from_table == "artifact_blob_pins"
            }) {
                if let Some(pin) = snapshot.rows.iter().find(|r| {
                    r.table == reference.from_table && r.key_digest == reference.from_key_digest
                }) {
                    assign(
                        pin,
                        Action::Keep,
                        Reason::PurgeControlAudit,
                        rows,
                        unresolved,
                    );
                }
            }
        }
    }
    Ok(())
}
