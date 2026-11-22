//! Workspace-wide export goes through the existing gateway, journal, package and artifact owners.
mod restore;
mod worker;
pub use restore::{MIGRATE_OPERATION, RESTORE_OPERATION};
pub(crate) use worker::QueueHook;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::{OperationInput, workspace as ops};
use crate::ports::storage::{
    ApplicationStore, ArchiveAction, ArchivePhase, NewEvent, NewOperation, NewQueueEntry,
    StorageError, StoredBackupFolder, StoredWorkspaceArchive, WorkspaceArchiveStorage,
    WorkspaceCheckpoint, WriteOptions, WriteTransaction, run_write,
};
use crate::ports::workspace_archive::WorkspaceArchiveFiles;
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
};
use crate::protocol::results::{DomainCommandReply, DomainDecision};
use crate::protocol::{CommandReply, DomainErrorCode, EventId, Id, OperationId};
use crate::services::artifacts::ArtifactService;
use crate::services::kernel::{Failure, Refusal, decided};
use crate::services::{approvals, conventions::control, packages::PackageService};
use serde_json::json;
use std::{path::Path, sync::Arc};

pub const EFFECT_CONSUMER: &str = "workspace_archive";
pub const BACKUP_OPERATION: &str = "workspace.backup";
pub const OPERATIONS: [&str; 3] = [BACKUP_OPERATION, RESTORE_OPERATION, MIGRATE_OPERATION];
const FOLDER_GRANT_MS: i64 = 30 * 60 * 1_000;

pub struct ArchivePorts {
    pub storage: Arc<dyn WorkspaceArchiveStorage>,
    pub files: Arc<dyn WorkspaceArchiveFiles>,
    pub app_build: String,
}

pub struct WorkspaceArchiveService {
    pub(super) store: Arc<dyn ApplicationStore>,
    storage: Arc<dyn WorkspaceArchiveStorage>,
    pub(super) files: Arc<dyn WorkspaceArchiveFiles>,
    pub(super) artifacts: Arc<ArtifactService>,
    packages: Arc<PackageService>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    pub(super) app_build: String,
}

