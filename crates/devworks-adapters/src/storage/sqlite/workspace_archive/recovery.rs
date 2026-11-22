//! Closed-store replacement. No connection pool, collector or dispatcher exists at this boundary.
use super::{inspect, invalid, io, record, standalone};
use crate::fs::workspace_archive::{
    ArchiveDirectory, NativeWorkspaceArchives, copy_verified_member, digest_private_file,
};
use crate::storage::sqlite::{DATABASE_FILE, SqliteConfig, conn};
use devworks_core::authority::destructive::FileIdentity;
use devworks_core::ports::IdKind;
use devworks_core::ports::storage::{
    ArchiveAction, ArchivePhase, Cancellation, FileDigest, StorageError, StoredWorkspaceArchive,
    WorkspaceArchive, WorkspaceCheckpoint,
};
use devworks_core::ports::workspace_archive::WorkspaceArchiveFiles;
use devworks_core::protocol::Id;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::fs::File;
use std::io::Read;
use std::path::Path;

const PENDING: &str = "restore-pending.json";
const PREPARED: &str = "restore-prepared.json";
const ROLLING_BACK: &str = "restore-rollback.json";
const OUTCOME: &str = "restore-outcome.json";
const MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;
const ARCHIVE_READ_SQL: &str =
    "SELECT record_json FROM workspace_archive_runs WHERE id=?1 AND phase='AwaitingRestart'";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arm {
    proposal: Id,
    stopped_boot: Id,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    prior: WorkspaceArchive,
    target_before: FileDigest,
    candidate: Id,
    candidate_identity: FileIdentity,
    candidate_digest: FileDigest,
    candidate_checkpoint: WorkspaceCheckpoint,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    restored: bool,
    detail: Option<String>,
}

fn present(path: &Path) -> Result<bool, StorageError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io(error)),
    }
}

fn read<T: DeserializeOwned>(
    directory: &ArchiveDirectory,
    filename: &str,
) -> Result<Option<T>, StorageError> {
    if !present(&directory.location().join(filename))? {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    directory
        .open_member(filename, MAX_RECORD_BYTES)?
        .reader
        .take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("workspace restore journal is too large"));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| invalid("workspace restore journal is unreadable"))
}

fn seal(
    directory: &ArchiveDirectory,
    filename: &str,
    temporary: &Id,
    value: &impl Serialize,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| invalid("workspace restore journal cannot be encoded"))?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid("workspace restore journal is too large"));
    }
    directory.seal_record(
        &format!("restore-{}.pending", temporary.as_str()),
        filename,
        &bytes,
        &Cancellation::default(),
    )
}

/// The desktop calls this after it has stopped command admission, dispatch, and local owned
/// work, while it still owns the store. A later startup also verifies that this exact SQLite
/// boot closed cleanly before committing any replacement.
pub fn arm_restore(
    data_root: &Path,
    awaiting: &StoredWorkspaceArchive,
    boot: &Id,
    temporary: &Id,
) -> Result<(), StorageError> {
    if awaiting.phase != ArchivePhase::AwaitingRestart
        || awaiting.operation.is_none()
        || !matches!(awaiting.action, ArchiveAction::Restore { .. })
    {
        return Err(StorageError::ScopeDenied);
    }
    let root = ArchiveDirectory::open(data_root, None)?;
    seal(
        &root,
        PENDING,
        temporary,
        &Arm {
            proposal: awaiting.id.clone(),
            stopped_boot: boot.clone(),
        },
    )
}

pub(in crate::storage::sqlite) fn require_no_armed_restore(
    data_root: &Path,
) -> Result<(), StorageError> {
    if present(&data_root.join(PENDING))? {
        return Err(invalid(
            "armed workspace restore needs the native file owners",
        ));
    }
    Ok(())
}

