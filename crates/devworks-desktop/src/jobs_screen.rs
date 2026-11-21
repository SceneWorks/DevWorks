//! Scheduled jobs (DW-50, JOB-01/02): the Schedules subview lists the app-level jobs or one
//! project's, with a right inner sidebar for the selected job (schedule, next and last run, hold
//! and authorization, the active run) and its controls (Run now, Enable, Disable, Edit, Delete,
//! Cancel the active run); the Runs subview pages the selected job's run history with each run's
//! status, trigger, occurrence and reason. The create/edit form builds the same `jobs.create` /
//! `jobs.edit` input an agent tool submits; nothing here decides what the job service decides
//! (authorization, occurrences, overlap, holds). A command whose outcome is not known keeps its
//! exact frame (Confirm outcomes, and the next launch, resubmit it).
use crate::frame;
use crate::ops::JobCommand;
use crate::paged::{self, Paged};
use crate::widgets::{self, Tone, chip};
use devworks_core::protocol::helpers::NativeCapability;
use devworks_core::protocol::records::SchemaValue;
use devworks_core::protocol::records::{
    ArtifactRecord, EffortName, JobDefinition, JobHold, JobInspection, JobRecord, JobRun,
    JobRunStatus, RouteCatalogModel, RouteIntent, Schedule,
};
use devworks_core::protocol::support::DynamicJson;
use devworks_core::protocol::{Counter, ProjectId};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};

/// The subviews (the screen registry's order).
pub const TABS: [&str; 2] = ["Schedules", "Runs"];
pub(crate) const SCHEDULE_KINDS: [&str; 3] = ["Manual", "Interval", "Calendar (cron)"];
pub(crate) const TIERS: [&str; 4] = ["Max", "High", "Mid", "Low"];
/// How an inference task is routed: a tier's pool, one named model, or OpenRouter's own route.
pub const ROUTES: [&str; 5] = [
    "Tier",
    "Named model",
    "Open Router Auto",
    "DevWorks Auto",
    "Open Router Auto Beta",
];
/// Effort levels (`Default` lets the provider choose).
pub const EFFORTS: [&str; 7] = [
    "Default", "Minimal", "Low", "Medium", "High", "XHigh", "Max",
];

/// A job being written: a new one, or an edit of the record it was opened on (submitted at that
/// revision, so a concurrent change is a revision conflict, never silently overwritten). Every
/// member of the definition is either shown in the form or kept from the record exactly: an edit
/// never resets the route, effort, target, pinned prompt version or capability it does not show.
#[derive(Clone, Debug)]
pub struct JobForm {
    pub editing: Option<JobRecord>,
    pub title: String,
    pub schedule_kind: usize,
    pub seconds: String,
    pub expression: String,
    pub timezone: String,
    /// 0: a native maintenance capability; 1: a scheduled inference task.
    pub definition_kind: usize,
    /// The chosen capability (its ID); `None` until one is chosen.
    pub capability: Option<String>,
    /// The capability's arguments (its input body, JSON).
    pub arguments: String,
    /// The saved prompt (an artifact's ID). An edit keeps the exact version it pinned unless
    /// another artifact is chosen.
    pub prompt: Option<String>,
    /// Index into [`ROUTES`], with the tier or named model it uses.
    pub route_kind: usize,
    pub tier: usize,
    pub model: String,
    /// Index into [`EFFORTS`].
    pub effort: usize,
    /// An app-level task's target project and an optional feature (IDs, empty for none).
    pub target_project: String,
    pub feature: String,
    pub enabled: bool,
}
impl Default for JobForm {
    fn default() -> Self {
        Self {
            editing: None,
            title: String::new(),
            schedule_kind: 2,
            seconds: "3600".into(),
            expression: "0 3 * * *".into(),
            timezone: jiff::tz::TimeZone::system()
                .iana_name()
                .unwrap_or("UTC")
                .to_owned(),
            definition_kind: 1,
            capability: None,
            arguments: "{}".into(),
            prompt: None,
            route_kind: 0,
            tier: 1,
            model: String::new(),
            effort: 0,
            target_project: String::new(),
            feature: String::new(),
            enabled: true,
        }
    }
}
impl JobForm {
    pub(crate) fn purge_owner<'a>(&'a self, new_scope: &'a Option<String>) -> Option<&'a str> {
        match &self.editing {
            Some(job) => match &job.scope {
                Some(devworks_core::protocol::data::DataScope::Project { project_id }) => {
                    Some(project_id.0.as_str())
                }
                _ => None,
            },
            None => new_scope.as_deref(),
        }
    }

