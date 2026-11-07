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
    ArtifactRecord, EffortName, JobDefinition, JobHold, JobInspection, JobRecord, JobRun,
    JobRunStatus, RouteCatalogModel, RouteIntent, Schedule,
};
use devworks_core::protocol::{Counter, DomainErrorCode};
use devworks_core::services::jobs::NativeCapability;
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};

/// The subviews (the screen registry's order).
pub const TABS: [&str; 2] = ["Schedules", "Runs"];
const SCHEDULE_KINDS: [&str; 3] = ["Manual", "Interval", "Calendar (cron)"];
const TIERS: [&str; 4] = ["Max", "High", "Mid", "Low"];
/// How an inference task is routed: a tier's pool, one named model, or OpenRouter's own route.
pub const ROUTES: [&str; 3] = ["Tier", "Named model", "OpenRouter Auto"];
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
    pub(super) fn purge_owner<'a>(&'a self, new_scope: &'a Option<String>) -> Option<&'a str> {
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

/// The wire schedule and definition a form describes, or why it is incomplete. Values the job
/// service validates (the expression, the zone, the arguments, the targets) are passed through
/// as written: its refusal is shown, never second-guessed here.
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
    let value: Value = if form.definition_kind == 0 {
        serde_json::from_str(&form.arguments)
            .map_err(|e| format!("The arguments are not valid JSON: {e}."))?
    } else {
        Value::Null
    };
    let definition = if form.definition_kind == 0 {
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
            Some(cap) => serde_json::to_value(&cap.schema).unwrap_or_default(),
            None => match form.original() {
                Some(JobDefinition::Native {
                    capability_id,
                    arguments,
                }) if capability_id.as_str() == chosen => {
                    serde_json::to_value(&arguments.schema).unwrap_or_default()
                }
                _ => return Err("No maintenance capability is available in this build.".into()),
            },
        };
        json!({"kind": "Native", "capabilityId": chosen, "arguments": {
            "schema": schema, "value": value}})
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
                serde_json::to_value(prompt).unwrap_or_default()
            }
            _ => match artifacts
                .iter()
                .find(|a| a.artifact.artifact_id.as_str() == chosen)
            {
                Some(a) => serde_json::to_value(&a.artifact).unwrap_or_default(),
                None => return Err("Choose the saved prompt (an artifact) the task runs.".into()),
            },
        };
        let route = match form.route_kind {
            1 => {
                let m = form.model.trim();
                if m.is_empty() {
                    return Err("Name the model the task is routed to.".into());
                }
                json!({"kind": "Model", "modelId": m})
            }
            2 => json!({"kind": "OpenRouterAuto"}),
            _ => json!({"kind": "Tier", "tier": TIERS[form.tier.min(TIERS.len() - 1)]}),
        };
        let mut d = json!({"kind": "Inference", "prompt": prompt, "route": route,
            "effort": EFFORTS[form.effort.min(EFFORTS.len() - 1)]});
        if !form.target_project.trim().is_empty() {
            d["projectId"] = json!(form.target_project.trim());
        }
        if !form.feature.trim().is_empty() {
            d["featureId"] = json!(form.feature.trim());
        }
        d
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

/// Appends a page in title order; a job already listed (renamed meanwhile) is kept once.
fn append_jobs(list: &mut Vec<JobRecord>, page: Vec<JobRecord>) {
    for j in page {
        if !list.iter().any(|k| k.id == j.id) {
            list.push(j);
        }
    }
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
    /// How many pages the shown list holds (the first and each More jobs page).
    pub list_pages: usize,
    /// Furthest page requested, including a More jobs request still in flight. A refresh must
    /// preserve that request even if it replaces the list before the later page replies.
    pub list_requested_pages: usize,
    /// The shown list's identity: a More jobs page continues only the list it was asked for.
    pub list_generation: u64,
    /// The last list or refresh identity handed out.
    pub generations: u64,
    /// A refresh re-reading every shown or requested page, replacing the list once all are read.
    pub list_refresh: Option<ListRefresh>,
    /// The listed scope: a project's jobs, or the app-level ones (`None`).
    pub scope: Option<String>,
    pub selected: Option<String>,
    pub inspection: Option<JobInspection>,
    pub inspection_error: Option<String>,
    /// The selected job's active run (from its `activeRunId`, inspected by ID: the latest
    /// occurrence may be a skipped one while it continues).
    pub active_run: Option<JobRun>,
    /// History pages loaded after the first, and where the next one starts.
    pub older: Vec<JobRun>,
    pub older_cursor: Option<String>,
    pub form: Option<JobForm>,
    pub form_error: Option<String>,
    pub confirm_delete: Option<JobRecord>,
    pub notice: Option<(Tone, String)>,
    pub capabilities: Vec<NativeCapability>,
    /// Scoped prompt pages belong to a single create/edit form, independently of artifacts UI.
    pub prompt_pages: std::collections::BTreeMap<Option<String>, PromptPage>,
    pub prompt_generation: u64,
    /// A one-line run status of each project's jobs, for the project's own views.
    pub project_status: std::collections::BTreeMap<String, String>,
}

