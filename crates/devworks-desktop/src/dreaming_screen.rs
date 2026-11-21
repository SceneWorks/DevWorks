//! Native Dreaming controls and history. Every operation uses the same gateway as agents and jobs.
use crate::backend::{Backend, Reply, TaskOutput, screen_slot};
use crate::frame::{self, Negotiate};
use crate::instructions_ops::TaskResult;
use crate::ops;
use crate::route::Route;
use crate::screen::{Cx, Effect, Screen, ScreenContext, ScreenId};
use devworks_core::operations::{OperationInput, dreaming as ops_input};
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::records::{
    DreamConfig, DreamRevertResult, DreamRun, DreamRunStatus, DreamSettingsRecord, EffortName,
    InstructionRecord,
};
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::support::{Limit, PositiveCounter};
use devworks_core::protocol::{CommandReply, Counter, Id, ProjectId, Revision};
use eframe::egui::{self, Ui};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DreamingSlot {
    DreamSettings(Option<String>),
    DreamRuns(Option<String>, bool),
    DreamInstructions(Option<String>, bool),
    DreamReport(Option<String>, Id),
    DreamReportText(Option<String>, Id),
    DreamRunCommand(Option<String>),
    DreamRevert(Option<String>),
    DreamSaveSettings(Option<String>),
    DreamInstructionText(Option<String>),
    DreamInstructionPrepare(Option<String>),
    DreamInstructionUpload(Option<String>),
    DreamInstructionSave(Option<String>),
}
screen_slot!(DreamingSlot, Dreaming);

impl DreamingSlot {
    fn project(&self) -> &Option<String> {
        use DreamingSlot::*;
        match self {
            DreamSettings(project)
            | DreamRuns(project, _)
            | DreamInstructions(project, _)
            | DreamReport(project, _)
            | DreamReportText(project, _)
            | DreamRunCommand(project)
            | DreamRevert(project)
            | DreamSaveSettings(project)
            | DreamInstructionText(project)
            | DreamInstructionPrepare(project)
            | DreamInstructionUpload(project)
            | DreamInstructionSave(project) => project,
        }
    }
}

#[derive(Default)]
pub struct DreamingScreen {
    project: Option<String>,
    settings: Option<DreamSettingsRecord>,
    draft: Option<DreamConfig>,
    runs: Vec<DreamRun>,
    runs_cursor: Option<String>,
    selected: Option<Id>,
    report: Option<DreamRun>,
    report_text: Option<String>,
    confirm_revert: bool,
    instructions: Vec<InstructionRecord>,
    instruction_cursor: Option<String>,
    instruction_revision: Option<Revision>,
    instruction_text: Option<String>,
    instruction_save_revision: Option<Revision>,
    staged_instruction: Option<PathBuf>,
    message: Option<String>,
}

fn request(backend: &dyn Backend, input: ops_input::Input) -> Vec<u8> {
    frame::build(
        backend,
        Negotiate::Latest,
        Some(OperationInput::Dreaming(input)),
    )
}
fn scope(project: &Option<String>) -> DataScope {
    project
        .as_ref()
        .and_then(|project| Id::new(project).ok())
        .map_or(DataScope::Workspace, |id| DataScope::Project {
            project_id: ProjectId(id),
        })
}

impl DreamingScreen {
    fn reset_project(&mut self, cx: &Cx<'_>) {
        let project = cx.selected_project().map(str::to_owned);
        if self.project != project {
            self.clean_stage(cx);
            *self = Self {
                project,
                ..Default::default()
            };
        }
    }

    fn clean_stage(&mut self, cx: &Cx<'_>) {
        if let (Some(path), Some(backend)) = (self.staged_instruction.take(), cx.backend()) {
            crate::attachments::remove_owned(&path, &backend.scratch_dir());
        }
    }

