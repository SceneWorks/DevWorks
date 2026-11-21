//! Workspace import review and run history. All durable work goes through the gateway.
use crate::backend::{Reply, TaskOutput, screen_slot};
use crate::frame::{self, Negotiate};
use crate::memory::RouteMemory;
use crate::ops;
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId};
use devworks_core::operations::{OperationInput, imports as api};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    ImportConfig, ImportIntentMode, ImportRun, ImportSettingsView, MutationSummary, RegisteredPath,
};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::{BoundedList, RelativePath};
use devworks_core::protocol::{Counter, DomainErrorCode, Id, OperationState, ProjectId};
use eframe::egui::{self, Ui};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum WorkImportsSlot {
    WorkImportPreview,
    WorkImportSettings,
    WorkImportConfigure,
    WorkImportApply,
    WorkImportRuns,
    WorkImportInspect,
    WorkImportReport,
    WorkImportCancel,
    WorkImportHistory,
}
screen_slot!(WorkImportsSlot, Imports);
use WorkImportsSlot::*;

#[derive(Default)]
pub struct ImportsUi {
    open: bool,
    history_work: Option<Id>,
    history_rows: Vec<devworks_core::protocol::records::ImportedWorkEvidence>,
    history_cursor: Option<String>,
    root: Option<PathBuf>,
    relative: String,
    projects: String,
    work: String,
    all_projects: bool,
    history: bool,
    git_links: bool,
    allow_schedule: bool,
    delta: bool,
    since: String,
    reviewed: Option<ImportConfig>,
    preview: Option<ImportRun>,
    settings: Option<ImportSettingsView>,
    settings_ready: bool,
    approval: Option<MutationSummary>,
    run_id: Option<Id>,
    run: Option<ImportRun>,
    runs: Vec<ImportRun>,
    next: Option<String>,
    report: Option<String>,
    error: Option<String>,
    poll_after: Option<Instant>,
}

impl ImportsUi {
    pub fn open(&mut self, cx: &mut Cx, root: Option<PathBuf>) {
        if self.root.is_none() {
            self.history = true;
            self.git_links = true;
            self.root = root;
        }
        self.open = true;
        self.history_work = None;
        self.list(cx, false);
    }

