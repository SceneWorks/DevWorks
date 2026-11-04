//! Scheduled jobs (DW-50, JOB-01/02): the Schedules subview lists the app-level jobs or one
//! project's, with a right inner sidebar for the selected job (schedule, next and last run, hold
//! and authorization, the active run) and its controls (Run now, Enable, Disable, Edit, Delete,
//! Cancel the active run); the Runs subview pages the selected job's run history with each run's
//! status, trigger, occurrence and reason. The create/edit form builds the same `jobs.create` /
//! `jobs.edit` input an agent tool submits; nothing here decides what the job service decides
//! (authorization, occurrences, overlap, holds). A command whose outcome is not known keeps its
//! exact frame (Confirm outcomes, and the next launch, resubmit it).
use super::{ListState, Shell};
use crate::backend::Slot;
use crate::ops::{self, Failure, JobChange};
use crate::route::Route;
use crate::theme;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::records::{
    ArtifactRecord, JobDefinition, JobHold, JobInspection, JobRecord, JobRun, JobRunStatus,
    Schedule,
};
use devworks_core::protocol::{Counter, DomainErrorCode};
use devworks_core::services::jobs::NativeCapability;
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};

/// The subviews (the screen registry's order).
pub const TABS: [&str; 2] = ["Schedules", "Runs"];
const SCHEDULE_KINDS: [&str; 3] = ["Manual", "Interval", "Calendar (cron)"];
const TIERS: [&str; 4] = ["Max", "High", "Mid", "Low"];

/// A job being written: a new one, or an edit of the record it was opened on (submitted at that
/// revision, so a concurrent change is a revision conflict, never silently overwritten).
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
    pub capability: usize,
    /// The capability's arguments (its input body, JSON).
    pub arguments: String,
    /// The saved prompt (an artifact's ID) and routing tier of an inference task.
    pub prompt: Option<String>,
    pub tier: usize,
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
            capability: 0,
            arguments: "{}".into(),
            prompt: None,
            tier: 1,
            enabled: true,
        }
    }
}
impl JobForm {
    /// A form opened on an existing job.
    pub fn edit(job: &JobRecord, caps: &[NativeCapability]) -> Self {
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
                f.capability = caps
                    .iter()
                    .position(|c| &c.capability_id == capability_id)
                    .unwrap_or(0);
                f.arguments = serde_json::to_string_pretty(&arguments.value.0).unwrap_or_default();
            }
            JobDefinition::Inference { prompt, route, .. } => {
                f.definition_kind = 1;
                f.prompt = Some(prompt.artifact_id.as_str().to_owned());
                let r = serde_json::to_value(route).unwrap_or_default();
                f.tier = TIERS
                    .iter()
                    .position(|t| r["tier"] == json!(t))
                    .unwrap_or(1);
            }
        }
        f
    }
}

