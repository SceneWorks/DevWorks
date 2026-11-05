//! All projects and Repositories (DW-11): the project portfolio with its lifecycle tabs and a
//! right inner sidebar for the selected project (edit, repository membership, archive, restore,
//! delete and its repository base directory override, SET-07/08), the project selector of the
//! left navigation and the Repositories screen (registrations, README/summary texts and index
//! removal). Every change is the same `projects.*`, `repositories.*` or `settings.*` command an
//! agent tool submits; nothing here decides what a use case decides. A command whose outcome is
//! not known keeps its exact frame (Confirm outcomes, and the next launch, resubmit it).
use super::{ListState, ListText, Shell, list_body};
use crate::backend::Slot;
use crate::memory::RouteMemory;
use crate::ops::{self, Failure};
use crate::route::Route;
use crate::theme;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::ports::storage::ConfigScope;
use devworks_core::protocol::records::{
    AccessMode, LifeState, ProjectRecord, RepositoryRecord, RepositoryText,
};
use devworks_core::protocol::{DomainErrorCode, Id, ProjectId};
use devworks_core::settings::validate::codes;
use devworks_core::settings::view::{Health, SettingsView};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The repository base directory setting a project may override (SET-07/08).
pub const BASE_DIRECTORY: &str = "repositories.baseDirectory";
const ARCHIVE: &str = "projects.archive";
const RESTORE: &str = "projects.restore";
const DELETE: &str = "projects.delete";
/// The lifecycle tabs of All projects.
pub const TABS: [&str; 3] = ["Active", "Archived", "Deleted"];

/// A new project being written.
#[derive(Clone, Debug, Default)]
pub struct CreateForm {
    pub name: String,
    pub description: String,
    pub repositories: BTreeSet<String>,
}

/// An edit of one project's name, description and access mode. It keeps the record it was
/// opened on: changes are detected against it and submitted at its revision, so a concurrent
/// change is a revision conflict, never silently overwritten.
#[derive(Clone, Debug)]
pub struct EditForm {
    pub project: String,
    pub snapshot: ProjectRecord,
    pub name: String,
    pub description: String,
    pub access: AccessMode,
}

/// A membership change being chosen for one project, against the record it was opened on.
#[derive(Clone, Debug)]
pub struct MembershipForm {
    pub project: String,
    pub snapshot: ProjectRecord,
    pub repositories: BTreeSet<String>,
}

/// A destructive change waiting for the user's confirmation of its concrete consequences.
#[derive(Clone, Debug)]
pub enum Confirm {
    Archive(ProjectRecord),
    Delete(ProjectRecord),
    RemoveIndex(RepositoryRecord),
}

/// One project's repository base directory, viewed at the project scope.
#[derive(Default)]
pub struct BaseDirectory {
    pub view: Option<SettingsView>,
    pub buffer: String,
    pub loading: bool,
    pub saving: bool,
    /// Reload after the save settles (drafts are cleared, not kept as a conflict).
    pub after_save: bool,
    /// The text the running save submitted: text typed after it stays a draft when the save
    /// settles.
    pub submitted: Option<String>,
    pub error: Option<String>,
}

/// A repository text as the Repositories screen shows it.
#[derive(Clone, Debug)]
pub enum TextState {
    Loading,
    /// Not indexed (or not published) yet: an honest unavailable state, never empty text.
    Unavailable(String),
    Ready {
        text: RepositoryText,
        /// Its bounded content once read (`None`: not read yet, or binary).
        content: Option<String>,
    },
    Failed(String),
}

#[derive(Default)]
pub struct ProjectsUi {
    /// Active and archived projects (the navigation selector and the first two tabs).
    pub list: ListState<ProjectRecord>,
    /// Deleted projects' tombstones.
    pub deleted: ListState<ProjectRecord>,
    /// Every registration (membership choices; Repositories without a selected project).
    pub repositories: ListState<RepositoryRecord>,
    /// The selected project's repositories and which project they were read for.
    pub members: ListState<RepositoryRecord>,
    pub members_of: Option<String>,
    pub create: Option<CreateForm>,
    pub edit: Option<EditForm>,
    pub membership: Option<MembershipForm>,
    /// The exact form each running save submitted, by project: its reply settles that form
    /// only, and an editor holding later edits stays open at the saved revision.
    pub edit_saving: BTreeMap<String, EditForm>,
    pub membership_saving: BTreeMap<String, MembershipForm>,
    pub confirm: Option<Confirm>,
    pub message: Option<(Tone, String)>,
    pub base: BTreeMap<String, BaseDirectory>,
    pub texts: BTreeMap<(String, bool), TextState>,
    pub git: BTreeMap<String, GitView>,
    /// Index status, runs and the section's indexing sources (DW-23).
    pub indexing: super::indexing::IndexingUi,
    /// Applied project mutations; a list query started before the latest is re-queried.
    pub mutations: u64,
    pub list_at: u64,
}

#[derive(Default)]
pub struct GitView {
    pub inspection: Option<devworks_core::protocol::records::GitInspection>,
    pub error: Option<String>,
    pub base: String,
    pub head: String,
    pub patch: Option<String>,
}

/// What the screens ask the shell to do.
#[derive(Clone, Debug)]
enum Action {
    Reload,
    Create(CreateForm),
    Edit(EditForm),
    Membership(MembershipForm),
    Lifecycle(&'static str, ProjectRecord),
    Open(String),
    LoadBase(String),
    ChooseBase(String),
    SaveBase(String),
    InheritBase(String),
    LoadTexts(String),
    InspectGit(String),
    DiffGit(String),
    RemoveIndex(RepositoryRecord),
    Index(super::indexing::IndexAction),
    /// Scheduled jobs scoped to the project.
    OpenJobs(String),
}

pub fn life_label(l: &LifeState) -> &'static str {
    match l {
        LifeState::Active => "Active",
        LifeState::Archived => "Archived",
        LifeState::Deleted => "Deleted",
    }
}
fn access_label(a: &AccessMode) -> &'static str {
    match a {
        AccessMode::Full => "Full access",
        AccessMode::Project => "Project scope",
    }
}
fn same_access(a: &AccessMode, b: &AccessMode) -> bool {
    matches!(
        (a, b),
        (AccessMode::Full, AccessMode::Full) | (AccessMode::Project, AccessMode::Project)
    )
}