    /// A form opened on an existing job, every definition member carried over.
    pub fn edit(job: &JobRecord) -> Self {
        let mut f = Self {
            editing: Some(job.clone()),
            title: job.title.0.clone(),
            enabled: job.enabled,
            ..Self::default()
        };
        match &job.schedule {
            Schedule::Manual {} => f.schedule_kind = 0,
            Schedule::Interval { seconds } => {
                f.schedule_kind = 1;
                f.seconds = seconds.0.clone();
            }
            Schedule::Calendar {
                expression,
                timezone,
            } => {
                f.schedule_kind = 2;
                f.expression = expression.0.clone();
                f.timezone = timezone.0.clone();
            }
        }
        match &job.definition {
            JobDefinition::Native {
                capability_id,
                arguments,
            } => {
                f.definition_kind = 0;
                f.capability = Some(capability_id.as_str().to_owned());
                f.arguments = serde_json::to_string_pretty(&arguments.value.0).unwrap_or_default();
            }
            JobDefinition::Inference {
                prompt,
                route,
                effort,
                project_id,
                feature_id,
            } => {
                f.definition_kind = 1;
                f.prompt = Some(prompt.artifact_id.as_str().to_owned());
                let r = serde_json::to_value(route).unwrap_or_default();
                match r["kind"].as_str() {
                    Some("Model") => {
                        f.route_kind = 1;
                        f.model = r["modelId"].as_str().unwrap_or_default().to_owned();
                    }
                    Some("OpenRouterAuto") => f.route_kind = 2,
                    Some("DevWorksAuto") => f.route_kind = 3,
                    Some("OpenRouterAutoBeta") => f.route_kind = 4,
                    _ => {
                        f.route_kind = 0;
                        f.tier = TIERS
                            .iter()
                            .position(|t| r["tier"] == json!(t))
                            .unwrap_or(1);
                    }
                }
                let e = serde_json::to_value(effort).unwrap_or_default();
                f.effort = EFFORTS.iter().position(|x| e == json!(x)).unwrap_or(0);
                f.target_project = project_id
                    .as_ref()
                    .map(|p| p.0.as_str().to_owned())
                    .unwrap_or_default();
                f.feature = feature_id
                    .as_ref()
                    .map(|r| r.0.as_str().to_owned())
                    .unwrap_or_default();
            }
        }
        f
    }
    /// The definition the form was opened on, if any.
    fn original(&self) -> Option<&JobDefinition> {
        self.editing.as_ref().map(|j| &j.definition)
    }
}