#[derive(Default)]
pub struct PromptPage {
    items: Vec<ArtifactRecord>,
    cursor: Option<String>,
    error: Option<String>,
    loading: bool,
}

impl JobsUi {
    pub(super) fn purge_draft(&self) -> Option<(String, serde_json::Value)> {
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
        self.older.clear();
        self.older_cursor = None;
    }
    /// Lists `scope`'s jobs (`None`: the app-level ones) from their first page, with no job
    /// selected.
    pub fn set_scope(&mut self, scope: Option<String>) {
        self.scope = scope;
        self.select(None);
        self.list = ListState::default();
        self.list_cursor = None;
        self.list_pages = 0;
        self.list_requested_pages = 0;
        self.list_refresh = None;
        self.list_generation = self.generation();
    }
    /// A new list or refresh identity.
    fn generation(&mut self) -> u64 {
        self.generations += 1;
        self.generations
    }
    /// Shows `items` (`pages` pages, the next one after `cursor`) as a new list.
    fn show_list(&mut self, items: Vec<JobRecord>, cursor: Option<String>, pages: usize) {
        self.list = ListState {
            items,
            loaded: true,
            error: None,
            more: cursor.is_some(),
        };
        self.list_cursor = cursor;
        self.list_pages = pages;
        self.list_requested_pages = pages;
        self.list_refresh = None;
        self.list_generation = self.generation();
    }
}