impl Shell {
    // -----------------------------------------------------------------------------------------
    // Requests

    /// Whether a command of `slot` is running or its outcome is still not confirmed.
    pub(super) fn waiting(&self, slot: &Slot) -> bool {
        self.pending(slot)
            || self
                .client
                .as_ref()
                .is_some_and(|c| c.unresolved().iter().any(|u| &u.slot == slot))
    }

    pub fn load_projects(&mut self) {
        self.projects.list_at = self.projects.mutations;
        self.query(
            Slot::ProjectsList,
            "Loading projects",
            "projects.list",
            |b| ops::projects_list(b, None),
        );
    }
    pub(super) fn load_deleted_projects(&mut self) {
        self.query(
            Slot::DeletedProjects,
            "Loading deleted projects",
            "projects.list",
            |b| ops::projects_list(b, Some("Deleted")),
        );
    }
    pub fn load_repositories(&mut self) {
        self.query(
            Slot::RepositoriesList,
            "Loading repositories",
            "repositories.list",
            |b| ops::repositories_list(b, None),
        );
        if let Some(p) = self.selected_project.clone() {
            self.query(
                Slot::ProjectRepositories(p.clone()),
                "Loading the project's repositories",
                "repositories.list",
                move |b| ops::repositories_list(b, Some(&p)),
            );
        }
    }
    fn load_base(&mut self, project: &str) {
        self.projects
            .base
            .entry(project.to_owned())
            .or_default()
            .loading = true;
        let p = project.to_owned();
        self.query(
            Slot::ProjectSettings(project.to_owned()),
            "Reading the repository base directory",
            "settings.read_effective",
            move |b| ops::project_settings_read(b, &p, &[BASE_DIRECTORY]),
        );
    }
    fn load_git(&mut self, repository: &str) {
        let r = repository.to_owned();
        self.query(
            Slot::GitInspect(r.clone()),
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
                Slot::RepositoryText(repository.to_owned(), readme),
                "Reading repository texts",
                op,
                move |b| ops::repository_text(b, op, &r),
            );
        }
    }

    /// The project the navigation selected, if it is still listed.
    pub fn selected_project_record(&self) -> Option<&ProjectRecord> {
        let id = self.selected_project.as_deref()?;
        self.projects
            .list
            .items
            .iter()
            .find(|p| p.id.0.as_str() == id)
    }
    pub fn selected_project(&self) -> Option<&str> {
        self.selected_project.as_deref()
    }
    /// Selects `project` in the navigation (its screens follow it); `None` clears it.
    pub fn select_project(&mut self, project: Option<String>) {
        if self.selected_project == project {
            return;
        }
        self.selected_project = project;
        self.projects.members = ListState::default();
        self.projects.members_of = None;
        if self.route == Route::Repositories {
            self.load_repositories();
        }
        if self.route == Route::Browser {
            self.load_browser();
        }
        if matches!(
            self.route,
            Route::Overview | Route::Features | Route::Issues
        ) {
            self.load_work();
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
                        self.load_project_jobs(&p);
                    }
                }
                Action::Create(form) => {
                    let (name, description, repos) = (
                        form.name.clone(),
                        form.description.clone(),
                        form.repositories.iter().cloned().collect::<Vec<_>>(),
                    );
                    self.command(
                        Slot::ProjectCreate,
                        "Creating a project",
                        "projects.create",
                        move |b| ops::project_create(b, &name, &description, &repos),
                    );
                }
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
                    let slot = Slot::ProjectCommand(p.id.0.as_str().to_owned());
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
                    let slot = Slot::ProjectCommand(p.id.0.as_str().to_owned());
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
                    let slot = Slot::ProjectCommand(p.id.0.as_str().to_owned());
                    let label = match op {
                        ARCHIVE => "Archiving the project",
                        RESTORE => "Restoring the project",
                        _ => "Deleting the project",
                    };
                    self.command(slot, label, op, move |b| ops::project_lifecycle(b, op, &p));
                }
                Action::Open(id) => {
                    self.select_project(Some(id));
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
                    if let Some(c) = &mut self.client {
                        c.command(
                            Slot::ProjectSettingsSave(p),
                            "Saving the repository base directory",
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
                            Slot::GitDiff(r.clone()),
                            "Reading Git patch",
                            "git.diff",
                            move |be| ops::git_diff(be, &r, &base, &head),
                        );
                    }
                }
                Action::LoadTexts(r) => {
                    self.load_texts(&r);
                    self.index_act(vec![super::indexing::IndexAction::Status(r)]);
                }
                Action::Index(a) => self.index_act(vec![a]),
                Action::OpenJobs(p) => {
                    self.jobs.set_scope(Some(p));
                    self.navigate(Route::Jobs);
                }
                Action::RemoveIndex(r) => {
                    let slot = Slot::RemoveIndex(r.id.as_str().to_owned());
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
    pub(super) fn apply_projects(
        &mut self,
        slot: Slot,
        reply: crate::backend::Reply,
    ) -> Option<(Slot, crate::backend::Reply)> {
        use crate::backend::Reply;
        match (slot, reply) {
            (Slot::ProjectsList, Reply::Query(_))
                if self.projects.list_at != self.projects.mutations =>
            {
                self.load_projects();
            }
            (Slot::ProjectsList, Reply::Query(q)) => match ops::projects(q) {
                Ok((items, more)) => {
                    self.projects.list = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    };
                    // A selection that no longer exists (deleted elsewhere) is dropped.
                    if let Some(s) = self.selected_project.clone()
                        && !self.projects.list.more
                        && !self
                            .projects
                            .list
                            .items
                            .iter()
                            .any(|p| p.id.0.as_str() == s)
                    {
                        self.select_project(None);
                    }
                    // Without a selection the browser sessions of every listed project show.
                    if self.route == Route::Browser && self.selected_project.is_none() {
                        self.load_browser();
                    }
                }
                Err(e) => self.projects.list.error = Some(e.describe()),
            },
            (Slot::DeletedProjects, Reply::Query(q)) => match ops::projects(q) {
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
            (Slot::RepositoriesList, Reply::Query(q)) => match ops::repositories(q) {
                Ok((items, more)) => {
                    self.projects.repositories = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    };
                }
                Err(e) => self.projects.repositories.error = Some(e.describe()),
            },
            (Slot::ProjectRepositories(p), Reply::Query(q)) => {
                // A reply for a project no longer selected is stale.
                if self.selected_project.as_deref() == Some(p.as_str()) {
                    match ops::repositories(q) {
                        Ok((items, more)) => {
                            self.projects.members = ListState {
                                items,
                                loaded: true,
                                error: None,
                                more,
                            };
                            self.projects.members_of = Some(p);
                        }
                        Err(e) => self.projects.members.error = Some(e.describe()),
                    }
                }
            }
            (Slot::ProjectCreate, Reply::Command(c)) => match ops::project_changed(c) {
                Ok(p) => {
                    self.projects.mutations += 1;
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
            (Slot::ProjectCommand(id), Reply::Command(c)) => match ops::project_changed(c) {
                Ok(p) => {
                    self.projects.mutations += 1;
                    let saved_edit = self.projects.edit_saving.remove(&id);
                    let saved_members = self.projects.membership_saving.remove(&id);
                    self.projects.edit =
                        edit_after_reply(self.projects.edit.take(), saved_edit.as_ref(), &p);
                    self.projects.membership = membership_after_reply(
                        self.projects.membership.take(),
                        saved_members.as_ref(),
                        &p,
                    );
                    self.projects.confirm = confirm_after_reply(self.projects.confirm.take(), &p);
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
                    if matches!(p.lifecycle, LifeState::Deleted)
                        && self.selected_project.as_deref() == Some(id.as_str())
                    {
                        self.select_project(None);
                    }
                    // Its lifecycle changed: the work screens re-read it (archived work is
                    // closed to change), and an archive count is taken afresh next time.
                    self.work.archive_counts.remove(&id);
                    if self.work.of.as_deref() == Some(id.as_str()) {
                        self.load_work();
                    }
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
            },
            (Slot::ProjectSettings(p), Reply::Query(q)) => {
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
            (Slot::ProjectSettingsHealth(p), Reply::Query(q)) => {
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
            (Slot::ProjectSettingsSave(p), Reply::Command(c)) => {
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
            (Slot::GitInspect(r), Reply::Query(q)) => {
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
            (Slot::GitDiff(r), Reply::Query(q)) => match ops::git_patch(q) {
                Ok(Some(artifact)) => {
                    self.query(
                        Slot::GitDiffContent(r),
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
            (Slot::GitDiffContent(r), Reply::Query(q)) => {
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
            (Slot::RepositoryText(r, readme), Reply::Query(q)) => {
                let state = match ops::repository_text_result(q) {
                    Ok(text) => {
                        let artifact = text.content.clone();
                        self.query(
                            Slot::RepositoryContent(r.clone(), readme),
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
            (Slot::RepositoryContent(r, readme), Reply::Query(q)) => {
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
            (Slot::RemoveIndex(r), Reply::Command(c)) => match ops::index_removed(c) {
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
            Slot::ProjectSettingsHealth(project.to_owned()),
            "Checking the repository base directory",
            "settings.validate",
            move |b| ops::project_settings_validate(b, &p, revision, &[(BASE_DIRECTORY, value)]),
        );
    }

    // -----------------------------------------------------------------------------------------
    // Navigation selector

    /// The project selector of the left navigation.
    pub(super) fn project_selector(&mut self, ui: &mut Ui) {
        let label = self
            .selected_project_record()
            .map(|p| p.name.0.clone())
            .unwrap_or_else(|| "No project selected".into());
        let mut pick: Option<Option<String>> = None;
        let r =
            egui::ComboBox::from_id_salt("project-selector")
                .selected_text(label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.selected_project.is_none(), "No project selected")
                        .clicked()
                    {
                        pick = Some(None);
                    }
                    for p in
                        self.projects.list.items.iter().filter(|p| {
                            matches!(p.lifecycle, LifeState::Active | LifeState::Archived)
                        })
                    {
                        let id = p.id.0.as_str();
                        let text = match p.lifecycle {
                            LifeState::Archived => format!("{} (archived)", p.name.0),
                            _ => p.name.0.clone(),
                        };
                        if ui
                            .selectable_label(self.selected_project.as_deref() == Some(id), text)
                            .clicked()
                        {
                            pick = Some(Some(id.to_owned()));
                        }
                    }
                });
        r.response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Selected project")
        });
        if let Some(p) = pick {
            self.select_project(p);
        }
    }

    // -----------------------------------------------------------------------------------------
    // All projects

    pub(super) fn portfolio_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Portfolio.screen();
        let online = self.online();
        let mut actions: Vec<Action> = Vec::new();
        let mut mem = self.routes.remove(&Route::Portfolio).unwrap_or_default();
        let tab = mem.tab.min(TABS.len() - 1);
        let mut reload = false;
        let busy_create = self.waiting(&Slot::ProjectCreate);
        let waiting: BTreeSet<String> = self
            .projects
            .list
            .items
            .iter()
            .chain(&self.projects.deleted.items)
            .map(|p| p.id.0.as_str().to_owned())
            .filter(|id| self.waiting(&Slot::ProjectCommand(id.clone())))
            .collect();
        let base_waiting: BTreeSet<String> = self
            .projects
            .base
            .keys()
            .filter(|id| self.waiting(&Slot::ProjectSettingsSave((*id).clone())))
            .cloned()
            .collect();
        let selected_nav = self.selected_project.clone();
        let job_status = self.jobs.project_status.clone();
        let projects = &mut self.projects;
        let scrolls = &mut self.scrolls;
        let mut new_tab = tab;
        {
            let mut st = (&mut *projects, &mut mem, &mut actions, &mut reload);
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
                                *st.3 = true;
                            }
                            let r = ui.add_enabled(
                                online.is_ok() && !busy_create,
                                egui::Button::new("New project"),
                            );
                            if r.clicked() && st.0.create.is_none() {
                                st.0.create = Some(CreateForm::default());
                            }
                        },
                        |ui, st, tab| {
                            let (projects, mem, actions, reload) =
                                (&mut *st.0, &mut *st.1, &mut *st.2, &mut *st.3);
                            if let Some((tone, text)) = &projects.message {
                                chip(ui, text.clone(), *tone);
                                ui.add_space(6.0);
                            }
                            create_form(ui, projects, busy_create, actions);
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut mem.filter)
                                    .hint_text("Filter projects")
                                    .desired_width(280.0),
                            );
                            r.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::Role::TextInput,
                                    true,
                                    "Filter projects",
                                )
                            });
                            let source = if tab == 2 {
                                &projects.deleted
                            } else {
                                &projects.list
                            };
                            let wanted = TABS[tab];
                            let shown = ListState {
                                items: source
                                    .items
                                    .iter()
                                    .filter(|p| life_label(&p.lifecycle) == wanted)
                                    .cloned()
                                    .collect(),
                                loaded: source.loaded,
                                error: source.error.clone(),
                                more: source.more,
                            };
                            let RouteMemory {
                                filter, selection, ..
                            } = mem;
                            list_body(
                                ui,
                                &online,
                                &shown,
                                ListText {
                                    what: "projects",
                                    empty_title: match tab {
                                        0 => "No projects yet",
                                        1 => "No archived projects",
                                        _ => "No deleted projects",
                                    },
                                    empty_body: match tab {
                                        0 => "Create a project to group repositories and work.",
                                        _ => "",
                                    },
                                    id_salt: "projects-list",
                                },
                                reload,
                                scrolls,
                                |p: &ProjectRecord| format!("{} {}", p.name.0, p.description.0),
                                |p: &ProjectRecord| p.id.0.as_str().to_owned(),
                                filter,
                                selection,
                                |ui, p: &ProjectRecord, selected| {
                                    let n = p.repository_ids.0.len();
                                    ui.selectable_label(
                                        selected,
                                        format!(
                                            "{}  ·  {n} repositor{}  ·  {}",
                                            p.name.0,
                                            if n == 1 { "y" } else { "ies" },
                                            access_label(&p.access_mode)
                                        ),
                                    )
                                    .clicked()
                                },
                            );
                        },
                    );
                    new_tab = t;
                },
                |ui, st| {
                    let (projects, mem, actions) = (&mut *st.0, &*st.1, &mut *st.2);
                    widgets::secondary_pane(ui, "Project", |ui| {
                        let found = mem.selection.as_deref().and_then(|id| {
                            projects
                                .list
                                .items
                                .iter()
                                .chain(&projects.deleted.items)
                                .find(|p| p.id.0.as_str() == id)
                                .cloned()
                        });
                        match found {
                            None => {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Empty {
                                        title: "Nothing selected".into(),
                                        body: "Select a project to see and change it here.".into(),
                                    },
                                );
                            }
                            Some(p) => {
                                let id = p.id.0.as_str().to_owned();
                                project_detail(
                                    ui,
                                    projects,
                                    &p,
                                    online.is_ok() && !waiting.contains(&id),
                                    base_waiting.contains(&id),
                                    selected_nav.as_deref() == Some(id.as_str()),
                                    actions,
                                );
                                // The project's scheduled jobs and their run status (JOB-01).
                                widgets::divider(ui);
                                ui.label(RichText::new("Scheduled jobs").strong());
                                match job_status.get(&id) {
                                    Some(s) => widgets::caption(ui, s.clone()),
                                    None => widgets::caption(ui, "Loading…"),
                                }
                                if widgets::secondary_button(ui, "Open scheduled jobs").clicked() {
                                    actions.push(Action::OpenJobs(id.clone()));
                                }
                            }
                        }
                    });
                },
            );
        }
        mem.tab = new_tab;
        let selection = mem.selection.clone();
        self.routes.insert(Route::Portfolio, mem);
        // The selected project's scheduled jobs (their run status), once per selection.
        if let Some(p) = selection
            && !self.jobs.project_status.contains_key(&p)
            && !self.pending(&Slot::ProjectJobs(p.clone()))
            && online.is_ok()
        {
            self.load_project_jobs(&p);
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
        let (title, body, go): (&str, String, &str) = match &confirm {
            Confirm::Archive(p) => (
                "Archive project",
                format!(
                    "Archive {}? DevWorks stops all of this project's running work immediately, \
                     without waiting for agents to reach a safe step, and admits no new work for \
                     it. Work already accepted by a remote system may still finish there. \
                     Restoring the project later leaves its work stopped until you continue it.{}",
                    p.name.0,
                    self.archive_work_line(p.id.0.as_str())
                ),
                "Archive project",
            ),
            Confirm::Delete(p) => (
                "Delete project",
                format!(
                    "Delete {}? DevWorks removes the project and its repository memberships. It \
                     never deletes repository folders, branches, worktrees or remotes, and it \
                     keeps work history, receipts, artifacts and settings revisions. This cannot \
                     be undone.",
                    p.name.0
                ),
                "Delete project",
            ),
            Confirm::RemoveIndex(r) => (
                "Remove repository index",
                format!(
                    "Remove the index of {}? DevWorks removes its index views and README and \
                     summary texts for every project that shares it. The repository, its files, \
                     memberships and work history are kept; indexing can publish it again.",
                    r.name.0
                ),
                "Remove index",
            ),
        };
        let mut decided: Option<bool> = None;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(420.0);
                ui.label(body);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(go).clicked() {
                        decided = Some(true);
                    }
                    if widgets::secondary_button(ui, "Cancel").clicked() {
                        decided = Some(false);
                    }
                });
            });
        if decided.is_some()
            && let Confirm::Archive(p) = &confirm
        {
            // A later confirmation counts the running work afresh.
            self.work.forget_archive_count(p.id.0.as_str());
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

    // -----------------------------------------------------------------------------------------
    // Repositories

    pub(super) fn repositories_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Repositories.screen();
        let online = self.online();
        let mut actions: Vec<Action> = Vec::new();
        let mut mem = self.routes.remove(&Route::Repositories).unwrap_or_default();
        let tab = mem.tab;
        let mut reload = false;
        let mut link = false;
        let project = self.selected_project_record().cloned();
        let removing: BTreeSet<String> = self
            .projects
            .repositories
            .items
            .iter()
            .map(|r| r.id.as_str().to_owned())
            .filter(|id| self.waiting(&Slot::RemoveIndex(id.clone())))
            .collect();
        let names: BTreeMap<String, String> = self
            .projects
            .list
            .items
            .iter()
            .map(|p| (p.id.0.as_str().to_owned(), p.name.0.clone()))
            .collect();
        let source = match &project {
            Some(p)
                if self.projects.members_of.as_deref() == Some(p.id.0.as_str())
                    || self.projects.members.error.is_some() =>
            {
                self.projects.members.clone_state()
            }
            Some(_) => ListState::default(),
            None => self.projects.repositories.clone_state(),
        };
        let projects = &mut self.projects;
        let scrolls = &mut self.scrolls;
        let mut new_tab = tab;
        {
            let mut st = (
                &mut *projects,
                &mut mem,
                &mut actions,
                &mut reload,
                &mut link,
            );
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
                            tabs: screen.subviews,
                            tab,
                        },
                        st,
                        |ui, st| {
                            if let Some((label, _)) = screen.link
                                && widgets::secondary_button(ui, label).clicked()
                            {
                                *st.4 = true;
                            }
                            if widgets::secondary_button(ui, "Refresh").clicked() {
                                *st.3 = true;
                            }
                        },
                        |ui, st, tab| {
                            let (projects, mem, actions, reload) =
                                (&mut *st.0, &mut *st.1, &mut *st.2, &mut *st.3);
                            if tab != 0 {
                                let what = screen.subviews.get(tab).copied().unwrap_or("This view");
                                widgets::state_block(
                                    ui,
                                    &ViewState::Unavailable {
                                        what: format!(
                                            "{what}: the code graph, health and review history"
                                        ),
                                        owner: "DW-28, DW-29".into(),
                                    },
                                );
                                return;
                            }
                            if let Some((tone, text)) = &projects.message {
                                chip(ui, text.clone(), *tone);
                                ui.add_space(6.0);
                            }
                            widgets::caption(
                                ui,
                                match &project {
                                    Some(p) => format!("Repositories of {}", p.name.0),
                                    None => "Every registered repository (select a project to \
                                             narrow this)"
                                        .into(),
                                },
                            );
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut mem.filter)
                                    .hint_text("Filter repositories")
                                    .desired_width(280.0),
                            );
                            r.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::Role::TextInput,
                                    true,
                                    "Filter repositories",
                                )
                            });
                            let before = mem.selection.clone();
                            let RouteMemory {
                                filter, selection, ..
                            } = mem;
                            list_body(
                                ui,
                                &online,
                                &source,
                                ListText {
                                    what: "repositories",
                                    empty_title: "No repositories",
                                    empty_body: "Registered repositories appear here once \
                                                 discovery finds them under the repository base \
                                                 directory (Indexing logs › Discover repositories).",
                                    id_salt: "repositories-list",
                                },
                                reload,
                                scrolls,
                                |r: &RepositoryRecord| {
                                    format!(
                                        "{} {}",
                                        r.name.0,
                                        r.remote_url.as_ref().map(|u| u.0.as_str()).unwrap_or("")
                                    )
                                },
                                |r: &RepositoryRecord| r.id.as_str().to_owned(),
                                filter,
                                selection,
                                |ui, r: &RepositoryRecord, selected| {
                                    ui.selectable_label(
                                        selected,
                                        format!(
                                            "{}  ·  {}  ·  {}",
                                            r.name.0,
                                            r.remote_url
                                                .as_ref()
                                                .map(|u| u.0.as_str())
                                                .unwrap_or("no remote"),
                                            coverage_label(r)
                                        ),
                                    )
                                    .clicked()
                                },
                            );
                            if mem.selection != before
                                && let Some(id) = mem.selection.clone()
                            {
                                actions.push(Action::LoadTexts(id));
                            }
                        },
                    );
                    new_tab = t;
                },
                |ui, st| {
                    // The selected repository's detail sits in the right inner sidebar, beside
                    // the list (never below it, where a long list would push it off screen).
                    let (projects, mem, actions) = (&mut *st.0, &*st.1, &mut *st.2);
                    widgets::secondary_pane(ui, "Repository", |ui| {
                        let found = mem
                            .selection
                            .as_deref()
                            .and_then(|id| source.items.iter().find(|r| r.id.as_str() == id));
                        match found {
                            None => {
                                widgets::state_block(
                                    ui,
                                    &ViewState::Empty {
                                        title: "Nothing selected".into(),
                                        body: "Select a repository to see its texts here.".into(),
                                    },
                                );
                            }
                            Some(r) => {
                                egui::ScrollArea::vertical()
                                    .id_salt("repository-detail")
                                    .show(ui, |ui| {
                                        let enabled =
                                            online.is_ok() && !removing.contains(r.id.as_str());
                                        repository_detail(
                                            ui, projects, r, &names, enabled, actions,
                                        );
                                        let mut index_actions = Vec::new();
                                        if let Some((tone, text)) = &projects.indexing.message {
                                            chip(ui, text.clone(), *tone);
                                        }
                                        super::indexing::index_block(
                                            ui,
                                            &projects.indexing,
                                            r,
                                            enabled,
                                            &mut index_actions,
                                        );
                                        actions
                                            .extend(index_actions.into_iter().map(Action::Index));
                                    });
                            }
                        }
                    });
                },
            );
        }
        mem.tab = new_tab;
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