/// The schedule and definition a form describes, or why it is incomplete. Values the job
/// service validates (the expression, the zone, the arguments, the targets) are passed through
/// as written: its refusal is shown, never second-guessed here.
pub fn form_values(
    form: &JobForm,
    caps: &[NativeCapability],
    artifacts: &[ArtifactRecord],
) -> Result<(Schedule, JobDefinition), String> {
    if form.title.trim().is_empty() {
        return Err("Give the job a title.".into());
    }
    let schedule = match form.schedule_kind {
        0 => Schedule::Manual {},
        1 => {
            let s = form.seconds.trim();
            if s.parse::<u64>().map_or(true, |n| n == 0) {
                return Err("The interval is a whole number of seconds.".into());
            }
            Schedule::Interval {
                seconds: frame::text(s).ok_or("The interval is a whole number of seconds.")?,
            }
        }
        _ => Schedule::Calendar {
            expression: frame::text(form.expression.trim())
                .ok_or("Check the schedule expression.")?,
            timezone: frame::text(form.timezone.trim()).ok_or("Check the schedule time zone.")?,
        },
    };
    let definition = if form.definition_kind == 0 {
        let value: Value = serde_json::from_str(&form.arguments)
            .map_err(|e| format!("The arguments are not valid JSON: {e}."))?;
        if !value.is_object() {
            return Err("The arguments are a JSON object.".into());
        }
        let Some(chosen) = &form.capability else {
            return Err(if caps.is_empty() {
                "No maintenance capability is available in this build.".into()
            } else {
                "Choose the maintenance capability the job runs.".into()
            });
        };
        // The capability as this build offers it; one the build no longer offers keeps the
        // schema it was saved with (the job service decides whether it still runs).
        let schema = match caps.iter().find(|c| c.capability_id.as_str() == chosen) {
            Some(cap) => cap.schema.clone(),
            None => match form.original() {
                Some(JobDefinition::Native {
                    capability_id,
                    arguments,
                }) if capability_id.as_str() == chosen => arguments.schema.clone(),
                _ => return Err("No maintenance capability is available in this build.".into()),
            },
        };
        JobDefinition::Native {
            capability_id: frame::id(chosen).ok_or("Check the maintenance capability.")?,
            arguments: SchemaValue {
                schema,
                value: DynamicJson(value),
            },
        }
    } else {
        let Some(chosen) = &form.prompt else {
            return Err("Choose the saved prompt (an artifact) the task runs.".into());
        };
        // The prompt the job was saved with keeps its exact (pinned) version; another artifact
        // is taken at its current version.
        let prompt = match form.original() {
            Some(JobDefinition::Inference { prompt, .. })
                if prompt.artifact_id.as_str() == chosen =>
            {
                prompt.clone()
            }
            _ => match artifacts
                .iter()
                .find(|a| a.artifact.artifact_id.as_str() == chosen)
            {
                Some(a) => a.artifact.clone(),
                None => return Err("Choose the saved prompt (an artifact) the task runs.".into()),
            },
        };
        let route = match form.route_kind {
            1 => {
                let m = form.model.trim();
                if m.is_empty() {
                    return Err("Name the model the task is routed to.".into());
                }
                RouteIntent::Model {
                    model_id: frame::id(m).ok_or("Check the model the task is routed to.")?,
                }
            }
            2 => RouteIntent::OpenRouterAuto {},
            3 => RouteIntent::DevWorksAuto {},
            4 => RouteIntent::OpenRouterAutoBeta {},
            _ => RouteIntent::Tier {
                tier: frame::text(TIERS[form.tier.min(TIERS.len() - 1)])
                    .ok_or("Check the routing tier.")?,
            },
        };
        let target = |value: &str| (!value.trim().is_empty()).then(|| value.trim().to_owned());
        JobDefinition::Inference {
            prompt,
            route,
            effort: frame::text(EFFORTS[form.effort.min(EFFORTS.len() - 1)])
                .ok_or("Check the effort.")?,
            project_id: frame::lift(
                target(&form.target_project).map(|p| frame::id(&p).map(ProjectId)),
            )
            .ok_or("Check the target project.")?,
            feature_id: frame::lift(target(&form.feature).map(|f| frame::text(&f)))
                .ok_or("Check the target feature.")?,
        }
    };
    Ok((schedule, definition))
}

/// Reconciles loaded older history pages with a refreshed first page (newest first): runs of the
/// previous first page that the refreshed one no longer holds moved past its end, so they lead
/// the older pages (which continue after the previous first page); a run now on the first page
/// is not repeated among them.
pub fn reconcile_history(before: &[JobRun], now: &[JobRun], older: &mut Vec<JobRun>) {
    let on_page = |r: &JobRun| now.iter().any(|n| n.id == r.id);
    let mut merged: Vec<JobRun> = before.iter().filter(|r| !on_page(r)).cloned().collect();
    merged.extend(
        older
            .drain(..)
            .filter(|r| !on_page(r) && !before.iter().any(|b| b.id == r.id)),
    );
    *older = merged;
}