/// The wire schedule and definition a form describes, or why it is incomplete. Values the job
/// service validates (the expression, the zone, the arguments) are passed through as written:
/// its refusal is shown, never second-guessed here.
pub fn form_values(
    form: &JobForm,
    caps: &[NativeCapability],
    artifacts: &[ArtifactRecord],
) -> Result<(Value, Value), String> {
    if form.title.trim().is_empty() {
        return Err("Give the job a title.".into());
    }
    let schedule = match form.schedule_kind {
        0 => json!({"kind": "Manual"}),
        1 => {
            let s = form.seconds.trim();
            if s.parse::<u64>().map_or(true, |n| n == 0) {
                return Err("The interval is a whole number of seconds.".into());
            }
            json!({"kind": "Interval", "seconds": s})
        }
        _ => json!({"kind": "Calendar", "expression": form.expression.trim(),
            "timezone": form.timezone.trim()}),
    };
    let definition = if form.definition_kind == 0 {
        let Some(cap) = caps.get(form.capability) else {
            return Err("No maintenance capability is available in this build.".into());
        };
        let value: Value = serde_json::from_str(&form.arguments)
            .map_err(|e| format!("The arguments are not valid JSON: {e}."))?;
        if !value.is_object() {
            return Err("The arguments are a JSON object.".into());
        }
        json!({"kind": "Native", "capabilityId": cap.capability_id, "arguments": {
            "schema": cap.schema, "value": value}})
    } else {
        let Some(artifact) = form.prompt.as_ref().and_then(|p| {
            artifacts
                .iter()
                .find(|a| a.artifact.artifact_id.as_str() == p)
        }) else {
            return Err("Choose the saved prompt (an artifact) the task runs.".into());
        };
        json!({"kind": "Inference", "prompt": artifact.artifact,
            "route": {"kind": "Tier", "tier": TIERS[form.tier.min(TIERS.len() - 1)]},
            "effort": "Default"})
    };
    Ok((schedule, definition))
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
/// A stored instant in the user's local time.
pub fn local_time(at: Option<&Counter>) -> String {
    at.and_then(|c| jiff::Timestamp::from_millisecond(c.get()).ok())
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M %Z")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}
fn status_label(s: Option<&JobRunStatus>) -> (&'static str, Tone) {
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
fn is_active(r: &JobRun) -> bool {
    matches!(
        r.status,
        Some(JobRunStatus::Queued | JobRunStatus::Running | JobRunStatus::Holding)
    )
}

/// Scheduled jobs state.
#[derive(Default)]
pub struct JobsUi {
    pub list: ListState<JobRecord>,
    /// Where the next page of the list starts (`None`: every job is listed).
    pub list_cursor: Option<String>,
    /// The listed scope: a project's jobs, or the app-level ones (`None`).
    pub scope: Option<String>,
    pub selected: Option<String>,
    pub inspection: Option<JobInspection>,
    pub inspection_error: Option<String>,
    /// History pages loaded after the first, and where the next one starts.
    pub older: Vec<JobRun>,
    pub older_cursor: Option<String>,
    pub form: Option<JobForm>,
    pub form_error: Option<String>,
    pub confirm_delete: Option<JobRecord>,
    pub notice: Option<(Tone, String)>,
    pub capabilities: Vec<NativeCapability>,
    /// A one-line run status of each project's jobs, for the project's own views.
    pub project_status: std::collections::BTreeMap<String, String>,
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
    EditJob(JobRecord),
    Save(JobForm),
    Command(&'static str, JobRecord),
    Cancel(JobRun),
}

impl Shell {
    pub fn load_jobs(&mut self) {
        let scope = self.jobs.scope.clone();
        self.query(
            Slot::JobsList,
            "Loading scheduled jobs",
            "jobs.list",
            move |b| ops::jobs_list(b, scope.as_deref()),
        );
        if let Some(j) = self.jobs.selected.clone() {
            self.load_job(&j);
        }
    }
    /// The run status of a project's jobs, for its sidebar on All projects.
    pub(super) fn load_project_jobs(&mut self, project: &str) {
        let p = project.to_owned();
        self.query(
            Slot::ProjectJobs(project.to_owned()),
            "Loading the project's scheduled jobs",
            "jobs.list",
            move |b| ops::jobs_list(b, Some(&p)),
        );
    }
    fn load_job(&mut self, job: &str) {
        let j = job.to_owned();
        self.query(
            Slot::JobInspect(job.to_owned()),
            "Loading the job",
            "jobs.inspect",
            move |b| ops::jobs_inspect(b, &j, None),
        );
    }
    fn jobs_act(&mut self, actions: Vec<JobAction>) {
        for a in actions {
            match a {
                JobAction::Reload => self.load_jobs(),
                JobAction::MoreJobs => {
                    let (scope, Some(cursor)) =
                        (self.jobs.scope.clone(), self.jobs.list_cursor.clone())
                    else {
                        continue;
                    };
                    let slot = Slot::JobsMore(scope.clone().unwrap_or_default());
                    self.query(slot, "Loading more jobs", "jobs.list", move |b| {
                        ops::jobs_list_after(b, scope.as_deref(), Some(&cursor))
                    });
                }
                JobAction::Scope(s) => {
                    self.jobs.scope = s;
                    self.jobs.selected = None;
                    self.jobs.inspection = None;
                    self.jobs.list = ListState::default();
                    self.jobs.list_cursor = None;
                    self.load_jobs();
                }
                JobAction::Select(id) => {
                    self.jobs.selected = Some(id.clone());
                    self.jobs.older.clear();
                    self.jobs.older_cursor = None;
                    self.load_job(&id);
                }
                JobAction::OlderRuns => {
                    let (Some(job), Some(cursor)) = (
                        self.jobs.selected.clone(),
                        self.jobs.older_cursor.clone().or_else(|| {
                            self.jobs
                                .inspection
                                .as_ref()
                                .and_then(|i| i.runs_next_cursor.as_ref())
                                .map(|c| c.0.clone())
                        }),
                    ) else {
                        continue;
                    };
                    self.query(
                        Slot::JobHistory(job.clone()),
                        "Loading older runs",
                        "jobs.inspect",
                        move |b| ops::jobs_inspect(b, &job, Some(&cursor)),
                    );
                }
                JobAction::NewJob => {
                    self.jobs.capabilities = self
                        .backend()
                        .map(|b| b.native_job_capabilities())
                        .unwrap_or_default();
                    self.jobs.form = Some(JobForm::default());
                    self.jobs.form_error = None;
                    if !self.artifacts.loaded {
                        self.load_artifacts();
                    }
                }
                JobAction::EditJob(job) => {
                    self.jobs.capabilities = self
                        .backend()
                        .map(|b| b.native_job_capabilities())
                        .unwrap_or_default();
                    self.jobs.form = Some(JobForm::edit(&job, &self.jobs.capabilities));
                    self.jobs.form_error = None;
                    if !self.artifacts.loaded {
                        self.load_artifacts();
                    }
                }
                JobAction::Save(form) => {
                    let values = form_values(&form, &self.jobs.capabilities, &self.artifacts.items);
                    let (schedule, definition) = match values {
                        Ok(v) => v,
                        Err(e) => {
                            self.jobs.form_error = Some(e);
                            continue;
                        }
                    };
                    let title = form.title.trim().to_owned();
                    match form.editing.clone() {
                        Some(job) => {
                            let slot = Slot::JobCommand(job.id.as_str().to_owned());
                            self.command(slot, "Saving the job", "jobs.edit", move |b| {
                                ops::job_edit(b, &job, &title, &schedule, &definition)
                            });
                        }
                        None => {
                            let (scope, enabled) = (self.jobs.scope.clone(), form.enabled);
                            self.command(
                                Slot::JobCreate,
                                "Creating a job",
                                "jobs.create",
                                move |b| {
                                    ops::job_create(
                                        b,
                                        scope.as_deref(),
                                        &title,
                                        &schedule,
                                        &definition,
                                        enabled,
                                    )
                                },
                            );
                        }
                    }
                }
                JobAction::Command(op, job) => {
                    self.jobs.confirm_delete = None;
                    let slot = Slot::JobCommand(job.id.as_str().to_owned());
                    let label = match op {
                        "jobs.run_now" => "Starting a run",
                        "jobs.enable" => "Enabling the job",
                        "jobs.disable" => "Disabling the job",
                        _ => "Deleting the job",
                    };
                    self.command(slot, label, op, move |b| ops::job_command(b, op, &job));
                }
                JobAction::Cancel(run) => {
                    let slot = Slot::JobCommand(run.job_id.as_str().to_owned());
                    self.command(slot, "Cancelling the run", "jobs.cancel", move |b| {
                        ops::job_cancel(b, &run)
                    });
                }
            }
        }
    }

    /// Applies a jobs reply; anything else is handed back.
    pub(super) fn apply_jobs(
        &mut self,
        slot: Slot,
        reply: crate::backend::Reply,
    ) -> Option<(Slot, crate::backend::Reply)> {
        use crate::backend::Reply;
        match (slot, reply) {
            (Slot::JobsList, Reply::Query(q)) => match ops::jobs_page(q) {
                Ok((items, cursor)) => {
                    self.jobs.list = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more: cursor.is_some(),
                    };
                    self.jobs.list_cursor = cursor;
                }
                Err(e) => self.jobs.list.error = Some(e.describe()),
            },
            (Slot::JobsMore(scope), Reply::Query(_))
                if scope != self.jobs.scope.clone().unwrap_or_default() => {}
            (Slot::JobsMore(_), Reply::Query(q)) => match ops::jobs_page(q) {
                Ok((items, cursor)) => {
                    // Appended in title order; a job already shown (renamed meanwhile) once.
                    for j in items {
                        if !self.jobs.list.items.iter().any(|k| k.id == j.id) {
                            self.jobs.list.items.push(j);
                        }
                    }
                    self.jobs.list.more = cursor.is_some();
                    self.jobs.list_cursor = cursor;
                }
                Err(e) => self.jobs.list.error = Some(e.describe()),
            },
            (Slot::ProjectJobs(p), Reply::Query(q)) => {
                let line = match ops::jobs(q) {
                    Ok((items, more)) => status_line(&items, more),
                    Err(e) => e.describe(),
                };
                self.jobs.project_status.insert(p, line);
            }
            (Slot::JobInspect(j), Reply::Query(q)) => {
                if self.jobs.selected.as_deref() != Some(j.as_str()) {
                    return None;
                }
                match ops::job_inspection(q) {
                    Ok(i) => {
                        self.jobs.inspection = Some(i);
                        self.jobs.inspection_error = None;
                    }
                    Err(Failure::Refused(f)) if f.code == DomainErrorCode::NotFound => {
                        self.jobs.selected = None;
                        self.jobs.inspection = None;
                    }
                    Err(e) => self.jobs.inspection_error = Some(e.describe()),
                }
            }
            (Slot::JobHistory(j), Reply::Query(q)) => {
                if self.jobs.selected.as_deref() != Some(j.as_str()) {
                    return None;
                }
                match ops::job_inspection(q) {
                    Ok(i) => {
                        self.jobs
                            .older
                            .extend(i.runs.map(|r| r.0).unwrap_or_default());
                        self.jobs.older_cursor = i.runs_next_cursor.map(|c| c.0);
                        if self.jobs.older_cursor.is_none() {
                            // The last page: nothing more to load.
                            self.jobs.older_cursor = Some(String::new());
                        }
                    }
                    Err(e) => self.jobs.inspection_error = Some(e.describe()),
                }
            }
            (slot @ (Slot::JobCommand(_) | Slot::JobCreate), Reply::Command(c)) => {
                let created = slot == Slot::JobCreate;
                match ops::job_changed(c) {
                    Ok(change) => {
                        let text = match &change {
                            JobChange::Job(j) if created => format!("Created “{}”.", j.title.0),
                            JobChange::Job(j) => format!("Saved “{}”.", j.title.0),
                            JobChange::Run(r) => {
                                format!("Run {}: {}.", r.id.as_str(), status_label(r.status.as_ref()).0)
                            }
                            JobChange::CancelAccepted => {
                                "Cancellation requested; the run's outcome is recorded once it has stopped.".into()
                            }
                        };
                        if let JobChange::Job(j) = &change {
                            if j.deleted_at_ms.is_some() {
                                self.jobs.selected = None;
                                self.jobs.inspection = None;
                            } else if created {
                                self.jobs.selected = Some(j.id.as_str().to_owned());
                            }
                        }
                        self.jobs.form = None;
                        self.jobs.notice = Some((Tone::Good, text));
                        self.load_jobs();
                    }
                    Err(e) => {
                        let text = match &e {
                            Failure::Refused(f) if f.code == DomainErrorCode::ApprovalRequired => {
                                "Only you can authorize a schedule; enable it from this screen."
                                    .to_owned()
                            }
                            Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict => {
                                "The job changed meanwhile; it was reloaded.".to_owned()
                            }
                            other => other.describe(),
                        };
                        if self.jobs.form.is_some() {
                            self.jobs.form_error = Some(text);
                        } else {
                            self.jobs.notice = Some((Tone::Bad, text));
                        }
                        self.load_jobs();
                    }
                }
            }
            other => return Some(other),
        }
        None
    }

    pub(super) fn jobs_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Jobs.screen();
        let online = self.online();
        let mut actions: Vec<JobAction> = Vec::new();
        let mut mem = self.routes.remove(&Route::Jobs).unwrap_or_default();
        let tab = mem.tab.min(TABS.len() - 1);
        let projects: Vec<(String, String)> = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().to_owned(), p.name.0.clone()))
            .collect();
        let busy = self
            .jobs
            .selected
            .as_ref()
            .is_some_and(|j| self.waiting(&Slot::JobCommand(j.clone())))
            || self.waiting(&Slot::JobCreate);
        let artifacts = self.artifacts.items.clone();
        let jobs = &mut self.jobs;
        let mut new_tab = tab;
        {
            let mut st = (&mut *jobs, &mut actions);
            widgets::with_right_sidebar(
                ui,
                theme::SIDEBAR,
                &mut st,
                |ui, st| {
                    let (_, t) = widgets::pane(
                        ui,
                        Header {
                            title: screen.title,
                            summary: screen.summary,
                            tabs: &TABS,
                            tab,
                        },
                        st,
                        |ui, st| {
                            if widgets::secondary_button(ui, "Refresh").clicked() {
                                st.1.push(JobAction::Reload);
                            }
                            if ui
                                .add_enabled(online.is_ok() && !busy, egui::Button::new("New job"))
                                .clicked()
                            {
                                st.1.push(JobAction::NewJob);
                            }
                        },
                        |ui, st, t| {
                            if let Err(reason) = &online {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Offline {
                                        reason: reason.clone(),
                                    },
                                );
                                return;
                            }
                            if let Some((tone, text)) = &st.0.notice {
                                chip(ui, text.clone(), *tone);
                            }
                            scope_row(ui, st.0, &projects, st.1);
                            ui.add_space(6.0);
                            if t == 0 {
                                schedules(ui, st.0, st.1);
                            } else {
                                runs(ui, st.0, st.1, busy);
                            }
                        },
                    );
                    new_tab = t;
                },
                |ui, st| {
                    if let Some(form) = st.0.form.as_mut() {
                        let caps = st.0.capabilities.clone();
                        let error = st.0.form_error.clone();
                        if let Some(done) =
                            form_ui(ui, form, &caps, &artifacts, error.as_deref(), busy)
                        {
                            match done {
                                FormEnd::Save => st.1.push(JobAction::Save(form.clone())),
                                FormEnd::Close => st.0.form = None,
                            }
                        }
                    } else {
                        details(ui, st.0, st.1, busy);
                    }
                },
            );
        }
        mem.tab = new_tab;
        self.routes.insert(Route::Jobs, mem);
        let ctx = ui.ctx().clone();
        if let Some(job) = self.jobs.confirm_delete.clone() {
            let mut open = true;
            let mut decided = None;
            egui::Window::new("Delete scheduled job")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(format!(
                        "Delete “{}”? It stops scheduling, its active run is cancelled, and its run history is kept.",
                        job.title.0
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            decided = Some(true);
                        }
                        if widgets::secondary_button(ui, "Keep").clicked() {
                            decided = Some(false);
                        }
                    });
                });
            match (open, decided) {
                (_, Some(true)) => actions.push(JobAction::Command("jobs.delete", job)),
                (false, _) | (_, Some(false)) => self.jobs.confirm_delete = None,
                _ => {}
            }
        }
        self.jobs_act(actions);
    }
}

