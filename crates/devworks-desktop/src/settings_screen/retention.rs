//! Workspace retention review in Settings. The gateway owns eligibility and deletion.
use crate::backend::Reply;
use crate::frame::{self, Negotiate};
use crate::ops;
use crate::screen::Cx;
use crate::settings_ops::SettingsSlot;
use devworks_core::operations::{OperationInput, maintenance as api};
use devworks_core::ports::storage::RetentionKind;
use devworks_core::protocol::Id;
use devworks_core::protocol::records::{PruneHold, PruneInspection, PruneKind, PrunePhase};
use devworks_core::protocol::results::OperationResult;
use eframe::egui::{self, Ui};
use std::time::{Duration, Instant};

const TARGETS: [(RetentionKind, &str); 4] = [
    (RetentionKind::Diagnostics, "Diagnostics"),
    (RetentionKind::IndexRuns, "Index runs"),
    (RetentionKind::Messages, "Operational messages"),
    (RetentionKind::Sessions, "Closed operational sessions"),
];

#[derive(Default)]
pub(crate) struct Panel {
    open: bool,
    target: usize,
    days: Option<u16>,
    max_items: Option<u16>,
    id: Option<Id>,
    detail: Option<PruneInspection>,
    confirm: bool,
    busy: bool,
    poll: Option<Instant>,
    notice: Option<String>,
}

pub(crate) fn is_slot(slot: &SettingsSlot) -> bool {
    matches!(
        slot,
        SettingsSlot::RetentionPreview
            | SettingsSlot::RetentionApply(_)
            | SettingsSlot::RetentionInspect
            | SettingsSlot::RetentionCancel
    )
}

impl Panel {
    pub(crate) fn open(&mut self) {
        self.open = true;
        if self.id.is_some() && self.detail.is_none() && !self.busy {
            self.poll = Some(Instant::now());
        }
    }

    pub(crate) fn preview_id(&self) -> Option<String> {
        self.id.as_ref().map(|id| id.as_str().to_owned())
    }

    pub(crate) fn restore_preview(&mut self, id: Option<&str>) {
        self.id = id.and_then(frame::id);
    }

    pub(crate) fn restore(&mut self, id: &str) {
        self.id = frame::id(id);
        self.busy = self.id.is_some();
        self.open();
    }