/// The schedule in words.
pub fn describe_schedule(s: &Schedule) -> String {
    match s {
        Schedule::Manual {} => "Manual (Run now only)".into(),
        Schedule::Interval { seconds } => format!("Every {} s", seconds.0),
        Schedule::Calendar {
            expression,
            timezone,
        } => format!("{} ({})", expression.0, timezone.0),
    }
}
/// A stored instant in the user's local time, or a dash when none is recorded.
pub fn local_time(at: Option<&Counter>) -> String {
    at.map_or_else(|| "—".into(), |c| crate::widgets::local_time(c.get()))
}
pub(crate) fn status_label(s: Option<&JobRunStatus>) -> (&'static str, Tone) {
    match s {
        Some(JobRunStatus::Queued) => ("Queued", Tone::Accent),
        Some(JobRunStatus::Running) => ("Running", Tone::Accent),
        Some(JobRunStatus::Holding) => ("Holding", Tone::Attention),
        Some(JobRunStatus::Succeeded) => ("Succeeded", Tone::Good),
        Some(JobRunStatus::Failed) => ("Failed", Tone::Bad),
        Some(JobRunStatus::Skipped) => ("Skipped", Tone::Neutral),
        Some(JobRunStatus::Cancelled) => ("Cancelled", Tone::Neutral),
        Some(JobRunStatus::OutcomeUnknown) => ("Outcome unknown", Tone::Bad),
        None => ("Never run", Tone::Neutral),
    }
}
/// Why an enabled job is not scheduling, in words.
pub fn hold_text(h: &JobHold) -> &'static str {
    match h {
        JobHold::ReactivationRequired => {
            "Stopped after a restore: enable it to reactivate the schedule."
        }
        JobHold::AuthorizationRequired => "Waiting for your authorization: enable it to run.",
        JobHold::ProjectClosed => "Stopped: its project is archived.",
    }
}
pub(crate) fn is_active(r: &JobRun) -> bool {
    matches!(
        r.status,
        Some(JobRunStatus::Queued | JobRunStatus::Running | JobRunStatus::Holding)
    )
}

/// Scheduled jobs state.
#[derive(Default)]
pub struct JobsUi {
    /// The scope's jobs: a refresh re-reads every page shown, so an expanded list stays expanded.
    pub list: Paged<JobRecord>,
    /// The listed scope: a project's jobs, or the app-level ones (`None`).
    pub scope: Option<String>,
    pub selected: Option<String>,
    pub inspection: Option<JobInspection>,
    pub inspection_error: Option<String>,
    /// The selected job's active run (from its `activeRunId`, inspected by ID: the latest
    /// occurrence may be a skipped one while it continues).
    pub active_run: Option<JobRun>,
    /// History pages loaded after the first, and where the next one starts.
    pub older: Paged<JobRun>,
    pub form: Option<JobForm>,
    pub form_error: Option<String>,
    pub confirm_delete: Option<JobRecord>,
    pub notice: Option<(Tone, String)>,
    pub capabilities: Vec<NativeCapability>,
    /// Scoped prompt pages belong to a single create/edit form, independently of artifacts UI.
    pub prompt_pages: std::collections::BTreeMap<Option<String>, Paged<ArtifactRecord>>,
    /// The form the prompt pages belong to: a reply for an earlier form is refused. (The form's
    /// identity, not a list's request generation.)
    pub prompt_generation: u64,
    /// A one-line run status of each project's jobs, for the project's own views.
    pub project_status: std::collections::BTreeMap<String, String>,
}

impl JobsUi {
    pub(crate) fn purge_draft(&self) -> Option<(String, serde_json::Value)> {
        let form = self.form.as_ref()?;
        let owner = form.purge_owner(&self.scope).unwrap_or("").to_owned();
        let original = form.editing.as_ref().and_then(|j| match &j.definition {
            JobDefinition::Inference { prompt, .. } => Some(prompt),
            _ => None,
        });
        let changed =
            original.is_none_or(|r| form.prompt.as_deref() != Some(r.artifact_id.as_str()));
        let selected = changed
            .then(|| {
                self.prompt_pages
                    .values()
                    .flat_map(|p| p.items.iter())
                    .find(|r| form.prompt.as_deref() == Some(r.artifact.artifact_id.as_str()))
                    .map(|r| &r.artifact)
            })
            .flatten();
        Some((
            owner,
            serde_json::json!({"form":format!("{form:?}"),"record":form.editing,"selectedPrompt":selected}),
        ))
    }

    /// Selects `job` (`None`: no job). A different selection drops everything held for the
    /// previous one (its inspection, active run and loaded history pages with their cursor), so
    /// nothing of one job is shown or paged as another's. Every selection change goes through
    /// here; the caller loads the newly selected job.
    pub fn select(&mut self, job: Option<String>) {
        if self.selected == job {
            return;
        }
        self.selected = job;
        self.inspection = None;
        self.inspection_error = None;
        self.active_run = None;
        self.older.reset();
    }
    /// Lists `scope`'s jobs (`None`: the app-level ones) from their first page, with no job
    /// selected.
    pub fn set_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
        self.select(None);
        self.list.reset();
    }
}