impl<T: Clone> ListState<T> {
    fn clone_state(&self) -> Self {
        Self {
            items: self.items.clone(),
            loaded: self.loaded,
            error: self.error.clone(),
            more: self.more,
        }
    }
}

fn coverage_label(r: &RepositoryRecord) -> String {
    match serde_json::to_value(&r.coverage.state)
        .ok()
        .as_ref()
        .and_then(Value::as_str)
    {
        Some("Unavailable") | None => "not indexed".into(),
        Some(s) => format!("index {}", s.to_lowercase()),
    }
}

fn create_form(ui: &mut Ui, projects: &mut ProjectsUi, busy: bool, actions: &mut Vec<Action>) {
    let mut cancel = false;
    let repositories = projects.repositories.items.clone();
    let Some(form) = projects.create.as_mut() else {
        return;
    };
    egui::Frame::new()
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("New project").strong());
            let r = ui.add(
                egui::TextEdit::singleline(&mut form.name)
                    .hint_text("Name")
                    .desired_width(320.0),
            );
            r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Name"));
            let r = ui.add(
                egui::TextEdit::multiline(&mut form.description)
                    .hint_text("Description")
                    .desired_rows(2)
                    .desired_width(f32::INFINITY),
            );
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::MultilineTextInput, true, "Description")
            });
            repository_choices(ui, &repositories, &mut form.repositories);
            ui.horizontal(|ui| {
                let ok = !form.name.trim().is_empty() && !busy;
                if ui.add_enabled(ok, egui::Button::new("Create")).clicked() {
                    actions.push(Action::Create(form.clone()));
                }
                if widgets::secondary_button(ui, "Cancel").clicked() {
                    cancel = true;
                }
                if busy {
                    widgets::caption(ui, "Creating…");
                }
            });
        });
    ui.add_space(8.0);
    if cancel && !busy {
        projects.create = None;
    }
}

