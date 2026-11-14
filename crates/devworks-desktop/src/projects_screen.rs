//! All projects and Repositories (DW-11): the project portfolio with its lifecycle tabs and a
//! right inner sidebar for the selected project (edit, repository membership, archive, restore,
//! delete and its repository base directory override, SET-07/08), and the Repositories screen
//! (registrations, README/summary texts, source control and index removal). The views report
//! what the user asked for as actions; the controller submits them.
use crate::memory::RouteMemory;
use crate::route::Route;
use crate::shell::indexing::{IndexAction, IndexingUi, index_block};
use crate::shell::{ListState, ListText, list_body};
use crate::theme;
use crate::virtual_list::Scrolls;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::records::{
    AccessMode, LifeState, ProjectRecord, RepositoryRecord, RepositoryText,
};
use devworks_core::settings::view::{Health, SettingsView};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};

/// The repository base directory setting a project may override (SET-07/08).
pub const BASE_DIRECTORY: &str = "repositories.baseDirectory";
pub const CONTAINER_RUNTIME: &str = "containers.preferredRuntime";
pub(crate) const ARCHIVE: &str = "projects.archive";
pub(crate) const RESTORE: &str = "projects.restore";
pub(crate) const DELETE: &str = "projects.delete";
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

/// The shell's project state: the project and repository lists every screen reads (the
/// navigation's project selector keeps them loaded), the All projects and Repositories screen,
/// and indexing. It reads as the All projects and Repositories screen's state.
#[derive(Default)]
pub struct ProjectsUi {
    /// Active and archived projects (the navigation selector and the first two tabs).
    pub list: ListState<ProjectRecord>,
    /// Every registration (membership choices; Repositories without a selected project).
    pub repositories: ListState<RepositoryRecord>,
    /// The selected project's repositories and which project they were read for.
    pub members: ListState<RepositoryRecord>,
    pub members_of: Option<String>,
    /// Applied project mutations; a list query started before the latest is re-queried.
    pub mutations: u64,
    pub list_at: u64,
    pub screen: ProjectsScreen,
    /// Index status, runs and the section's indexing sources (DW-23).
    pub indexing: IndexingUi,
}
impl Deref for ProjectsUi {
    type Target = ProjectsScreen;
    fn deref(&self) -> &ProjectsScreen {
        &self.screen
    }
}
impl DerefMut for ProjectsUi {
    fn deref_mut(&mut self) -> &mut ProjectsScreen {
        &mut self.screen
    }
}

/// The shell's project and repository lists as one screen call sees them.
#[derive(Clone, Copy)]
pub struct ProjectLists<'a> {
    pub list: &'a ListState<ProjectRecord>,
    pub repositories: &'a ListState<RepositoryRecord>,
    pub members: &'a ListState<RepositoryRecord>,
    pub members_of: Option<&'a str>,
    /// Index status, runs and the section's indexing sources.
    pub indexing: &'a IndexingUi,
}

/// All projects and Repositories.
#[derive(Default)]
pub struct ProjectsScreen {
    /// Deleted projects' tombstones.
    pub deleted: ListState<ProjectRecord>,
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
}

#[derive(Default)]
pub struct GitView {
    pub inspection: Option<devworks_core::protocol::records::GitInspection>,
    pub error: Option<String>,
    pub base: String,
    pub head: String,
    pub patch: Option<String>,
}

/// What the screens ask their controller to do.
#[derive(Clone, Debug)]
pub(crate) enum Action {
    Reload,
    Purge(ProjectRecord),
    Create(CreateForm),
    Edit(EditForm),
    Membership(MembershipForm),
    Lifecycle(&'static str, ProjectRecord),
    Open(String),
    OpenGraph(String, String),
    LoadBase(String),
    ChooseBase(String),
    SaveBase(String),
    InheritBase(String),
    LoadTexts(String),
    InspectGit(String),
    DiffGit(String),
    RemoveIndex(RepositoryRecord),
    Index(IndexAction),
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
pub(crate) fn access_label(a: &AccessMode) -> &'static str {
    match a {
        AccessMode::Full => "Full access",
        AccessMode::Project => "Project scope",
    }
}
pub(crate) fn same_access(a: &AccessMode, b: &AccessMode) -> bool {
    matches!(
        (a, b),
        (AccessMode::Full, AccessMode::Full) | (AccessMode::Project, AccessMode::Project)
    )
}
/// What All projects shows besides the screen's own state.
pub(crate) struct PortfolioView<'a> {
    pub online: &'a Result<(), String>,
    pub lists: ProjectLists<'a>,
    /// A project creation is running or not confirmed yet.
    pub busy_create: bool,
    /// Projects (by ID) whose command is running or not confirmed yet.
    pub waiting: BTreeSet<String>,
    /// Projects (by ID) whose base directory save is running or not confirmed yet.
    pub base_waiting: BTreeSet<String>,
    /// The project the navigation selected.
    pub selected_nav: Option<&'a str>,
    /// The run status line of each project's scheduled jobs.
    pub job_status: &'a BTreeMap<String, String>,
}