    fn load_settings(&self, cx: &mut Cx<'_>) {
        let scope = scope(&self.project);
        cx.query(
            DreamingSlot::DreamSettings(self.project.clone()),
            "Loading Dreaming settings",
            "dreaming.settings",
            |b| {
                request(
                    b,
                    ops_input::Input::SettingsQuery(ops_input::SettingsQueryInput { scope }),
                )
            },
        );
    }

    fn load_runs(&self, cx: &mut Cx<'_>, append: bool) {
        let scope = scope(&self.project);
        let cursor = self.runs_cursor.as_deref().filter(|_| append);
        cx.query(
            DreamingSlot::DreamRuns(self.project.clone(), append),
            "Loading Dreaming runs",
            "dreaming.runs",
            |b| {
                frame::with(b, Negotiate::Latest, || {
                    Some(OperationInput::Dreaming(ops_input::Input::Runs(
                        ops_input::RunsInput {
                            scope,
                            page: frame::page(40, cursor)?,
                        },
                    )))
                })
            },
        );
    }

    fn load_instructions(&self, cx: &mut Cx<'_>, append: bool) {
        let scope = scope(&self.project);
        let cursor = self.instruction_cursor.as_deref().filter(|_| append);
        cx.query(
            DreamingSlot::DreamInstructions(self.project.clone(), append),
            "Loading Dreaming instructions",
            "dreaming.instructions",
            |b| {
                frame::with(b, Negotiate::Latest, || {
                    Some(OperationInput::Dreaming(
                        ops_input::Input::InstructionsQuery(ops_input::InstructionsQueryInput {
                            scope,
                            page: Some(frame::page(40, cursor)?),
                        }),
                    ))
                })
            },
        );
    }

    fn load_report(&self, cx: &mut Cx<'_>, run_id: Id) {
        cx.query(
            DreamingSlot::DreamReport(self.project.clone(), run_id.clone()),
            "Loading Dreaming report",
            "dreaming.report",
            |b| {
                request(
                    b,
                    ops_input::Input::Report(ops_input::ReportInput { run_id }),
                )
            },
        );
    }

    fn run(&self, cx: &mut Cx<'_>, resume: Option<Id>, cancel: Option<Id>) {
        let Some(settings) = &self.settings else {
            return;
        };
        let input = ops_input::RunInput {
            scope: scope(&self.project),
            expected_settings_revision: settings.revision.clone(),
            resume_run_id: resume,
            cancel_run_id: cancel,
        };
        cx.command(
            DreamingSlot::DreamRunCommand(self.project.clone()),
            "Updating Dreaming run",
            "dreaming.run",
            |b| request(b, ops_input::Input::Run(input)),
        );
    }

    fn save_settings(&self, cx: &mut Cx<'_>, config: DreamConfig) {
        let Some(settings) = &self.settings else {
            return;
        };
        let input = ops_input::SettingsCommandInput {
            scope: scope(&self.project),
            expected_revision: settings.revision.clone(),
            config,
        };
        cx.command(
            DreamingSlot::DreamSaveSettings(self.project.clone()),
            "Saving Dreaming settings",
            "dreaming.settings",
            |b| request(b, ops_input::Input::SettingsCommand(input)),
        );
    }

    fn edit_instructions(&self, cx: &mut Cx<'_>, record: &InstructionRecord) {
        let artifact = record.content.clone();
        if let Some(client) = cx.client.as_deref_mut() {
            client.task(
                DreamingSlot::DreamInstructionText(self.project.clone()).into(),
                "Reading instruction text",
                move |backend| crate::instructions_ops::load_text(backend, &artifact),
            );
        }
    }

    fn prepare_instruction(&mut self, cx: &mut Cx<'_>, draft: &str) {
        let Some(revision) = self.instruction_revision.clone() else {
            return;
        };
        if let Some(client) = cx.client.as_deref_mut() {
            let project = self.project.clone();
            let draft = draft.to_owned();
            if client.task(
                DreamingSlot::DreamInstructionPrepare(project.clone()).into(),
                "Preparing instruction text",
                move |backend| prepare_text(backend, project.as_deref(), &draft),
            ) {
                self.instruction_save_revision = Some(revision);
            }
        }
    }