fn repository_choices(ui: &mut Ui, all: &[RepositoryRecord], chosen: &mut BTreeSet<String>) {
    if all.is_empty() {
        widgets::caption(
            ui,
            "No repositories are registered yet; you can add them to the project later.",
        );
        return;
    }
    widgets::caption(ui, "Repositories");
    egui::ScrollArea::vertical()
        .id_salt("repository-choices")
        .max_height(160.0)
        .show(ui, |ui| {
            for r in all {
                let id = r.id.as_str().to_owned();
                let mut on = chosen.contains(&id);
                if ui.checkbox(&mut on, r.name.0.as_str()).changed() {
                    if on {
                        chosen.insert(id);
                    } else {
                        chosen.remove(&id);
                    }
                }
            }
        });
}

fn project_detail(
    ui: &mut Ui,
    projects: &mut ProjectsUi,
    p: &ProjectRecord,
    enabled: bool,
    base_saving: bool,
    is_open: bool,
    actions: &mut Vec<Action>,
) {
    let id = p.id.0.as_str().to_owned();
    ui.label(RichText::new(&p.name.0).strong().size(16.0));
    if !p.description.0.is_empty() {
        ui.label(&p.description.0);
    }
    ui.horizontal_wrapped(|ui| {
        chip(
            ui,
            life_label(&p.lifecycle),
            match p.lifecycle {
                LifeState::Active => Tone::Good,
                LifeState::Archived => Tone::Attention,
                LifeState::Deleted => Tone::Neutral,
            },
        );
        chip(ui, access_label(&p.access_mode), Tone::Neutral);
        widgets::caption(ui, format!("Revision {}", p.revision.as_str()));
    });
    if !enabled {
        widgets::caption(
            ui,
            "A change to this project is in progress or not confirmed yet.",
        );
    }
    ui.add_space(6.0);
    let names: BTreeMap<&str, &str> = projects
        .repositories
        .items
        .iter()
        .map(|r| (r.id.as_str(), r.name.0.as_str()))
        .collect();
    widgets::caption(ui, "Repositories");
    if p.repository_ids.0.is_empty() {
        ui.label("None yet.");
    }
    for r in &p.repository_ids.0 {
        ui.label(format!(
            "• {}",
            names.get(r.as_str()).copied().unwrap_or(r.as_str())
        ));
    }
    ui.add_space(8.0);
    match p.lifecycle {
        LifeState::Deleted => {
            widgets::state_block(
                ui,
                &ViewState::Empty {
                    title: "Deleted".into(),
                    body: "This project was deleted from DevWorks. Its repositories, their \
                           folders, branches, worktrees and remotes were not touched; its history \
                           is kept."
                        .into(),
                },
            );
        }
        LifeState::Archived => {
            widgets::caption(
                ui,
                "Archived: no work runs or starts here. Restoring leaves its work stopped until \
                 you continue it.",
            );
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new("Restore"))
                    .clicked()
                {
                    actions.push(Action::Lifecycle(RESTORE, p.clone()));
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Delete…"))
                    .clicked()
                {
                    projects.confirm = Some(Confirm::Delete(p.clone()));
                }
            });
        }
        LifeState::Active => {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(!is_open, egui::Button::new("Open project"))
                    .clicked()
                {
                    actions.push(Action::Open(id.clone()));
                }
                if ui.add_enabled(enabled, egui::Button::new("Edit")).clicked() {
                    projects.edit = Some(EditForm {
                        project: id.clone(),
                        snapshot: p.clone(),
                        name: p.name.0.clone(),
                        description: p.description.0.clone(),
                        access: p.access_mode.clone(),
                    });
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Repositories…"))
                    .clicked()
                {
                    projects.membership = Some(MembershipForm {
                        project: id.clone(),
                        snapshot: p.clone(),
                        repositories: p
                            .repository_ids
                            .0
                            .iter()
                            .map(|r| r.as_str().to_owned())
                            .collect(),
                    });
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Archive…"))
                    .clicked()
                {
                    projects.confirm = Some(Confirm::Archive(p.clone()));
                }
            });
            edit_form(ui, projects, p, enabled, actions);
            membership_form(ui, projects, p, enabled, actions);
            ui.add_space(10.0);
            base_directory(ui, projects, &id, base_saving, actions);
        }
    }
}

