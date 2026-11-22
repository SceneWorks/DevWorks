//! Native update controls. The sidebar and Settings share the same observed update state.
use crate::backend::{Client, Reply, Slot, TaskOutput};
use crate::frame::{self, Negotiate};
use devworks_core::operations::{OperationInput, workspace};
use devworks_core::ports::update::DownloadedUpdate;
use devworks_core::protocol::records::UpdateChannel;
use devworks_core::services::update::{Progress, UpdateStatus};
use eframe::egui;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum UpdateSlot {
    ObserveAppUpdate,
    DiscoverAppUpdate,
    SetAppReleaseChannel,
    ApproveAppRestart,
}
crate::backend::screen_slot!(UpdateSlot, Updates);

#[derive(Default)]
pub(super) struct Panel {
    pub open: bool,
    status: Option<UpdateStatus>,
    confirm: Option<DownloadedUpdate>,
    next_check: Option<Instant>,
    next_status: Option<Instant>,
    error: Option<String>,
}

impl Panel {
    pub fn caption(&self) -> String {
        let Some(status) = &self.status else {
            return "Updates".into();
        };
        match &status.progress {
            Progress::Checking => "Checking for updates…".into(),
            Progress::Downloading { version, .. } => format!("Downloading {version}…"),
            Progress::Idle => match &status.current {
                Some(update) => format!("DevWorks {} available", update.record.version.0),
                None => "Updates".into(),
            },
        }
    }

    pub fn tick(&mut self, client: Option<&mut Client>, ctx: &egui::Context) {
        let Some(client) = client else { return };
        let now = Instant::now();
        let timely = |at: Option<Instant>| at.is_none_or(|at| now >= at);
        if timely(self.next_check) {
            self.check(client, false);
            self.next_check = Some(now + Duration::from_secs(30));
        }
        let busy = client.is_pending(&Slot::Updates(UpdateSlot::DiscoverAppUpdate));
        if timely(self.next_status)
            && !client.is_pending(&Slot::Updates(UpdateSlot::ObserveAppUpdate))
        {
            client.task(
                UpdateSlot::ObserveAppUpdate.into(),
                "Reading app update status",
                |backend| {
                    backend
                        .update_status()
                        .map(|status| TaskOutput::AppUpdateStatus(Box::new(status)))
                },
            );
            self.next_status = Some(
                now + if self.open || busy {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs(15)
                },
            );
        }
        if let Some(next) = self.next_status.into_iter().chain(self.next_check).min() {
            ctx.request_repaint_after(
                next.saturating_duration_since(now)
                    .max(Duration::from_millis(100)),
            );
        }
    }

    fn check(&mut self, client: &mut Client, manual: bool) {
        let slot = UpdateSlot::DiscoverAppUpdate;
        if client.is_pending(&Slot::Updates(slot.clone())) {
            return;
        }
        client.network_task(slot.into(), "Checking DevWorks releases", move |backend| {
            backend.poll_updates(manual)?;
            Ok(TaskOutput::Refreshed)
        });
        self.next_status = None;
    }

    pub fn apply(&mut self, slot: UpdateSlot, reply: Reply) -> bool {
        match reply {
            Reply::Task(Ok(TaskOutput::AppUpdateStatus(status))) => self.status = Some(*status),
            Reply::Task(Ok(TaskOutput::Refreshed)) => {
                self.error = None;
                self.next_status = None;
                if slot == UpdateSlot::SetAppReleaseChannel {
                    self.next_check = None;
                }
                return slot == UpdateSlot::ApproveAppRestart;
            }
            Reply::Task(Err(error)) if slot != UpdateSlot::ObserveAppUpdate => {
                self.error = Some(error);
                self.next_status = None;
            }
            _ => {}
        }
        false
    }