impl WorkspaceArchiveService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        ports: ArchivePorts,
        artifacts: Arc<ArtifactService>,
        packages: Arc<PackageService>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            storage: ports.storage,
            files: ports.files,
            artifacts,
            packages,
            ids,
            clock,
            app_build: ports.app_build,
        }
    }

    pub fn shipped_schema(&self) -> Result<i64, StorageError> {
        self.storage
            .shipped_migrations()?
            .last()
            .map(|entry| entry.version)
            .ok_or(StorageError::ViewUnavailable)
    }

    /// The native folder chooser calls this through the trusted host. Generic command input only
    /// names the resulting one-operation grant; it cannot add or choose an OS path.
    pub fn register_folder(
        &self,
        ctx: &InvocationContext,
        path: &Path,
    ) -> Result<(Id, i64), StorageError> {
        human(ctx)?;
        let folder = self.files.bind_external_folder(path)?;
        let ctx = ctx.clone();
        run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            authority_in(tx, &ctx)?;
            let grant = StoredBackupFolder {
                id: tx.new_id(IdKind::Grant),
                owner: ctx.principal().clone(),
                store: tx.store().store_id.clone(),
                epoch: tx.store().restore_epoch,
                folder,
                expires_at_ms: tx.now_ms().saturating_add(FOLDER_GRANT_MS),
                redeemed_by: None,
            };
            tx.workspace_archives()?.register_backup_folder(&grant)?;
            Ok((grant.id, tx.committed_sequence()?))
        })
    }

    fn prepare_backup(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        input: &ops::BackupInput,
    ) -> Result<PreparedBackup, Failure> {
        human(ctx)?;
        let grant = env.read(|s| {
            let grant = s
                .workspace_archives()?
                .backup_folder(&input.destination_grant_id)?;
            grant
                .filter(|g| grant_matches(ctx, g, s.stamp().now_ms))
                .ok_or(StorageError::ScopeDenied)
        })?;
        let id = self.ids.next_id(IdKind::Operation);
        let path = self.files.create_stage(&id)?;
        let cancellation = env.cancellation.cloned().unwrap_or_default();
        let captured = self.packages.with_checkpoint_files(|| {
            let Some(checkpoint) = self.storage.capture_checkpoint(
                input.expected_store_sequence.get(),
                &path,
                &cancellation,
            )?
            else {
                return Ok(None);
            };
            if checkpoint.store_id != *ctx.store_id()
                || checkpoint.restore_epoch != ctx.restore_epoch().get()
                || env.stopped(self.clock.now_ms().get())
            {
                return Err(StorageError::Cancelled);
            }
            self.files.stage_packages(&id, &checkpoint, &cancellation)?;
            Ok(Some(checkpoint))
        });
        let checkpoint = match captured {
            Ok(Some(checkpoint)) => checkpoint,
            Ok(None) => {
                let _ = self.files.discard_stage(&id);
                return Err(Refusal::new(
                    DomainErrorCode::SourceChanged,
                    "workspace-checkpoint",
                    None,
                )
                .retry(crate::protocol::RetryAdvice::RefreshAndNewRequest)
                .into());
            }
            Err(error) => {
                let _ = self.files.discard_stage(&id);
                return Err(error.into());
            }
        };
        Ok(PreparedBackup {
            id,
            checkpoint,
            grant,
        })
    }

    fn backup(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &ops::BackupInput,
    ) -> Result<DomainDecision, Failure> {
        authority_in(tx, ctx)?;
        let prepared = command
            .prepared::<Result<PreparedBackup, Failure>>()
            .ok_or_else(|| StorageError::InvalidRecord("backup was not prepared".into()))?
            .as_ref()
            .map_err(Clone::clone)?;
        let grant = tx
            .workspace_archives()?
            .backup_folder(&input.destination_grant_id)?
            .filter(|grant| grant_matches(ctx, grant, tx.now_ms()))
            .ok_or(StorageError::ScopeDenied)?;
        if grant != prepared.grant
            || prepared.checkpoint.sequence != input.expected_store_sequence.get()
        {
            return Err(
                Refusal::new(DomainErrorCode::SourceChanged, "workspace-checkpoint", None).into(),
            );
        }
        let operation = OperationId(prepared.id.clone());
        let deliver = tx.new_id(IdKind::QueueEntry);
        let owner = ctx.principal().clone();
        let controls = control::capture(tx, ctx)?;
        let job = worker::Job {
            actor: owner.clone(),
            actor_kind: ctx.principal_kind().as_str().into(),
            store_id: tx.store().store_id.clone(),
            restore_epoch: tx.store().restore_epoch,
            source_context: approvals::save_source_context(ctx),
            fences: command.authorization().fences().to_vec(),
            delivery: deliver.clone(),
            controls: controls.roots,
            assignment_generation: controls.assignment_generation,
        };
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: BACKUP_OPERATION.into(),
            schema_version: i64::from(command.schema_revision().0),
            args_digest: command.fingerprint().as_str().into(),
            payload: json!(job),
        })?;
        for blob in &prepared.checkpoint.blobs {
            match crate::services::artifacts::pin_checkpoint_blob(tx, &operation.0, blob) {
                Err(StorageError::ScopeDenied) => {
                    return Err(Refusal::new(
                        DomainErrorCode::SourceChanged,
                        "workspace-artifacts",
                        None,
                    )
                    .into());
                }
                result => result?,
            }
        }
        tx.workspace_archives()?
            .redeem_backup_folder(&grant.id, &operation)?;
        let now = tx.now_ms();
        let record = StoredWorkspaceArchive {
            id: operation.0.clone(),
            stage_id: prepared.id.clone(),
            owner: owner.clone(),
            store: grant.store,
            epoch: grant.epoch,
            created_at_ms: now,
            expires_at_ms: None,
            revision: 1,
            phase: ArchivePhase::Queued,
            action: ArchiveAction::Backup {
                checkpoint: prepared.checkpoint.clone(),
                destination: grant.folder,
            },
            operation: Some(operation.clone()),
            manifest: None,
            migration_manifest: None,
            completion: None,
            detail: None,
        };
        tx.workspace_archives()?
            .put_workspace_archive(&record, None)?;
        let event_id = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event_id.clone(),
            actor: owner,
            project_id: None,
            root_id: None,
            operation_id: Some(operation.clone()),
            aggregate_kind: EFFECT_CONSUMER.into(),
            aggregate_id: operation.0.as_str().into(),
            aggregate_revision: 1,
            kind: "WorkspaceBackupRequested".into(),
            payload: json!({"checkpoint":prepared.checkpoint.sequence}),
        })?;
        tx.queue().enqueue(NewQueueEntry {
            id: deliver,
            consumer: EFFECT_CONSUMER.into(),
            event_id,
            root_id: None,
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms: now,
            payload: json!({}),
        })?;
        Ok(DomainDecision::Accepted {
            operation_id: operation,
        })
    }
}