fn prepare(
    config: &SqliteConfig,
    archives: &NativeWorkspaceArchives,
    path: &Path,
    arm: &Arm,
) -> Result<Prepared, StorageError> {
    let cancellation = Cancellation::default();
    let now = config.clock.now_ms().get();
    let old = inspect(path, now)?;
    let live = super::immutable_probe(path)?;
    let clean_boot = conn::db(
        live.query_row(
            "SELECT id FROM app_boots WHERE shutdown_state='Clean' AND stopped_at IS NOT NULL
         AND id=(SELECT id FROM app_boots ORDER BY started_at DESC,rowid DESC LIMIT 1)",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional(),
    )?;
    let approved: StoredWorkspaceArchive =
        record(&live, ARCHIVE_READ_SQL, [arm.proposal.as_str()])?
            .ok_or_else(|| invalid("workspace restore approval is no longer current"))?;
    if clean_boot.as_deref() != Some(arm.stopped_boot.as_str())
        || approved.store != old.store_id
        || approved.epoch != old.restore_epoch
        || approved.stage_id != arm.proposal
    {
        return Err(invalid("workspace restore target or stopped boot changed"));
    }
    let operation = approved
        .operation
        .as_ref()
        .ok_or_else(|| invalid("workspace restore has no operation receipt"))?;
    let journaled: bool = conn::db(live.query_row(
        "SELECT EXISTS(SELECT 1 FROM command_receipts r JOIN operations o ON o.id=r.operation_id
         WHERE r.actor_id=?1 AND o.id=?2 AND o.capability='workspace.restore' AND o.state='IntentRecorded')",
        params![approved.owner.0.as_str(), operation.0.as_str()],
        |row| row.get(0),
    ))?;
    if !journaled {
        return Err(invalid(
            "workspace restore approval has no original receipt",
        ));
    }
    let ArchiveAction::Restore { archive, migrated } = &approved.action else {
        return Err(invalid("workspace restore approval has the wrong action"));
    };

    let original = archives.staged_database(&approved.stage_id)?;
    if archives.read_stage(&approved.stage_id)? != *archive
        || inspect(&original, archive.checkpoint.captured_at_ms)? != archive.checkpoint
    {
        return Err(invalid("the approved restore input changed"));
    }
    let (source, expected_image, expected_checkpoint) = match migrated {
        Some(image) => (
            archives.staged_database(&image.stage_id)?,
            &image.database,
            &image.checkpoint,
        ),
        None => (original, &archive.database, &archive.checkpoint),
    };
    let build_schema = super::MIGRATIONS
        .last()
        .map_or(0, |migration| migration.version);
    if expected_checkpoint.schema_version != build_schema
        || expected_checkpoint.store_id != archive.checkpoint.store_id
        || expected_checkpoint.restore_epoch != archive.checkpoint.restore_epoch
        || expected_checkpoint.sequence != archive.checkpoint.sequence
        || expected_checkpoint.blobs != archive.checkpoint.blobs
        || expected_checkpoint.packages != archive.checkpoint.packages
        || inspect(&source, expected_checkpoint.captured_at_ms)? != *expected_checkpoint
    {
        return Err(invalid(
            "the approved migrated image changed or is incompatible",
        ));
    }

    // Freeze the complete stopped destination, including authoritative files. It remains
    // separately discoverable even if the same operation ID exists in the incoming checkpoint.
    let original_id = config.ids.next_id(IdKind::Operation);
    let prior_path = archives.create_stage(&original_id)?;
    let target_before = digest_private_file(path, u64::MAX, &cancellation)?;
    copy_verified_member(
        File::open(path).map_err(io)?,
        &prior_path,
        &target_before,
        &cancellation,
    )?;
    archives.stage_packages(&original_id, &old, &cancellation)?;
    archives.stage_blobs(&original_id, &old, &cancellation)?;
    let prior = archives.seal_stage(&original_id, old, &config.app_build, &cancellation)?;

    let candidate = config.ids.next_id(IdKind::Operation);
    let candidate_path = archives.create_stage(&candidate)?;
    copy_verified_member(
        File::open(source).map_err(io)?,
        &candidate_path,
        expected_image,
        &cancellation,
    )?;
    fence_candidate(config, &candidate_path, &live, &prior, &approved, archive)?;
    let candidate_checkpoint = inspect(&candidate_path, now)?;
    let candidate_digest = digest_private_file(&candidate_path, u64::MAX, &cancellation)?;
    let candidate_dir = archives.stage(&candidate)?;
    let candidate_file = candidate_dir.open_member(DATABASE_FILE, candidate_digest.bytes)?;
    candidate_dir.flush()?;
    archives.install_members(&approved.stage_id, archive, &cancellation)?;
    Ok(Prepared {
        target_before,
        prior,
        candidate,
        candidate_identity: candidate_file.identity.ok_or(StorageError::ScopeDenied)?,
        candidate_digest,
        candidate_checkpoint,
    })
}