    pub(crate) fn window(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("Operational retention")
            .open(&mut open)
            .default_width(580.0)
            .show(ctx, |ui| self.ui(cx, ui));
        self.open = open;
        if self.poll.is_some_and(|when| Instant::now() >= when) && !self.busy {
            self.inspect(cx);
        }
        if self.poll.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let changing = !self.busy && self.poll.is_none();
        ui.add_enabled_ui(changing, |ui| {
            let previous_target = self.target;
            egui::ComboBox::from_label("Records")
                .selected_text(TARGETS[self.target].1)
                .show_ui(ui, |ui| {
                    for (i, (_, label)) in TARGETS.iter().enumerate() {
                        ui.selectable_value(&mut self.target, i, *label);
                    }
                });
            let days = ui.add(
                egui::DragValue::new(self.days.get_or_insert(30))
                    .range(1..=500)
                    .suffix(" days old"),
            );
            days.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Slider, true, "Minimum age"));
            let limit = ui.add(
                egui::DragValue::new(self.max_items.get_or_insert(50))
                    .range(1..=500)
                    .prefix("Up to ")
                    .suffix(" records"),
            );
            limit.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Slider, true, "Maximum records")
            });
            if days.changed() || limit.changed() || previous_target != self.target {
                self.detail = None;
                self.id = None;
                self.confirm = false;
            }
            if ui.button("Preview eligible records").clicked() {
                let now = jiff::Timestamp::now().as_millisecond();
                let days = i64::from(self.days.unwrap_or(30));
                let cutoff = now.saturating_sub(days * 86_400_000).max(0);
                self.submit(cx, cutoff, self.max_items.unwrap_or(50).into(), None);
            }
        });
        if let Some(detail) = self.detail.clone() {
            self.detail(cx, ui, &detail);
        }
        if self.busy {
            ui.spinner();
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
    }

    fn detail(&mut self, cx: &mut Cx, ui: &mut Ui, detail: &PruneInspection) {
        let preview = &detail.preview;
        ui.label(format!(
            "{} selected · {} held{}",
            preview.candidate_ids.0.len(),
            detail.held.0.len(),
            if detail.more_held { " (more held)" } else { "" }
        ));
        ui.label(format!(
            "Requested cutoff {}",
            millis(detail.requested_cutoff_ms.get())
        ));
        ui.label(format!(
            "Effective cutoff {}",
            millis(detail.effective_cutoff_ms.get())
        ));
        if detail.more_candidates {
            ui.label("More eligible records remain outside this preview.");
        }
        egui::ScrollArea::vertical()
            .max_height(230.0)
            .show(ui, |ui| {
                for row in &preview.candidate_ids.0 {
                    ui.label(format!(
                        "{} · revision {}",
                        row.id.as_str(),
                        row.revision.get()
                    ));
                }
                for row in &detail.held.0 {
                    ui.label(format!("{} · {}", row.id.as_str(), hold(&row.reason)));
                }
            });
        match detail.phase {
            PrunePhase::Previewed => {
                let valid = preview.expires_at_ms.get() > jiff::Timestamp::now().as_millisecond();
                ui.label(format!(
                    "Preview expires {}",
                    millis(preview.expires_at_ms.get())
                ));
                ui.checkbox(&mut self.confirm, "Remove the selected records");
                if ui
                    .add_enabled(
                        valid && self.confirm && !self.busy && !preview.candidate_ids.0.is_empty(),
                        egui::Button::new("Confirm removal"),
                    )
                    .clicked()
                {
                    self.submit(
                        cx,
                        detail.requested_cutoff_ms.get(),
                        detail.max_items.get(),
                        Some((preview.id.clone(), preview.revision.get())),
                    );
                }
            }
            PrunePhase::Queued => {
                ui.label("Waiting for retention to finish");
                if ui
                    .add_enabled(!self.busy, egui::Button::new("Cancel removal"))
                    .clicked()
                {
                    let preview_id = preview.id.clone();
                    let expected_revision = preview.revision.clone();
                    self.busy = cx.command(
                        SettingsSlot::RetentionCancel,
                        "Cancelling retention",
                        "maintenance.cancel",
                        move |b| {
                            frame::with(b, Negotiate::Latest, || {
                                Some(OperationInput::Maintenance(api::Input::Cancel(
                                    api::CancelInput {
                                        preview_id,
                                        expected_revision,
                                    },
                                )))
                            })
                        },
                    );
                }
            }
            _ => {
                if let Some(outcome) = &detail.outcome {
                    if let Some(reason) = &outcome.reason {
                        ui.label(&reason.0);
                    } else {
                        ui.label(format!(
                            "Removed {} records and {} dependent delivery or session rows",
                            outcome.records_removed.get(),
                            outcome.owned_rows_removed.get()
                        ));
                    }
                }
            }
        }
    }

    fn submit(&mut self, cx: &mut Cx, cutoff: i64, limit: u32, commit: Option<(Id, i64)>) {
        let kind = TARGETS[self.target].0;
        let slot = commit
            .as_ref()
            .map_or(SettingsSlot::RetentionPreview, |(id, _)| {
                SettingsSlot::RetentionApply(id.as_str().to_owned())
            });
        self.notice = None;
        self.busy = cx.command(
            slot,
            "Submitting retention request",
            kind.operation(),
            move |b| {
                frame::with(b, Negotiate::Initial, || {
                    let preview_only = commit.is_none();
                    let (preview_id, expected_revision) = commit
                        .map_or((None, None), |(id, revision)| {
                            (Some(id), frame::number(revision))
                        });
                    let older_than_ms = frame::number(cutoff)?;
                    let max_items = frame::integer(i64::from(limit))?;
                    macro_rules! input {
                        ($variant:ident, $ty:ident) => {
                            api::Input::$variant(api::$ty {
                                preview_only,
                                preview_id,
                                expected_revision,
                                older_than_ms,
                                max_items,
                            })
                        };
                    }
                    Some(OperationInput::Maintenance(match kind {
                        RetentionKind::Diagnostics => {
                            input!(PruneDiagnostics, PruneDiagnosticsInput)
                        }
                        RetentionKind::IndexRuns => input!(PruneIndexRuns, PruneIndexRunsInput),
                        RetentionKind::Messages => input!(PruneMessages, PruneMessagesInput),
                        RetentionKind::Sessions => input!(PruneSessions, PruneSessionsInput),
                    }))
                })
            },
        );
    }

    fn inspect(&mut self, cx: &mut Cx) {
        let Some(preview_id) = self.id.clone() else {
            return;
        };
        if cx.query(
            SettingsSlot::RetentionInspect,
            "Reading retention preview",
            "maintenance.inspect",
            move |b| {
                frame::with(b, Negotiate::Latest, || {
                    Some(OperationInput::Maintenance(api::Input::Inspect(
                        api::InspectInput { preview_id },
                    )))
                })
            },
        ) {
            self.poll = None;
            self.busy = true;
        }
    }

    fn seen(&mut self, value: PruneInspection) {
        if self.id.as_ref() == Some(&value.preview.id) {
            self.target = match value.preview.kind {
                PruneKind::Diagnostics => 0,
                PruneKind::IndexRuns => 1,
                PruneKind::Messages => 2,
                PruneKind::Sessions => 3,
            };
            self.notice = None;
            self.poll = matches!(value.phase, PrunePhase::Queued)
                .then(|| Instant::now() + Duration::from_secs(1));
            self.detail = Some(value);
        }
    }

    pub(crate) fn apply(&mut self, cx: &mut Cx, slot: SettingsSlot, reply: Reply) {
        self.busy = false;
        match (slot, reply) {
            (SettingsSlot::RetentionPreview, Reply::Command(reply)) => {
                match ops::command_result(reply) {
                    Ok(
                        OperationResult::MaintenancePruneDiagnostics { value }
                        | OperationResult::MaintenancePruneIndexRuns { value }
                        | OperationResult::MaintenancePruneMessages { value }
                        | OperationResult::MaintenancePruneSessions { value },
                    ) => {
                        self.id = Some(value.id);
                        self.detail = None;
                        self.confirm = false;
                        self.poll = Some(Instant::now());
                        self.inspect(cx);
                    }
                    Err(error) => self.notice = Some(error.describe()),
                    _ => self.notice = Some("Unexpected retention preview receipt".into()),
                }
            }
            (SettingsSlot::RetentionApply(id), Reply::Command(reply)) => {
                let unresolved = crate::backend::is_unknown(&reply);
                let accepted = ops::accepted_decision(reply);
                self.confirm = false;
                self.detail = None;
                if accepted.is_ok() || unresolved {
                    self.id = frame::id(&id);
                    self.poll = Some(Instant::now());
                    self.inspect(cx);
                } else if let Err(error) = accepted {
                    self.id = None;
                    self.poll = None;
                    self.notice = Some(error.describe());
                }
            }
            (SettingsSlot::RetentionInspect, Reply::Query(reply)) => match ops::query_result(reply)
            {
                Ok((OperationResult::MaintenanceInspect { value }, _)) => self.seen(value),
                Err(ops::Failure::Refused(ref fault))
                    if matches!(
                        fault.code,
                        devworks_core::protocol::DomainErrorCode::NotFound
                            | devworks_core::protocol::DomainErrorCode::PermissionDenied
                    ) =>
                {
                    self.id = None;
                    self.detail = None;
                    self.notice = Some("The saved retention preview is unavailable.".into());
                }
                Err(error) => {
                    self.notice = Some(error.describe());
                    if self.id.is_some() {
                        self.poll = Some(Instant::now() + Duration::from_secs(2));
                    }
                }
                _ => self.notice = Some("Unexpected retention status".into()),
            },
            (SettingsSlot::RetentionCancel, Reply::Command(reply)) => {
                match ops::command_result(reply) {
                    Ok(OperationResult::MaintenanceCancel { value }) => self.seen(value),
                    Err(error) => self.notice = Some(error.describe()),
                    _ => self.notice = Some("Unexpected retention cancellation receipt".into()),
                }
            }
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
}

fn millis(value: i64) -> String {
    jiff::Timestamp::from_millisecond(value)
        .map_or_else(|_| value.to_string(), |stamp| stamp.to_string())
}

fn hold(reason: &PruneHold) -> &'static str {
    match reason {
        PruneHold::AuthoredHistory => "Authored history",
        PruneHold::PublishedEvidence => "Published evidence",
        PruneHold::LiveOperation => "Active or unresolved operation",
        PruneHold::CurrentWitness => "Current index run",
        PruneHold::UnresolvedFailure => "Unresolved index failure",
        PruneHold::OpenConversation => "Open question or blocker",
        PruneHold::PendingDelivery => "Unread or pending delivery",
        PruneHold::HandoverWindow => "Recent activity or handover",
        PruneHold::ReplayHead => "Required for replay",
        PruneHold::Referenced => "Referenced by retained records",
        PruneHold::UnclassifiedReference => "Ownership could not be verified",
    }
}
