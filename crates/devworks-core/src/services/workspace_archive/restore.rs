//! Native selection seals an immutable candidate before a human can authorize a restore.
use super::{WorkspaceArchiveService, authority_in, human};
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::operations::workspace::{MigrateInput, RestoreInput};
use crate::ports::storage::{
    ArchiveAction, ArchivePhase, ArtifactScope, AttemptBinding, Cancellation, NewEvent,
    NewOperation, ReadOptions, StorageError, StoredWorkspaceArchive, WorkspaceArchive,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{IdKind, Preparation, Prepared};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::MutationSummary;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, EventId, Id, OperationId, OperationState};
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::kernel::{Failure, Refusal, rev};
use serde_json::{Value, json};
use std::path::Path;

pub const RESTORE_OPERATION: &str = "workspace.restore";
pub const MIGRATE_OPERATION: &str = "workspace.migrate";
const PROPOSAL_TTL_MS: i64 = 60 * 60 * 1_000;

fn same_artifact(expected: &ArtifactRef, actual: &ArtifactRef) -> bool {
    expected.artifact_id == actual.artifact_id
        && expected.version_id == actual.version_id
        && expected.sha256.as_str() == actual.sha256.as_str()
        && expected.byte_length.get() == actual.byte_length.get()
        && expected.media_type == actual.media_type
}

fn owner(ctx: &InvocationContext, record: &StoredWorkspaceArchive) -> bool {
    record.owner == *ctx.principal()
        && record.store == *ctx.store_id()
        && record.epoch == ctx.restore_epoch().get()
}

fn trusted(record: &StoredWorkspaceArchive, now: i64) -> bool {
    record.phase == ArchivePhase::Prepared && record.expires_at_ms.is_none_or(|end| end > now)
}

fn permission() -> Failure {
    Refusal::new(DomainErrorCode::PermissionDenied, "workspace-archive", None).into()
}

impl WorkspaceArchiveService {
    /// Native Settings receives the verified snapshot and exact manifest references after the
    /// complete candidate is retained in app-controlled staging.
    pub fn prepare_restore(
        &self,
        ctx: &InvocationContext,
        folder: &Path,
        cancellation: &Cancellation,
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        human(ctx)?;
        let scope = self.store.info();
        if scope.store_id != *ctx.store_id() || scope.restore_epoch != ctx.restore_epoch().get() {
            return Err(StorageError::ScopeDenied);
        }
        let selection = self.files.bind_external_folder(folder)?;
        let id = self.ids.next_id(IdKind::Operation);
        let archive = self
            .files
            .stage_external_archive(&selection, &id, cancellation)?;
        let source = self.files.staged_database(&id)?;
        if self
            .storage
            .inspect_checkpoint(&source, archive.checkpoint.captured_at_ms)?
            != archive.checkpoint
            || self.files.staged_database_digest(&id, cancellation)? != archive.database
        {
            return Err(StorageError::InvalidRecord(
                "archive does not bind its complete workspace checkpoint".into(),
            ));
        }
        let migrations = self.storage.shipped_migrations()?;
        let manifest = serde_json::to_vec(&archive)
            .map_err(|_| StorageError::InvalidRecord("workspace checkpoint manifest".into()))?;
        let prep = serde_json::to_vec(&json!({
            "format":"DevWorksWorkspaceMigrationPlan","revision":1,
            "proposedRestore":id,"destinationStore":scope.store_id,
            "destinationEpoch":scope.restore_epoch,"checkpoint":archive.database,
            "sourceSchema":archive.checkpoint.schema_version,"shippedMigrations":migrations,
        }))
        .map_err(|_| StorageError::InvalidRecord("workspace migration plan".into()))?;
        self.record_proposal(ctx, id, archive, &manifest, &prep)
    }

