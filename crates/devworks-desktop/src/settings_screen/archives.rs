//! Native controls for the manual workspace archive service.
use crate::backend::{Reply, TaskOutput};
use crate::frame::{self, Negotiate};
use crate::screen::{Cx, Effect};
use crate::settings_ops::SettingsSlot;
use devworks_core::operations::{OperationInput, workspace as api};
use devworks_core::ports::storage::{
    ArchiveAction, ArchivePhase, Cancellation, RestoredWorkspace, StoredWorkspaceArchive,
};
use devworks_core::protocol::{Counter, Id, Revision};
use devworks_core::services::workspace_archive::{
    BACKUP_OPERATION, MIGRATE_OPERATION, RESTORE_OPERATION,
};
use eframe::egui;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Status {
    pub current: Option<StoredWorkspaceArchive>,
    pub restored: Option<RestoredWorkspace>,
    pub shipped_schema: i64,
}

#[derive(Default)]
pub(crate) struct Panel {
    open: bool,
    status: Status,
    folder: Option<(Id, i64, String)>,
    restore_plan: Option<StoredWorkspaceArchive>,
    confirmation: bool,
    cancel_review: Option<Cancellation>,
    poll: Option<Instant>,
    notice: Option<String>,
}

pub(crate) fn is_slot(slot: &SettingsSlot) -> bool {
    matches!(
        slot,
        SettingsSlot::WorkspaceStatus
            | SettingsSlot::WorkspaceFolder
            | SettingsSlot::WorkspaceReview
            | SettingsSlot::WorkspaceMigrationPlan
            | SettingsSlot::WorkspaceCancel
            | SettingsSlot::WorkspaceCommand
            | SettingsSlot::WorkspaceClose
    )
}

impl Panel {
    pub fn open(&mut self) {
        self.open = true;
        self.poll = Some(Instant::now());
    }

    pub fn window(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let changing = [
            SettingsSlot::WorkspaceFolder,
            SettingsSlot::WorkspaceReview,
            SettingsSlot::WorkspaceMigrationPlan,
            SettingsSlot::WorkspaceCancel,
            SettingsSlot::WorkspaceCommand,
            SettingsSlot::WorkspaceClose,
        ]
        .iter()
        .any(|slot| cx.waiting(slot));
        if self.poll.is_some_and(|when| Instant::now() >= when)
            && !changing
            && !cx.pending(&SettingsSlot::WorkspaceStatus)
        {
            self.poll = None;
            if let Some(client) = cx.client.as_deref_mut() {
                client.task(
                    SettingsSlot::WorkspaceStatus.into(),
                    "Reading workspace storage",
                    |b| {
                        b.workspace_archive_status()
                            .map(|status| TaskOutput::WorkspaceArchive(Box::new(status)))
                    },
                );
            }
        }
        let mut open = true;
        egui::Window::new("Workspace backup and restore")
            .open(&mut open)
            .default_width(515.0)
            .show(ctx, |ui| {
                self.ui(cx, ui, changing);
            });
        self.open = open;
        if self.poll.is_some() {
            ctx.request_repaint_after(Duration::from_millis(450));
        }
    }