/// A refresh of the jobs list in progress: the pages read so far, from the first, deduplicated.
pub struct ListRefresh {
    pub generation: u64,
    pub pages: usize,
    pub items: Vec<JobRecord>,
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
    /// Reads the next page of the jobs refresh `generation`, after `cursor`.
    fn refresh_next_jobs(&mut self, generation: u64, cursor: String) {
        let scope = self.jobs.scope.clone();
        let submitted = self.query(
            Slot::JobsRefresh(generation),
            "Loading scheduled jobs",
            "jobs.list",
            move |b| ops::jobs_list_after(b, scope.as_deref(), Some(&cursor)),
        );
        if !submitted {
            // Offline or closing: the shown list stays as it was.
            self.jobs.list_refresh = None;
        }
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
                    let slot = Slot::JobsMore(self.jobs.list_generation);
                    if self.query(slot, "Loading more jobs", "jobs.list", move |b| {
                        ops::jobs_list_after(b, scope.as_deref(), Some(&cursor))
                    }) {
                        self.jobs.list_requested_pages =
                            self.jobs.list_requested_pages.max(self.jobs.list_pages + 1);
                    }
                }
                JobAction::Scope(s) => {
                    self.jobs.set_scope(s);
                    self.load_jobs();
                }
                JobAction::Select(id) => {
                    self.jobs.select(Some(id.clone()));
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
                JobAction::MorePrompts(project) => {
                    if let Some(cursor) = self
                        .jobs
                        .prompt_pages
                        .get(&project)
                        .and_then(|p| p.cursor.clone())
                    {
                        self.load_prompt_page(project, Some(cursor));
                    }
                }
                JobAction::RetryPrompts(project) => {
                    self.jobs.prompt_pages.remove(&project);
                    self.load_prompt_page(project, None);
                }
                JobAction::NewJob => {
                    self.jobs.capabilities = self
                        .backend()
                        .map(|b| b.native_job_capabilities())
                        .unwrap_or_default();
                    self.jobs.form = Some(JobForm::default());
                    self.jobs.form_error = None;
                    self.jobs.prompt_generation += 1;
                    self.jobs.prompt_pages.clear();
                }
                JobAction::EditJob(job) => {
                    self.jobs.capabilities = self
                        .backend()
                        .map(|b| b.native_job_capabilities())
                        .unwrap_or_default();
                    self.jobs.form = Some(JobForm::edit(&job));
                    self.jobs.form_error = None;
                    self.jobs.prompt_generation += 1;
                    self.jobs.prompt_pages.clear();
                }
                JobAction::Save(form) => {
                    let values =
                        form_values(&form, &self.jobs.capabilities, &self.prompt_candidates());
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
                // More pages are shown or requested: read them before replacing the list,
                // including a More jobs response that this refresh will make stale.
                Ok((items, Some(cursor)))
                    if self.jobs.list_pages.max(self.jobs.list_requested_pages) > 1 =>
                {
                    let generation = self.jobs.generation();
                    self.jobs.list_refresh = Some(ListRefresh {
                        generation,
                        pages: 1,
                        items,
                    });
                    self.refresh_next_jobs(generation, cursor);
                }
                Ok((items, cursor)) => self.jobs.show_list(items, cursor, 1),
                Err(e) => {
                    self.jobs.list_refresh = None;
                    self.jobs.list.error = Some(e.describe());
                }
            },
            (Slot::JobsRefresh(g), reply) => {
                if self.jobs.list_refresh.as_ref().map(|r| r.generation) != Some(g) {
                    return None;
                }
                let page = match reply {
                    Reply::Query(q) => ops::jobs_page(q).map_err(|e| e.describe()),
                    _ => Err("the jobs list could not be read".into()),
                };
                match page {
                    Ok((items, cursor)) => {
                        let mut refresh = self.jobs.list_refresh.take()?;
                        append_jobs(&mut refresh.items, items);
                        refresh.pages += 1;
                        // Include pages requested while this refresh was in flight.
                        match cursor {
                            Some(c)
                                if refresh.pages
                                    < self.jobs.list_pages.max(self.jobs.list_requested_pages) =>
                            {
                                self.jobs.list_refresh = Some(refresh);
                                self.refresh_next_jobs(g, c);
                            }
                            cursor => self.jobs.show_list(refresh.items, cursor, refresh.pages),
                        }
                    }
                    Err(e) => {
                        self.jobs.list_refresh = None;
                        self.jobs.list.error = Some(e);
                    }
                }
            }
            // A page asked for a list since replaced continues none of it.
            (Slot::JobsMore(g), Reply::Query(_)) if g != self.jobs.list_generation => {}
            (Slot::JobsMore(_), Reply::Query(q)) => match ops::jobs_page(q) {
                Ok((items, cursor)) => {
                    append_jobs(&mut self.jobs.list.items, items);
                    self.jobs.list.more = cursor.is_some();
                    self.jobs.list_cursor = cursor;
                    self.jobs.list_pages += 1;
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
                        // The active run is the job's `activeRunId`, never assumed to be the
                        // latest occurrence (a skipped one may be newer).
                        match i.job.active_run_id.clone() {
                            None => self.jobs.active_run = None,
                            Some(a) if i.run.as_ref().is_some_and(|r| r.id == a) => {
                                self.jobs.active_run = i.run.clone();
                            }
                            Some(a) => {
                                let (job, run) = (j.clone(), a.as_str().to_owned());
                                self.query(
                                    Slot::JobActiveRun(j.clone()),
                                    "Loading the active run",
                                    "jobs.inspect",
                                    move |b| ops::jobs_inspect_run(b, &job, &run),
                                );
                            }
                        }
                        // Older pages already loaded continue after the previous first page:
                        // runs that moved off the refreshed first page join them, so none falls
                        // into a gap at the boundary.
                        if self.jobs.older_cursor.is_some()
                            && let Some(before) = &self.jobs.inspection
                        {
                            reconcile_history(
                                before.runs.as_ref().map_or(&[][..], |r| &r.0[..]),
                                i.runs.as_ref().map_or(&[][..], |r| &r.0[..]),
                                &mut self.jobs.older,
                            );
                        }
                        self.jobs.inspection = Some(i);
                        self.jobs.inspection_error = None;
                    }
                    Err(Failure::Refused(f)) if f.code == DomainErrorCode::NotFound => {
                        self.jobs.select(None);
                    }
                    Err(e) => self.jobs.inspection_error = Some(e.describe()),
                }
            }
            (Slot::JobPrompts(generation, project), Reply::Query(q)) => {
                if generation != self.jobs.prompt_generation || self.jobs.form.is_none() {
                    return None;
                }
                let page = self.jobs.prompt_pages.entry(project).or_default();
                page.loading = false;
                match ops::prompt_artifact_page(q) {
                    Ok((items, cursor)) => {
                        for item in items {
                            if !page
                                .items
                                .iter()
                                .any(|old| old.artifact.artifact_id == item.artifact.artifact_id)
                            {
                                page.items.push(item);
                            }
                        }
                        page.cursor = cursor;
                        page.error = None;
                    }
                    Err(error) => page.error = Some(error.describe()),
                }
            }
            (Slot::JobActiveRun(j), Reply::Query(q)) => {
                if self.jobs.selected.as_deref() != Some(j.as_str()) {
                    return None;
                }
                match ops::job_inspection(q) {
                    Ok(i) => {
                        // Still the job's active run (it may have ended meanwhile).
                        let current = self
                            .jobs
                            .inspection
                            .as_ref()
                            .and_then(|s| s.job.active_run_id.clone());
                        self.jobs.active_run = i
                            .run
                            .filter(|r| Some(&r.id) == current.as_ref() && is_active(r));
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
                                self.jobs.select(None);
                            } else if created {
                                self.jobs.select(Some(j.id.as_str().to_owned()));
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

    /// The project whose prompts the open form may run: the job's project, or the project an
    /// app-level task targets (once it names a listed project, or the one the job already
    /// targets).
    fn prompt_project(&self) -> Option<String> {
        let form = self.jobs.form.as_ref()?;
        let scope = match &form.editing {
            Some(job) => match &job.scope {
                Some(devworks_core::protocol::data::DataScope::Project { project_id }) => {
                    Some(project_id.0.as_str().to_owned())
                }
                _ => None,
            },
            None => self.jobs.scope.clone(),
        };
        scope.or_else(|| {
            let target = form.target_project.trim();
            let saved = form.editing.as_ref().and_then(|j| match &j.definition {
                JobDefinition::Inference { project_id, .. } => {
                    project_id.as_ref().map(|p| p.0.as_str().to_owned())
                }
                JobDefinition::Native { .. } => None,
            });
            (form.definition_kind == 1
                && !target.is_empty()
                && (self
                    .projects
                    .list
                    .items
                    .iter()
                    .any(|p| p.id.0.as_str() == target)
                    || saved.as_deref() == Some(target)))
            .then(|| target.to_owned())
        })
    }
    fn load_prompt_page(&mut self, project: Option<String>, cursor: Option<String>) {
        if self.jobs.form.is_none() {
            return;
        }
        let slot = Slot::JobPrompts(self.jobs.prompt_generation, project.clone());
        if self.pending(&slot) {
            return;
        }
        let scope = project.clone();
        if self.query(slot, "Loading saved prompts", "artifacts.list", move |b| {
            ops::prompt_artifacts(b, scope.as_deref(), cursor.as_deref())
        }) {
            self.jobs.prompt_pages.entry(project).or_default().loading = true;
        }
    }
    fn load_prompt_candidates(&mut self) {
        if self.jobs.form.is_none() {
            return;
        }
        let mut scopes = vec![None];
        if let Some(project) = self.prompt_project() {
            scopes.push(Some(project));
        }
        for scope in scopes {
            if !self.jobs.prompt_pages.contains_key(&scope) {
                self.load_prompt_page(scope, None);
            }
        }
    }
    /// Only workspace and the currently selected project are eligible for this form.
    fn prompt_candidates(&self) -> Vec<ArtifactRecord> {
        let project = self.prompt_project();
        let mut seen = std::collections::BTreeSet::new();
        self.jobs
            .prompt_pages
            .iter()
            .filter(|(scope, _)| scope.is_none() || (project.is_some() && **scope == project))
            .flat_map(|(_, page)| page.items.iter())
            .filter(|item| seen.insert(item.artifact.artifact_id.as_str()))
            .cloned()
            .collect()
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
        self.load_prompt_candidates();
        let prompt_project = self.prompt_project();
        let artifacts = self.prompt_candidates();
        let catalog = self.chat.routing.catalog.clone();
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
                        for (scope, page) in &st.0.prompt_pages {
                            if scope.is_some() && scope != &prompt_project {
                                continue;
                            }
                            let name = if scope.is_some() {
                                "project"
                            } else {
                                "workspace"
                            };
                            if page.loading {
                                widgets::caption(ui, format!("Loading {name} prompts…"));
                            }
                            if let Some(error) = &page.error {
                                widgets::caption(ui, error);
                                if ui
                                    .add_enabled(
                                        !page.loading,
                                        egui::Button::new(format!("Reload {name} prompts")),
                                    )
                                    .clicked()
                                {
                                    st.1.push(JobAction::RetryPrompts(scope.clone()));
                                }
                            } else if page.cursor.is_some()
                                && ui
                                    .add_enabled(
                                        !page.loading,
                                        egui::Button::new(format!("Load more {name} prompts")),
                                    )
                                    .clicked()
                            {
                                st.1.push(JobAction::MorePrompts(scope.clone()));
                            }
                        }
                        if let Some(done) = form_ui(
                            ui,
                            form,
                            &caps,
                            &artifacts,
                            &catalog,
                            error.as_deref(),
                            busy,
                        ) {
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