    fn record_proposal(
        &self,
        ctx: &InvocationContext,
        id: Id,
        archive: WorkspaceArchive,
        manifest: &[u8],
        migration: &[u8],
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        let custody = |bytes: &[u8], kind: &str| {
            self.artifacts
                .stage_host_bytes(&*self.store, bytes, json!({"kind":kind}))?
                .map_err(|_| StorageError::ViewUnavailable)
        };
        let staged = custody(manifest, "RestoreBackupManifest")?;
        let migration_staged = match custody(migration, "RestoreMigrationManifest") {
            Ok(value) => value,
            Err(error) => {
                self.artifacts
                    .settle_host_bytes(&*self.store, &staged, false);
                return Err(error);
            }
        };
        let held = staged.clone();
        let migration_held = migration_staged.clone();
        let artifacts = self.artifacts.clone();
        let ctx = ctx.clone();
        let result = run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            authority_in(tx, &ctx)?;
            let owner = ctx.principal().clone();
            let publish = |tx: &mut dyn WriteTransaction, staged, title, kind| {
                artifacts
                    .commit_host_bytes_in_scope(
                        tx,
                        staged,
                        HostArtifactPublication {
                            title,
                            media_type: "application/json",
                            provenance: json!({"kind":kind,"proposalId":id}),
                            scope: ArtifactScope::Workspace,
                            owner: owner.clone(),
                        },
                    )
                    .map(super::worker::artifact_ref)
            };
            let manifest = publish(
                tx,
                &held,
                "Reviewed workspace archive",
                "RestoreBackupManifest",
            )?;
            let migration_manifest = publish(
                tx,
                &migration_held,
                "Shipped migration plan",
                "RestoreMigrationManifest",
            )?;
            let now = tx.now_ms();
            let saved = StoredWorkspaceArchive {
                id: id.clone(),
                stage_id: id,
                owner,
                store: tx.store().store_id.clone(),
                epoch: tx.store().restore_epoch,
                created_at_ms: now,
                expires_at_ms: Some(now.saturating_add(PROPOSAL_TTL_MS)),
                revision: 1,
                phase: ArchivePhase::Prepared,
                action: ArchiveAction::Restore {
                    archive,
                    migrated: None,
                },
                operation: None,
                manifest: Some(manifest),
                migration_manifest: Some(migration_manifest),
                completion: None,
                detail: None,
            };
            tx.workspace_archives()?
                .put_workspace_archive(&saved, None)?;
            Ok(saved)
        });
        let committed = result.is_ok() || matches!(result, Err(StorageError::CommitUnknown));
        self.artifacts
            .settle_host_bytes(&*self.store, &staged, committed);
        self.artifacts
            .settle_host_bytes(&*self.store, &migration_staged, committed);
        result
    }

    pub fn prepare_active_migration_check(
        &self,
        ctx: &InvocationContext,
    ) -> Result<StoredWorkspaceArchive, StorageError> {
        self.migration_actor(ctx)?;
        let checkpoint = self.storage.active_checkpoint()?;
        if checkpoint.store_id != *ctx.store_id()
            || checkpoint.restore_epoch != ctx.restore_epoch().get()
            || checkpoint.schema_version != self.shipped_schema()?
            || checkpoint.migrations != self.storage.shipped_migrations()?
        {
            return Err(StorageError::InvalidRecord(
                "the active workspace must pass the native migration gate".into(),
            ));
        }
        let manifest = serde_json::to_vec(&json!({
            "format":"DevWorksWorkspaceMigrationPlan","revision":1,
            "destinationStore":checkpoint.store_id,"destinationEpoch":checkpoint.restore_epoch,
            "sourceSchema":checkpoint.schema_version,"shippedMigrations":checkpoint.migrations,
        }))
        .map_err(|_| StorageError::InvalidRecord("active migration plan".into()))?;
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                &manifest,
                json!({"kind":"WorkspaceMigrationManifest"}),
            )?
            .map_err(|_| StorageError::ViewUnavailable)?;
        let held = staged.clone();
        let service = self.artifacts.clone();
        let id = self.ids.next_id(IdKind::Operation);
        let ctx = ctx.clone();
        let result = run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            migration_authority(tx, &ctx)?;
            let version = service.commit_host_bytes_in_scope(
                tx,
                &held,
                HostArtifactPublication {
                    title: "Current workspace migration ledger",
                    media_type: "application/json",
                    provenance: json!({"kind":"WorkspaceMigrationManifest","planId":id}),
                    scope: ArtifactScope::Workspace,
                    owner: ctx.principal().clone(),
                },
            )?;
            let now = tx.now_ms();
            let record = StoredWorkspaceArchive {
                id: id.clone(),
                stage_id: id,
                owner: ctx.principal().clone(),
                store: tx.store().store_id.clone(),
                epoch: tx.store().restore_epoch,
                created_at_ms: now,
                expires_at_ms: Some(now.saturating_add(PROPOSAL_TTL_MS)),
                revision: 1,
                phase: ArchivePhase::Prepared,
                action: ArchiveAction::VerifyMigration { checkpoint },
                operation: None,
                manifest: None,
                detail: None,
                completion: None,
                migration_manifest: Some(super::worker::artifact_ref(version)),
            };
            tx.workspace_archives()?
                .put_workspace_archive(&record, None)?;
            Ok(record)
        });
        self.artifacts.settle_host_bytes(
            &*self.store,
            &staged,
            result.is_ok() || matches!(result, Err(StorageError::CommitUnknown)),
        );
        result
    }

    fn migration_actor(&self, ctx: &InvocationContext) -> Result<(), StorageError> {
        if !matches!(ctx.principal_kind(), crate::context::PrincipalKind::Host) {
            return human(ctx);
        }
        let info = self.store.info();
        if ctx.principal() != &info.host_actor
            || ctx.store_id() != &info.store_id
            || ctx.restore_epoch().get() != info.restore_epoch
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    pub fn inspect_workspace_archive(
        &self,
        ctx: &InvocationContext,
        id: &Id,
    ) -> Result<Option<StoredWorkspaceArchive>, StorageError> {
        human(ctx)?;
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            if read.stamp().store_id != *ctx.store_id()
                || read.stamp().restore_epoch != ctx.restore_epoch().get()
            {
                return Err(StorageError::ScopeDenied);
            }
            Ok(read
                .workspace_archives()?
                .workspace_archive(id)?
                .filter(|record| owner(ctx, record)))
        })
    }

    pub fn status(
        &self,
        ctx: &InvocationContext,
    ) -> Result<
        (
            Option<StoredWorkspaceArchive>,
            Option<crate::ports::storage::RestoredWorkspace>,
        ),
        StorageError,
    > {
        human(ctx)?;
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let stamp = read.stamp();
            if stamp.store_id != *ctx.store_id() || stamp.restore_epoch != ctx.restore_epoch().get()
            {
                return Err(StorageError::ScopeDenied);
            }
            let archives = read.workspace_archives()?;
            let pending = archives
                .restore_awaiting_restart()?
                .filter(|record| owner(ctx, record));
            Ok((
                match pending {
                    Some(record) => Some(record),
                    None => {
                        archives.latest_workspace_archive(ctx.principal(), stamp.restore_epoch)?
                    }
                },
                archives.latest_workspace_restore(stamp.restore_epoch)?,
            ))
        })
    }

    pub fn replay_prior_restore(
        &self,
        ctx: &InvocationContext,
        frame: &[u8],
    ) -> Result<Option<crate::protocol::results::DomainCommandReply>, StorageError> {
        use crate::admission::{CallerKey, Selection, decode};
        use crate::operations::{OperationInput, workspace};
        use crate::ports::storage::ArchivedRestoreKey;
        if human(ctx).is_err() {
            return Ok(None);
        }
        let Ok(decoded) = decode(
            Selection {
                operation: RESTORE_OPERATION,
                mode: crate::registry::Mode::Command,
            },
            frame,
        ) else {
            return Ok(None);
        };
        let (
            CallerKey::Command(request_key),
            OperationInput::Workspace(workspace::Input::Restore(input)),
        ) = (decoded.key, decoded.input)
        else {
            return Ok(None);
        };
        if input.expected_store_id != *ctx.store_id() {
            return Ok(None);
        }
        let Some(scope) = ctx.historical_workspace_scope(&input.expected_restore_epoch) else {
            return Ok(None);
        };
        let result = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let stamp = read.stamp();
            if stamp.store_id != *ctx.store_id()
                || stamp.restore_epoch != ctx.restore_epoch().get()
                || read.actors().actor_kind(ctx.principal())?
                    != Some(crate::context::PrincipalKind::Human)
            {
                return Err(StorageError::ScopeDenied);
            }
            read.workspace_archives()?
                .archived_restore_receipt(&ArchivedRestoreKey {
                    owner: ctx.principal(),
                    scope: scope.as_str(),
                    request_key: &request_key,
                    proposal: &input.proposal_id,
                    fingerprint: decoded.fingerprint.as_str(),
                    revision: decoded.revision,
                })
        })?
        .map(|receipt| crate::protocol::CommandReply::Recorded {
            protocol: crate::protocol::Protocol::Draft1,
            receipt,
            replayed: true,
        });
        if let Some(reply) = &result {
            crate::protocol::results::validate_command_reply(reply)
                .map_err(StorageError::InvalidRecord)?;
        }
        Ok(result)
    }

    pub fn unresolved_effects(
        &self,
        ctx: &InvocationContext,
        after: i64,
    ) -> Result<Vec<(i64, crate::ports::storage::RestoredEffect)>, StorageError> {
        human(ctx)?;
        read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let stamp = read.stamp();
            if stamp.store_id != *ctx.store_id() || stamp.restore_epoch != ctx.restore_epoch().get()
            {
                return Err(StorageError::ScopeDenied);
            }
            read.workspace_archives()?
                .unresolved_restore_effects(stamp.restore_epoch, after, 25)
        })
    }

    pub fn observe_restored_git(
        &self,
        ctx: &InvocationContext,
        id: i64,
        git: &crate::services::git::GitService,
    ) -> Result<bool, StorageError> {
        human(ctx)?;
        let effect = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let stamp = read.stamp();
            if stamp.store_id != *ctx.store_id() || stamp.restore_epoch != ctx.restore_epoch().get()
            {
                return Err(StorageError::ScopeDenied);
            }
            read.workspace_archives()?
                .restore_effect(stamp.restore_epoch, id)?
                .filter(|effect| effect.capability.starts_with("git."))
                .ok_or(StorageError::ScopeDenied)
        })?;
        let manifest = self.files.read_stage(&effect.origin_stage)?;
        let source = self.files.staged_database(&effect.origin_stage)?;
        let original = self
            .storage
            .archived_effect_operation(&source, &manifest, &effect)?;
        let Some(proof) = git.observe_archived_local(ctx, &original) else {
            return Ok(false);
        };
        let ctx = ctx.clone();
        run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            authority_in(tx, &ctx)?;
            if tx
                .workspace_archives()?
                .restore_effect(effect.epoch, id)?
                .as_ref()
                != Some(&effect)
            {
                return Err(StorageError::ScopeDenied);
            }
            tx.workspace_archives()?
                .record_restored_observation(&effect, &proof)?;
            Ok(true)
        })
    }

    /// The backup owner can re-observe a published export without dispatching an archived
    /// command. Both the immutable original intent and the original sealed bytes must match.
    pub fn observe_restored_export(
        &self,
        ctx: &InvocationContext,
        id: i64,
        chosen_parent: &Path,
    ) -> Result<bool, StorageError> {
        human(ctx)?;
        let effect = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let stamp = read.stamp();
            if stamp.store_id != *ctx.store_id() || stamp.restore_epoch != ctx.restore_epoch().get()
            {
                return Err(StorageError::ScopeDenied);
            }
            read.workspace_archives()?
                .restore_effect(stamp.restore_epoch, id)?
                .filter(|saved| saved.capability == super::BACKUP_OPERATION)
                .ok_or(StorageError::ScopeDenied)
        })?;
        let manifest = self.files.read_stage(&effect.origin_stage)?;
        let image = self.files.staged_database(&effect.origin_stage)?;
        let original = self
            .storage
            .archived_backup_intent(&image, &manifest, &effect)?;
        let ArchiveAction::Backup {
            checkpoint,
            destination,
        } = &original.action
        else {
            return Err(StorageError::ScopeDenied);
        };
        let actual = self.files.bind_external_folder(chosen_parent)?;
        if original.owner != *ctx.principal() || actual != *destination {
            return Err(StorageError::ScopeDenied);
        }
        let source = self.files.read_stage(&original.stage_id)?;
        if source.operation_id != original.id || source.checkpoint != *checkpoint {
            return Err(StorageError::ScopeDenied);
        }
        if !self.files.observe_publication(
            &actual,
            &original.id,
            &source,
            &Cancellation::default(),
        )? {
            return Ok(false);
        }
        let ctx = ctx.clone();
        run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            authority_in(tx, &ctx)?;
            if tx
                .workspace_archives()?
                .restore_effect(effect.epoch, id)?
                .as_ref()
                != Some(&effect)
            {
                return Err(StorageError::ScopeDenied);
            }
            tx.workspace_archives()?.record_restored_observation(
                &effect, &json!({
                    "kind":"ObservedWorkspaceBackup", "operation": original.id, "database":source.database,
                }),
            )?;
            let event = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id:event,actor:ctx.principal().clone(),project_id:None,root_id:None,operation_id:None,
                aggregate_kind:"workspace_archive".into(),aggregate_id:original.id.as_str().to_owned(),
                aggregate_revision:original.revision,kind:"ArchivedWorkspaceBackupObserved".into(),
                payload: json!({"epoch":effect.epoch,"originStore":effect.origin,"originEpoch":effect.origin_epoch}),
            })?;
            Ok(true)
        })
    }

    /// Assemble the typed completion of the original restore at the runtime boot boundary.
    /// The old request and its scope remain separate from command lookup in this new epoch.
    pub fn complete_restored_workspace(&self) -> Result<(), StorageError> {
        use crate::protocol::records::BackupRecord;
        use crate::protocol::results::OperationCompletion;
        let epoch = self.store.info().restore_epoch;
        if epoch == 0 {
            return Ok(());
        }
        let restored = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.workspace_archives()?.latest_workspace_restore(epoch)
        })?;
        let Some(restored) = restored.filter(|saved| saved.completion.is_none()) else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(&restored.source)
            .map_err(|_| StorageError::InvalidRecord("workspace restore manifest".into()))?;
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                &bytes,
                json!({"kind":"RestoredWorkspaceManifest"}),
            )?
            .map_err(|_| StorageError::ViewUnavailable)?;
        let held = staged.clone();
        let artifacts = self.artifacts.clone();
        let schema_version = rev(self.shipped_schema()?);
        let result = run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            if tx.store().restore_epoch != restored.epoch {
                return Err(StorageError::ScopeDenied);
            }
            let host = tx.store().host_actor.clone();
            let owner = if tx.actors().actor_kind(&restored.owner)?
                == Some(crate::context::PrincipalKind::Human)
            {
                restored.owner.clone()
            } else {
                host.clone()
            };
            let version = artifacts.commit_host_bytes_in_scope(
                tx,
                &held,
                HostArtifactPublication {
                    title: "Restored workspace manifest",
                    media_type: "application/json",
                    provenance: json!({
                        "kind":"RestoredWorkspaceManifest", "restoreEpoch":restored.epoch,
                        "originOperation":restored.operation
                    }),
                    scope: ArtifactScope::Workspace,
                    owner,
                },
            )?;
            let completed = OperationCompletion::Succeeded {
                protocol: crate::protocol::Protocol::Draft1,
                operation_id: restored.operation.clone(),
                revision: rev(2),
                result: OperationResult::WorkspaceRestore {
                    value: BackupRecord {
                        id: restored.proposal.clone(),
                        revision: rev(2),
                        manifest: super::worker::artifact_ref(version),
                        schema_version,
                        created_at_ms: super::super::kernel::counter(tx.now_ms()),
                        state: OperationState::ObservedComplete,
                    },
                },
            };
            completed.validate().map_err(StorageError::InvalidRecord)?;
            tx.workspace_archives()?.record_restored_completion(
                epoch,
                &restored.operation,
                &completed,
            )?;
            let id = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id,
                actor: host,
                project_id: None,
                root_id: None,
                operation_id: None,
                aggregate_kind: "workspace_archive".into(),
                aggregate_id: restored.proposal.as_str().to_owned(),
                aggregate_revision: 2,
                kind: "WorkspaceRestoreCompleted".into(),
                payload: json!({"source":restored.source.checkpoint.store_id,"restoreEpoch":epoch}),
            })?;
            Ok(())
        });
        self.artifacts.settle_host_bytes(
            &*self.store,
            &staged,
            result.is_ok() || matches!(result, Err(StorageError::CommitUnknown)),
        );
        result
    }

    pub fn note_shutdown_failure(
        &self,
        ctx: &InvocationContext,
        proposal: &Id,
        reason: &str,
    ) -> Result<(), StorageError> {
        let (ctx, proposal, detail) = (
            ctx.clone(),
            proposal.clone(),
            reason.chars().take(1024).collect(),
        );
        run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            authority_in(tx, &ctx)?;
            let mut record = tx
                .workspace_archives()?
                .workspace_archive(&proposal)?
                .filter(|r| owner(&ctx, r) && r.phase == ArchivePhase::AwaitingRestart)
                .ok_or(StorageError::ScopeDenied)?;
            let revision = record.revision;
            record.revision += 1;
            record.detail = Some(detail);
            tx.workspace_archives()?
                .put_workspace_archive(&record, Some(revision))
        })
    }

    pub fn cancel_archive(
        &self,
        ctx: &InvocationContext,
        id: &Id,
        expected_revision: i64,
    ) -> Result<(), StorageError> {
        use crate::protocol::OperationState;
        use crate::services::queue::ConsumerHook;
        human(ctx)?;
        let (ctx, id) = (ctx.clone(), id.clone());
        run_write(&*self.store, WriteOptions::command(), move |tx| {
            authority_in(tx, &ctx)?;
            let mut record = tx
                .workspace_archives()?
                .workspace_archive(&id)?
                .filter(|record| owner(&ctx, record) && record.revision == expected_revision)
                .ok_or(StorageError::ScopeDenied)?;
            match (&record.action, record.phase) {
                (
                    ArchiveAction::Restore { .. },
                    ArchivePhase::Prepared | ArchivePhase::AwaitingRestart,
                ) => {
                    if let Some(id) = &record.operation {
                        let current = tx
                            .operations()
                            .operation(id)?
                            .ok_or(StorageError::ScopeDenied)?;
                        tx.operations().transition(
                            id,
                            current.revision,
                            None,
                            OperationState::CancelledBeforeDispatch,
                            None,
                        )?;
                    }
                    record.revision += 1;
                    record.phase = ArchivePhase::Cancelled;
                    record.detail = Some("Workspace restore cancelled before shutdown.".into());
                    tx.workspace_archives()?
                        .put_workspace_archive(&record, Some(expected_revision))?;
                    emit(tx, &record, "WorkspaceRestoreCancelled", json!({}))?;
                }
                (ArchiveAction::Backup { .. }, ArchivePhase::Queued) => {
                    let operation = record.operation.as_ref().ok_or(StorageError::ScopeDenied)?;
                    let current = tx
                        .operations()
                        .operation(operation)?
                        .ok_or(StorageError::ScopeDenied)?;
                    let delivery = super::worker::decode(&current)?.delivery;
                    let result = tx.queue().cancel(&delivery)?;
                    if result.state == crate::ports::storage::QueueState::Cancelled {
                        super::worker::QueueHook.cancelled(tx, &result)?;
                    }
                }
                _ => return Err(StorageError::ScopeDenied),
            }
            Ok(())
        })
    }

    pub(super) fn restore(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &RestoreInput,
    ) -> Result<DomainDecision, Failure> {
        authority_in(tx, ctx)?;
        if ctx.store_id() != &input.expected_store_id
            || ctx.restore_epoch() != &input.expected_restore_epoch
        {
            return Err(permission());
        }
        if tx
            .workspace_archives()?
            .restore_awaiting_restart()?
            .is_some()
        {
            return Err(Refusal::new(DomainErrorCode::Conflict, "workspace-restore", None).into());
        }
        let mut proposal = tx
            .workspace_archives()?
            .workspace_archive(&input.proposal_id)?
            .filter(|proposal| owner(ctx, proposal) && trusted(proposal, tx.now_ms()))
            .ok_or_else(permission)?;
        if proposal
            .manifest
            .as_ref()
            .is_none_or(|manifest| !same_artifact(manifest, &input.backup))
        {
            return Err(permission());
        }
        let ArchiveAction::Restore { archive, migrated } = &proposal.action else {
            return Err(permission());
        };
        let supported = self
            .storage
            .shipped_migrations()?
            .last()
            .map_or(0, |m| m.version);
        let ready = migrated
            .as_ref()
            .map_or(archive.checkpoint.schema_version, |value| {
                value.checkpoint.schema_version
            });
        if supported != ready {
            return Err(
                Refusal::new(DomainErrorCode::SchemaMismatch, "workspace-migration", None).into(),
            );
        }
        let operation = OperationId(tx.new_id(IdKind::Operation));
        tx.operations().record_intent(NewOperation {
            id:operation.clone(),root_id:None,assignment_id:None,capability:RESTORE_OPERATION.into(),
            schema_version:i64::from(command.schema_revision().0),
            args_digest:command.fingerprint().as_str().into(),
            payload:json!({"proposal":proposal.id,"selection":input.backup,"expectedStore":input.expected_store_id,
                "expectedEpoch":input.expected_restore_epoch}),
        })?;
        apply_restoration_intent(tx, &mut proposal, operation.clone())?;
        Ok(DomainDecision::Accepted {
            operation_id: operation,
        })
    }

    pub(super) fn prepare_migration(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &MigrateInput,
    ) -> Result<Option<Prepared>, StorageError> {
        let plan = self.migration_candidate(ctx, env, input);
        Ok(Some(Prepared::new(plan)))
    }

    fn migration_candidate(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &MigrateInput,
    ) -> Result<PreparedMigration, StorageError> {
        self.migration_actor(ctx)?;
        let record = env.read(|read| {
            let archives = read.workspace_archives()?;
            let selected = match &input.proposal_id {
                Some(id) => archives.workspace_archive(id)?,
                None => archives.workspace_migration_plan(&input.migration_manifest.artifact_id)?,
            };
            selected
                .filter(|record| owner(ctx, record) && trusted(record, read.stamp().now_ms))
                .ok_or(StorageError::ScopeDenied)
        })?;
        if record
            .migration_manifest
            .as_ref()
            .is_none_or(|manifest| !same_artifact(manifest, &input.migration_manifest))
        {
            return Err(StorageError::ScopeDenied);
        }
        let (archive, migrated) = match (&record.action, &input.proposal_id) {
            (ArchiveAction::VerifyMigration { checkpoint }, None) => {
                let actual = self.storage.active_checkpoint()?;
                if actual.store_id != checkpoint.store_id
                    || actual.restore_epoch != checkpoint.restore_epoch
                    || actual.schema_version != input.expected_schema_revision.get()
                    || actual.schema_version != checkpoint.schema_version
                    || actual.schema_digest != checkpoint.schema_digest
                    || actual.migrations != checkpoint.migrations
                    || actual.migrations != self.storage.shipped_migrations()?
                {
                    return Err(StorageError::ScopeDenied);
                }
                return Ok(PreparedMigration {
                    record,
                    image: None,
                });
            }
            (ArchiveAction::Restore { archive, migrated }, Some(_)) => (archive, migrated),
            _ => return Err(StorageError::ScopeDenied),
        };
        if input.expected_schema_revision.get() != archive.checkpoint.schema_version {
            return Err(StorageError::ScopeDenied);
        }
        if migrated.is_some()
            || self
                .storage
                .shipped_migrations()?
                .last()
                .is_some_and(|migration| migration.version == archive.checkpoint.schema_version)
        {
            return Ok(PreparedMigration {
                record,
                image: None,
            });
        }
        let stage_id = self.ids.next_id(IdKind::Operation);
        let destination = self.files.create_stage(&stage_id)?;
        let source = self.files.staged_database(&record.stage_id)?;
        let cancellation = env.cancellation.cloned().unwrap_or_default();
        let checkpoint = self.storage.migrate_checkpoint(
            &source,
            archive,
            &destination,
            &self.app_build,
            &cancellation,
        )?;
        let database = self
            .files
            .staged_database_digest(&stage_id, &cancellation)?;
        let image = crate::ports::storage::MigratedWorkspaceImage {
            stage_id,
            checkpoint,
            database,
        };
        Ok(PreparedMigration {
            record,
            image: Some(image),
        })
    }

    pub(super) fn migrate(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<(DomainDecision, OperationId), Failure> {
        migration_authority(tx, ctx)?;
        let plan = command
            .prepared::<Result<PreparedMigration, StorageError>>()
            .ok_or(StorageError::ScopeDenied)?
            .as_ref()
            .map_err(Clone::clone)?;
        let mut actual = tx
            .workspace_archives()?
            .workspace_archive(&plan.record.id)?
            .filter(|record| owner(ctx, record) && trusted(record, tx.now_ms()))
            .ok_or_else(permission)?;
        if actual.revision != plan.record.revision {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "workspace-restore",
                Some(actual.id),
            )
            .into());
        }
        let changed = plan.image.is_some();
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let intent = tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: MIGRATE_OPERATION.into(),
            schema_version: i64::from(command.schema_revision().0),
            args_digest: command.fingerprint().as_str().into(),
            payload: json!({"planId":actual.id,"manifest":actual.migration_manifest}),
        })?;
        let attempt = tx.operations().begin_attempt(
            &operation,
            intent.revision,
            AttemptBinding {
                policy_revision: Some(ctx.policy_revision().get()),
                ..AttemptBinding::default()
            },
        )?;
        let dispatched = tx.operations().transition(
            &operation,
            intent.revision + 1,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )?;
        if let Some(image) = &plan.image {
            let ArchiveAction::Restore { migrated, .. } = &mut actual.action else {
                return Err(permission());
            };
            *migrated = Some(image.clone());
            let previous = actual.revision;
            actual.revision += 1;
            tx.workspace_archives()?
                .put_workspace_archive(&actual, Some(previous))?;
            let mut audited = actual.clone();
            audited.operation = Some(operation.clone());
            emit(
                tx,
                &audited,
                "WorkspaceCheckpointMigrated",
                json!({"schemaVersion":image.checkpoint.schema_version}),
            )?;
        }
        let result = OperationResult::WorkspaceMigrate {
            value: MutationSummary {
                id: actual.id,
                revision: rev(actual.revision),
                changed,
            },
        };
        tx.operations().transition(
            &operation,
            dispatched.revision,
            Some(&attempt.fence()),
            OperationState::ObservedComplete,
            Some(json!({"result":result})),
        )?;
        let decision = if changed {
            DomainDecision::Applied { result }
        } else {
            DomainDecision::NoChange { result }
        };
        Ok((decision, operation))
    }
}