fn fence_candidate(
    config: &SqliteConfig,
    path: &Path,
    live: &Connection,
    prior: &WorkspaceArchive,
    approved: &StoredWorkspaceArchive,
    source: &WorkspaceArchive,
) -> Result<(), StorageError> {
    let mut writer = conn::open(path, conn::Role::Writer, config.busy_timeout)?;
    let epoch = prior
        .checkpoint
        .restore_epoch
        .max(source.checkpoint.restore_epoch)
        .checked_add(1)
        .ok_or_else(|| invalid("workspace restore epoch exhausted"))?;
    let transaction = conn::db(writer.transaction())?;
    let now = config.clock.now_ms().get();
    conn::db(transaction.execute(
        "UPDATE store_meta SET store_id=?1,restore_epoch=?2 WHERE singleton=1",
        params![prior.checkpoint.store_id.0.as_str(), epoch],
    ))?;
    // Archived grant rows remain available as evidence while native access is revoked.
    let actor = config.host_actor.0.as_str();
    conn::db(transaction.execute(
        "INSERT INTO actors(id,kind,display_name) VALUES(?1,'host','DevWorks host')
         ON CONFLICT(id) DO NOTHING",
        [actor],
    ))?;
    conn::db(transaction.execute(
        "UPDATE authority_grants SET state='Revoked',revoked_by=?1,revision=revision+1
         WHERE state='Active'",
        [actor],
    ))?;
    use super::encode;
    let restore = approved
        .operation
        .as_ref()
        .ok_or(StorageError::ScopeDenied)?;
    let receipt = conn::db(live.query_row(
        "SELECT json_object('actor',actor_id,'scope',scope_key,'request',idempotency_key,
          'fingerprint',payload_digest,'reply',json(result_json)) FROM command_receipts
          WHERE operation_id=?1",
        [restore.0.as_str()],
        |row| row.get::<_, String>(0),
    ))?;
    conn::db(transaction.execute(
        "INSERT INTO workspace_restore_history(epoch,proposal,operation,source_manifest,prior_manifest,receipt)
         VALUES(?1,?2,?3,?4,?5,?6)",
        params![epoch,approved.id.as_str(),restore.0.as_str(),encode(source)?,encode(prior)?,receipt],
    ))?;
    copy_effects(&transaction, live, epoch, restore.0.as_str(), source, prior)?;
    // Historical dispatch is retained for observation. Deliveries from the imported epoch
    // must never re-enter an effect consumer under the new host's authority.
    conn::db(transaction.execute_batch(
        "UPDATE queue_entries SET state='Cancelled',owner_id=NULL,lease_until=NULL,
          claim_boot_id=NULL,cancel_requested=1,awaiting_eligibility=0,revision=revision+1
          WHERE state IN ('Ready','Claimed','Failed','Reconciling');
         UPDATE resume_grants SET state='Revoked',revision=revision+1 WHERE state='Active';
         UPDATE operations SET state='CancelledBeforeDispatch',revision=revision+1
          WHERE state IN ('IntentRecorded','Preflighted') AND NOT EXISTS
          (SELECT 1 FROM operation_attempts a WHERE a.operation_id=operations.id AND a.state NOT IN ('Preflighted','CancelledBeforeDispatch'));
         UPDATE operations SET state='OutcomeUnknown',revision=revision+1
          WHERE state IN ('Dispatching','RetryPending','Preflighted','IntentRecorded');
         UPDATE approval_resumes SET state='Refused',revision=revision+1 WHERE state IN ('Pending','Unknown');
         UPDATE packages SET enabled=0,revision=revision+1 WHERE enabled=1;
         UPDATE package_registry SET revision=revision+1;
         UPDATE package_versions SET approval_json=NULL WHERE approval_json IS NOT NULL;
         UPDATE packages SET trust_json=NULL WHERE trust_json IS NOT NULL;
         UPDATE package_proposals SET state='Superseded',revision=revision+1,decided_at=max(created_at,0) WHERE state='Pending';
         UPDATE root_controls SET reactivate=0 WHERE reactivate=1;
         UPDATE schedules SET hold='ReactivationRequired',revision=revision+1
          WHERE enabled=1 AND deleted_at IS NULL AND hold IS NULL;
         UPDATE owned_processes SET state='Failed',revision=revision+1,
          detail='Prior workspace process custody was not transferred by restore'
          WHERE state NOT IN ('Failed','Reaped');
         UPDATE owned_containers SET state='Failed',revision=revision+1,
          detail='Prior workspace container custody was not transferred by restore'
          WHERE state NOT IN ('Failed','Stopped');
         UPDATE browser_sessions SET state='Failed',revision=revision+1,targets='[]',
          detail='Prior workspace browser custody was not transferred by restore'
          WHERE state NOT IN ('Failed','Closed');"
    ))?;
    conn::db(transaction.execute(
        "UPDATE resource_leases SET state='Released',ended_at=max(requested_at,?1),
         end_reason='Cancelled',revision=revision+1 WHERE state<>'Released'",
        [now],
    ))?;
    conn::db(transaction.execute(
        "UPDATE watchers SET phase='Settled',outcome_kind='Interrupted',stop_requested=1,
         match_pending=0,settled_at=max(created_at,?1),updated_at=max(updated_at,?1),revision=revision+1,
         outcome_detail='Restored watchers require a new current request'
         WHERE phase IN ('Active','Stopping')",
        [now],
    ))?;
    hold_roots(config, &transaction, epoch)?;
    conn::db(transaction.commit())?;
    let mode = conn::db(
        writer.pragma_update_and_check(None, "journal_mode", "DELETE", |row| {
            row.get::<_, String>(0)
        }),
    )?;
    if !mode.eq_ignore_ascii_case("delete") {
        return Err(invalid("cannot seal the restore candidate"));
    }
    writer.close().map_err(|(_, error)| conn::db_err(error))?;
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io)
}