    fn apply_instruction_task(&mut self, cx: &mut Cx<'_>, reply: Result<TaskOutput, String>) {
        match reply {
            Ok(TaskOutput::Instruction(TaskResult::Text(text))) => {
                self.instruction_text = Some(text)
            }
            Ok(TaskOutput::Instruction(TaskResult::Prepared { frame, staged })) => {
                self.staged_instruction = Some(staged);
                if !cx.command(
                    DreamingSlot::DreamInstructionUpload(self.project.clone()),
                    "Storing instruction text",
                    "artifacts.save",
                    |_| frame,
                ) {
                    self.clean_stage(cx);
                }
            }
            Err(error) => self.message = Some(error),
            _ => self.message = Some("The instruction task returned an unexpected result.".into()),
        }
    }
}

impl Screen for DreamingScreen {
    type Slot = DreamingSlot;
    const ID: ScreenId = ScreenId::Dreaming;

    fn load(&mut self, cx: &mut Cx<'_>, _: Route) {
        self.reset_project(cx);
        self.load_settings(cx);
        self.load_runs(cx, false);
        self.load_instructions(cx, false);
        cx.load_router_catalog();
    }

    fn refresh(&mut self, cx: &mut Cx<'_>, _: Route) {
        if self.project.as_deref() != cx.selected_project() {
            self.load(cx, Route::Dreaming);
            return;
        }
        if self.runs_cursor.is_none()
            && !cx.pending(&DreamingSlot::DreamRuns(self.project.clone(), false))
        {
            self.load_runs(cx, false);
        }
        if let Some(id) = &self.selected
            && !cx.pending(&DreamingSlot::DreamReport(self.project.clone(), id.clone()))
        {
            self.load_report(cx, id.clone());
        }
    }