/// The run status of a scope's jobs in one line.
pub fn status_line(jobs: &[JobRecord], more: bool) -> String {
    if jobs.is_empty() {
        return "No scheduled jobs.".into();
    }
    let enabled = jobs.iter().filter(|j| j.enabled).count();
    let running = jobs.iter().filter(|j| j.active_run_id.is_some()).count();
    let held = jobs
        .iter()
        .filter(|j| j.enabled && j.hold.is_some())
        .count();
    let failed = jobs
        .iter()
        .filter(|j| {
            matches!(
                j.last_run_status,
                Some(JobRunStatus::Failed | JobRunStatus::OutcomeUnknown)
            )
        })
        .count();
    let mut parts = vec![
        format!("{}{} jobs", jobs.len(), if more { "+" } else { "" }),
        format!("{enabled} enabled"),
    ];
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if held > 0 {
        parts.push(format!("{held} held"));
    }
    if failed > 0 {
        parts.push(format!("{failed} last run failed"));
    }
    parts.join(" · ")
}

/// What the screen asks the shell to do.
#[derive(Clone, Debug)]
pub enum JobAction {
    Reload,
    MoreJobs,
    Scope(Option<String>),
    Select(String),
    OlderRuns,
    NewJob,
    MorePrompts(Option<String>),
    RetryPrompts(Option<String>),
    EditJob(JobRecord),
    Save(JobForm),
    Command(JobCommand, JobRecord),
    Cancel(JobRun),
}

pub(crate) enum FormEnd {
    Save,
    Close,
}

pub(crate) fn scope_row(
    ui: &mut Ui,
    jobs: &JobsUi,
    projects: &[(String, String)],
    actions: &mut Vec<JobAction>,
) {
    ui.horizontal(|ui| {
        let current = match &jobs.scope {
            None => "App (all projects)".to_owned(),
            Some(p) => projects
                .iter()
                .find(|(id, _)| id == p)
                .map_or_else(|| p.clone(), |(_, n)| n.clone()),
        };
        egui::ComboBox::from_label("Scope")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(jobs.scope.is_none(), "App (all projects)")
                    .clicked()
                {
                    actions.push(JobAction::Scope(None));
                }
                for (id, name) in projects {
                    if ui
                        .selectable_label(jobs.scope.as_deref() == Some(id), name)
                        .clicked()
                    {
                        actions.push(JobAction::Scope(Some(id.clone())));
                    }
                }
            });
    });
}