fn edit_form(
    ui: &mut Ui,
    projects: &mut ProjectsUi,
    p: &ProjectRecord,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let mut close = false;
    let Some(form) = projects
        .edit
        .as_mut()
        .filter(|f| f.project == p.id.0.as_str())
    else {
        return;
    };
    ui.add_space(6.0);
    let r = ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(f32::INFINITY));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Project name"));
    let r = ui.add(
        egui::TextEdit::multiline(&mut form.description)
            .desired_rows(2)
            .desired_width(f32::INFINITY),
    );
    r.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::MultilineTextInput, true, "Project description")
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Agent access:");
        for (mode, text) in [
            (AccessMode::Full, "Full access"),
            (AccessMode::Project, "Project scope"),
        ] {
            if ui.radio(same_access(&form.access, &mode), text).clicked() {
                form.access = mode;
            }
        }
    });
    widgets::caption(
        ui,
        "Agent access is this project's access profile setting; only you can change it.",
    );
    ui.horizontal(|ui| {
        let ok = enabled && !form.name.trim().is_empty();
        if ui.add_enabled(ok, egui::Button::new("Save")).clicked() {
            actions.push(Action::Edit(form.clone()));
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            close = true;
        }
    });
    if close {
        projects.edit = None;
    }
}

/// Rebases one field onto a returned record: a value the user did not change since `base` takes
/// the returned value (which may differ, e.g. an access profile changed in settings); a changed
/// value is the user's and stays.
fn rebase<T>(mine: T, base: &T, returned: T, same: impl Fn(&T, &T) -> bool) -> T {
    if same(&mine, base) { returned } else { mine }
}