fn hold_roots(
    config: &SqliteConfig,
    candidate: &Connection,
    epoch: i64,
) -> Result<(), StorageError> {
    let mut statement = conn::db(candidate.prepare(
        "SELECT root_id FROM root_controls WHERE desired_state='Enabled'
         UNION SELECT root_id FROM executions WHERE phase<>'Stopped'
         ORDER BY root_id",
    ))?;
    let roots: Vec<String> =
        conn::db(conn::db(statement.query_map([], |row| row.get(0)))?.collect())?;
    let origin = format!("restore-{epoch}");
    let now = config.clock.now_ms().get();
    for root in roots {
        conn::db(candidate.execute(
            "INSERT INTO root_controls(root_id,desired_state,revision,current_generation,updated_at)
             VALUES(?1,'Paused',1,0,?2) ON CONFLICT(root_id) DO UPDATE SET
              desired_state='Paused',reactivate=0,revision=root_controls.revision+1,
              updated_at=max(root_controls.updated_at,excluded.updated_at)",
            params![root,now],
        ))?;
        let hold = config.ids.next_id(IdKind::Link);
        conn::db(candidate.execute(
            "INSERT INTO blockers(id,root_id,cause,origin_id,origin_revision,state,revision,
             clearance_json,reason,created_at)
             SELECT ?1,?2,'RestartContinue',?3,1,'Active',1,?4,
              'Workspace restored. Continue to resume this work after external effects are settled.',?5
              WHERE NOT EXISTS(SELECT 1 FROM blockers WHERE root_id=?2 AND assignment_id IS NULL
                AND operation_id IS NULL AND cause='RestartContinue' AND state='Active')",
            params![hold.as_str(),root,origin,serde_json::json!({"clearsWith":"Continue","restoreEpoch":epoch}).to_string(),now],
        ))?;
        conn::db(candidate.execute(
            "UPDATE assignments SET state='Waiting',slot_held=0,revision=revision+1
             WHERE root_id=?1 AND state='Active'",
            [&root],
        ))?;
    }
    Ok(())
}

