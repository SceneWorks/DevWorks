//! The permanent project purge review: its state and the window that shows the exact deletion
//! manifest. The requests and the review's steps are in `shell::project_purge`.
use crate::paged::Paged;
use crate::theme;
use devworks_core::protocol::records::{
    ProjectPurgeEntry, ProjectPurgeManifest, ProjectRecord, PurgePhase,
};
use eframe::egui;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct PurgeUi {
    pub project: Option<ProjectRecord>,
    pub manifest: Option<ProjectPurgeManifest>,
    /// The manifest's entries, fenced by the manifest's digest and revision (a page of another
    /// manifest is refused).
    pub entries: Paged<ProjectPurgeEntry>,
    pub busy: bool,
    pub frozen: bool,
    pub acknowledged: bool,
    pub cleanup_acknowledged: bool,
    pub error: Option<String>,
    pub recovering: bool,
    pub last_poll: Option<std::time::Instant>,
    /// The last manifest read for each project under review (its ID), kept across restarts.
    pub reviews: BTreeMap<String, serde_json::Value>,
}

/// What the window asked for in one frame.
#[derive(Default)]
pub struct PurgeIntent {
    pub confirm: bool,
    pub close: bool,
    pub more: bool,
    pub retry: bool,
    pub prepare: bool,
}

impl PurgeUi {
    /// Every page of the manifest has been read.
    pub fn fully_read(&self) -> bool {
        self.entries.loaded && !self.entries.more()
    }
    /// Forgets the review in progress and keeps the stored manifests.
    pub(crate) fn reset(&mut self) {
        *self = Self {
            reviews: std::mem::take(&mut self.reviews),
            ..Self::default()
        };
    }

    /// The review window: the manifest, its entries and the controls the phase allows.
    pub fn window(&mut self, ctx: &egui::Context) -> PurgeIntent {
        let mut out = PurgeIntent::default();
        egui::Window::new("Permanent project purge")
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(
                    "Review the complete deletion manifest. This permanently removes owned \
                DevWorks data and local drafts. Shared records remain. Git and \
                repository files are preserved.",
                );
                if let Some(m) = &self.manifest {
                    ui.label(format!(
                        "Manifest {} · revision {} · {:?}",
                        m.id.as_str(),
                        m.revision.as_str(),
                        m.phase
                    ));
                    ui.label(format!(
                        "Remove {} records and {} bytes; keep {} records and {} bytes.",
                        m.record_count.get(),
                        m.managed_byte_count.get(),
                        m.kept_record_count.get(),
                        m.kept_byte_count.get()
                    ));
                    egui::ScrollArea::vertical()
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for e in &self.entries.items {
                                ui.label(format!(
                                    "{:?} · {} · {} records · {} bytes · {}",
                                    e.disposition,
                                    e.record_class.0,
                                    e.record_count.get(),
                                    e.byte_count.get(),
                                    e.reason.0
                                ));
                            }
                        });
                    if matches!(m.phase, PurgePhase::Prepared) {
                        if !self.fully_read() {
                            out.more = ui
                                .add_enabled(
                                    !self.busy,
                                    egui::Button::new("Read next manifest page"),
                                )
                                .clicked();
                        }
                        ui.checkbox(
                            &mut self.acknowledged,
                            "I reviewed every entry and want to permanently purge this project",
                        );
                        out.confirm = ui
                            .add_enabled(
                                self.fully_read() && self.acknowledged && !self.busy,
                                egui::Button::new("Permanently purge"),
                            )
                            .clicked();
                    } else {
                        ui.label(format!(
                            "{} physical effects remain. Held effects retry with their original \
                    reviewed identities.",
                            m.pending_effects.get()
                        ));
                        out.retry = ui
                            .add_enabled(!self.busy, egui::Button::new("Refresh purge status"))
                            .clicked();
                    }
                }
                if let Some(e) = &self.error {
                    ui.colored_label(theme::RED, e);
                    if self
                        .manifest
                        .as_ref()
                        .is_none_or(|m| matches!(m.phase, PurgePhase::Prepared))
                    {
                        out.prepare = ui
                            .add_enabled(!self.busy, egui::Button::new("Prepare a fresh manifest"))
                            .clicked();
                    }
                }
                if self.busy {
                    ui.spinner();
                }
                let can_close =
                    self.manifest.as_ref().is_none_or(|m| {
                        matches!(m.phase, PurgePhase::Prepared | PurgePhase::Complete)
                    }) || self.cleanup_acknowledged;
                out.close = ui
                    .add_enabled(can_close && !self.busy, egui::Button::new("Close review"))
                    .clicked();
            });
        out
    }
}