enum FormEnd {
    Save,
    Close,
}

fn scope_row(
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

fn schedules(ui: &mut Ui, jobs: &JobsUi, actions: &mut Vec<JobAction>) {
    let list = &jobs.list;
    if let Some(e) = &list.error {
        let state = if list.loaded {
            ViewState::Stale {
                what: "scheduled jobs".into(),
            }
        } else {
            ViewState::Failed {
                what: "scheduled jobs".into(),
                error: e.clone(),
            }
        };
        if widgets::state_block(ui, &state) == widgets::StateAction::Retry {
            actions.push(JobAction::Reload);
        }
    }
    if !list.loaded {
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "scheduled jobs".into(),
            },
        );
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
            if list.more && widgets::secondary_button(ui, "More jobs").clicked() {
                actions.push(JobAction::MoreJobs);
            }
        });
}

fn run_row(ui: &mut Ui, r: &JobRun, actions: &mut Vec<JobAction>, busy: bool) {
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

fn runs(ui: &mut Ui, jobs: &JobsUi, actions: &mut Vec<JobAction>, busy: bool) {
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
            for r in first.iter().chain(&jobs.older) {
                run_row(ui, r, actions, busy);
                widgets::divider(ui);
            }
            let more = match &jobs.older_cursor {
                Some(c) => !c.is_empty(),
                None => i.runs_next_cursor.is_some(),
            };
            if more && widgets::secondary_button(ui, "Older runs").clicked() {
                actions.push(JobAction::OlderRuns);
            }
        });
}