    fn apply(&mut self, cx: &mut Cx<'_>, slot: DreamingSlot, reply: Reply) {
        if slot.project() != &self.project {
            if let Reply::Task(Ok(TaskOutput::Instruction(TaskResult::Prepared { staged, .. }))) =
                reply
                && let Some(backend) = cx.backend()
            {
                crate::attachments::remove_owned(&staged, &backend.scratch_dir());
            }
            return;
        }
        match reply {
            Reply::Query(outcome) => match ops::query_result(outcome) {
                Ok((OperationResult::DreamingSettings { value }, _)) => {
                    self.settings = Some(value);
                }
                Ok((OperationResult::DreamingRuns { value }, _)) => {
                    if !matches!(slot, DreamingSlot::DreamRuns(_, true)) {
                        self.runs.clear();
                    }
                    self.runs.extend(value.items.0);
                    self.runs_cursor = value.next_cursor.map(|c| c.0);
                }
                Ok((OperationResult::DreamingInstructions { value }, revision)) => {
                    if !matches!(slot, DreamingSlot::DreamInstructions(_, true)) {
                        self.instructions.clear();
                    }
                    self.instructions.extend(value.items.0);
                    self.instruction_cursor = value.next_cursor.map(|c| c.0);
                    self.instruction_revision = Revision::new(revision).ok();
                }
                Ok((OperationResult::DreamingReport { value }, _))
                    if self.selected.as_ref() == Some(&value.id) =>
                {
                    self.report = Some(value)
                }
                Ok(_) => {}
                Err(error) => self.message = Some(error.describe()),
            },
            Reply::Task(Ok(TaskOutput::DreamingReport(text))) => {
                if let DreamingSlot::DreamReportText(_, id) = &slot
                    && self.selected.as_ref() == Some(id)
                {
                    self.report_text = Some(text);
                }
            }
            Reply::Task(result) => self.apply_instruction_task(cx, result),
            Reply::Command(outcome) => {
                if let Ok(CommandReply::Recorded { receipt, .. }) = &outcome
                    && matches!(&receipt.decision, DomainDecision::Accepted { .. })
                {
                    self.message = Some("Dreaming run queued.".into());
                    self.load_runs(cx, false);
                    return;
                }
                match ops::command_result(outcome) {
                    Ok(OperationResult::ArtifactsSave { value }) => {
                        self.clean_stage(cx);
                        if let Some(expected_revision) = self.instruction_save_revision.take() {
                            let input = ops_input::InstructionsCommandInput {
                                scope: scope(&self.project),
                                expected_revision,
                                content: value.artifact,
                            };
                            cx.command(
                                DreamingSlot::DreamInstructionSave(self.project.clone()),
                                "Saving Dreaming instructions",
                                "dreaming.instructions",
                                |b| request(b, ops_input::Input::InstructionsCommand(input)),
                            );
                        }
                    }
                    Ok(OperationResult::DreamingInstructions { .. }) => {
                        self.message = Some("Instructions saved.".into());
                        self.instruction_text = None;
                        self.load_instructions(cx, false);
                        self.load_settings(cx);
                    }
                    Ok(OperationResult::DreamingSettings { value }) => {
                        self.settings = Some(value);
                        self.draft = None;
                        self.message = Some("Dreaming settings saved.".into());
                    }
                    Ok(OperationResult::DreamingRun { value }) => {
                        self.selected = Some(value.id.clone());
                        self.message = value
                            .detail
                            .as_ref()
                            .map(|d| format!("Dreaming: {:?}", d.status));
                        self.load_report(cx, value.id);
                        self.load_runs(cx, false);
                    }
                    Ok(OperationResult::DreamingRevert {
                        value: DreamRevertResult::Current(value),
                    }) => {
                        self.message = Some(format!(
                            "Reversed {} actions. {} conflicts remain.",
                            value.applied.0.len(),
                            value.remaining.get()
                        ));
                        self.load_report(cx, value.run_id);
                    }
                    Ok(_) => self.message = Some("Dreaming returned an unexpected result.".into()),
                    Err(error) => {
                        if matches!(slot, DreamingSlot::DreamInstructionUpload(_))
                            && !matches!(&error, ops::Failure::Unknown)
                        {
                            self.clean_stage(cx);
                        }
                        self.message = Some(error.describe());
                    }
                }
            }
            other => cx.unhandled(&slot, other),
        }
    }

    fn ui(&mut self, cx: &mut Cx<'_>, _: Route, ui: &mut Ui) {
        self.reset_project(cx);
        ui.horizontal(|ui| {
            ui.heading("Dreaming");
            if ui.button("Refresh").clicked() {
                self.load(cx, Route::Dreaming);
            }
            if ui
                .add_enabled(
                    self.settings.is_some()
                        && !cx.waiting(&DreamingSlot::DreamRunCommand(self.project.clone())),
                    egui::Button::new("Run now"),
                )
                .clicked()
            {
                self.run(cx, None, None);
            }
        });
        if let Some(message) = &self.message {
            ui.label(message);
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.collapsing("Settings", |ui| self.settings_ui(cx, ui));
            ui.collapsing("Instructions", |ui| self.instructions_ui(cx, ui));
            ui.separator();
            ui.heading("Run history");
            let mut open = None;
            for run in &self.runs {
                let state = run
                    .detail
                    .as_ref()
                    .map(|detail| format!("{:?}", detail.status))
                    .unwrap_or_else(|| format!("{:?}", run.state));
                if ui
                    .selectable_label(
                        self.selected.as_ref() == Some(&run.id),
                        format!("{}  {state}", run.id.as_str()),
                    )
                    .clicked()
                {
                    open = Some(run.id.clone());
                }
            }
            if let Some(id) = open {
                self.selected = Some(id.clone());
                self.report = None;
                self.report_text = None;
                self.confirm_revert = false;
                self.load_report(cx, id);
            }
            if self.runs_cursor.is_some() && ui.button("More runs").clicked() {
                self.load_runs(cx, true);
            }
            self.report_ui(cx, ui);
        });
    }

    fn context(&self, _: Route, memory: &crate::memory::RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: memory.tab,
            selection: self.selected.as_ref().map(|id| id.as_str().to_owned()),
        }
    }

    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        if self.project.as_deref() == Some(scope.project_id.0.as_str()) {
            *self = Self::default();
        }
    }
}