    fn ui(&mut self, cx: &mut Cx, ui: &mut egui::Ui, changing: bool) {
        ui.label("A backup includes workspace records and their managed files. Reconnect external accounts after a restore if prompted.");
        if self.status.shipped_schema > 0 {
            ui.label(format!(
                "Supported storage revision {}",
                self.status.shipped_schema
            ));
        }
        if let Some(restored) = &self.status.restored {
            ui.label(format!(
                "Restored checkpoint {} from storage revision {}. Current restore epoch {}.",
                restored.source.checkpoint.sequence,
                restored.source.checkpoint.schema_version,
                restored.epoch
            ));
            ui.label("Scheduled jobs and installed packages require explicit reactivation. Use Continue for the work you want to resume.");
            if restored.unresolved_effects > 0 {
                ui.label(format!(
                    "{} previous external operations still require reconciliation.",
                    restored.unresolved_effects
                ));
            }
        }
        let current = self.status.current.clone();
        let queued = current
            .as_ref()
            .is_some_and(|row| row.phase == ArchivePhase::Queued);
        ui.add_enabled_ui(!changing && !queued, |ui| {
            if ui.button("Verify current migration ledger").clicked()
                && let Some(client) = cx.client.as_deref_mut()
            {
                let received = client.control_epoch();
                client.task(
                    SettingsSlot::WorkspaceMigrationPlan.into(),
                    "Checking current storage schema",
                    move |b| {
                        let plan = b.workspace_migration_plan(received)?;
                        Ok(TaskOutput::WorkspaceArchive(Box::new(Status {
                            current: Some(plan),
                            ..Status::default()
                        })))
                    },
                );
            }
            if ui.button("Choose backup destination…").clicked() {
                self.choose(cx, false);
            }
            if let Some((grant, sequence, display)) = &self.folder {
                ui.label(format!("{} · checkpoint {}", display, sequence));
                if ui.button("Create workspace backup").clicked() {
                    let grant = grant.clone();
                    let expected = Counter::new(*sequence).expect("verified sequence");
                    self.submit(
                        cx,
                        BACKUP_OPERATION,
                        api::Input::Backup(api::BackupInput {
                            destination_grant_id: grant,
                            expected_store_sequence: expected,
                        }),
                    );
                    self.folder = None;
                }
            }
            if ui.button("Choose backup to restore…").clicked() {
                self.choose(cx, true);
            }
            if let Some(plan) = self.restore_plan.clone().or_else(|| {
                current
                    .clone()
                    .filter(|value| value.phase == ArchivePhase::Prepared)
            }) {
                self.restore_review(cx, ui, plan);
            }
        });
        if let Some(row) = current {
            if matches!(
                row.phase,
                ArchivePhase::Queued | ArchivePhase::AwaitingRestart
            ) && ui
                .add_enabled(!changing, egui::Button::new("Cancel storage action"))
                .clicked()
                && let Some(client) = cx.client.as_deref_mut()
            {
                let received = client.control_epoch();
                let id = row.id.clone();
                let revision = row.revision;
                client.task(
                    SettingsSlot::WorkspaceCancel.into(),
                    "Cancelling storage action",
                    move |b| {
                        b.cancel_workspace_archive(&id, revision, received)?;
                        Ok(TaskOutput::Refreshed)
                    },
                );
            }
            match (row.phase, &row.action) {
                (ArchivePhase::Queued, ArchiveAction::Backup { .. }) => {
                    ui.spinner();
                    ui.label("Copying and verifying the workspace backup.");
                }
                (ArchivePhase::Published, ArchiveAction::Backup { destination, .. }) => {
                    ui.label(format!(
                        "Backup saved in {}",
                        destination
                            .path
                            .join(format!("devworks-backup-{}", row.id.as_str()))
                            .display()
                    ));
                }
                (ArchivePhase::AwaitingRestart, ArchiveAction::Restore { .. }) => {
                    ui.label("Restore is approved. Close DevWorks from here and reopen it to apply the backup.");
                    if ui
                        .add_enabled(!changing, egui::Button::new("Close DevWorks to restore"))
                        .clicked()
                        && let Some(client) = cx.client.as_deref_mut()
                    {
                        let received = client.control_epoch();
                        client.task(
                            SettingsSlot::WorkspaceClose.into(),
                            "Preparing restore shutdown",
                            move |b| {
                                b.request_workspace_restore_shutdown(&row.id, received)?;
                                Ok(TaskOutput::Refreshed)
                            },
                        );
                    }
                }
                (ArchivePhase::Failed | ArchivePhase::Cancelled | ArchivePhase::Unknown, _) => {
                    ui.label(
                        row.detail
                            .as_deref()
                            .unwrap_or("The outcome needs attention."),
                    );
                }
                _ => {}
            }
        }
        if let Some(cancel) = &self.cancel_review
            && cx.pending(&SettingsSlot::WorkspaceReview)
            && ui.button("Cancel verification").clicked()
        {
            cancel.cancel();
        }
        if let Some(message) = &self.notice {
            ui.label(message);
        }
    }