/// The project editor after a project command of `p` succeeded. A form of another project is
/// untouched. This project's form closes when the project is no longer active, or when its own
/// save (`saved`) holds everything it shows. Otherwise it stays open, rebased onto `p` (whatever
/// command succeeded, so its next save is at the current revision): each field the user did not
/// change takes `p`'s value, each changed field keeps the user's. A field is changed when it
/// differs from what this form's save submitted, or, when the reply is not its save, from the
/// record the form was opened on.
pub fn edit_after_reply(
    live: Option<EditForm>,
    saved: Option<&EditForm>,
    p: &ProjectRecord,
) -> Option<EditForm> {
    let f = live?;
    if f.project != p.id.0.as_str() {
        return Some(f);
    }
    if !matches!(p.lifecycle, LifeState::Active) {
        return None;
    }
    let (name, description, access) = match saved {
        Some(s) => (s.name.clone(), s.description.clone(), s.access.clone()),
        None => (
            f.snapshot.name.0.clone(),
            f.snapshot.description.0.clone(),
            f.snapshot.access_mode.clone(),
        ),
    };
    let unsaved =
        f.name != name || f.description != description || !same_access(&f.access, &access);
    if saved.is_some() && !unsaved {
        return None;
    }
    Some(EditForm {
        project: f.project,
        name: rebase(f.name, &name, p.name.0.clone(), String::eq),
        description: rebase(
            f.description,
            &description,
            p.description.0.clone(),
            String::eq,
        ),
        access: rebase(f.access, &access, p.access_mode.clone(), same_access),
        snapshot: p.clone(),
    })
}

