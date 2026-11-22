//! The backup effect publishes one immutable archive and reconciles its original destination.
use super::{BACKUP_OPERATION, EFFECT_CONSUMER, WorkspaceArchiveService, authority_in};
use crate::ports::AuthorityFence;
use crate::ports::storage::{
    ArchiveAction, ArchivePhase, ArtifactScope, Cancellation, OperationRecord, QueueEntry,
    QueueState, ReadOptions, StorageError, StoredVersion, StoredWorkspaceArchive, WorkspaceArchive,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::BackupRecord;
use crate::protocol::results::OperationResult;
use crate::protocol::{ActorId, Id, OperationState, RootId, StoreId};
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::effect::{EffectOutcome, JournaledEffect};
use crate::services::effect_job;
use crate::services::kernel::{counter, rev};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Job {
    pub actor: ActorId,
    pub actor_kind: String,
    pub store_id: StoreId,
    pub restore_epoch: i64,
    pub source_context: Value,
    pub fences: Vec<AuthorityFence>,
    pub delivery: Id,
    pub controls: Vec<(RootId, i64)>,
    pub assignment_generation: Option<i64>,
}

pub(super) fn decode(operation: &OperationRecord) -> Result<Job, StorageError> {
    if operation.capability != BACKUP_OPERATION {
        return Err(StorageError::ScopeDenied);
    }
    serde_json::from_value(operation.payload.clone())
        .map_err(|_| StorageError::InvalidRecord("invalid workspace backup effect".into()))
}

fn bound(archive: &StoredWorkspaceArchive, operation: &OperationRecord, job: &Job) -> bool {
    archive.operation.as_ref() == Some(&operation.id)
        && archive.store == job.store_id
        && archive.epoch == job.restore_epoch
        && archive.owner == job.actor
}

fn check_dispatch(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    job: &Job,
) -> Result<(), StorageError> {
    effect_job::attempt_holds(tx, operation, EFFECT_CONSUMER)?;
    let delivery = tx
        .queue()
        .entry(&job.delivery)?
        .filter(|entry| {
            entry.consumer == EFFECT_CONSUMER && entry.operation_id.as_ref() == Some(&operation.id)
        })
        .ok_or(StorageError::ScopeDenied)?;
    let attempt = operation
        .latest_attempt()
        .ok_or(StorageError::ScopeDenied)?;
    if delivery.state != QueueState::Claimed
        || delivery.lease_until_ms.is_none_or(|end| end <= tx.now_ms())
        || attempt.binding.owner_generation != Some(delivery.generation)
        || delivery.claim_boot_id.as_ref() != tx.store().boot_id.as_ref()
        || Some(&attempt.boot_id) != tx.store().boot_id.as_ref()
    {
        return Err(StorageError::LeaseLost {
            kind: EFFECT_CONSUMER,
            id: job.delivery.as_str().into(),
        });
    }
    if delivery.cancel_requested {
        return Err(StorageError::Cancelled);
    }
    let (_, ctx) = effect_job::restore(
        tx,
        &effect_job::JobBinding {
            store_id: &job.store_id,
            restore_epoch: job.restore_epoch,
            fences: &job.fences,
            actor: &job.actor,
            actor_kind: &job.actor_kind,
            source_context: &job.source_context,
            control_roots: &job.controls,
            assignment_generation: job.assignment_generation,
        },
        &operation.id,
    )?;
    authority_in(tx, &ctx)
}

pub(super) fn artifact_ref(version: StoredVersion) -> ArtifactRef {
    ArtifactRef {
        artifact_id: version.artifact_id,
        version_id: version.id,
        sha256: Digest(version.blob_digest),
        byte_length: counter(version.byte_count),
        media_type: version.media_type,
    }
}

fn result(archive: &StoredWorkspaceArchive) -> EffectOutcome {
    let ArchiveAction::Backup { checkpoint, .. } = &archive.action else {
        return EffectOutcome::Failed("Workspace backup has the wrong operation record".into());
    };
    match (archive.phase, archive.manifest.as_ref()) {
        (ArchivePhase::Published, Some(manifest)) => {
            EffectOutcome::Completed(json!({"result":OperationResult::WorkspaceBackup{
                value:BackupRecord{id:archive.id.clone(),revision:rev(archive.revision),manifest:manifest.clone(),
                    schema_version:rev(checkpoint.schema_version),created_at_ms:counter(archive.created_at_ms),
                    state:OperationState::ObservedComplete}
            }}))
        }
        (ArchivePhase::Cancelled | ArchivePhase::Failed, _) => EffectOutcome::Failed(
            archive
                .detail
                .clone()
                .unwrap_or_else(|| "Workspace backup stopped".into()),
        ),
        _ => EffectOutcome::Unknown("Workspace backup still requires observation".into()),
    }
}

impl WorkspaceArchiveService {
    fn backup_record(
        &self,
        operation: &OperationRecord,
        job: &Job,
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            if read.stamp().store_id != job.store_id
                || read.stamp().restore_epoch != job.restore_epoch
            {
                return Err(StorageError::ScopeDenied);
            }
            read.workspace_archives()?
                .workspace_archive_for_operation(&operation.id)?
                .filter(|record| bound(record, operation, job))
                .ok_or(StorageError::ScopeDenied)
        })
    }

    fn finish_backup(
        &self,
        operation: &OperationRecord,
        job: &Job,
        archive: &WorkspaceArchive,
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        let bytes = serde_json::to_vec(archive)
            .map_err(|_| StorageError::InvalidRecord("workspace manifest encoding".into()))?;
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                &bytes,
                json!({"kind":"WorkspaceBackupManifest"}),
            )?
            .map_err(|_| StorageError::ViewUnavailable)?;
        let held = staged.clone();
        let artifacts = self.artifacts.clone();
        let operation = operation.clone();
        let job = job.clone();
        let result = run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            let mut current = tx
                .workspace_archives()?
                .workspace_archive_for_operation(&operation.id)?
                .filter(|archive| bound(archive, &operation, &job))
                .ok_or(StorageError::ScopeDenied)?;
            if current.phase == ArchivePhase::Published {
                return Ok((current, false));
            }
            if tx.store().store_id != job.store_id
                || tx.store().restore_epoch != job.restore_epoch
                || !matches!(current.phase, ArchivePhase::Queued | ArchivePhase::Unknown)
            {
                return Err(StorageError::ScopeDenied);
            }
            effect_job::attempt_holds(tx, &operation, EFFECT_CONSUMER)?;
            let version=artifacts.commit_host_bytes_in_scope(tx,&held,HostArtifactPublication {
                title:"Workspace backup manifest",media_type:"application/json",
                provenance:json!({"kind":"WorkspaceBackupManifest","operationId":operation.id}),
                scope:ArtifactScope::Workspace,owner:current.owner.clone(),
            })?;
            current.manifest = Some(artifact_ref(version));
            current.phase = ArchivePhase::Published;
            current.detail = None;
            let previous = current.revision;
            current.revision += 1;
            crate::services::artifacts::release_checkpoint_blobs(tx, &operation.id.0)?;
            tx.workspace_archives()?
                .put_workspace_archive(&current, Some(previous))?;
            Ok((current, true))
        });
        self.artifacts.settle_host_bytes(
            &*self.store,
            &staged,
            result.as_ref().is_ok_and(|(_, published)| *published)
                || matches!(result, Err(StorageError::CommitUnknown)),
        );
        result.map(|(record, _)| record)
    }

    fn execute_backup(
        &self,
        operation: &OperationRecord,
        job: &Job,
        cancellation: &Cancellation,
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        let record = self.backup_record(operation, job)?;
        if record.phase != ArchivePhase::Queued {
            return Ok(record);
        }
        let ArchiveAction::Backup {
            checkpoint,
            destination,
        } = &record.action
        else {
            return Err(StorageError::ScopeDenied);
        };
        let validate = || {
            let operation = operation.clone();
            let job = job.clone();
            run_write(&*self.store, WriteOptions::host_state(), move |tx| {
                check_dispatch(tx, &operation, &job)
            })
        };
        validate()?;
        let manifest = match self.files.read_stage(&record.stage_id) {
            Ok(manifest) => manifest,
            Err(StorageError::NotFound { .. }) => {
                self.files
                    .stage_blobs(&record.stage_id, checkpoint, cancellation)?;
                self.files.seal_stage(
                    &record.stage_id,
                    checkpoint.clone(),
                    &self.app_build,
                    cancellation,
                )?
            }
            Err(error) => return Err(error),
        };
        if manifest.operation_id != record.id || manifest.checkpoint != *checkpoint {
            return Err(StorageError::ScopeDenied);
        }
        self.files.publish(
            destination,
            &record.id,
            &manifest,
            cancellation,
            &mut || validate(),
        )?;
        self.finish_backup(operation, job, &manifest)
    }

    fn stop_before_publication(
        &self,
        operation: &OperationRecord,
        job: &Job,
        phase: ArchivePhase,
        reason: String,
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        let operation = operation.clone();
        let job = job.clone();
        run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            effect_job::attempt_holds(tx, &operation, EFFECT_CONSUMER)?;
            let mut current = tx
                .workspace_archives()?
                .workspace_archive_for_operation(&operation.id)?
                .filter(|record| bound(record, &operation, &job))
                .ok_or(StorageError::ScopeDenied)?;
            if current.phase != ArchivePhase::Queued {
                return Ok(current);
            }
            let previous = current.revision;
            current.revision += 1;
            current.phase = phase;
            current.detail = Some(reason);
            crate::services::artifacts::release_checkpoint_blobs(tx, &operation.id.0)?;
            tx.workspace_archives()?
                .put_workspace_archive(&current, Some(previous))?;
            Ok(current)
        })
    }
}