impl DreamingScreen {
    fn settings_ui(&mut self, cx: &mut Cx<'_>, ui: &mut Ui) {
        let Some(settings) = &self.settings else {
            ui.label("Settings are unavailable.");
            return;
        };
        let config = self.draft.get_or_insert_with(|| settings.config.clone());
        ui.checkbox(&mut config.enabled, "Enable Dreaming");
        edit_id(ui, "Model ID", &mut config.model_id);
        edit_id(ui, "Schedule ID", &mut config.schedule_id);
        egui::ComboBox::from_id_salt("dreaming-effort")
            .selected_text(format!(
                "{:?}",
                config.effort.as_ref().unwrap_or(&EffortName::Default)
            ))
            .show_ui(ui, |ui| {
                for choice in [
                    EffortName::Default,
                    EffortName::Minimal,
                    EffortName::Low,
                    EffortName::Medium,
                    EffortName::High,
                    EffortName::XHigh,
                    EffortName::Max,
                ] {
                    if ui.button(format!("{choice:?}")).clicked() {
                        config.effort = Some(choice);
                    }
                }
            });
        let mut limit = i64::from(config.batch_limit.get());
        if number(ui, "Records per batch", &mut limit, 1..=500) {
            config.batch_limit = Limit::new(limit as u32).expect("settings bounds");
        }
        edit_counter(
            ui,
            "Minimum interval (ms)",
            &mut config.minimum_interval_ms,
            0..=31_536_000_000,
        );
        edit_positive(ui, "Turn limit", &mut config.max_turns, 1..=100);
        edit_positive(ui, "Token limit", &mut config.max_tokens, 1..=1_000_000);
        edit_positive(
            ui,
            "Time limit (ms)",
            &mut config.max_elapsed_ms,
            1000..=3_600_000,
        );
        edit_counter(
            ui,
            "Cost limit (millionths of USD)",
            &mut config.max_micro_usd,
            0..=i64::MAX,
        );
        if ui
            .add_enabled(
                !cx.waiting(&DreamingSlot::DreamSaveSettings(self.project.clone())),
                egui::Button::new("Save settings"),
            )
            .clicked()
        {
            let mut saved = config.clone();
            saved.instructions = settings.config.instructions.clone();
            self.save_settings(cx, saved);
        }
    }

    fn instructions_ui(&mut self, cx: &mut Cx<'_>, ui: &mut Ui) {
        for record in &self.instructions {
            if ui
                .button(format!(
                    "{} revision {}",
                    record.name.0,
                    record.revision.get()
                ))
                .clicked()
            {
                self.edit_instructions(cx, record);
            }
        }
        if self.instruction_cursor.is_some() && ui.button("More instruction revisions").clicked() {
            self.load_instructions(cx, true);
        }
        if self.instruction_text.is_none() && ui.button("New instruction revision").clicked() {
            self.instruction_text = Some(String::new());
        }
        let busy = cx.pending(&DreamingSlot::DreamInstructionPrepare(self.project.clone()))
            || cx.waiting(&DreamingSlot::DreamInstructionUpload(self.project.clone()))
            || cx.waiting(&DreamingSlot::DreamInstructionSave(self.project.clone()));
        if let Some(draft) = &mut self.instruction_text {
            ui.add_enabled(
                !busy,
                egui::TextEdit::multiline(draft)
                    .desired_rows(8)
                    .desired_width(f32::INFINITY),
            );
            if ui
                .add_enabled(
                    !busy && !draft.trim().is_empty(),
                    egui::Button::new("Save new revision"),
                )
                .clicked()
            {
                let draft = draft.clone();
                self.prepare_instruction(cx, &draft);
            }
        }
    }