/// The membership form after a project command of `p` succeeded (as [`edit_after_reply`]).
pub fn membership_after_reply(
    live: Option<MembershipForm>,
    saved: Option<&MembershipForm>,
    p: &ProjectRecord,
) -> Option<MembershipForm> {
    let f = live?;
    if f.project != p.id.0.as_str() {
        return Some(f);
    }
    if !matches!(p.lifecycle, LifeState::Active) {
        return None;
    }
    let members = |r: &ProjectRecord| -> BTreeSet<String> {
        r.repository_ids
            .0
            .iter()
            .map(|r| r.as_str().to_owned())
            .collect()
    };
    let base = saved.map_or_else(|| members(&f.snapshot), |s| s.repositories.clone());
    if saved.is_some() && f.repositories == base {
        return None;
    }
    Some(MembershipForm {
        project: f.project,
        repositories: rebase(f.repositories, &base, members(p), BTreeSet::eq),
        snapshot: p.clone(),
    })
}

/// A waiting archive or delete confirmation after a project command of `p` succeeded: it is
/// rebased onto `p` (so confirming applies at the current revision and shows its current name),
/// or dropped when `p`'s lifecycle moved and its consequences no longer describe the project.
pub fn confirm_after_reply(live: Option<Confirm>, p: &ProjectRecord) -> Option<Confirm> {
    let rebased = |r: ProjectRecord| {
        if r.id.0.as_str() != p.id.0.as_str() {
            Some(r)
        } else if life_label(&r.lifecycle) == life_label(&p.lifecycle) {
            Some(p.clone())
        } else {
            None
        }
    };
    match live? {
        Confirm::Archive(r) => rebased(r).map(Confirm::Archive),
        Confirm::Delete(r) => rebased(r).map(Confirm::Delete),
        other @ Confirm::RemoveIndex(_) => Some(other),
    }
}

fn membership_form(
    ui: &mut Ui,
    projects: &mut ProjectsUi,
    p: &ProjectRecord,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let mut close = false;
    let all = projects.repositories.items.clone();
    let Some(form) = projects
        .membership
        .as_mut()
        .filter(|f| f.project == p.id.0.as_str())
    else {
        return;
    };
    ui.add_space(6.0);
    repository_choices(ui, &all, &mut form.repositories);
    widgets::caption(
        ui,
        "Removing a repository only removes it from this project: other projects that share it \
         and its files are not affected.",
    );
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("Save repositories"))
            .clicked()
        {
            actions.push(Action::Membership(form.clone()));
        }
        if widgets::secondary_button(ui, "Cancel").clicked() {
            close = true;
        }
    });
    if close {
        projects.membership = None;
    }
}

fn base_directory(
    ui: &mut Ui,
    projects: &mut ProjectsUi,
    project: &str,
    saving: bool,
    actions: &mut Vec<Action>,
) {
    widgets::caption(ui, "Repository base directory");
    let Some(b) = projects.base.get_mut(project) else {
        actions.push(Action::LoadBase(project.to_owned()));
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "the repository base directory".into(),
            },
        );
        return;
    };
    if let Some(e) = &b.error {
        chip(ui, e.clone(), Tone::Bad);
    }
    let Some(view) = b.view.as_mut() else {
        if !b.loading && b.error.is_some() && widgets::secondary_button(ui, "Retry").clicked() {
            actions.push(Action::LoadBase(project.to_owned()));
        }
        return;
    };
    let Some(item) = view.item(BASE_DIRECTORY) else {
        return;
    };
    let source = item.source.label();
    let overridden = matches!(
        item.source,
        devworks_core::settings::view::Source::Project { .. }
    );
    let effective = item.effective.as_str().unwrap_or("").to_owned();
    let health = item.health.clone();
    let dirty = item.is_dirty();
    widgets::caption(
        ui,
        if overridden {
            format!("Overridden for this project ({source}). Effective: {effective}")
        } else {
            format!("Inherited from {source}. Effective: {effective}")
        },
    );
    if let Health::Unavailable(why) = &health {
        chip(ui, format!("Unavailable: {why}"), Tone::Attention);
    }
    let r = ui.add(
        egui::TextEdit::singleline(&mut b.buffer)
            .hint_text("Inherit the workspace default")
            .desired_width(f32::INFINITY),
    );
    r.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Repository base directory")
    });
    if r.changed() {
        let v = b.buffer.trim().to_owned();
        if v.is_empty() {
            let _ = view.inherit(BASE_DIRECTORY);
        } else {
            let _ = view.edit(BASE_DIRECTORY, json!(v));
        }
    }
    let issues: Vec<String> = view
        .item(BASE_DIRECTORY)
        .map(|i| i.issues.iter().map(|x| x.message.clone()).collect())
        .unwrap_or_default();
    for i in issues {
        chip(ui, i, Tone::Bad);
    }
    widgets::caption(
        ui,
        "New clones for this project go here. Changing it never moves existing repositories or \
         changes their recorded paths.",
    );
    ui.horizontal_wrapped(|ui| {
        if widgets::secondary_button(ui, "Choose…").clicked() {
            actions.push(Action::ChooseBase(project.to_owned()));
        }
        if ui
            .add_enabled(
                overridden && !saving,
                egui::Button::new("Use workspace default"),
            )
            .clicked()
        {
            actions.push(Action::InheritBase(project.to_owned()));
        }
        if ui
            .add_enabled(
                dirty && !saving && !b.saving,
                egui::Button::new("Save directory"),
            )
            .clicked()
        {
            actions.push(Action::SaveBase(project.to_owned()));
        }
        if saving || b.saving {
            widgets::caption(ui, "Saving…");
        }
    });
}