pub(crate) fn schedules(ui: &mut Ui, jobs: &JobsUi, actions: &mut Vec<JobAction>) {
    let list = &jobs.list;
    let (mut reload, mut more) = (false, false);
    let ready = paged::gate(ui, &Ok(()), list, "scheduled jobs", &mut reload, &mut more);
    if reload {
        actions.push(JobAction::Reload);
    }
    if more {
        actions.push(JobAction::MoreJobs);
    }
    if !ready {
        return;
    }
    if list.items.is_empty() {
        ui.label(RichText::new("No scheduled jobs").strong());
        widgets::caption(
            ui,
            "Create a maintenance job or a scheduled inference task with New job.",
        );
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("jobs-list")
        .show(ui, |ui| {
            for j in &list.items {
                let selected = jobs.selected.as_deref() == Some(j.id.as_str());
                let state = if !j.enabled {
                    "disabled"
                } else if j.hold.is_some() {
                    "held"
                } else if j.active_run_id.is_some() {
                    "running"
                } else {
                    "enabled"
                };
                let text = format!(
                    "{}  ·  {}  ·  {}  ·  next {}",
                    j.title.0,
                    describe_schedule(&j.schedule),
                    state,
                    local_time(j.next_run_at_ms.as_ref())
                );
                let r = ui.selectable_label(selected, text);
                if r.clicked() {
                    actions.push(JobAction::Select(j.id.as_str().to_owned()));
                }
            }
            let mut more = false;
            paged::load_more_control(ui, list, &mut more);
            if more {
                actions.push(JobAction::MoreJobs);
            }
        });
}

pub(crate) fn run_row(ui: &mut Ui, r: &JobRun, actions: &mut Vec<JobAction>, busy: bool) {
    ui.horizontal_wrapped(|ui| {
        let (label, tone) = status_label(r.status.as_ref());
        chip(ui, label, tone);
        let trigger = match r.trigger {
            Some(devworks_core::protocol::records::JobTrigger::Manual) => "Run now",
            _ => "Scheduled",
        };
        ui.label(format!(
            "{trigger} · {}",
            local_time(r.occurrence_at_ms.as_ref())
        ));
        if let Some(n) = r.resumptions.as_ref().filter(|n| n.get() > 0) {
            widgets::caption(ui, format!("resumed {}×", n.get()));
        }
        if let Some(h) = &r.hold {
            widgets::caption(ui, format!("waiting: {}", h.0));
        }
        if is_active(r) {
            let label = if r.cancel_requested == Some(true) {
                "Cancelling…"
            } else {
                "Cancel"
            };
            if ui
                .add_enabled(
                    !busy && r.cancel_requested != Some(true),
                    egui::Button::new(label),
                )
                .clicked()
            {
                actions.push(JobAction::Cancel(r.clone()));
            }
        }
    });
    if let Some(reason) = &r.reason {
        widgets::caption(ui, reason.0.clone());
    }
}

pub(crate) fn runs(ui: &mut Ui, jobs: &JobsUi, actions: &mut Vec<JobAction>, busy: bool) {
    let Some(i) = &jobs.inspection else {
        widgets::caption(ui, "Select a job on Schedules to see its runs.");
        return;
    };
    ui.label(RichText::new(format!("Runs of “{}”", i.job.title.0)).strong());
    let first = i.runs.as_ref().map(|r| r.0.as_slice()).unwrap_or_default();
    if first.is_empty() {
        widgets::caption(ui, "No runs yet.");
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("job-runs")
        .show(ui, |ui| {
            for r in first.iter().chain(&jobs.older.items) {
                run_row(ui, r, actions, busy);
                widgets::divider(ui);
            }
            let more = if jobs.older.loaded {
                jobs.older.more()
            } else {
                i.runs_next_cursor.is_some()
            };
            if more && widgets::secondary_button(ui, "Older runs").clicked() {
                actions.push(JobAction::OlderRuns);
            }
        });
}

pub(crate) fn details(ui: &mut Ui, jobs: &mut JobsUi, actions: &mut Vec<JobAction>, busy: bool) {
    let Some(i) = jobs.inspection.clone() else {
        ui.label(RichText::new("Job").strong());
        widgets::caption(ui, "Select a job to see its schedule, runs and controls.");
        if let Some(e) = &jobs.inspection_error {
            widgets::caption(ui, e.clone());
        }
        return;
    };
    let j = &i.job;
    ui.label(RichText::new(j.title.0.clone()).strong());
    widgets::caption(ui, describe_schedule(&j.schedule));
    let kind = match &j.definition {
        JobDefinition::Native { capability_id, .. } => {
            format!("Maintenance: {}", capability_id.as_str())
        }
        JobDefinition::Inference { .. } => "Scheduled inference task".to_owned(),
    };
    widgets::caption(ui, kind);
    ui.add_space(6.0);
    if !j.enabled {
        chip(ui, "Disabled", Tone::Neutral);
    } else if let Some(h) = &j.hold {
        chip(ui, hold_text(h), Tone::Attention);
    } else {
        chip(ui, "Enabled", Tone::Good);
    }
    if j.authorized == Some(false) {
        chip(ui, "Not authorized by you yet", Tone::Attention);
    }
    ui.label(format!(
        "Next run: {}",
        local_time(j.next_run_at_ms.as_ref())
    ));
    let (status, tone) = status_label(j.last_run_status.as_ref());
    ui.horizontal(|ui| {
        ui.label("Last run:");
        chip(ui, status, tone);
        ui.label(local_time(j.last_run_started_at_ms.as_ref()));
    });
    if let Some(e) = &j.last_error {
        widgets::caption(ui, e.0.clone());
    }
    if let Some(run) = jobs.active_run.clone().filter(is_active) {
        ui.add_space(4.0);
        ui.label(RichText::new("Active run").strong());
        run_row(ui, &run, actions, busy);
    }
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new("Run now"))
            .clicked()
        {
            actions.push(JobAction::Command(JobCommand::RunNow, j.clone()));
        }
        if (!j.enabled || j.hold.is_some() || j.authorized == Some(false))
            && ui.add_enabled(!busy, egui::Button::new("Enable")).clicked()
        {
            actions.push(JobAction::Command(JobCommand::Enable, j.clone()));
        }
        if j.enabled
            && ui
                .add_enabled(!busy, egui::Button::new("Disable"))
                .clicked()
        {
            actions.push(JobAction::Command(JobCommand::Disable, j.clone()));
        }
        if ui.add_enabled(!busy, egui::Button::new("Edit")).clicked() {
            actions.push(JobAction::EditJob(j.clone()));
        }
        if ui
            .add_enabled(!busy, egui::Button::new("Delete…"))
            .clicked()
        {
            jobs.confirm_delete = Some(j.clone());
        }
    });
    widgets::caption(
        ui,
        "Scheduled runs never catch up missed occurrences, and an occurrence while a run is active is skipped.",
    );
}