impl JournaledEffect for WorkspaceArchiveService {
    fn resumes_after_observation(&self) -> bool {
        true
    }
    fn observes_delivery_cancellation(&self) -> bool {
        true
    }

    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::default())
    }
    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
    ) -> EffectOutcome {
        let Ok(job) = decode(operation) else {
            return EffectOutcome::Failed("Invalid workspace backup effect".into());
        };
        match self.execute_backup(operation, &job, cancellation) {
            Ok(record) => result(&record),
            Err(failure) => {
                // Observation deliberately uses its own token. A cancelled copy may have
                // crossed the publication boundary just before the cancellation arrived.
                let observed = self.observe_cancellable(operation, &Cancellation::default());
                if !matches!(observed, EffectOutcome::NotApplied(_)) {
                    return observed;
                }
                let (phase, reason) = if matches!(failure, StorageError::Cancelled) {
                    (
                        ArchivePhase::Cancelled,
                        "Backup cancelled before publication".into(),
                    )
                } else {
                    (ArchivePhase::Failed, failure.to_string())
                };
                self.stop_before_publication(operation, &job, phase, reason)
                    .map_or_else(
                        |_| {
                            EffectOutcome::Unknown(
                                "The unpublished backup result could not be recorded".into(),
                            )
                        },
                        |record| result(&record),
                    )
            }
        }
    }
    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        self.observe_cancellable(operation, &Cancellation::default())
    }
    fn observe_cancellable(
        &self,
        operation: &OperationRecord,
        cancellation: &Cancellation,
    ) -> EffectOutcome {
        let Ok(job) = decode(operation) else {
            return EffectOutcome::Failed("Invalid workspace backup effect".into());
        };
        let observed = (|| -> Result<EffectOutcome, StorageError> {
            let saved = self.backup_record(operation, &job)?;
            if !matches!(saved.phase, ArchivePhase::Queued | ArchivePhase::Unknown) {
                return Ok(result(&saved));
            }
            let ArchiveAction::Backup {
                checkpoint,
                destination,
            } = &saved.action
            else {
                return Err(StorageError::ScopeDenied);
            };
            let output = self.files.read_stage(&saved.stage_id);
            match output {
                Ok(manifest)
                    if manifest.operation_id == saved.id && manifest.checkpoint == *checkpoint =>
                {
                    if self.files.observe_publication(
                        destination,
                        &saved.id,
                        &manifest,
                        cancellation,
                    )? {
                        return Ok(result(&self.finish_backup(operation, &job, &manifest)?));
                    }
                }
                Err(StorageError::NotFound { .. }) => {}
                _ => return Err(StorageError::ScopeDenied),
            }
            if saved.phase == ArchivePhase::Unknown {
                return Ok(result(&saved));
            }
            let cancelled = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                let entry = read
                    .queue()
                    .entry(&job.delivery)?
                    .ok_or(StorageError::ScopeDenied)?;
                if entry.consumer != EFFECT_CONSUMER
                    || entry.operation_id.as_ref() != Some(&operation.id)
                {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(entry.cancel_requested)
            })?;
            if cancelled {
                return Ok(result(&self.stop_before_publication(
                    operation,
                    &job,
                    ArchivePhase::Cancelled,
                    "Backup cancelled before publication".into(),
                )?));
            }
            Ok(EffectOutcome::NotApplied(
                "The original workspace archive was not published".into(),
            ))
        })();
        observed.unwrap_or_else(|error| {
            let detail = format!("The original workspace export could not be verified. {error}");
            let operation = operation.clone();
            let reason = detail.clone();
            let _ = run_write(&*self.store, WriteOptions::settlement(), move |tx| {
                effect_job::attempt_holds(tx, &operation, EFFECT_CONSUMER)?;
                let Some(mut row) = tx
                    .workspace_archives()?
                    .workspace_archive_for_operation(&operation.id)?
                    .filter(|row| {
                        row.phase == ArchivePhase::Queued && bound(row, &operation, &job)
                    })
                else {
                    return Ok(());
                };
                let revision = row.revision;
                row.revision += 1;
                row.phase = ArchivePhase::Unknown;
                row.detail = Some(reason.chars().take(1024).collect());
                tx.workspace_archives()?
                    .put_workspace_archive(&row, Some(revision))
            });
            EffectOutcome::Unknown(detail)
        })
    }
}

pub(crate) struct QueueHook;
impl crate::services::queue::ConsumerHook for QueueHook {
    fn consumer(&self) -> &'static str {
        EFFECT_CONSUMER
    }
    fn cancelled(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<(), StorageError> {
        let id = entry
            .operation_id
            .as_ref()
            .ok_or(StorageError::ScopeDenied)?;
        let operation = tx
            .operations()
            .operation(id)?
            .ok_or(StorageError::ScopeDenied)?;
        let job = decode(&operation)?;
        let mut record = tx
            .workspace_archives()?
            .workspace_archive_for_operation(id)?
            .filter(|record| bound(record, &operation, &job))
            .ok_or(StorageError::ScopeDenied)?;
        if operation.state != OperationState::CancelledBeforeDispatch
            || record.phase != ArchivePhase::Queued
        {
            return Err(StorageError::ScopeDenied);
        }
        let previous = record.revision;
        record.revision += 1;
        record.phase = ArchivePhase::Cancelled;
        record.detail = Some("Backup cancelled before publication".into());
        crate::services::artifacts::release_checkpoint_blobs(tx, &id.0)?;
        tx.workspace_archives()?
            .put_workspace_archive(&record, Some(previous))
    }
}