fn details(ui: &mut Ui, jobs: &mut JobsUi, actions: &mut Vec<JobAction>, busy: bool) {
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
    if let Some(run) = i.run.as_ref().filter(|r| is_active(r)) {
        ui.add_space(4.0);
        ui.label(RichText::new("Active run").strong());
        run_row(ui, run, actions, busy);
    }
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new("Run now"))
            .clicked()
        {
            actions.push(JobAction::Command("jobs.run_now", j.clone()));
        }
        if (!j.enabled || j.hold.is_some() || j.authorized == Some(false))
            && ui.add_enabled(!busy, egui::Button::new("Enable")).clicked()
        {
            actions.push(JobAction::Command("jobs.enable", j.clone()));
        }
        if j.enabled
            && ui
                .add_enabled(!busy, egui::Button::new("Disable"))
                .clicked()
        {
            actions.push(JobAction::Command("jobs.disable", j.clone()));
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
fn field(ui: &mut Ui, value: &mut String, label: &str, multiline: bool) {
    ui.label(label);
    let edit = if multiline {
        egui::TextEdit::multiline(value).desired_rows(4)
    } else {
        egui::TextEdit::singleline(value)
    };
    let r = ui.add(edit.desired_width(f32::INFINITY));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
}

fn form_ui(
    ui: &mut Ui,
    form: &mut JobForm,
    caps: &[NativeCapability],
    artifacts: &[ArtifactRecord],
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
        if caps.is_empty() {
            widgets::caption(
                ui,
                "No maintenance capability is available in this build; the items that deliver them add them here.",
            );
        } else {
            egui::ComboBox::from_label("Capability")
                .selected_text(caps.get(form.capability).map_or("", |c| c.operation))
                .show_ui(ui, |ui| {
                    for (i, c) in caps.iter().enumerate() {
                        ui.selectable_value(&mut form.capability, i, c.operation);
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
            .map_or("Choose an artifact", |a| a.name.0.as_str());
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
        egui::ComboBox::from_label("Routing tier")
            .selected_text(TIERS[form.tier.min(3)])
            .show_ui(ui, |ui| {
                for (i, t) in TIERS.iter().enumerate() {
                    ui.selectable_value(&mut form.tier, i, *t);
                }
            });
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