    fn choose(&mut self, cx: &mut Cx, restore: bool) {
        let Some(backend) = cx.backend() else { return };
        let received = backend.control_epoch();
        let Some(path) = cx.dialogs.pick_folder(None) else {
            return;
        };
        let Some(client) = cx.client.as_deref_mut() else {
            return;
        };
        self.notice = None;
        if restore {
            let cancel = Cancellation::default();
            self.cancel_review = Some(cancel.clone());
            self.confirmation = false;
            self.restore_plan = None;
            client.task(
                SettingsSlot::WorkspaceReview.into(),
                "Verifying backup and managed files",
                move |b| {
                    let record = b.workspace_restore_plan(&path, received, &cancel)?;
                    Ok(TaskOutput::WorkspaceArchive(Box::new(Status {
                        current: Some(record),
                        ..Status::default()
                    })))
                },
            );
        } else {
            let display = path.display().to_string();
            self.folder = None;
            if client.task(
                SettingsSlot::WorkspaceFolder.into(),
                "Registering backup destination",
                move |b| {
                    let (grant, sequence) = b.workspace_backup_folder(&path, received)?;
                    Ok(TaskOutput::WorkspaceBackupFolder(grant, sequence))
                },
            ) {
                self.notice = Some(display);
            }
        }
    }

    fn restore_review(&mut self, cx: &mut Cx, ui: &mut egui::Ui, plan: StoredWorkspaceArchive) {
        let ArchiveAction::Restore { archive, migrated } = &plan.action else {
            return;
        };
        if plan.phase != ArchivePhase::Prepared {
            return;
        }
        ui.separator();
        ui.label(format!(
            "Verified checkpoint {} · storage revision {} · {} managed files",
            archive.checkpoint.sequence,
            archive.checkpoint.schema_version,
            archive.checkpoint.blobs.len()
                + archive
                    .checkpoint
                    .packages
                    .iter()
                    .map(|package| package.files.len())
                    .sum::<usize>(),
        ));
        if archive.checkpoint.schema_version < self.status.shipped_schema && migrated.is_none() {
            if let Some(migration_manifest) = plan.migration_manifest
                && ui.button("Migrate the staged backup").clicked()
            {
                self.submit(
                    cx,
                    MIGRATE_OPERATION,
                    api::Input::Migrate(api::MigrateInput {
                        migration_manifest,
                        expected_schema_revision: Revision::new(archive.checkpoint.schema_version)
                            .expect("verified revision"),
                        proposal_id: Some(plan.id),
                    }),
                );
            }
            return;
        }
        ui.checkbox(
            &mut self.confirmation,
            "Replace this workspace with the verified checkpoint",
        );
        if ui
            .add_enabled(self.confirmation, egui::Button::new("Approve restore"))
            .clicked()
            && let Some(manifest) = plan.manifest
        {
            self.submit(
                cx,
                RESTORE_OPERATION,
                api::Input::Restore(api::RestoreInput {
                    backup: manifest,
                    expected_store_id: plan.store,
                    expected_restore_epoch: Counter::new(plan.epoch).expect("verified epoch"),
                    proposal_id: plan.id,
                }),
            );
            self.confirmation = false;
        }
    }

    fn submit(&mut self, cx: &mut Cx, operation: &'static str, input: api::Input) {
        if let Some(client) = cx.client.as_deref_mut() {
            let frame = frame::build(
                client.backend().as_ref(),
                Negotiate::Initial,
                Some(OperationInput::Workspace(input)),
            );
            client.workspace_archive(
                SettingsSlot::WorkspaceCommand.into(),
                "Submitting workspace storage action",
                operation,
                frame,
            );
        }
    }