    fn report_ui(&mut self, cx: &mut Cx<'_>, ui: &mut Ui) {
        let Some(run) = &self.report else { return };
        let id = run.id.clone();
        ui.separator();
        if let Some(detail) = &run.detail {
            ui.label(format!(
                "{:?} · {:?} · {}",
                detail.status,
                detail.trigger,
                detail.initiator.0.as_str()
            ));
            ui.label(format!(
                "{} input tokens · {} output tokens · {} millionths of USD",
                detail.input_tokens.get(),
                detail.output_tokens.get(),
                detail.cost_micro_usd.get()
            ));
            if detail.usage_uncertain {
                ui.label("Some provider usage remains unconfirmed.");
            }
            if let Some(reason) = &detail.reason {
                ui.label(&reason.0);
            }
            let busy = cx.waiting(&DreamingSlot::DreamRunCommand(self.project.clone()));
            if ui
                .add_enabled(
                    detail.can_continue && !busy,
                    egui::Button::new("Resume saved run"),
                )
                .clicked()
            {
                self.run(cx, Some(id.clone()), None);
            }
            if matches!(
                &detail.status,
                DreamRunStatus::Queued | DreamRunStatus::Running
            ) && ui
                .add_enabled(!busy, egui::Button::new("Cancel run"))
                .clicked()
            {
                self.run(cx, None, Some(id.clone()));
            }
            if let Some(digest) = &detail.journal_digest
                && !matches!(
                    &detail.status,
                    DreamRunStatus::Queued
                        | DreamRunStatus::Running
                        | DreamRunStatus::OutcomeUnknown
                )
            {
                if !self.confirm_revert && ui.button("Revert run changes…").clicked() {
                    self.confirm_revert = true;
                }
                if self.confirm_revert {
                    ui.label("Reverse this run’s changes where the original evidence and revisions still match.");
                    if ui
                        .add_enabled(
                            !cx.waiting(&DreamingSlot::DreamRevert(self.project.clone())),
                            egui::Button::new("Confirm revert"),
                        )
                        .clicked()
                    {
                        let input = ops_input::RevertInput {
                            run_id: id,
                            expected_revision: run.revision.clone(),
                            expected_change_digest: digest.clone(),
                            proposal_id: None,
                            all_changes: Some(true),
                        };
                        self.confirm_revert = false;
                        cx.command(
                            DreamingSlot::DreamRevert(self.project.clone()),
                            "Reverting Dreaming changes",
                            "dreaming.revert",
                            |b| request(b, ops_input::Input::Revert(input)),
                        );
                    }
                }
            }
        }
        if let Some(artifact) = &run.report
            && ui.button("Read run report").clicked()
            && let Some(client) = cx.client.as_deref_mut()
        {
            let reference = artifact.clone();
            let project = match &run.scope {
                DataScope::Workspace => None,
                DataScope::Project { project_id } => Some(project_id.clone()),
            };
            client.task(
                DreamingSlot::DreamReportText(self.project.clone(), run.id.clone()).into(),
                "Reading Dreaming report",
                move |backend| read_report(backend, project.as_ref(), &reference),
            );
        }
        if let Some(text) = &self.report_text {
            ui.label(text);
        }
        for proposal in &run.proposal_ids.0 {
            if ui
                .link(format!("Review proposal {}", proposal.as_str()))
                .clicked()
            {
                cx.navigate(Route::Approvals);
                cx.effects
                    .push(Effect::InspectApproval(proposal.as_str().to_owned()));
            }
        }
    }
}