/// A labelled text input (the label is its accessible name).
pub(crate) fn field(ui: &mut Ui, value: &mut String, label: &str, multiline: bool) {
    ui.label(label);
    let edit = if multiline {
        egui::TextEdit::multiline(value).desired_rows(4)
    } else {
        egui::TextEdit::singleline(value)
    };
    let r = ui.add(edit.desired_width(f32::INFINITY));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
}

pub(crate) fn form_ui(
    ui: &mut Ui,
    form: &mut JobForm,
    caps: &[NativeCapability],
    artifacts: &[ArtifactRecord],
    catalog: &[RouteCatalogModel],
    error: Option<&str>,
    busy: bool,
) -> Option<FormEnd> {
    let mut end = None;
    ui.label(
        RichText::new(if form.editing.is_some() {
            "Edit job"
        } else {
            "New job"
        })
        .strong(),
    );
    field(ui, &mut form.title, "Title", false);
    egui::ComboBox::from_label("Schedule")
        .selected_text(SCHEDULE_KINDS[form.schedule_kind.min(2)])
        .show_ui(ui, |ui| {
            for (i, k) in SCHEDULE_KINDS.iter().enumerate() {
                ui.selectable_value(&mut form.schedule_kind, i, *k);
            }
        });
    match form.schedule_kind {
        1 => {
            field(ui, &mut form.seconds, "Every (seconds, at least 60)", false);
        }
        2 => {
            field(
                ui,
                &mut form.expression,
                "Cron expression (minute hour day month weekday)",
                false,
            );
            field(
                ui,
                &mut form.timezone,
                "Time zone (IANA, e.g. America/New_York)",
                false,
            );
        }
        _ => {}
    }
    ui.add_space(4.0);
    ui.label("What it runs");
    ui.horizontal(|ui| {
        ui.selectable_value(&mut form.definition_kind, 1, "Inference task");
        ui.selectable_value(&mut form.definition_kind, 0, "Maintenance");
    });
    if form.definition_kind == 0 {
        if caps.is_empty() && form.capability.is_none() {
            widgets::caption(
                ui,
                "No maintenance capability is available in this build; the items that deliver them add them here.",
            );
        } else {
            // A saved capability this build no longer offers is still shown (by its ID), never
            // silently replaced by the first one offered.
            let current = form
                .capability
                .as_deref()
                .map_or("Choose a capability", |id| {
                    caps.iter()
                        .find(|c| c.capability_id.as_str() == id)
                        .map_or(id, |c| c.operation)
                });
            egui::ComboBox::from_label("Capability")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for c in caps {
                        let id = c.capability_id.as_str().to_owned();
                        if ui
                            .selectable_label(form.capability.as_deref() == Some(&id), c.operation)
                            .clicked()
                        {
                            form.capability = Some(id);
                        }
                    }
                });
            field(ui, &mut form.arguments, "Arguments (JSON)", true);
        }
    } else {
        let current = form
            .prompt
            .as_ref()
            .and_then(|p| {
                artifacts
                    .iter()
                    .find(|a| a.artifact.artifact_id.as_str() == p)
            })
            .map_or(
                form.prompt.as_deref().unwrap_or("Choose an artifact"),
                |a| a.name.0.as_str(),
            );
        egui::ComboBox::from_label("Saved prompt")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for a in artifacts {
                    let id = a.artifact.artifact_id.as_str().to_owned();
                    if ui
                        .selectable_label(form.prompt.as_deref() == Some(&id), a.name.0.as_str())
                        .clicked()
                    {
                        form.prompt = Some(id);
                    }
                }
            });
        if let Some(JobDefinition::Inference { prompt, .. }) = form.original()
            && form.prompt.as_deref() == Some(prompt.artifact_id.as_str())
        {
            widgets::caption(
                ui,
                format!(
                    "Runs the version it was saved with ({}); choose another prompt to take a current version.",
                    prompt.version_id.as_str()
                ),
            );
        }
        egui::ComboBox::from_label("Routing")
            .selected_text(ROUTES[form.route_kind.min(ROUTES.len() - 1)])
            .show_ui(ui, |ui| {
                for (i, r) in ROUTES.iter().enumerate() {
                    ui.selectable_value(&mut form.route_kind, i, *r);
                }
            });
        match form.route_kind {
            0 => {
                egui::ComboBox::from_label("Routing tier")
                    .selected_text(TIERS[form.tier.min(TIERS.len() - 1)])
                    .show_ui(ui, |ui| {
                        for (i, t) in TIERS.iter().enumerate() {
                            ui.selectable_value(&mut form.tier, i, *t);
                        }
                    });
            }
            1 => {
                let selected = if form.model.is_empty() {
                    "Choose a model".into()
                } else {
                    devworks_core::protocol::Id::new(&form.model)
                        .ok()
                        .map(|model_id| {
                            crate::routing::intent_label(catalog, &RouteIntent::Model { model_id })
                        })
                        .unwrap_or_else(|| form.model.clone())
                };
                egui::ComboBox::from_label("Model")
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for (label, intent) in crate::routing::options(catalog) {
                            if let RouteIntent::Model { model_id } = intent {
                                ui.selectable_value(
                                    &mut form.model,
                                    model_id.as_str().to_owned(),
                                    label,
                                );
                            }
                        }
                    });
                // A saved legacy ID remains inspectable and is never replaced by whichever
                // inventory entry now occupies its previous position.
                if !form.model.is_empty() {
                    widgets::caption(ui, format!("Model ID: {}", form.model));
                }
            }
            _ => {}
        }
        let intent = serde_json::from_value::<RouteIntent>(match form.route_kind {
            0 => json!({"kind":"Tier", "tier":TIERS[form.tier.min(TIERS.len()-1)]}),
            1 => json!({"kind":"Model", "modelId": if form.model.is_empty() { "unselected" } else { &form.model }}),
            3 => json!({"kind":"DevWorksAuto"}),
            4 => json!({"kind":"OpenRouterAutoBeta"}),
            _ => json!({"kind":"OpenRouterAuto"}),
        }).ok();
        let offered = intent
            .as_ref()
            .map(|i| crate::routing::efforts(catalog, i))
            .unwrap_or_else(|| vec![EffortName::Default]);
        let chosen: EffortName =
            serde_json::from_value(json!(EFFORTS[form.effort.min(EFFORTS.len() - 1)]))
                .expect("effort");
        egui::ComboBox::from_label("Effort")
            .selected_text(crate::routing::effort_label(&chosen))
            .show_ui(ui, |ui| {
                for effort in &offered {
                    let wire = serde_json::to_value(effort).unwrap();
                    let index = EFFORTS
                        .iter()
                        .position(|e| wire == json!(e))
                        .expect("known effort");
                    ui.selectable_value(
                        &mut form.effort,
                        index,
                        crate::routing::effort_label(effort),
                    );
                }
            });
        if !offered
            .iter()
            .any(|e| serde_json::to_value(e).ok() == serde_json::to_value(&chosen).ok())
        {
            widgets::caption(
                ui,
                "The saved effort is retained; no current model for this route declares support for it.",
            );
        }
        field(
            ui,
            &mut form.target_project,
            "Target project (its ID; empty for none)",
            false,
        );
        field(
            ui,
            &mut form.feature,
            "Target feature (its ID; empty for none)",
            false,
        );
        widgets::caption(
            ui,
            "The task runs as a scheduled agent with your configured tools and approvals; usage is reported per run with no job spending cap.",
        );
    }
    if form.editing.is_none() {
        ui.checkbox(&mut form.enabled, "Enabled (you authorize this schedule)");
    }
    if let Some(e) = error {
        chip(ui, e, Tone::Bad);
    }
    ui.horizontal(|ui| {
        if ui.add_enabled(!busy, egui::Button::new("Save")).clicked() {
            end = Some(FormEnd::Save);
        }
        if widgets::secondary_button(ui, "Close").clicked() {
            end = Some(FormEnd::Close);
        }
    });
    end
}