    pub fn apply(&mut self, cx: &mut Cx, slot: SettingsSlot, reply: Reply) {
        match (slot, reply) {
            (
                SettingsSlot::WorkspaceFolder,
                Reply::Task(Ok(TaskOutput::WorkspaceBackupFolder(grant, sequence))),
            ) => {
                self.folder = Some((grant, sequence, self.notice.take().unwrap_or_default()));
            }
            (
                SettingsSlot::WorkspaceReview,
                Reply::Task(Ok(TaskOutput::WorkspaceArchive(status))),
            ) => {
                self.cancel_review = None;
                self.restore_plan = status.current;
            }
            (
                SettingsSlot::WorkspaceStatus,
                Reply::Task(Ok(TaskOutput::WorkspaceArchive(status))),
            ) => {
                self.status = *status;
                if let Some(fresh) = &self.status.current
                    && self
                        .restore_plan
                        .as_ref()
                        .is_some_and(|prior| prior.id == fresh.id)
                {
                    self.restore_plan = Some(fresh.clone());
                }
                if self
                    .status
                    .current
                    .as_ref()
                    .is_some_and(|entry| entry.phase == ArchivePhase::Queued)
                {
                    self.poll = Some(Instant::now() + Duration::from_millis(700));
                }
            }
            (SettingsSlot::WorkspaceCommand, Reply::Command(reply)) => {
                self.notice = match reply {
                    Ok(devworks_core::protocol::CommandReply::Recorded { receipt, .. }) => {
                        match &receipt.decision {
                            devworks_core::protocol::results::DomainDecision::Refused { fault } => {
                                Some(format!(
                                    "Storage action refused ({:?}). Review the selection and try again.",
                                    fault.code
                                ))
                            }
                            devworks_core::protocol::results::DomainDecision::Applied {
                                result: devworks_core::protocol::results::OperationResult::WorkspaceMigrate { .. },
                            } => Some("Staged backup migrated. Review it before approving restore.".into()),
                            devworks_core::protocol::results::DomainDecision::NoChange {
                                result: devworks_core::protocol::results::OperationResult::WorkspaceMigrate { .. },
                            } => Some("Storage migration ledger verified.".into()),
                            _ => None,
                        }
                    }
                    Ok(_) => Some("Confirm the outcome of the original storage request.".into()),
                    Err(error) => Some(format!("Storage action failed ({error:?}).")),
                };
                self.poll = Some(Instant::now());
            }
            (
                SettingsSlot::WorkspaceMigrationPlan,
                Reply::Task(Ok(TaskOutput::WorkspaceArchive(status))),
            ) => {
                if let Some(plan) = status.current
                    && let ArchiveAction::VerifyMigration { checkpoint } = plan.action
                    && let Some(migration_manifest) = plan.migration_manifest
                {
                    self.submit(
                        cx,
                        MIGRATE_OPERATION,
                        api::Input::Migrate(api::MigrateInput {
                            migration_manifest,
                            expected_schema_revision: Revision::new(checkpoint.schema_version)
                                .expect("verified revision"),
                            proposal_id: None,
                        }),
                    );
                }
            }
            (SettingsSlot::WorkspaceCancel, Reply::Task(Ok(TaskOutput::Refreshed))) => {
                self.poll = Some(Instant::now());
            }
            (SettingsSlot::WorkspaceClose, Reply::Task(Ok(TaskOutput::Refreshed))) => {
                cx.effects.push(Effect::CloseForWorkspaceRestore);
            }
            (_, Reply::Task(Err(error))) => {
                self.notice = Some(error);
            }
            (_, reply) => cx.unhandled(&"workspace storage", reply),
        }
    }
}