fn edit_id(ui: &mut Ui, label: &str, value: &mut Option<Id>) {
    ui.horizontal(|ui| {
        ui.label(label);
        let mut text = value
            .as_ref()
            .map_or_else(String::new, |id| id.as_str().to_owned());
        if ui.text_edit_singleline(&mut text).changed()
            && (text.is_empty() || Id::new(&text).is_ok())
        {
            *value = Id::new(text).ok();
        }
    });
}
fn number(ui: &mut Ui, label: &str, value: &mut i64, range: std::ops::RangeInclusive<i64>) -> bool {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(value).range(range)).changed()
    })
    .inner
}
fn edit_counter(
    ui: &mut Ui,
    label: &str,
    value: &mut Option<Counter>,
    range: std::ops::RangeInclusive<i64>,
) {
    let mut proposed = value.as_ref().map_or(*range.start(), Counter::get);
    if number(ui, label, &mut proposed, range) {
        *value = Counter::new(proposed).ok();
    }
}
fn edit_positive(
    ui: &mut Ui,
    label: &str,
    value: &mut Option<PositiveCounter>,
    range: std::ops::RangeInclusive<i64>,
) {
    let mut proposed = value
        .as_ref()
        .and_then(|v| v.0.parse().ok())
        .unwrap_or(*range.start());
    if number(ui, label, &mut proposed, range) {
        *value = Some(PositiveCounter(proposed.to_string()));
    }
}

fn prepare_text(
    backend: &dyn Backend,
    project: Option<&str>,
    text: &str,
) -> Result<TaskOutput, String> {
    let Some(project) = project else {
        return crate::instructions_ops::prepare_save(backend, "Dreaming instructions", text);
    };
    if text.trim().is_empty() || text.len() > 65_536 {
        return Err("Enter up to 65,536 bytes of instruction text.".into());
    }
    let id = ProjectId(Id::new(project).map_err(|_| "Invalid project.")?);
    let staged = backend.stage_project_text(Some(&id), "md", text.as_bytes())?;
    let built = backend.mint_project_upload(&id, &staged).map(|ticket| {
        ops::save_project_artifact(
            backend,
            project,
            ticket.as_str(),
            "Dreaming instructions",
            "text/markdown",
            &devworks_core::fingerprint::sha256_hex(text.as_bytes()),
            text.len() as u64,
        )
    });
    match built {
        Ok(frame) => Ok(TaskOutput::Instruction(TaskResult::Prepared {
            frame,
            staged,
        })),
        Err(error) => {
            crate::attachments::remove_owned(&staged, &backend.scratch_dir());
            Err(error)
        }
    }
}

fn read_report(
    backend: &dyn Backend,
    project: Option<&ProjectId>,
    reference: &devworks_core::protocol::provider::ArtifactRef,
) -> Result<TaskOutput, String> {
    let raw = ops::read_artifact_bytes_in(backend, project, reference)?;
    let report: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
    let mut text = String::new();
    if let Some(narrative) = report["narrative"].as_str() {
        text.push_str(narrative);
    } else if !report["modelResult"].is_null() {
        let result = serde_json::from_value(report["modelResult"].clone())
            .map_err(|error| error.to_string())?;
        let raw = ops::read_artifact_bytes_in(backend, project, &result)?;
        let bodies: Vec<devworks_core::protocol::provider::ProviderBody> =
            serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
        for body in bodies {
            if let devworks_core::protocol::provider::ProviderBody::TextDelta {
                text: delta, ..
            } = body
            {
                text.push_str(&delta);
            }
        }
    }
    if let Some(actions) = report["actions"].as_array() {
        for action in actions.iter().rev() {
            let Some(tool) = action["tool"].as_str() else {
                continue;
            };
            use std::fmt::Write;
            let label = action["error"]
                .as_str()
                .unwrap_or(if action["changed"] == true {
                    "changed Knowledge"
                } else {
                    "journaled"
                });
            let _ = write!(text, "\n{}  {tool}  {label}", action["position"]);
        }
    }
    Ok(TaskOutput::DreamingReport(text))
}
