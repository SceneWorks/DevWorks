//! The Scheduled jobs controller: it reads and changes jobs through the same `jobs.*` frames an
//! agent tool submits, and implements [`Screen`] for [`JobsUi`]. Its state and views are in
//! [`crate::jobs_screen`].
use crate::backend::Reply;
use crate::jobs_ops::JobsSlot;
pub use crate::jobs_screen::*;
use crate::memory::RouteMemory;
use crate::ops::{self, Failure, JobChange, JobCommand};
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId, controller};
use crate::theme;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::DomainErrorCode;
use devworks_core::protocol::records::{ArtifactRecord, JobDefinition};
use eframe::egui::{self, Ui};

controller!(JobsCtl, jobs: JobsUi);

impl Screen for JobsUi {
    type Slot = JobsSlot;
    const ID: ScreenId = ScreenId::Jobs;
    /// The shell loads the router catalog first (the form's selectors).
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        let mut ctl = JobsCtl { jobs: self, cx };
        ctl.load_jobs();
        if !ctl.projects.list.loaded {
            ctl.load_projects();
        }
    }
    /// Run status moves while the screen is open (claims, runs, cancellations).
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !cx.pending(&JobsSlot::JobsList) && self.list_refresh.is_none() {
            JobsCtl { jobs: self, cx }.load_jobs();
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: JobsSlot, reply: Reply) {
        if let Some((slot, reply)) = (JobsCtl { jobs: self, cx }).apply_jobs(slot, reply) {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        JobsCtl { jobs: self, cx }.jobs_screen(ui);
    }
    fn purge_live(&self, out: &mut crate::project_purge::Live) {
        if let Some((owner, draft)) = self.purge_draft() {
            out.projects
                .entry(owner)
                .or_insert_with(|| serde_json::json!({}))["job"] = draft;
        }
    }
    fn remove_purged(&mut self, scope: &devworks_core::ports::storage::ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        let job_form = self
            .form
            .take()
            .filter(|f| f.purge_owner(&self.scope) != Some(p));
        if self.scope.as_deref() == Some(p) {
            *self = Default::default();
        }
        self.form = job_form;
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: self.selected.clone(),
        }
    }
}

/// What the shell and other screens ask of the jobs controller.
impl JobsUi {
    pub(crate) fn load_jobs(&mut self, cx: &mut Cx) {
        JobsCtl { jobs: self, cx }.load_jobs();
    }
    /// The run status of a project's jobs, for its sidebar on All projects.
    pub(crate) fn load_project_jobs(&mut self, cx: &mut Cx, project: &str) {
        JobsCtl { jobs: self, cx }.load_project_jobs(project);
    }
}

impl JobsCtl<'_, '_> {
    fn load_jobs(&mut self) {
        let scope = self.jobs.scope.clone();
        self.query(
            JobsSlot::JobsList,
            "Loading scheduled jobs",
            "jobs.list",
            move |b| ops::jobs_list(b, scope.as_deref()),
        );
        if let Some(j) = self.jobs.selected.clone() {
            self.load_job(&j);
        }
    }
    /// The run status of a project's jobs, for its sidebar on All projects.
    fn load_project_jobs(&mut self, project: &str) {
        let p = project.to_owned();
        self.query(
            JobsSlot::ProjectJobs(project.to_owned()),
            "Loading the project's scheduled jobs",
            "jobs.list",
            move |b| ops::jobs_list(b, Some(&p)),
        );
    }
    /// Reads the next page of the jobs refresh `generation`, after `cursor`.
    fn refresh_next_jobs(&mut self, generation: u64, cursor: String) {
        let scope = self.jobs.scope.clone();
        let submitted = self.query(
            JobsSlot::JobsRefresh(generation),
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
            JobsSlot::JobInspect(job.to_owned()),
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
                    let slot = JobsSlot::JobsMore(self.jobs.list_generation);
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
                        JobsSlot::JobHistory(job.clone()),
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
                            let slot = JobsSlot::JobCommand(job.id.as_str().to_owned());
                            self.command(slot, "Saving the job", "jobs.edit", move |b| {
                                ops::job_edit(b, &job, &title, &schedule, &definition)
                            });
                        }
                        None => {
                            let (scope, enabled) = (self.jobs.scope.clone(), form.enabled);
                            self.command(
                                JobsSlot::JobCreate,
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
                JobAction::Command(command, job) => {
                    self.jobs.confirm_delete = None;
                    let slot = JobsSlot::JobCommand(job.id.as_str().to_owned());
                    let (label, op) = match command {
                        JobCommand::RunNow => ("Starting a run", "jobs.run_now"),
                        JobCommand::Enable => ("Enabling the job", "jobs.enable"),
                        JobCommand::Disable => ("Disabling the job", "jobs.disable"),
                        JobCommand::Delete => ("Deleting the job", "jobs.delete"),
                    };
                    self.command(slot, label, op, move |b| ops::job_command(b, command, &job));
                }
                JobAction::Cancel(run) => {
                    let slot = JobsSlot::JobCommand(run.job_id.as_str().to_owned());
                    self.command(slot, "Cancelling the run", "jobs.cancel", move |b| {
                        ops::job_cancel(b, &run)
                    });
                }
            }
        }
    }