fn copy_effects(
    candidate: &Connection,
    stopped: &Connection,
    epoch: i64,
    restore_operation: &str,
    source: &WorkspaceArchive,
    prior: &WorkspaceArchive,
) -> Result<(), StorageError> {
    const STATES: &str =
        "('Dispatching','OutcomeUnknown','RetryPending','NeedsInput','ObservedComplete')";
    conn::db(candidate.execute(
        &format!("INSERT INTO workspace_restore_effects
            (epoch,timeline,origin_store,origin_epoch,operation,capability,fingerprint,root,state,unresolved)
            SELECT ?1,'source',?2,?3,id,capability,args_digest,root_id,state,state<>'ObservedComplete'
              FROM operations WHERE state IN {STATES}"),
        params![epoch,source.checkpoint.store_id.0.as_str(),source.checkpoint.restore_epoch],
    ))?;
    let mut rows = conn::db(stopped.prepare(&format!(
        "SELECT id,capability,args_digest,root_id,state FROM operations WHERE state IN {STATES} AND id<>?1"
    )))?;
    let mut rows = conn::db(rows.query([restore_operation]))?;
    let mut insert = conn::db(candidate.prepare(
        "INSERT INTO workspace_restore_effects
         (epoch,timeline,origin_store,origin_epoch,operation,capability,fingerprint,root,state,unresolved)
         VALUES(?1,'destination',?2,?3,?4,?5,?6,?7,?8,?8<>'ObservedComplete')",
    ))?;
    while let Some(row) = conn::db(rows.next())? {
        conn::db(insert.execute(params![
            epoch,
            prior.checkpoint.store_id.0.as_str(),
            prior.checkpoint.restore_epoch,
            conn::db(row.get::<_, String>(0))?,
            conn::db(row.get::<_, String>(1))?,
            conn::db(row.get::<_, String>(2))?,
            conn::db(row.get::<_, Option<String>>(3))?,
            conn::db(row.get::<_, String>(4))?,
        ]))?;
    }
    // The stopped destination can contain the observed outcome of a dispatch captured as
    // unknown in the source. Only exact provenance establishes that it was the same request.
    conn::db(candidate.execute(
        "UPDATE workspace_restore_effects SET unresolved=0
         WHERE epoch=?1 AND unresolved=1
         AND EXISTS(SELECT 1 FROM workspace_restore_effects observed
           WHERE observed.epoch=?1 AND observed.timeline<>workspace_restore_effects.timeline
           AND observed.origin_store=workspace_restore_effects.origin_store
           AND observed.origin_epoch=workspace_restore_effects.origin_epoch
           AND observed.operation=workspace_restore_effects.operation
           AND observed.capability=workspace_restore_effects.capability
           AND observed.fingerprint=workspace_restore_effects.fingerprint
           AND observed.state='ObservedComplete')",
        [epoch],
    ))?;
    Ok(())
}

fn verify(
    path: &Path,
    digest: &FileDigest,
    checkpoint: &WorkspaceCheckpoint,
) -> Result<(), StorageError> {
    if digest_private_file(path, digest.bytes, &Cancellation::default())? != *digest
        || inspect(path, checkpoint.captured_at_ms)? != *checkpoint
    {
        return Err(invalid(
            "stopped workspace image differs from its verified identity",
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rollback {
    replacement: Id,
    detail: String,
}

fn finish_rollback(
    root: &ArchiveDirectory,
    archives: &NativeWorkspaceArchives,
    path: &Path,
    prepared: &Prepared,
    rollback: Rollback,
) -> Result<Outcome, StorageError> {
    let digest = &prepared.prior.database;
    let checkpoint = &prepared.prior.checkpoint;
    if verify(path, digest, checkpoint).is_err() {
        if root.open_member(DATABASE_FILE, u64::MAX)?.identity != Some(prepared.candidate_identity)
        {
            return Err(invalid("the database changed during restore recovery"));
        }
        let stage = archives.stage(&rollback.replacement)?;
        verify(&stage.location().join(DATABASE_FILE), digest, checkpoint)?;
        root.publish_database(&stage, DATABASE_FILE)?;
        verify(path, digest, checkpoint)?;
    }
    Ok(Outcome {
        restored: false,
        detail: Some(rollback.detail),
    })
}

fn publish(
    config: &SqliteConfig,
    root: &ArchiveDirectory,
    archives: &NativeWorkspaceArchives,
    path: &Path,
    prepared: &Prepared,
    journal: &ArchiveDirectory,
) -> Result<Outcome, StorageError> {
    standalone(path)?;
    if let Some(rollback) = read(journal, ROLLING_BACK)? {
        return finish_rollback(root, archives, path, prepared, rollback);
    }
    let active_identity = root.open_member(DATABASE_FILE, u64::MAX)?.identity;
    let candidate_dir = archives.stage(&prepared.candidate)?;
    if active_identity != Some(prepared.candidate_identity) {
        verify(path, &prepared.target_before, &prepared.prior.checkpoint)?;
        verify(
            &candidate_dir.location().join(DATABASE_FILE),
            &prepared.candidate_digest,
            &prepared.candidate_checkpoint,
        )?;
        root.publish_database(&candidate_dir, DATABASE_FILE)?;
    }
    match verify(
        path,
        &prepared.candidate_digest,
        &prepared.candidate_checkpoint,
    ) {
        Ok(()) => Ok(Outcome {
            restored: true,
            detail: None,
        }),
        Err(error) => {
            // A matching descriptor identity proves this is the candidate we installed.
            // Never overwrite an unknown active file, even if it is at the expected path.
            if root.open_member(DATABASE_FILE, u64::MAX)?.identity
                != Some(prepared.candidate_identity)
            {
                return Err(error);
            }
            let prior = archives.staged_database(&prepared.prior.operation_id)?;
            verify(&prior, &prepared.prior.database, &prepared.prior.checkpoint)?;
            let rollback = config.ids.next_id(IdKind::Operation);
            let rollback_path = archives.create_stage(&rollback)?;
            copy_verified_member(
                File::open(prior).map_err(io)?,
                &rollback_path,
                &prepared.prior.database,
                &Cancellation::default(),
            )?;
            let rollback_dir = archives.stage(&rollback)?;
            rollback_dir.flush()?;
            let rollback = Rollback {
                replacement: rollback,
                detail: error.to_string(),
            };
            seal(
                journal,
                ROLLING_BACK,
                &config.ids.next_id(IdKind::Operation),
                &rollback,
            )?;
            finish_rollback(root, archives, path, prepared, rollback)
        }
    }
}

fn record_refused_restore(
    path: &Path,
    config: &SqliteConfig,
    arm: &Arm,
    reason: &str,
) -> Result<(), StorageError> {
    // This runs before a publish or after a verified rollback. The ordinary stopped-store
    // identity must still match the original approval before changing even its audit state.
    let inspected = inspect(path, config.clock.now_ms().get())?;
    let mut writer = conn::open(path, conn::Role::Writer, config.busy_timeout)?;
    let tx = conn::db(writer.transaction())?;
    let Some(mut stored): Option<StoredWorkspaceArchive> =
        record(&tx, ARCHIVE_READ_SQL, [arm.proposal.as_str()])?
    else {
        return Ok(());
    };
    if stored.store != inspected.store_id || stored.epoch != inspected.restore_epoch {
        return Err(invalid(
            "the stopped store changed before recording restore failure",
        ));
    }
    if let Some(op) = &stored.operation {
        conn::db(tx.execute(
            "UPDATE operations SET state='Failed',revision=revision+1 WHERE id=?1 AND
             capability='workspace.restore' AND state='IntentRecorded'",
            [op.0.as_str()],
        ))?;
    }
    stored.phase = ArchivePhase::Failed;
    stored.revision += 1;
    stored.detail = Some(reason.chars().take(1024).collect());
    conn::db(tx.execute(
        "UPDATE workspace_archive_runs SET phase='Failed',revision=?2,record_json=?3
         WHERE phase='AwaitingRestart' AND id=?1",
        params![
            arm.proposal.as_str(),
            stored.revision,
            super::encode(&stored)?
        ],
    ))?;
    conn::db(tx.commit())?;
    writer.close().map_err(|(_, error)| conn::db_err(error))?;
    Ok(())
}

pub(in crate::storage::sqlite) fn recover_restore(
    config: &SqliteConfig,
    archives: &NativeWorkspaceArchives,
    path: &Path,
) -> Result<(), StorageError> {
    if !present(&config.data_root.join(PENDING))? {
        return Ok(());
    }
    let root = ArchiveDirectory::open(&config.data_root, None)?;
    let arm: Arm = read(&root, PENDING)?.ok_or(StorageError::CommitUnknown)?;
    let stage = archives.stage(&arm.proposal)?;
    let outcome = match read::<Outcome>(&stage, OUTCOME)? {
        Some(outcome) => outcome,
        None => {
            let prepared = match read(&stage, PREPARED)? {
                Some(prepared) => Ok(prepared),
                None => prepare(config, archives, path, &arm).and_then(|prepared| {
                    seal(
                        &stage,
                        PREPARED,
                        &config.ids.next_id(IdKind::Operation),
                        &prepared,
                    )?;
                    Ok(prepared)
                }),
            };
            let outcome = match prepared {
                Ok(prepared) => publish(config, &root, archives, path, &prepared, &stage)?,
                // A failed preflight cannot switch the store. An uncertain seal is kept for
                // verification at the next launch and must not be mistaken for a failed restore.
                Err(StorageError::CommitUnknown) => return Err(StorageError::CommitUnknown),
                Err(error) => Outcome {
                    restored: false,
                    detail: Some(error.to_string()),
                },
            };
            seal(
                &stage,
                OUTCOME,
                &config.ids.next_id(IdKind::Operation),
                &outcome,
            )?;
            outcome
        }
    };
    if !outcome.restored {
        record_refused_restore(
            path,
            config,
            &arm,
            outcome
                .detail
                .as_deref()
                .unwrap_or("The restore could not prepare a verified workspace."),
        )?;
    }
    std::fs::remove_file(root.location().join(PENDING)).map_err(io)?;
    root.flush().map_err(|_| StorageError::CommitUnknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::artifacts::NativeArtifactFiles;
    use crate::packages::NativePackageFiles;
    use crate::storage::sqlite::SqliteStore;
    use devworks_core::ports::storage::{ApplicationStore, WorkspaceArchiveStorage};
    use devworks_core::protocol::ActorId;
    use devworks_test_support::{T0, TempRoot, TestClock, TestIds};
    use std::sync::Arc;

    fn config(root: &TempRoot) -> SqliteConfig {
        let mut config = SqliteConfig::new(
            root.path(),
            Arc::new(TestClock::new(T0)),
            Arc::new(TestIds::new()),
            ActorId(Id::new("local-host").unwrap()),
        );
        config.workspace_archives = Some(Arc::new(NativeWorkspaceArchives::new(
            root.path().to_owned(),
            Arc::new(NativeArtifactFiles::new(root.path(), root.join("cache"))),
            Arc::new(NativePackageFiles::new(root.join("packages"))),
        )));
        config
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Cutover {
        BeforeSwitch,
        AfterSwitch,
        CorruptedCandidate,
    }

    #[test]
    fn restart_finishes_before_switch() {
        test_cutover(Cutover::BeforeSwitch);
    }

    #[test]
    fn closed_store_publication_preserves_both_histories_and_does_not_repeat() {
        test_cutover(Cutover::AfterSwitch);
    }

    #[test]
    fn known_corrupted_candidate_returns_to_the_verified_previous_store() {
        test_cutover(Cutover::CorruptedCandidate);
    }

    fn test_cutover(cutover: Cutover) {
        let root = TempRoot::new("devworks-restore-publish").created();
        let setup = config(&root);
        let archives = setup.workspace_archives.clone().unwrap();
        let store = SqliteStore::open(setup).unwrap();
        let id = Id::new("restore-proposal").unwrap();
        let author = ActorId(Id::new("archive-author").unwrap());
        let effect = Id::new("colliding-effect").unwrap();
        let path = archives.create_stage(&id).unwrap();
        let add_effect = |digest: &'static str| {
            let author = author.clone();
            let effect = effect.clone();
            move |db: &Connection| {
                conn::db(db.execute_batch(
                    "INSERT INTO actors(id,kind,display_name)
                     VALUES('archive-author','human','Captured value')
                     ON CONFLICT(id) DO NOTHING",
                ))?;
                conn::db(db.execute(
                    "INSERT INTO operations(id,capability,schema_version,args_digest,payload_json,state,revision,created_at)
                     VALUES(?1,'test.effect',1,?2,'{}','ObservedComplete',1,1)
                     ON CONFLICT(id) DO UPDATE SET args_digest=excluded.args_digest",
                    params![effect.as_str(), digest],
                ))?;
                let label = if digest == "source-fingerprint" {
                    "Captured value"
                } else {
                    "Latest edit"
                };
                conn::db(db.execute(
                    "UPDATE actors SET display_name=?2 WHERE id=?1",
                    params![author.0.as_str(), label],
                ))?;
                Ok(())
            }
        };
        store.with_writer_connection(|db| conn::db(db.execute_batch(
            "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at)
              VALUES('restore-project','Restored project','Active',1,1,1);
             INSERT INTO work_items(id,project_id,kind,title,priority,manual_order,revision,scope_revision,created_at,brief_json)
              VALUES('affected','restore-project','issue','Affected issue',0,0,1,1,1,'{}'),
                    ('observed','restore-project','issue','Observed issue',0,0,1,1,1,'{}'),
                    ('local','restore-project','issue','Local issue',0,0,1,1,1,'{}');
             INSERT INTO root_controls(root_id,desired_state,revision,current_generation)
              VALUES('affected','Enabled',1,0),('observed','Enabled',1,0),('local','Enabled',1,0);
             INSERT INTO operations(id,root_id,capability,schema_version,args_digest,payload_json,state,revision,created_at)
              VALUES('observed-effect','observed','test.observation',1,'same-fingerprint','{}','OutcomeUnknown',1,1);
             INSERT INTO schedules(id,enabled,revision,timezone,definition_json,title)
              VALUES('restored-schedule',1,1,'UTC','{}','Restored schedule');"
        ))).unwrap();
        store
            .with_writer_connection(add_effect("source-fingerprint"))
            .unwrap();
        let checkpoint = store
            .capture_checkpoint(0, &path, &Cancellation::default())
            .unwrap()
            .unwrap();
        let archive = archives
            .seal_stage(&id, checkpoint, "test-build", &Cancellation::default())
            .unwrap();
        store
            .with_writer_connection(add_effect("destination-fingerprint"))
            .unwrap();
        store.with_writer_connection(|db| conn::db(db.execute_batch(
            "UPDATE operations SET state='ObservedComplete',revision=2 WHERE id='observed-effect';
             INSERT INTO operations(id,root_id,capability,schema_version,args_digest,payload_json,state,revision,created_at)
              VALUES('unresolved-effect','affected','test.uncertain',1,'dest-only','{}','OutcomeUnknown',1,1);"
        ))).unwrap();
        let info = store.info();
        let restore = devworks_core::protocol::OperationId(Id::new("restore-operation").unwrap());
        let approved = StoredWorkspaceArchive {
            id: id.clone(),
            stage_id: id.clone(),
            owner: author,
            store: info.store_id.clone(),
            epoch: info.restore_epoch,
            created_at_ms: T0,
            expires_at_ms: None,
            revision: 2,
            phase: ArchivePhase::AwaitingRestart,
            action: ArchiveAction::Restore {
                archive,
                migrated: None,
            },
            operation: Some(restore.clone()),
            manifest: None,
            migration_manifest: None,
            detail: None,
        };
        let record_json = super::super::encode(&approved).unwrap();
        store.with_writer_connection(move |db| {
            conn::db(db.execute(
                "INSERT INTO operations(id,capability,schema_version,args_digest,payload_json,state,revision,created_at)
                 VALUES(?1,'workspace.restore',1,'restore-fingerprint','{}','IntentRecorded',1,1)",
                [restore.0.as_str()],
            ))?;
            conn::db(db.execute(
                "INSERT INTO workspace_archive_runs(id,owner,operation_id,revision,phase,record_json)
                 VALUES('restore-proposal','archive-author',?1,2,'AwaitingRestart',?2)",
                params![restore.0.as_str(), record_json],
            ))?;
            conn::db(db.execute(
                "INSERT INTO command_receipts(actor_id,scope_key,idempotency_key,command_kind,payload_digest,operation_id,result_json,created_at)
                 VALUES('archive-author','old-context','restore-request','workspace.restore','restore-fingerprint',?1,'{}',1)",
                [restore.0.as_str()],
            ))?;
            Ok(())
        }).unwrap();
        arm_restore(
            root.path(),
            &approved,
            info.boot_id.as_ref().unwrap(),
            &Id::new("pending-arm").unwrap(),
        )
        .unwrap();
        drop(store);
        let mut next = config(&root);
        // Simulate a crash exactly after switching the authoritative file, before settlement.
        // The lock would ordinarily be held by SqliteStore::open. The fixture has no other owner.
        next.ids = Arc::new(devworks_test_support::NumberedIds::new("restore-next"));
        let root_dir = ArchiveDirectory::open(root.path(), None).unwrap();
        let arm: Arm = read(&root_dir, PENDING).unwrap().unwrap();
        let database = root.join(DATABASE_FILE);
        let prepared = prepare(&next, &archives, &database, &arm).unwrap();
        let stage = archives.stage(&id).unwrap();
        seal(
            &stage,
            PREPARED,
            &Id::new("ready-stage").unwrap(),
            &prepared,
        )
        .unwrap();
        if cutover != Cutover::BeforeSwitch {
            root_dir
                .publish_database(&archives.stage(&prepared.candidate).unwrap(), DATABASE_FILE)
                .unwrap();
        }
        if cutover == Cutover::CorruptedCandidate {
            use std::io::Write;
            let mut damaged = std::fs::OpenOptions::new()
                .write(true)
                .open(&database)
                .unwrap();
            damaged.write_all(b"damaged first page").unwrap();
            damaged.sync_all().unwrap();
        }
        let reopened = SqliteStore::open(next).unwrap();
        assert_eq!(reopened.info().store_id, info.store_id);
        if cutover == Cutover::CorruptedCandidate {
            assert_eq!(reopened.info().restore_epoch, info.restore_epoch);
            let actual = reopened
                .with_writer_connection(|db| {
                    conn::db(db.query_row(
                        "SELECT phase, (SELECT display_name FROM actors WHERE id='archive-author')
                   FROM workspace_archive_runs WHERE id='restore-proposal'",
                        [],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    ))
                })
                .unwrap();
            assert_eq!(actual, ("Failed".into(), "Latest edit".into()));
            assert!(!root.join(PENDING).exists());
            assert!(read::<Rollback>(&stage, ROLLING_BACK).unwrap().is_some());
            drop(reopened);
            assert_eq!(
                SqliteStore::open(config(&root))
                    .unwrap()
                    .info()
                    .restore_epoch,
                info.restore_epoch
            );
            return;
        }
        assert_eq!(reopened.info().restore_epoch, info.restore_epoch + 1);
        assert!(!root.join(PENDING).exists());
        let data = reopened
            .with_writer_connection(|db| {
                conn::db(db.query_row(
                    "SELECT display_name,args_digest,json_extract(receipt,'$.request')
                 FROM actors,operations,workspace_restore_history
                 WHERE actors.id='archive-author' AND operations.id='colliding-effect'",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                ))
            })
            .unwrap();
        assert_eq!(
            data,
            (
                "Captured value".into(),
                "source-fingerprint".into(),
                "restore-request".into()
            )
        );
        devworks_core::ports::storage::read_snapshot(&reopened, &Default::default(), |snapshot| {
            use devworks_core::protocol::RootId;
            let archives = snapshot.workspace_archives()?;
            let root = |id| RootId(Id::new(id).unwrap());
            assert!(archives.restore_effect_held(Some(&root("affected")), None)?);
            assert!(!archives.restore_effect_held(Some(&root("observed")), None)?);
            assert!(!archives.restore_effect_held(Some(&root("local")), None)?);
            assert_eq!(
                archives
                    .latest_workspace_restore(1)?
                    .unwrap()
                    .unresolved_effects,
                1
            );
            Ok(())
        })
        .unwrap();
        reopened.with_writer_connection(|db| {
            let states: (i64, i64) = conn::db(db.query_row(
                "SELECT (SELECT count(*) FROM root_controls WHERE desired_state='Paused'),
                   (SELECT count(*) FROM schedules WHERE hold='ReactivationRequired' AND activated_epoch=0)",
                [], |row| Ok((row.get(0)?,row.get(1)?)),
            ))?;
            assert_eq!(states, (3, 1));
            Ok(())
        }).unwrap();
        let earlier = conn::open_probe(
            &archives
                .staged_database(&prepared.prior.operation_id)
                .unwrap(),
        )
        .unwrap();
        let earlier: String = earlier
            .query_row(
                "SELECT args_digest FROM operations WHERE id='colliding-effect'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(earlier, "destination-fingerprint");
        drop(reopened);
        let again = SqliteStore::open(config(&root)).unwrap();
        assert_eq!(again.info().restore_epoch, info.restore_epoch + 1);
    }
}
