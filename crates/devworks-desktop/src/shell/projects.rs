//! All projects and Repositories requests and replies. Every change is the same `projects.*`,
//! `repositories.*` or `settings.*` command an agent tool submits; nothing here decides what a
//! use case decides. A command whose outcome is not known keeps its exact frame (Confirm
//! outcomes, and the next launch, resubmit it).
use crate::backend::{Reply, Slot, Unresolved};
use crate::memory::RouteMemory;
use crate::ops::{self, Failure};
use crate::project_purge::Live;
use crate::projects_ops::ProjectsSlot;
use crate::projects_screen::{
    ARCHIVE, Action, DELETE, PortfolioView, RESTORE, RepositoriesView, confirm_window, same_access,
};
pub use crate::projects_screen::{
    BASE_DIRECTORY, BaseDirectory, CONTAINER_RUNTIME, Confirm, CreateForm, EditForm, GitView,
    MembershipForm, ProjectsScreen, ProjectsUi, TABS, TextState, confirm_after_reply,
    edit_after_reply, life_label, membership_after_reply,
};
use crate::route::Route;
use crate::screen::{Cx, Effect, Screen, ScreenContext, ScreenId, controller};
use crate::shell::ListState;
use crate::shell::indexing::IndexAction;
use crate::widgets::Tone;
use devworks_core::ports::storage::{ConfigScope, ProjectPurgeClientScope};
use devworks_core::protocol::records::LifeState;
use devworks_core::protocol::{DomainErrorCode, Id, ProjectId};
use devworks_core::settings::validate::codes;
use devworks_core::settings::view::{Health, SettingsView};
use eframe::egui::{self, Ui};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

controller!(ProjectsCtl, projects: ProjectsScreen);