#[derive(Clone)]
struct PreparedBackup {
    id: Id,
    checkpoint: WorkspaceCheckpoint,
    grant: StoredBackupFolder,
}

fn grant_matches(ctx: &InvocationContext, grant: &StoredBackupFolder, now: i64) -> bool {
    grant.owner == *ctx.principal()
        && grant.store == *ctx.store_id()
        && grant.epoch == ctx.restore_epoch().get()
        && grant.redeemed_by.is_none()
        && grant.expires_at_ms > now
}

fn human(ctx: &InvocationContext) -> Result<(), StorageError> {
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.session_id().is_some()
        || ctx.project_id().is_some()
        || ctx.root_id().is_some()
        || ctx.assignment_id().is_some()
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

fn authority_in(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<(), StorageError> {
    human(ctx)?;
    if tx.store().store_id != *ctx.store_id()
        || tx.store().restore_epoch != ctx.restore_epoch().get()
        || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

pub(crate) fn restore_effect_held(
    tx: &mut dyn WriteTransaction,
    root: Option<&crate::protocol::RootId>,
    capability: Option<&str>,
) -> Result<bool, StorageError> {
    if tx.store().restore_epoch == 0 {
        return Ok(false);
    }
    match tx.workspace_archives() {
        Ok(read) => read.restore_effect_held(root, capability),
        Err(StorageError::ViewUnavailable) => Ok(false),
        Err(error) => Err(error),
    }
}

impl CommandHandler for WorkspaceArchiveService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        if !ctx.workspace_archive_matches(command.input()) {
            return Ok(None);
        }
        match command.input() {
            OperationInput::Workspace(ops::Input::Backup(input)) => {
                Ok(Some(Prepared::new(self.prepare_backup(ctx, env, input))))
            }
            OperationInput::Workspace(ops::Input::Migrate(input)) => {
                self.prepare_migration(ctx, env, input)
            }
            _ => Ok(None),
        }
    }

    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
        let Some(prepared) = prepared else { return };
        if matches!(reply, Some(CommandReply::CommandStatusUnknown { .. })) {
            return;
        }
        if let Some(Ok(backup)) = prepared.get::<Result<PreparedBackup, Failure>>()
            && !matches!(
                reply,
                Some(CommandReply::Recorded { receipt, .. }) if matches!(&receipt.decision,
                    DomainDecision::Accepted {operation_id} if operation_id.0 == backup.id)
            )
        {
            let _ = self.files.discard_stage(&backup.id);
        }
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        if !ctx.workspace_archive_matches(command.input()) {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::PermissionDenied,
                    "workspace-archive-confirmation",
                    None,
                )
                .into()),
            );
        }
        let result = match command.input() {
            OperationInput::Workspace(ops::Input::Backup(input)) => {
                self.backup(ctx, command, tx, input)
            }
            OperationInput::Workspace(ops::Input::Restore(input)) => {
                self.restore(ctx, command, tx, input)
            }
            OperationInput::Workspace(ops::Input::Migrate(_)) => {
                return match self.migrate(ctx, command, tx) {
                    Ok((result, operation)) => {
                        let mut decision = decided(tx, Ok(result))?;
                        decision.operation_id = Some(operation);
                        Ok(decision)
                    }
                    Err(error) => decided(tx, Err(error)),
                };
            }
            _ => Err(StorageError::ScopeDenied.into()),
        };
        let operation = match &result {
            Ok(DomainDecision::Accepted { operation_id }) => Some(operation_id.clone()),
            _ => None,
        };
        let mut decision = decided(tx, result)?;
        decision.operation_id = operation;
        Ok(decision)
    }
}