    /// Applies a jobs reply; anything else is handed back.
    fn apply_jobs(
        &mut self,
        slot: JobsSlot,
        reply: crate::backend::Reply,
    ) -> Option<(JobsSlot, crate::backend::Reply)> {
        use crate::backend::Reply;
        match (slot, reply) {
            (JobsSlot::JobsList, Reply::Query(q)) => match ops::jobs_page(q) {
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
            (JobsSlot::JobsRefresh(g), reply) => {
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
            (JobsSlot::JobsMore(g), Reply::Query(_)) if g != self.jobs.list_generation => {}
            (JobsSlot::JobsMore(_), Reply::Query(q)) => match ops::jobs_page(q) {
                Ok((items, cursor)) => {
                    append_jobs(&mut self.jobs.list.items, items);
                    self.jobs.list.more = cursor.is_some();
                    self.jobs.list_cursor = cursor;
                    self.jobs.list_pages += 1;
                }
                Err(e) => self.jobs.list.error = Some(e.describe()),
            },
            (JobsSlot::ProjectJobs(p), Reply::Query(q)) => {
                let line = match ops::jobs(q) {
                    Ok((items, more)) => status_line(&items, more),
                    Err(e) => e.describe(),
                };
                self.jobs.project_status.insert(p, line);
            }
            (JobsSlot::JobInspect(j), Reply::Query(q)) => {
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
                                    JobsSlot::JobActiveRun(j.clone()),
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
            (JobsSlot::JobPrompts(generation, project), Reply::Query(q)) => {
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
            (JobsSlot::JobActiveRun(j), Reply::Query(q)) => {
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
            (JobsSlot::JobHistory(j), Reply::Query(q)) => {
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
            (slot @ (JobsSlot::JobCommand(_) | JobsSlot::JobCreate), Reply::Command(c)) => {
                let created = slot == JobsSlot::JobCreate;
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
        let slot = JobsSlot::JobPrompts(self.jobs.prompt_generation, project.clone());
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

    fn jobs_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Jobs.screen();
        let online = self.online().map_err(str::to_owned);
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
            .is_some_and(|j| self.waiting(&JobsSlot::JobCommand(j.clone())))
            || self.waiting(&JobsSlot::JobCreate);
        self.load_prompt_candidates();
        let prompt_project = self.prompt_project();
        let artifacts = self.prompt_candidates();
        let catalog = self.catalog.to_vec();
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
                (_, Some(true)) => actions.push(JobAction::Command(JobCommand::Delete, job)),
                (false, _) | (_, Some(false)) => self.jobs.confirm_delete = None,
                _ => {}
            }
        }
        self.jobs_act(actions);
    }
}