    pub fn window(&mut self, client: Option<&mut Client>, ctx: &egui::Context, version: &str) {
        if !self.open {
            self.confirm = None;
            return;
        }
        let mut client = client;
        let mut open = true;
        egui::Window::new("Updates")
            .open(&mut open)
            .collapsible(false)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(format!("Installed DevWorks {version}"));
                if let Some(status) = self.status.as_ref() {
                    show_status(ui, status);
                    let mut prerelease = matches!(status.channel, UpdateChannel::Prerelease);
                    if ui
                        .checkbox(&mut prerelease, "Include prerelease versions")
                        .changed()
                        && let Some(client) = client.as_deref_mut()
                    {
                        let epoch = client.control_epoch();
                        client.task(
                            UpdateSlot::SetAppReleaseChannel.into(),
                            "Saving release channel",
                            move |backend| {
                                let channel = if prerelease {
                                    UpdateChannel::Prerelease
                                } else {
                                    UpdateChannel::Stable
                                };
                                backend.set_update_channel(channel, epoch)?;
                                Ok(TaskOutput::Refreshed)
                            },
                        );
                    }

                    if let Some(update) = &status.current
                        && ui
                            .add_enabled(status.can_install, egui::Button::new("Review update"))
                            .clicked()
                    {
                        self.confirm = Some(update.clone());
                    }

                    if matches!(status.progress, Progress::Downloading { .. })
                        && ui.button("Cancel download").clicked()
                        && let Some(client) = client.as_deref()
                    {
                        client.backend().cancel_update_download();
                    }
                }
                if ui.button("Check now").clicked()
                    && let Some(client) = client.as_deref_mut()
                {
                    self.check(client, true);
                }
                if let Some(error) = &self.error {
                    ui.label(error);
                }
            });
        self.open = open;
        if let Some(update) = self.confirm.clone() {
            let mut dismissed = false;
            let confirmed = egui::Modal::new(egui::Id::unique("confirm-app-update")).show(ctx, |ui| {
                let stage = &update.stage;
                ui.heading(format!("Install DevWorks {} and restart?", update.record.version.0));
                ui.label("DevWorks will save its state and stop active work. Select Continue after restart to resume held work.");
                ui.label(format!("Target {}", stage.manifest.target));
                ui.label(format!("Build {}", stage.manifest.build));
                ui.label(format!("Manifest {}", stage.manifest_digest));
                if update.record.migration_required {
                    let next = stage.manifest.migrations.last().map_or(0, |item| item.version);
                    ui.label(format!("This release updates the workspace to storage revision {next}. The previous app can return only if it can still write the resulting data safely."));
                }
                ui.horizontal(|ui| {
                    dismissed = ui.button("Keep current version").clicked();
                    ui.button("Approve install and restart").clicked()
                }).inner
            }).inner;
            if confirmed || dismissed || !self.open {
                self.confirm = None;
            }
            if confirmed && let Some(client) = client {
                let epoch = client.control_epoch();
                let input = workspace::ApproveInstallInput {
                    update_id: update.record.id,
                    expected_revision: update.record.revision,
                    binary_digest: update.record.digest,
                    proposal_id: update.proposal_id,
                };
                client.task(
                    UpdateSlot::ApproveAppRestart.into(),
                    "Recording exact install approval",
                    move |backend| {
                        let frame = frame::build(
                            backend,
                            Negotiate::Initial,
                            Some(OperationInput::Workspace(workspace::Input::ApproveInstall(
                                input.clone(),
                            ))),
                        );
                        backend.approve_app_update(&frame, &input.proposal_id, epoch)?;
                        Ok(TaskOutput::Refreshed)
                    },
                );
            }
        }
    }
}

fn show_status(ui: &mut egui::Ui, status: &UpdateStatus) {
    if status.build != "development" {
        ui.label(format!("Installed build {}", status.build));
    }
    match &status.progress {
        Progress::Checking => {
            ui.spinner();
            ui.label("Checking DevWorks releases.");
        }
        Progress::Downloading {
            version,
            received,
            total,
        } => {
            ui.label(format!("Downloading DevWorks {version}"));
            if *total > 0 {
                ui.add(
                    egui::ProgressBar::new((*received as f32 / *total as f32).min(1.0))
                        .show_percentage(),
                );
            } else {
                ui.spinner();
            }
        }
        Progress::Idle => {
            if let Some(update) = &status.current {
                ui.label(format!(
                    "DevWorks {} is staged and its signature is verified.",
                    update.record.version.0
                ));
                if !status.can_install && status.issue.is_none() {
                    ui.label("A fresh release check is required before installation.");
                }
            } else if status.checked_at_ms.is_some() && status.issue.is_none() {
                ui.label("No applicable newer app release was found.");
            }
        }
    }
    if let Some(time) = status
        .checked_at_ms
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
    {
        let local = time.to_zoned(jiff::tz::TimeZone::system());
        ui.label(format!(
            "Last checked {}",
            local.strftime("%b %d, %Y at %H:%M")
        ));
    }
    if let Some(issue) = &status.issue {
        ui.label(issue);
    }
    if let Some(detail) = &status.last_detail {
        ui.separator();
        ui.label(detail);
    }
}