fn repository_detail(
    ui: &mut Ui,
    projects: &mut ProjectsUi,
    r: &RepositoryRecord,
    names: &BTreeMap<String, String>,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    egui::Frame::new()
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(&r.name.0).strong());
            if ui.small_button("Inspect source control").clicked() {
                actions.push(Action::InspectGit(r.id.as_str().to_owned()));
            }
            if let Some(view) = projects.git.get_mut(r.id.as_str()) {
                if let Some(error) = &view.error {
                    widgets::caption(ui, error);
                }
                if let Some(state) = &view.inspection {
                    widgets::caption(
                        ui,
                        if state.dirty {
                            "Uncommitted changes"
                        } else {
                            "Clean checkout"
                        },
                    );
                    let github = r
                        .remote_url
                        .as_ref()
                        .and_then(|u| {
                            devworks_core::services::git::github::Repository::from_remote(&u.0)
                        })
                        .map(|r| format!("https://github.com/{}/{}", r.owner, r.name));
                    if let Some(branch) = &state.branch {
                        if let Some(url) = &github {
                            ui.hyperlink_to(
                                &branch.0,
                                devworks_core::services::git::github::branch_url(url, &branch.0)
                                    .unwrap_or_else(|| url.clone()),
                            );
                        } else {
                            ui.label(&branch.0);
                        }
                    }
                    if let Some(url) = &github {
                        ui.hyperlink_to(
                            &state.head.hex,
                            format!("{url}/commit/{}", state.head.hex),
                        );
                    } else {
                        ui.monospace(&state.head.hex);
                    }
                }
                egui::CollapsingHeader::new("Compare commits")
                    .id_salt(("git-diff", r.id.as_str()))
                    .show(ui, |ui| {
                        ui.label("Base commit");
                        ui.text_edit_singleline(&mut view.base);
                        ui.label("Head commit");
                        ui.text_edit_singleline(&mut view.head);
                        if ui.small_button("Read patch").clicked() {
                            actions.push(Action::DiffGit(r.id.as_str().to_owned()));
                        }
                        if let Some(patch) = &view.patch {
                            egui::ScrollArea::vertical()
                                .max_height(320.0)
                                .show(ui, |ui| {
                                    ui.monospace(patch);
                                });
                        }
                    });
            }

            ui.horizontal_wrapped(|ui| {
                chip(ui, life_label(&r.lifecycle), Tone::Neutral);
                chip(
                    ui,
                    if r.primary {
                        "Local checkout"
                    } else {
                        "Remote only"
                    },
                    Tone::Neutral,
                );
                chip(ui, coverage_label(r), Tone::Neutral);
            });
            widgets::caption(
                ui,
                format!(
                    "Remote: {}",
                    r.remote_url
                        .as_ref()
                        .map(|u| u.0.as_str())
                        .unwrap_or("none")
                ),
            );
            let in_projects: Vec<&str> = r
                .project_ids
                .0
                .iter()
                .map(|p| names.get(p.0.as_str()).map_or(p.0.as_str(), String::as_str))
                .collect();
            widgets::caption(
                ui,
                if in_projects.is_empty() {
                    "In no project".to_owned()
                } else {
                    format!("In {}", in_projects.join(", "))
                },
            );
            for (readme, title) in [(true, "README"), (false, "Summary")] {
                ui.add_space(6.0);
                widgets::caption(ui, title);
                match projects.texts.get(&(r.id.as_str().to_owned(), readme)) {
                    None => {
                        if widgets::secondary_button(ui, "Load").clicked() {
                            actions.push(Action::LoadTexts(r.id.as_str().to_owned()));
                        }
                    }
                    Some(TextState::Loading) => widgets::caption(ui, "Loading…"),
                    Some(TextState::Unavailable(why)) => {
                        widgets::caption(ui, why.clone());
                        if widgets::secondary_button(ui, &format!("Retry {title}")).clicked() {
                            actions.push(Action::LoadTexts(r.id.as_str().to_owned()));
                        }
                    }
                    Some(TextState::Failed(e)) => {
                        chip(ui, e.clone(), Tone::Bad);
                        if widgets::secondary_button(ui, &format!("Retry {title}")).clicked() {
                            actions.push(Action::LoadTexts(r.id.as_str().to_owned()));
                        }
                    }
                    Some(TextState::Ready { text, content }) => {
                        widgets::caption(
                            ui,
                            format!(
                                "View {} · {} bytes · {}",
                                text.view_id.as_str(),
                                text.content.byte_length.get(),
                                text.content.media_type
                            ),
                        );
                        match content {
                            Some(c) => {
                                egui::ScrollArea::vertical()
                                    .id_salt(("repository-text", readme))
                                    .max_height(220.0)
                                    .show(ui, |ui| ui.label(c));
                            }
                            None => widgets::caption(ui, "Reading…"),
                        }
                    }
                }
            }
            ui.add_space(8.0);
            let indexed = !r.indexed_view_ids.0.is_empty();
            if ui
                .add_enabled(enabled && indexed, egui::Button::new("Remove index…"))
                .clicked()
            {
                projects.confirm = Some(Confirm::RemoveIndex(r.clone()));
            }
        });
}