/// What Repositories shows besides the screen's own state.
pub(crate) struct RepositoriesView<'a> {
    pub online: &'a Result<(), String>,
    /// The project the navigation selected, if it is still listed.
    pub project: Option<ProjectRecord>,
    /// The repositories shown: the selected project's, or every registration.
    pub source: ListState<RepositoryRecord>,
    /// Repositories (by ID) whose index removal is running or not confirmed yet.
    pub removing: BTreeSet<String>,
    /// Project names by ID.
    pub names: BTreeMap<String, String>,
    pub indexing: &'a IndexingUi,
}

impl ProjectsScreen {
    /// All projects. Returns whether the user asked to reload.
    pub(crate) fn portfolio(
        &mut self,
        ui: &mut Ui,
        v: &PortfolioView,
        mem: &mut RouteMemory,
        scrolls: &mut Scrolls,
        actions: &mut Vec<Action>,
    ) -> bool {
        let screen = Route::Portfolio.screen();
        let online = v.online;
        let busy_create = v.busy_create;
        let tab = mem.tab.min(TABS.len() - 1);
        let mut reload = false;
        let mut new_tab = tab;
        {
            let mut st = (&mut *self, &mut *mem, &mut *actions, &mut reload);
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
                            create_form(
                                ui,
                                projects,
                                &v.lists.repositories.items,
                                busy_create,
                                actions,
                            );
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
                                v.lists.list
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
                                online,
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
                            v.lists
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
                                    &v.lists.repositories.items,
                                    &p,
                                    online.is_ok() && !v.waiting.contains(&id),
                                    v.base_waiting.contains(&id),
                                    v.selected_nav == Some(id.as_str()),
                                    actions,
                                );
                                // The project's scheduled jobs and their run status (JOB-01).
                                widgets::divider(ui);
                                ui.label(RichText::new("Scheduled jobs").strong());
                                match v.job_status.get(&id) {
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
        reload
    }

    /// Repositories. Returns whether the user asked to reload, and to follow the screen's link.
    pub(crate) fn repositories(
        &mut self,
        ui: &mut Ui,
        v: &RepositoriesView,
        mem: &mut RouteMemory,
        scrolls: &mut Scrolls,
        actions: &mut Vec<Action>,
    ) -> (bool, bool) {
        let screen = Route::Repositories.screen();
        let online = v.online;
        let project = &v.project;
        let source = &v.source;
        let tab = mem.tab;
        let mut reload = false;
        let mut link = false;
        let mut new_tab = tab;
        {
            let mut st = (&mut *self, &mut *mem, &mut *actions, &mut reload, &mut link);
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
                                match project {
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
                                online,
                                source,
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
                                            online.is_ok() && !v.removing.contains(r.id.as_str());
                                        repository_detail(
                                            ui, projects, r, &v.names, enabled, actions,
                                        );
                                        let indexing = v.indexing;
                                        let mut index_actions = Vec::new();
                                        if let Some((tone, text)) = &indexing.message {
                                            chip(ui, text.clone(), *tone);
                                        }
                                        index_block(ui, indexing, r, enabled, &mut index_actions);
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
        (reload, link)
    }
}

/// The confirmation of a destructive change: `Some(true)` to go ahead, `Some(false)` to cancel.
/// `archive_work` describes the running work an archive stops.
pub(crate) fn confirm_window(
    ctx: &egui::Context,
    confirm: &Confirm,
    archive_work: &str,
) -> Option<bool> {
    let (title, body, go): (&str, String, &str) = match confirm {
        Confirm::Archive(p) => (
            "Archive project",
            format!(
                "Archive {}? DevWorks stops all of this project's running work immediately, \
                 without waiting for agents to reach a safe step, and admits no new work for \
                 it. Work already accepted by a remote system may still finish there. \
                 Restoring the project later leaves its work stopped until you continue it.{}",
                p.name.0, archive_work
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
    decided
}

impl<T: Clone> ListState<T> {
    pub(crate) fn clone_state(&self) -> Self {
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

fn create_form(
    ui: &mut Ui,
    projects: &mut ProjectsScreen,
    repositories: &[RepositoryRecord],
    busy: bool,
    actions: &mut Vec<Action>,
) {
    let mut cancel = false;
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
            repository_choices(ui, repositories, &mut form.repositories);
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

#[allow(clippy::too_many_arguments)]
fn project_detail(
    ui: &mut Ui,
    projects: &mut ProjectsScreen,
    repositories: &[RepositoryRecord],
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
    let names: BTreeMap<&str, &str> = repositories
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
            if ui
                .add_enabled(enabled, egui::Button::new("Review permanent purge"))
                .clicked()
            {
                actions.push(Action::Purge(p.clone()));
            }
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
            membership_form(ui, projects, repositories, p, enabled, actions);
            ui.add_space(10.0);
            base_directory(ui, projects, &id, base_saving, actions);
        }
    }
}

fn edit_form(
    ui: &mut Ui,
    projects: &mut ProjectsScreen,
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
    projects: &mut ProjectsScreen,
    all: &[RepositoryRecord],
    p: &ProjectRecord,
    enabled: bool,
    actions: &mut Vec<Action>,
) {
    let mut close = false;
    let Some(form) = projects
        .membership
        .as_mut()
        .filter(|f| f.project == p.id.0.as_str())
    else {
        return;
    };
    ui.add_space(6.0);
    repository_choices(ui, all, &mut form.repositories);
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
    projects: &mut ProjectsScreen,
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
    if let Some(runtime) = view.item(CONTAINER_RUNTIME) {
        let effective = runtime
            .effective
            .as_str()
            .unwrap_or("unavailable")
            .to_owned();
        let inherited = !matches!(
            runtime.source,
            devworks_core::settings::view::Source::Project { .. }
        );
        widgets::caption(ui, format!("Container runtime · Effective: {effective}"));
        let mut choice = match &runtime.draft {
            Some(Value::Null) => "inherit".to_owned(),
            Some(value) => value.as_str().unwrap_or("inherit").to_owned(),
            None if inherited => "inherit".to_owned(),
            None => runtime.effective.as_str().unwrap_or("inherit").to_owned(),
        };
        ui.add_enabled_ui(!saving && !b.saving, |ui| {
            egui::ComboBox::from_id_salt(("project-container-runtime", project))
                .selected_text(match choice.as_str() {
                    "inherit" => "Inherit workspace runtime",
                    "docker" => "Docker",
                    "podman" => "Podman",
                    "apple" => "Apple container",
                    _ => "Unavailable",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut choice, "inherit".into(), "Inherit workspace runtime");
                    ui.selectable_value(&mut choice, "docker".into(), "Docker");
                    ui.selectable_value(&mut choice, "podman".into(), "Podman");
                    ui.selectable_value(&mut choice, "apple".into(), "Apple container");
                })
                .response
                .widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::Role::ComboBox,
                        true,
                        "Project container runtime",
                    )
                });
        });
        if choice == "inherit" {
            let _ = view.inherit(CONTAINER_RUNTIME);
        } else {
            let _ = view.edit(CONTAINER_RUNTIME, json!(choice));
        }
        widgets::caption(
            ui,
            "An unavailable runtime stays selected until you choose another.",
        );
    }
    let dirty = !view.dirty_keys().is_empty();
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
                egui::Button::new("Save project settings"),
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
    projects: &mut ProjectsScreen,
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
            if ui
                .add_enabled(enabled, egui::Button::new("Open repository"))
                .clicked()
            {
                actions.push(Action::OpenGraph(
                    r.id.as_str().to_owned(),
                    r.name.0.clone(),
                ));
            }
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