impl Screen for ProjectsScreen {
    type Slot = ProjectsSlot;
    const ID: ScreenId = ScreenId::Projects;
    fn load(&mut self, cx: &mut Cx, route: Route) {
        let mut ctl = ProjectsCtl { projects: self, cx };
        match route {
            Route::Portfolio => {
                ctl.load_projects();
                ctl.load_deleted_projects();
                ctl.load_repositories();
                if let Some(p) = ctl
                    .routes
                    .get(&Route::Portfolio)
                    .and_then(|m| m.selection.clone())
                {
                    ctl.effects.push(Effect::LoadProjectJobs(p));
                }
            }
            _ => {
                ctl.load_projects();
                ctl.load_repositories();
                if ctl.cx.projects.indexing.sources.is_none() {
                    ctl.index(IndexAction::LoadSources);
                }
            }
        }
    }
    fn refresh(&mut self, cx: &mut Cx, route: Route) {
        if !cx.pending(&Slot::ProjectsList) && !cx.pending(&Slot::RepositoriesList) {
            Screen::load(self, cx, route);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: ProjectsSlot, reply: Reply) {
        if let Some((slot, reply)) =
            (ProjectsCtl { projects: self, cx }).apply_projects(slot, reply)
        {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui) {
        let mut ctl = ProjectsCtl { projects: self, cx };
        match route {
            Route::Portfolio => ctl.portfolio_screen(ui),
            _ => ctl.repositories_screen(ui),
        }
    }
    fn restore_in_flight(&mut self, slot: &ProjectsSlot, _u: &Unresolved) {
        if let ProjectsSlot::ProjectSettingsSave(p) = slot {
            self.base.entry(p.clone()).or_default().saving = true;
        }
    }
    fn purge_live(&self, out: &mut Live) {
        let mut put = |project: &str, kind: &str, value: serde_json::Value| {
            out.projects
                .entry(project.into())
                .or_insert_with(|| json!({}))[kind] = value;
        };
        if let Some(f) = &self.membership {
            put(&f.project, "membership", json!(f.repositories));
        }
        if let Some(f) = &self.edit {
            put(
                &f.project,
                "project",
                json!({"name":f.name,"description":f.description,"access":f.access}),
            );
        }
        for (p, f) in &self.base {
            put(p, "directory", json!(f.buffer));
        }
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        let p = scope.project_id.0.as_str();
        self.deleted.items.retain(|r| r.id != scope.project_id);
        self.edit = self.edit.take().filter(|f| f.project != p);
        self.membership = self.membership.take().filter(|f| f.project != p);
        self.base.remove(p);
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

impl ProjectsCtl<'_, '_> {
    // -----------------------------------------------------------------------------------------
    // Requests

    fn load_deleted_projects(&mut self) {
        self.query(
            ProjectsSlot::DeletedProjects,
            "Loading deleted projects",
            "projects.list",
            |b| ops::projects_list(b, Some("Deleted")),
        );
    }
    fn load_base(&mut self, project: &str) {
        self.projects
            .base
            .entry(project.to_owned())
            .or_default()
            .loading = true;
        let p = project.to_owned();
        self.query(
            ProjectsSlot::ProjectSettings(project.to_owned()),
            "Reading project settings",
            "settings.read_effective",
            move |b| ops::project_settings_read(b, &p, &[BASE_DIRECTORY, CONTAINER_RUNTIME]),
        );
    }
    fn load_git(&mut self, repository: &str) {
        let r = repository.to_owned();
        self.query(
            ProjectsSlot::GitInspect(r.clone()),
            "Inspecting repository",
            "git.inspect",
            move |be| ops::git_inspect(be, &r),
        );
    }
    fn load_texts(&mut self, repository: &str) {
        for readme in [true, false] {
            let key = (repository.to_owned(), readme);
            if matches!(self.projects.texts.get(&key), Some(TextState::Loading)) {
                continue;
            }
            self.projects.texts.insert(key, TextState::Loading);
            let (r, op) = (
                repository.to_owned(),
                if readme {
                    "repositories.readme"
                } else {
                    "repositories.summary"
                },
            );
            self.query(
                ProjectsSlot::RepositoryText(repository.to_owned(), readme),
                "Reading repository texts",
                op,
                move |b| ops::repository_text(b, op, &r),
            );
        }
    }

    fn act(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Reload => {
                    self.load_projects();
                    self.load_deleted_projects();
                    self.load_repositories();
                    if let Some(p) = self
                        .routes
                        .get(&Route::Portfolio)
                        .and_then(|m| m.selection.clone())
                    {
                        self.effects.push(Effect::LoadProjectJobs(p));
                    }
                }
                Action::Create(form) => {
                    let (name, description, repos) = (
                        form.name.clone(),
                        form.description.clone(),
                        form.repositories.iter().cloned().collect::<Vec<_>>(),
                    );
                    self.command(
                        ProjectsSlot::ProjectCreate,
                        "Creating a project",
                        "projects.create",
                        move |b| ops::project_create(b, &name, &description, &repos),
                    );
                }
                Action::Purge(project) => self.effects.push(Effect::ReviewPurge(project)),
                Action::Edit(form) => {
                    let p = form.snapshot.clone();
                    let name = (form.name != p.name.0).then_some(form.name.clone());
                    let description =
                        (form.description != p.description.0).then_some(form.description.clone());
                    let access =
                        (!same_access(&form.access, &p.access_mode)).then_some(form.access.clone());
                    if name.is_none() && description.is_none() && access.is_none() {
                        self.projects.edit = None;
                        continue;
                    }
                    let slot = ProjectsSlot::ProjectCommand(p.id.0.as_str().to_owned());
                    self.projects
                        .edit_saving
                        .insert(p.id.0.as_str().to_owned(), form);
                    self.command(slot, "Saving the project", "projects.edit", move |b| {
                        ops::project_edit(
                            b,
                            &p,
                            name.as_deref(),
                            description.as_deref(),
                            access.as_ref(),
                        )
                    });
                }
                Action::Membership(form) => {
                    let p = form.snapshot.clone();
                    let repos: Vec<String> = form.repositories.iter().cloned().collect();
                    let slot = ProjectsSlot::ProjectCommand(p.id.0.as_str().to_owned());
                    self.projects
                        .membership_saving
                        .insert(p.id.0.as_str().to_owned(), form);
                    self.command(
                        slot,
                        "Changing the project's repositories",
                        "projects.membership",
                        move |b| ops::project_membership(b, &p, &repos),
                    );
                }
                Action::Lifecycle(op, p) => {
                    let slot = ProjectsSlot::ProjectCommand(p.id.0.as_str().to_owned());
                    let label = match op {
                        ARCHIVE => "Archiving the project",
                        RESTORE => "Restoring the project",
                        _ => "Deleting the project",
                    };
                    self.command(slot, label, op, move |b| ops::project_lifecycle(b, op, &p));
                }
                Action::OpenGraph(id, name) => self.effects.push(Effect::OpenGraph(id, name)),
                Action::Open(id) => {
                    self.effects.push(Effect::SelectProject(Some(id)));
                    self.navigate(Route::Overview);
                }
                Action::LoadBase(p) => self.load_base(&p),
                Action::ChooseBase(p) => {
                    let start = self
                        .projects
                        .base
                        .get(&p)
                        .and_then(|b| b.view.as_ref())
                        .and_then(|v| v.item(BASE_DIRECTORY))
                        .and_then(|i| i.shown().as_str().map(str::to_owned));
                    if let Some(dir) = self.dialogs.pick_folder(start.as_deref())
                        && let Some(b) = self.projects.base.get_mut(&p)
                    {
                        b.buffer = dir.display().to_string();
                        if let Some(v) = &mut b.view {
                            let _ = v.edit(BASE_DIRECTORY, json!(b.buffer));
                        }
                    }
                }
                Action::InheritBase(p) => {
                    if let Some(v) = self.projects.base.get_mut(&p).and_then(|b| b.view.as_mut()) {
                        let _ = v.inherit(BASE_DIRECTORY);
                    }
                }
                Action::SaveBase(p) => {
                    let Some(b) = self.backend() else { continue };
                    let Some(frame) = self
                        .projects
                        .base
                        .get(&p)
                        .and_then(|x| x.view.as_ref())
                        .and_then(|v| ops::settings_submit(&*b, v, false))
                    else {
                        continue;
                    };
                    if let Some(x) = self.projects.base.get_mut(&p) {
                        x.saving = true;
                        x.submitted = Some(x.buffer.trim().to_owned());
                    }
                    if let Some(c) = &mut self.cx.client {
                        c.command(
                            ProjectsSlot::ProjectSettingsSave(p).into(),
                            "Saving project settings",
                            "settings.save_revision",
                            frame,
                        );
                    }
                }
                Action::InspectGit(r) => self.load_git(&r),
                Action::DiffGit(r) => {
                    if let Some(view) = self.projects.git.get(&r) {
                        let (base, head) =
                            (view.base.trim().to_owned(), view.head.trim().to_owned());
                        self.query(
                            ProjectsSlot::GitDiff(r.clone()),
                            "Reading Git patch",
                            "git.diff",
                            move |be| ops::git_diff(be, &r, &base, &head),
                        );
                    }
                }
                Action::LoadTexts(r) => {
                    self.load_texts(&r);
                    self.index(IndexAction::Status(r));
                }
                Action::Index(a) => self.index(a),
                Action::OpenJobs(p) => self.effects.push(Effect::OpenJobs(p)),
                Action::RemoveIndex(r) => {
                    let slot = ProjectsSlot::RemoveIndex(r.id.as_str().to_owned());
                    self.command(
                        slot,
                        "Removing a repository index",
                        "repositories.remove_index",
                        move |b| ops::remove_index(b, &r),
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    /// Applies a reply for one of the project slots; returns it back when it is not one.
    fn apply_projects(
        &mut self,
        slot: ProjectsSlot,
        reply: Reply,
    ) -> Option<(ProjectsSlot, Reply)> {
        match (slot, reply) {
            (ProjectsSlot::DeletedProjects, Reply::Query(q)) => match ops::projects(q) {
                Ok((items, more)) => {
                    self.projects.deleted = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    };
                }
                Err(e) => self.projects.deleted.error = Some(e.describe()),
            },
            (ProjectsSlot::ProjectCreate, Reply::Command(c)) => match ops::project_changed(c) {
                Ok(p) => {
                    self.effects.push(Effect::ProjectChanged(p.clone()));
                    self.projects.create = None;
                    self.projects.message = Some((Tone::Good, format!("Created {}.", p.name.0)));
                    self.routes.entry(Route::Portfolio).or_default().selection =
                        Some(p.id.0.as_str().to_owned());
                    self.announce(format!("Project {} created.", p.name.0));
                    self.load_projects();
                    self.load_repositories();
                }
                Err(e) => self.project_failure("Creating the project", e),
            },
            (ProjectsSlot::ProjectCommand(id), Reply::Command(c)) => {
                match ops::project_changed(c) {
                    Ok(p) => {
                        // The project lists, the navigation's selection (a deleted project is no
                        // longer selected) and the work screens follow the change; its archive
                        // count is taken afresh next time.
                        self.effects.push(Effect::ProjectChanged(p.clone()));
                        let saved_edit = self.projects.edit_saving.remove(&id);
                        let saved_members = self.projects.membership_saving.remove(&id);
                        self.projects.edit =
                            edit_after_reply(self.projects.edit.take(), saved_edit.as_ref(), &p);
                        self.projects.membership = membership_after_reply(
                            self.projects.membership.take(),
                            saved_members.as_ref(),
                            &p,
                        );
                        self.projects.confirm =
                            confirm_after_reply(self.projects.confirm.take(), &p);
                        // A saved access change is a new project-scope settings revision: the
                        // project's base directory view rereads it (keeping its draft; a read already
                        // running is superseded) unless its own save is running, whose reply rereads
                        // it anyway.
                        let access_sent = saved_edit
                            .as_ref()
                            .is_some_and(|s| !same_access(&s.access, &s.snapshot.access_mode));
                        if access_sent
                            && self
                                .projects
                                .base
                                .get(&id)
                                .is_some_and(|b| b.view.is_some() && !b.saving)
                        {
                            self.load_base(&id);
                        }
                        let text = match p.lifecycle {
                            LifeState::Archived => format!(
                                "{} is archived. Its running work was stopped at once; restoring it \
                             leaves that work stopped until you continue it.",
                                p.name.0
                            ),
                            LifeState::Deleted => format!(
                                "{} is deleted. Its repositories, their folders, branches, \
                             worktrees and remotes were not touched.",
                                p.name.0
                            ),
                            LifeState::Active => format!("{} is saved.", p.name.0),
                        };
                        self.projects.message = Some((Tone::Good, text.clone()));
                        self.announce(text);
                        self.load_projects();
                        self.load_deleted_projects();
                        self.load_repositories();
                    }
                    Err(e) => {
                        // The forms keep their drafts; an unknown outcome stays pending.
                        if !matches!(e, Failure::Unknown) {
                            self.projects.edit_saving.remove(&id);
                            self.projects.membership_saving.remove(&id);
                        }
                        self.project_failure("The project change", e)
                    }
                }
            }
            (ProjectsSlot::ProjectSettings(p), Reply::Query(q)) => {
                let after_save;
                {
                    let b = self.projects.base.entry(p.clone()).or_default();
                    b.loading = false;
                    after_save = std::mem::take(&mut b.after_save);
                    match ops::settings_entries(q) {
                        Ok((revision, entries)) => {
                            match &mut b.view {
                                Some(v) if after_save => v.saved(revision, &entries),
                                Some(v) => v.refresh(revision, &entries),
                                None => {
                                    b.view = Some(SettingsView::new(
                                        ConfigScope::Project(ProjectId(
                                            Id::new(p.clone()).expect("listed project id"),
                                        )),
                                        revision,
                                        &entries,
                                    ));
                                }
                            }
                            if after_save {
                                b.saving = false;
                            }
                            // Text typed while the save ran stays a draft on the saved value.
                            let later = std::mem::take(&mut b.submitted)
                                .filter(|sent| after_save && b.buffer.trim() != sent);
                            match (later, &mut b.view) {
                                (Some(_), Some(v)) => {
                                    let typed = b.buffer.trim().to_owned();
                                    if typed.is_empty() {
                                        let _ = v.inherit(BASE_DIRECTORY);
                                    } else {
                                        let _ = v.edit(BASE_DIRECTORY, json!(typed));
                                    }
                                }
                                _ => {
                                    b.buffer = b
                                        .view
                                        .as_ref()
                                        .and_then(|v| v.item(BASE_DIRECTORY))
                                        .and_then(|i| i.shown().as_str().map(str::to_owned))
                                        .unwrap_or_default();
                                }
                            }
                            b.error = None;
                        }
                        Err(e) => {
                            b.saving = false;
                            b.error = Some(e.describe());
                        }
                    }
                }
                self.check_base_health(&p);
            }
            (ProjectsSlot::ProjectSettingsHealth(p), Reply::Query(q)) => {
                if let (Ok(report), Some(v)) = (
                    ops::settings_report(q),
                    self.projects.base.get_mut(&p).and_then(|b| b.view.as_mut()),
                ) {
                    let health =
                        match report.issues.0.iter().find(|i| {
                            i.path.0 == BASE_DIRECTORY && i.code.0 == codes::PATH_UNAVAILABLE
                        }) {
                            Some(i) => Health::Unavailable(i.message.0.clone()),
                            None => Health::Available,
                        };
                    v.set_health(BASE_DIRECTORY, health);
                }
            }
            (ProjectsSlot::ProjectSettingsSave(p), Reply::Command(c)) => {
                let outcome = ops::settings_saved(c);
                match outcome {
                    Ok(()) => {
                        if let Some(b) = self.projects.base.get_mut(&p) {
                            b.after_save = true;
                        }
                        self.announce("The repository base directory was saved.");
                        self.load_base(&p);
                    }
                    Err(Failure::Unknown) => {
                        self.announce(
                            "The repository base directory: outcome not confirmed. Use Confirm \
                             outcomes to check again.",
                        );
                    }
                    Err(e) => {
                        if let Some(b) = self.projects.base.get_mut(&p) {
                            b.saving = false;
                            b.submitted = None;
                            b.error = Some(e.describe());
                        }
                        if matches!(&e, Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict)
                        {
                            self.load_base(&p);
                        }
                    }
                }
            }
            (ProjectsSlot::GitInspect(r), Reply::Query(q)) => {
                let view = self.projects.git.entry(r).or_default();
                match ops::git_inspected(q) {
                    Ok(value) => {
                        view.head = value.head.hex.clone();
                        view.inspection = Some(value);
                        view.error = None;
                    }
                    Err(e) => view.error = Some(e.describe()),
                }
            }
            (ProjectsSlot::GitDiff(r), Reply::Query(q)) => match ops::git_patch(q) {
                Ok(Some(artifact)) => {
                    self.query(
                        ProjectsSlot::GitDiffContent(r),
                        "Reading patch artifact",
                        "artifacts.read",
                        move |be| ops::artifact_read(be, &artifact),
                    );
                }
                Ok(None) => {
                    self.projects.git.entry(r).or_default().patch =
                        Some("No changes between these commits.".into())
                }
                Err(e) => self.projects.git.entry(r).or_default().error = Some(e.describe()),
            },
            (ProjectsSlot::GitDiffContent(r), Reply::Query(q)) => {
                let view = self.projects.git.entry(r).or_default();
                match ops::read_content(q) {
                    Ok(devworks_core::protocol::records::ArtifactReadContent::Text {
                        content,
                        truncated,
                        ..
                    }) => {
                        view.patch = Some(if truncated {
                            format!("{}\n[Preview truncated]", content.0)
                        } else {
                            content.0
                        });
                        view.error = None;
                    }
                    Ok(_) => view.error = Some("The patch artifact did not contain text.".into()),
                    Err(e) => view.error = Some(e.describe()),
                }
            }
            (ProjectsSlot::RepositoryText(r, readme), Reply::Query(q)) => {
                let state = match ops::repository_text_result(q) {
                    Ok(text) => {
                        let artifact = text.content.clone();
                        self.query(
                            ProjectsSlot::RepositoryContent(r.clone(), readme),
                            "Reading a repository text",
                            "artifacts.read",
                            move |b| ops::artifact_read(b, &artifact),
                        );
                        TextState::Ready {
                            text,
                            content: None,
                        }
                    }
                    Err(Failure::Refused(f)) if f.code == DomainErrorCode::ViewUnavailable => {
                        TextState::Unavailable(
                            "Not indexed yet: this text appears once repository indexing \
                             publishes a view of the repository (DW-23)."
                                .into(),
                        )
                    }
                    Err(e) => TextState::Failed(e.describe()),
                };
                self.projects.texts.insert((r, readme), state);
            }
            (ProjectsSlot::RepositoryContent(r, readme), Reply::Query(q)) => {
                if let Some(TextState::Ready { content, .. }) =
                    self.projects.texts.get_mut(&(r, readme))
                {
                    *content = Some(match ops::read_content(q) {
                        Ok(devworks_core::protocol::records::ArtifactReadContent::Text {
                            content: c,
                            truncated,
                            ..
                        }) => {
                            if truncated {
                                format!("{}\n…", c.0)
                            } else {
                                c.0
                            }
                        }
                        Ok(_) => "This text is stored as binary content.".into(),
                        Err(e) => format!("The text could not be read: {}", e.describe()),
                    });
                }
            }
            (ProjectsSlot::RemoveIndex(r), Reply::Command(c)) => match ops::index_removed(c) {
                Ok(m) => {
                    self.projects.texts.retain(|(k, _), _| k != &r);
                    let text = if m.changed {
                        "The repository's index was removed. The repository, its files, \
                         memberships and work history were kept."
                    } else {
                        "The repository had no index to remove."
                    };
                    self.projects.message = Some((Tone::Good, text.into()));
                    self.announce(text);
                    self.load_repositories();
                }
                Err(e) => self.project_failure("Removing the index", e),
            },
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }

    fn project_failure(&mut self, what: &str, e: Failure) {
        let (tone, text) = match &e {
            Failure::Unknown => (
                Tone::Attention,
                format!(
                    "{what}: not yet confirmed whether it was applied. Use Confirm outcomes to \
                     check again; it is never applied twice."
                ),
            ),
            Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict => (
                Tone::Bad,
                format!(
                    "{what} was not applied: the project changed meanwhile. Review it and try again."
                ),
            ),
            Failure::Refused(f) => {
                let target = f.target_kind.as_ref().map(|k| k.0.as_str()).unwrap_or("");
                let why = match target {
                    "project_not_quiescent" => {
                        "its work is still stopping. Try again once it has stopped.".to_owned()
                    }
                    "project_active" => "archive the project first.".to_owned(),
                    "repository_in_use" => {
                        "the project's work still names that repository.".to_owned()
                    }
                    "project_archived" => "restore the project first.".to_owned(),
                    "project_stopping" => {
                        "its archival is still stopping its work. Try again in a moment.".to_owned()
                    }
                    _ => e.describe(),
                };
                (Tone::Bad, format!("{what} was not applied: {why}"))
            }
            other => (
                Tone::Bad,
                format!("{what} was not applied: {}", other.describe()),
            ),
        };
        self.projects.message = Some((tone, text.clone()));
        self.announce(text);
        if matches!(&e, Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict) {
            self.load_projects();
        }
    }

    fn check_base_health(&mut self, project: &str) {
        let Some((revision, value)) = self
            .projects
            .base
            .get(project)
            .and_then(|b| b.view.as_ref())
            .and_then(|v| Some((v.revision, v.item(BASE_DIRECTORY)?.effective.clone())))
        else {
            return;
        };
        let p = project.to_owned();
        self.query(
            ProjectsSlot::ProjectSettingsHealth(project.to_owned()),
            "Checking the repository base directory",
            "settings.validate",
            move |b| ops::project_settings_validate(b, &p, revision, &[(BASE_DIRECTORY, value)]),
        );
    }

    // -----------------------------------------------------------------------------------------
    // All projects

    fn portfolio_screen(&mut self, ui: &mut Ui) {
        let mut actions: Vec<Action> = Vec::new();
        let mut mem = self.routes.remove(&Route::Portfolio).unwrap_or_default();
        let lists = self.cx.projects;
        let waiting: BTreeSet<String> = lists
            .list
            .items
            .iter()
            .chain(&self.projects.deleted.items)
            .map(|p| p.id.0.as_str().to_owned())
            .filter(|id| self.waiting(&ProjectsSlot::ProjectCommand(id.clone())))
            .collect();
        let base_waiting: BTreeSet<String> = self
            .projects
            .base
            .keys()
            .filter(|id| self.waiting(&ProjectsSlot::ProjectSettingsSave((*id).clone())))
            .cloned()
            .collect();
        let online = self.cx.online.clone();
        let view = PortfolioView {
            online: &online,
            lists,
            busy_create: self.waiting(&ProjectsSlot::ProjectCreate),
            waiting,
            base_waiting,
            selected_nav: self.cx.selected_project.as_deref(),
            job_status: self.cx.job_status,
        };
        let reload = self
            .projects
            .portfolio(ui, &view, &mut mem, self.cx.scrolls, &mut actions);
        let selection = mem.selection.clone();
        self.routes.insert(Route::Portfolio, mem);
        // The selected project's scheduled jobs (their run status), once per selection.
        if let Some(p) = selection
            && !self.job_status.contains_key(&p)
            && !self.pending(&Slot::ProjectJobs(p.clone()))
            && online.is_ok()
        {
            self.effects.push(Effect::LoadProjectJobs(p));
        }
        self.confirm_dialog(ui.ctx(), &mut actions);
        if reload {
            actions.push(Action::Reload);
        }
        self.act(actions);
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(confirm) = self.projects.confirm.clone() else {
            return;
        };
        let archive_work = match &confirm {
            Confirm::Archive(p) => self.archive_work_line(p.id.0.as_str()),
            _ => String::new(),
        };
        let decided = confirm_window(ctx, &confirm, &archive_work);
        if decided.is_some()
            && let Confirm::Archive(p) = &confirm
        {
            // A later confirmation counts the running work afresh.
            self.effects
                .push(Effect::ForgetArchiveCount(p.id.0.as_str().to_owned()));
        }
        match decided {
            Some(true) => {
                self.projects.confirm = None;
                actions.push(match confirm {
                    Confirm::Archive(p) => Action::Lifecycle(ARCHIVE, p),
                    Confirm::Delete(p) => Action::Lifecycle(DELETE, p),
                    Confirm::RemoveIndex(r) => Action::RemoveIndex(r),
                });
            }
            Some(false) => self.projects.confirm = None,
            None => {}
        }
    }

    /// The archive confirmation's line about the work it stops (WORK-16).
    fn archive_work_line(&mut self, project: &str) -> String {
        match self.archive_counts.get(project) {
            Some((roots, running)) if *running > 0 => format!(
                " {running} of its {roots} features and issues have agents working on them now; \
                 archiving stops them at once."
            ),
            Some((roots, _)) => {
                format!(" Its {roots} features and issues have no agent working now.")
            }
            None => {
                self.effects
                    .push(Effect::CountArchiveWork(project.to_owned()));
                " Counting the work that is running…".into()
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Repositories

    fn repositories_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Repositories.screen();
        let mut actions: Vec<Action> = Vec::new();
        let mut mem = self.routes.remove(&Route::Repositories).unwrap_or_default();
        let lists = self.cx.projects;
        let project = self.cx.selected_project.as_deref().and_then(|id| {
            lists
                .list
                .items
                .iter()
                .find(|p| p.id.0.as_str() == id)
                .cloned()
        });
        let removing: BTreeSet<String> = lists
            .repositories
            .items
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .filter(|id| self.waiting(&ProjectsSlot::RemoveIndex(id.clone())))
            .collect();
        let names: BTreeMap<String, String> = lists
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().to_owned(), p.name.0.clone()))
            .collect();
        let source = match &project {
            Some(p)
                if lists.members_of == Some(p.id.0.as_str()) || lists.members.error.is_some() =>
            {
                lists.members.clone_state()
            }
            Some(_) => ListState::default(),
            None => lists.repositories.clone_state(),
        };
        let online = self.cx.online.clone();
        let view = RepositoriesView {
            online: &online,
            project,
            source,
            removing,
            names,
            indexing: lists.indexing,
        };
        let (reload, link) =
            self.projects
                .repositories(ui, &view, &mut mem, self.cx.scrolls, &mut actions);
        let selected = mem.selection.clone();
        self.routes.insert(Route::Repositories, mem);
        self.confirm_dialog(ui.ctx(), &mut actions);
        if reload {
            actions.push(Action::Reload);
            // Refresh reads the selected repository's README and summary again too (a view
            // published since, or a read that failed).
            if let Some(r) = selected {
                actions.push(Action::LoadTexts(r));
            }
        }
        self.act(actions);
        if link && let Some((_, target)) = screen.link {
            self.navigate(target);
        }
    }
}