    pub fn window(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        let mut open = self.open;
        let title = if self.history_work.is_some() {
            "Imported work history"
        } else {
            "Import work history"
        };
        egui::Window::new(title)
            .open(&mut open)
            .default_width(600.0)
            .show(ctx, |ui| {
                if self.history_work.is_some() {
                    self.draw_history(cx, ui);
                } else {
                    self.draw(cx, ui);
                }
            });
        self.open = open;
        if self.poll_after.is_some_and(|at| Instant::now() >= at) {
            self.inspect(cx);
        }
        if self.poll_after.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    pub fn open_history(&mut self, cx: &mut Cx, work: &str) {
        let Ok(work) = Id::new(work.to_owned()) else {
            return;
        };
        self.open = true;
        self.error = None;
        self.history_work = Some(work);
        self.history_rows.clear();
        self.history_cursor = None;
        self.load_history(cx);
    }

    fn load_history(&mut self, cx: &mut Cx) {
        let Some(work_id) = self.history_work.clone() else {
            return;
        };
        let cursor = self.history_cursor.clone();
        cx.query(
            WorkImportHistory,
            "Reading imported evidence",
            "imports.history",
            move |b| {
                frame::with(b, Negotiate::Latest, || {
                    Some(OperationInput::Imports(api::Input::History(
                        api::HistoryInput {
                            work_id,
                            page: frame::page(20, cursor.as_deref())?,
                        },
                    )))
                })
            },
        );
    }

    fn draw_history(&mut self, cx: &mut Cx, ui: &mut Ui) {
        egui::ScrollArea::vertical().max_height(600.0).show(ui, |ui| {
            ui.label("These records describe work in the source system. Use the board to change current work.");
            for row in &self.history_rows {
                ui.collapsing(format!("{} · {}", row.kind.0, row.source_id.as_str()), |ui| {
                    if let Some(current) = &row.current_reference {
                        ui.label(format!("Indexed at {} · {}", current.view_id.as_str(), current.path.0));
                    } else if row.kind.0 == "CodeReference" {
                        ui.label("No matching published index view for this historical reference.");
                    }
                    if let Ok(text) = serde_json::to_string_pretty(&row.evidence.0) {
                        ui.add(egui::Label::new(rich_report(&text)).selectable(true));
                    }
                });
            }
            if let Some(error) = &self.error {
                ui.colored_label(crate::theme::RED, error);
            }
            if self.history_cursor.is_some() && ui.button("More historical evidence").clicked() {
                self.load_history(cx);
            }
        });
    }

    fn draw(&mut self, cx: &mut Cx, ui: &mut Ui) {
        egui::ScrollArea::vertical()
            .max_height(600.0)
            .show(ui, |ui| {
                let freeze = cx.waiting(&WorkImportPreview)
                    || cx.waiting(&WorkImportConfigure)
                    || cx.waiting(&WorkImportApply);
                let before = self
                    .config()
                    .ok()
                    .and_then(|c| serde_json::to_value(c).ok());
                let before_window = (self.delta, self.since.clone());
                if let Some(root) = &self.root {
                    ui.label(
                        "Put the sealed CodeGraph export in this folder, then choose it below.",
                    );
                    ui.label(root.display().to_string());
                    if ui
                        .add_enabled(!freeze, egui::Button::new("Choose export…"))
                        .clicked()
                        && let Some(path) = cx.dialogs.pick_work_import(root)
                    {
                        match path.strip_prefix(root).ok().and_then(|p| p.to_str()) {
                            Some(relative) => self.relative = relative.replace('\\', "/"),
                            None => {
                                self.error = Some("Choose a snapshot in the import folder.".into())
                            }
                        }
                    }
                }
                ui.add_enabled_ui(!freeze, |ui| {
                    edit_line(ui, "Snapshot path in the import folder", &mut self.relative);
                    ui.checkbox(
                        &mut self.all_projects,
                        "Import all projects in this snapshot",
                    );
                    if !self.all_projects {
                        edit_line(ui, "Source project IDs", &mut self.projects);
                        edit_line(ui, "Source work IDs", &mut self.work);
                    }
                    ui.checkbox(&mut self.history, "Include history and evidence");
                    ui.add_enabled_ui(self.history, |ui| {
                        ui.checkbox(&mut self.git_links, "Include Git and review references");
                    });
                    ui.checkbox(&mut self.delta, "Preview an incremental import");
                    if self.delta {
                        edit_line(
                            ui,
                            "Updated since (Unix milliseconds, optional after a successful import)",
                            &mut self.since,
                        );
                    }
                    ui.checkbox(
                        &mut self.allow_schedule,
                        "Allow a separately created schedule to use this selection",
                    );
                });
                let after = self
                    .config()
                    .ok()
                    .and_then(|c| serde_json::to_value(c).ok());
                if before != after || before_window != (self.delta, self.since.clone()) {
                    self.reviewed = None;
                    self.preview = None;
                    self.settings_ready = false;
                    self.approval = None;
                    self.report = None;
                }
                if ui
                    .add_enabled(!freeze, egui::Button::new("Preview import"))
                    .clicked()
                {
                    self.preview(cx);
                }
                if let Some(preview) = &self.preview {
                    ui.label(format!(
                        "Source {} · {} selected records",
                        preview.source_namespace.as_ref().map_or("", Id::as_str),
                        preview.total.as_ref().map_or(0, |n| n.get())
                    ));
                    ui.monospace(preview.source_snapshot.as_str());
                    if ui.button("Read preview report").clicked() {
                        Self::read_report(cx, preview.report.clone());
                    }
                    if let Some(settings) = &self.settings {
                        ui.label(format!(
                            "Saved selection revision {}{}",
                            settings.revision.as_ref().map_or(0, |r| r.get()),
                            if settings.approved {
                                " · approved"
                            } else {
                                ""
                            }
                        ));
                    }
                    if ui
                        .add_enabled(
                            !freeze && self.settings_ready && self.approval.is_none(),
                            egui::Button::new("Approve reviewed selection"),
                        )
                        .clicked()
                    {
                        self.approve(cx);
                    }
                    if ui
                        .add_enabled(
                            !freeze && self.approval.is_some(),
                            egui::Button::new("Apply this snapshot"),
                        )
                        .clicked()
                    {
                        self.apply_snapshot(cx);
                    }
                }
                if let Some(error) = &self.error {
                    ui.colored_label(crate::theme::RED, error);
                }
                ui.separator();
                ui.heading("Import runs");
                if ui.button("Refresh runs").clicked() {
                    self.list(cx, false);
                }
                for run in &self.runs {
                    let label = format!(
                        "{}  {:?}  {}",
                        run.id.as_str(),
                        run.state,
                        run.source_namespace.as_ref().map_or("", Id::as_str)
                    );
                    if ui
                        .selectable_label(self.run_id.as_ref() == Some(&run.id), label)
                        .clicked()
                    {
                        self.run_id = Some(run.id.clone());
                        self.poll_after = Some(Instant::now());
                    }
                }
                if self.next.is_some() && ui.button("Older runs").clicked() {
                    self.list(cx, true);
                }
                if let Some(run) = &self.run {
                    ui.label(format!(
                        "{:?} · {} of {} records processed",
                        run.state,
                        run.processed.as_ref().map_or(0, |n| n.get()),
                        run.total.as_ref().map_or(0, |n| n.get())
                    ));
                    if ui.button("Read run report").clicked() {
                        Self::read_report(cx, run.report.clone());
                    }
                    if self.run_id.as_ref() == Some(&run.id)
                        && self.poll_after.is_some()
                        && ui
                            .add_enabled(
                                !cx.waiting(&WorkImportCancel),
                                egui::Button::new("Stop import"),
                            )
                            .clicked()
                    {
                        let input = api::CancelInput {
                            run_id: run.id.clone(),
                            expected_revision: run.revision.clone(),
                        };
                        cx.command(
                            WorkImportCancel,
                            "Stopping the import",
                            "imports.cancel",
                            move |b| {
                                frame::build(
                                    b,
                                    Negotiate::Latest,
                                    Some(OperationInput::Imports(api::Input::Cancel(input))),
                                )
                            },
                        );
                    }
                }
                if let Some(report) = &self.report {
                    ui.collapsing("Report details", |ui| {
                        ui.add(egui::Label::new(rich_report(report)).selectable(true));
                    });
                }
            });
    }

    fn config(&self) -> Result<ImportConfig, String> {
        let source = RegisteredPath {
            root_id: Id::new("work-import-inbox").expect("static root ID"),
            relative: serde_json::from_value::<RelativePath>(serde_json::json!(
                self.relative.trim()
            ))
            .map_err(|_| "Choose the snapshot file first.")?,
        };
        let project_ids = if self.all_projects {
            Vec::new()
        } else {
            ids(&self.projects)?
        };
        let work_ids = if self.all_projects {
            Vec::new()
        } else {
            ids(&self.work)?
        };
        if !self.all_projects && project_ids.is_empty() && work_ids.is_empty() {
            return Err("Select source projects or work IDs.".into());
        }
        Ok(ImportConfig {
            source,
            project_ids: BoundedList(project_ids.into_iter().map(ProjectId).collect()),
            work_ids: BoundedList(work_ids),
            include_history: self.history,
            include_git_links: self.history && self.git_links,
            all_projects: Some(self.all_projects),
            allow_scheduled_delta: Some(self.allow_schedule),
        })
    }

    fn preview(&mut self, cx: &mut Cx) {
        let config = match self.config() {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        let updated_since_ms = if self.delta && !self.since.trim().is_empty() {
            let time = self
                .since
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|n| Counter::new(n).ok());
            if time.is_none() {
                self.error = Some("Enter the lower bound in Unix milliseconds.".into());
                return;
            }
            time
        } else {
            None
        };
        self.preview = None;
        self.report = None;
        self.settings_ready = false;
        self.approval = None;
        self.error = None;
        self.reviewed = Some(config.clone());
        let mode = Some(if self.delta {
            ImportIntentMode::Delta
        } else {
            ImportIntentMode::Full
        });
        cx.query(
            WorkImportPreview,
            "Previewing selected work",
            "imports.preview",
            move |b| {
                frame::build(
                    b,
                    Negotiate::Latest,
                    Some(OperationInput::Imports(api::Input::Preview(
                        api::PreviewInput {
                            config,
                            mode,
                            updated_since_ms,
                        },
                    ))),
                )
            },
        );
    }

    fn settings(&mut self, cx: &mut Cx, source_namespace: Id) {
        self.settings = None;
        self.settings_ready = false;
        cx.query(
            WorkImportSettings,
            "Reading saved import selection",
            "imports.settings",
            move |b| {
                frame::build(
                    b,
                    Negotiate::Latest,
                    Some(OperationInput::Imports(api::Input::Settings(
                        api::SettingsInput {
                            source_namespace,
                            proposal: None,
                        },
                    ))),
                )
            },
        );
    }

    fn approve(&mut self, cx: &mut Cx) {
        let Some(config) = self.reviewed.clone() else {
            return;
        };
        self.error = None;
        let input = api::ConfigureInput {
            config_id: self.settings.as_ref().and_then(|s| s.id.clone()),
            expected_revision: self.settings.as_ref().and_then(|s| s.revision.clone()),
            config,
            approve: Some(true),
        };
        cx.command(
            WorkImportConfigure,
            "Approving the reviewed import",
            "imports.configure",
            move |b| {
                frame::build(
                    b,
                    Negotiate::Latest,
                    Some(OperationInput::Imports(api::Input::Configure(input))),
                )
            },
        );
    }

    fn apply_snapshot(&mut self, cx: &mut Cx) {
        let (Some(config), Some(preview)) = (&self.approval, &self.preview) else {
            return;
        };
        self.error = None;
        let input = api::ApplyInput {
            config_id: config.id.clone(),
            expected_revision: config.revision.clone(),
            source_snapshot: preview.source_snapshot.clone(),
            preview_digest: preview.report.sha256.clone(),
        };
        cx.command(
            WorkImportApply,
            "Submitting the approved import",
            "imports.apply",
            move |b| {
                frame::build(
                    b,
                    Negotiate::Latest,
                    Some(OperationInput::Imports(api::Input::Apply(input))),
                )
            },
        );
    }

    fn inspect(&mut self, cx: &mut Cx) {
        self.poll_after = None;
        let Some(run_id) = self.run_id.clone() else {
            return;
        };
        cx.query(
            WorkImportInspect,
            "Checking import progress",
            "imports.inspect",
            move |b| {
                frame::build(
                    b,
                    Negotiate::Latest,
                    Some(OperationInput::Imports(api::Input::Inspect(
                        api::InspectInput { run_id },
                    ))),
                )
            },
        );
    }

    fn list(&mut self, cx: &mut Cx, append: bool) {
        let cursor = if append { self.next.clone() } else { None };
        if !append {
            self.runs.clear();
        }
        cx.query(
            WorkImportRuns,
            "Reading work import runs",
            "imports.runs",
            move |b| {
                frame::with(b, Negotiate::Latest, || {
                    Some(OperationInput::Imports(api::Input::Runs(api::RunsInput {
                        page: frame::page(20, cursor.as_deref())?,
                        source_namespace: None,
                    })))
                })
            },
        );
    }

    fn read_report(cx: &mut Cx, artifact: ArtifactRef) {
        if let Some(client) = cx.client.as_deref_mut() {
            client.task(
                WorkImportReport.into(),
                "Reading exact import report",
                move |b| {
                    let bytes = ops::read_artifact_bytes(b, &artifact)?;
                    let report = serde_json::from_slice::<serde_json::Value>(&bytes)
                        .map_err(|e| e.to_string())?;
                    let formatted =
                        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
                    Ok(TaskOutput::WorkImportReport(formatted))
                },
            );
        }
    }

    fn query_reply(
        &mut self,
        cx: &mut Cx,
        slot: WorkImportsSlot,
        query: devworks_core::gateway::QueryOutcome,
    ) {
        match ops::query_result(query) {
            Ok((OperationResult::ImportsPreview { value }, _)) => {
                if let Some(namespace) = value.source_namespace.clone() {
                    self.settings(cx, namespace);
                }
                Self::read_report(cx, value.report.clone());
                self.preview = Some(value);
            }
            Ok((OperationResult::ImportsSettings { value }, _)) => {
                self.settings = Some(value);
                self.settings_ready = true;
            }
            Err(ops::Failure::Refused(fault))
                if slot == WorkImportSettings && fault.code == DomainErrorCode::NotFound =>
            {
                self.settings_ready = true;
            }
            Ok((OperationResult::ImportsRuns { value }, _)) => {
                self.runs.extend(value.items.0);
                self.next = value.next_cursor.map(|c| c.0);
            }
            Ok((OperationResult::ImportsInspect { value }, _))
                if self.run_id.as_ref() == Some(&value.id) =>
            {
                if !matches!(
                    value.state,
                    OperationState::ObservedComplete
                        | OperationState::Failed
                        | OperationState::CancelledBeforeDispatch
                        | OperationState::NeedsInput
                ) {
                    self.poll_after = Some(Instant::now() + Duration::from_secs(1));
                } else if self.run.as_ref().is_none_or(|r| r.state != value.state) {
                    self.list(cx, false);
                    cx.load_projects();
                }
                self.run = Some(value);
            }
            Ok((OperationResult::ImportsInspect { .. }, _)) => {}
            Ok((OperationResult::ImportsHistory { value }, _)) => {
                self.history_rows.extend(value.items.0);
                self.history_cursor = value.next_cursor.map(|cursor| cursor.0);
            }
            Ok(_) => self.error = Some("Unexpected import result.".into()),
            Err(error) => self.error = Some(error.describe()),
        }
    }
}

fn ids(text: &str) -> Result<Vec<Id>, String> {
    let ids = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|id| !id.is_empty())
        .map(|value| Id::new(value.to_owned()).map_err(|_| "Invalid source ID.".to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    if ids.len() > 128 {
        return Err("Select up to 128 source IDs per field.".into());
    }
    Ok(ids)
}

fn edit_line(ui: &mut Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.text_edit_singleline(value)
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
}

fn rich_report(text: &str) -> egui::RichText {
    egui::RichText::new(text).monospace()
}

impl Screen for ImportsUi {
    type Slot = WorkImportsSlot;
    const ID: ScreenId = ScreenId::Imports;
    fn load(&mut self, _cx: &mut Cx, _route: Route) {}
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.draw(cx, ui);
    }
    fn context(&self, _route: Route, _mem: &RouteMemory) -> ScreenContext {
        ScreenContext::default()
    }
    fn apply(&mut self, cx: &mut Cx, slot: Self::Slot, reply: Reply) {
        match (slot.clone(), reply) {
            (_, Reply::Query(query)) => self.query_reply(cx, slot, query),
            (WorkImportConfigure, Reply::Command(command)) => match ops::command_result(command) {
                Ok(OperationResult::ImportsConfigure { value }) => self.approval = Some(value),
                Ok(_) => self.error = Some("Unexpected import approval receipt.".into()),
                Err(error) => self.error = Some(error.describe()),
            },
            (WorkImportApply, Reply::Command(command)) => match ops::accepted_decision(command) {
                Ok(id) => {
                    self.run_id = Some(id.0);
                    self.approval = None;
                    self.inspect(cx);
                }
                Err(error) => self.error = Some(error.describe()),
            },
            (WorkImportCancel, Reply::Command(command)) => match ops::command_result(command) {
                Ok(OperationResult::ImportsCancel { .. }) => self.inspect(cx),
                Ok(_) => self.error = Some("Unexpected cancellation receipt.".into()),
                Err(error) => self.error = Some(error.describe()),
            },
            (WorkImportReport, Reply::Task(Ok(TaskOutput::WorkImportReport(report)))) => {
                self.report = Some(report)
            }
            (WorkImportReport, Reply::Task(Err(error))) => self.error = Some(error),
            (_, reply) => cx.unhandled(&slot, reply),
        }
    }
}