fn migration_authority(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<(), StorageError> {
    if matches!(ctx.principal_kind(), crate::context::PrincipalKind::Host)
        && ctx.principal() == &tx.store().host_actor
        && ctx.store_id() == &tx.store().store_id
        && ctx.restore_epoch().get() == tx.store().restore_epoch
    {
        return Ok(());
    }
    authority_in(tx, ctx)
}

struct PreparedMigration {
    record: StoredWorkspaceArchive,
    image: Option<crate::ports::storage::MigratedWorkspaceImage>,
}

fn apply_restoration_intent(
    tx: &mut dyn WriteTransaction,
    record: &mut StoredWorkspaceArchive,
    operation: OperationId,
) -> Result<(), StorageError> {
    let previous = record.revision;
    record.revision += 1;
    record.operation = Some(operation);
    record.phase = ArchivePhase::AwaitingRestart;
    tx.workspace_archives()?
        .put_workspace_archive(record, Some(previous))?;
    emit(
        tx,
        record,
        "WorkspaceRestoreConfirmed",
        json!({"expectedEpoch":record.epoch}),
    )
}

fn emit(
    tx: &mut dyn WriteTransaction,
    archive: &StoredWorkspaceArchive,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event,
        actor: archive.owner.clone(),
        project_id: None,
        root_id: None,
        operation_id: archive.operation.clone(),
        aggregate_kind: "workspace_archive".into(),
        aggregate_id: archive.id.as_str().into(),
        aggregate_revision: archive.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}
