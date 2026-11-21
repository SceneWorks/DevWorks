//! The shell: navigation, the integrated page layout, the shared conversation and its
//! app-level sidebar, Settings, the completion overlay, status and the close lifecycle. It holds
//! view state only; every record comes through [`Client`] from the runtime's gateway.
use crate::backend::{Backend, Client, Reply, Slot, TaskOutput, Unresolved, is_unknown};
use crate::chat::Chat;
use crate::knowledge_screen::KnowledgeScreen;
use crate::memory::{Memory, RouteMemory};
use crate::notifications::{CardAction, Completions};
use crate::ops::{self, Failure};
use crate::paged::{Applied, Fetch, Page};
use crate::projects_ops::ProjectsSlot;
use crate::projects_screen::ProjectLists;
use crate::route::{NAV_ORDER, NavGroup, Route};
use crate::screen::{Cx, DynScreen, Effect, ScreenId};
use crate::settings_screen::SettingsScreen;
use crate::theme;
use crate::virtual_list::Scrolls;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{LifeState, ProjectRecord};
use eframe::egui::{self, Align, Frame, Layout, Panel, RichText, Ui, ViewportCommand, accesskit};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

mod addons;
pub mod chat;
mod conventions;
mod databases;
pub mod decisions;
pub mod graph;
pub mod help;
pub mod indexing;
mod instructions;
pub mod jobs;
mod knowledge;
mod project_purge;
pub mod projects;
mod settings;
mod settings_secrets;
pub mod usage;
mod watchers;
pub mod work;

/// Native file and folder choosers (rfd); replaced by scripted choices in tests.
pub trait Dialogs {
    fn pick_files(&mut self) -> Vec<PathBuf>;
    fn pick_folder(&mut self, start: Option<&str>) -> Option<PathBuf>;
    fn save_file(&mut self, _name: &str) -> Option<PathBuf> {
        None
    }
    /// One portable convention manifest selected by a human.
    fn pick_convention(&mut self) -> Option<PathBuf> {
        self.pick_files().into_iter().next()
    }
    /// A package bundle to install (the first picked file by default).
    fn pick_package(&mut self) -> Option<PathBuf> {
        self.pick_files().into_iter().next()
    }
}

/// The platform dialogs.
pub struct NativeDialogs;
impl Dialogs for NativeDialogs {
    fn save_file(&mut self, name: &str) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title("Save convention source")
            .set_file_name(name)
            .save_file()
    }
    fn pick_files(&mut self) -> Vec<PathBuf> {
        rfd::FileDialog::new()
            .set_title("Attach images or files")
            .pick_files()
            .unwrap_or_default()
    }
    fn pick_convention(&mut self) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title("Import a convention manifest")
            .add_filter("Convention manifest", &["json"])
            .pick_file()
    }
    fn pick_package(&mut self) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title("Install a package bundle")
            .add_filter("DevWorks package", &["dwpkg"])
            .pick_file()
    }
    fn pick_folder(&mut self, start: Option<&str>) -> Option<PathBuf> {
        let mut d = rfd::FileDialog::new().set_title("Choose a directory");
        if let Some(s) = start.filter(|s| std::path::Path::new(s).is_dir()) {
            d = d.set_directory(s);
        }
        d.pick_folder()
    }
}

/// The engine's bounded shutdown, driven by the close lifecycle.
pub trait HostControl {
    /// The Quiescing seam (AD 20 step 1): asked once when the close starts, while the shell
    /// stops admitting new requests. The engine starts its shutdown here, whose run-control
    /// step holds every active work root for manual Continue before anything is persisted.
    fn begin_quiesce(&mut self);
    /// Whether what `begin_quiesce` started has finished (bounded by [`Self::quiesce_bound`]).
    fn quiesced(&mut self) -> bool;
    /// How long Quiescing waits for [`Self::quiesced`].
    fn quiesce_bound(&self) -> Duration {
        QUIESCE_BOUND
    }
    /// The host's shutdown progress, shown while closing.
    fn progress(&self) -> Option<String> {
        None
    }
    fn begin_shutdown(&mut self);
    /// A short description of the finished shutdown, once it finished.
    fn poll_shutdown(&mut self) -> Option<String>;
}

/// Main-window close: `Open → Quiescing → Persisting → ShuttingDown → Exiting` (AD 20). The
/// window stays responsive and shows progress; nothing keeps running afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Open,
    /// No new requests are admitted; requests in flight finish (bounded).
    Quiescing {
        since: Instant,
    },
    /// View state is saved.
    Persisting,
    /// The runtime stops delivery, collection, housekeeping and owned processes.
    ShuttingDown,
    /// Done; the window closes on the next frame.
    Exiting {
        summary: String,
    },
    Closed,
}

/// Longest wait for in-flight UI requests before shutdown proceeds anyway.
pub const QUIESCE_BOUND: Duration = Duration::from_secs(10);
/// How often Settings → Provider connections reloads while a sign-in is open.
pub const CONNECTION_POLL: Duration = Duration::from_secs(3);
/// Shortest time between two hint-driven re-reads of the visible lists.
pub const HINT_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Height of the global toolbar; the completion overlay anchors just below it.
pub const TOOLBAR: f32 = 46.0;

pub struct ShellInit {
    /// The application, or why it is offline.
    pub backend: Result<Arc<dyn Backend>, String>,
    pub dialogs: Box<dyn Dialogs>,
    /// Where pane state is restored from and saved to (`None`: not persisted).
    pub memory_path: Option<PathBuf>,
    pub control: Option<Box<dyn HostControl>>,
    pub version: String,
}

pub struct Shell {
    client: Option<Client>,
    offline: Option<String>,
    dialogs: Box<dyn Dialogs>,
    memory_path: Option<PathBuf>,
    version: String,
    route: Route,
    intelligence_open: bool,
    routes: BTreeMap<Route, RouteMemory>,
    pub chat: Chat,
    pub settings: SettingsScreen,
    pub help: crate::help_screen::HelpScreen,
    pub knowledge: KnowledgeScreen,
    pub conventions: crate::conventions_screen::ConventionsScreen,
    pub artifacts: crate::artifacts_screen::ArtifactsScreen,
    pub resources: crate::artifacts_screen::ResourcesScreen,
    pub browser: crate::artifacts_screen::BrowserScreen,
    pub addons: crate::addons::AddonsScreen,
    /// The project lists of the navigation's project selector, All projects and Repositories
    /// (DW-11), and Indexing.
    pub projects: crate::projects_screen::ProjectsUi,
    pub purge: project_purge::PurgeUi,
    placeholder: crate::placeholder_screen::PlaceholderScreen,
    pub jobs: jobs::JobsUi,
    /// Features, the board, Issues and Overview of the selected project (DW-12).
    pub work: work::WorkUi,
    pub usage: crate::usage_screen::UsageScreen,
    pub graph: crate::graph_screen::GraphScreen,
    pub databases: crate::databases_screen::DatabasesScreen,
    /// Watchers: the global list, the selected conversation's and the selected feature's.
    pub watchers: crate::watchers_screen::WatchersScreen,
    /// The project the navigation selected; its screens follow it.
    selected_project: Option<String>,
    pub completions: Completions,
    pub decisions: decisions::DecisionsUi,
    scrolls: Scrolls,
    lifecycle: Lifecycle,
    status: Option<String>,
    hints: Option<watch::Receiver<u64>>,
    /// A hint arrived within [`HINT_REFRESH_INTERVAL`] of the last refresh: one refresh is due
    /// when that interval ends, so the last hint of a burst is still shown.
    hint_refresh_due: bool,
    /// Input or a reply may have changed an editor since the purge snapshot was published.
    editors_changed: bool,
    started: bool,
    saved: Option<Memory>,
    last_save: Instant,
    last_poll: Instant,
    /// The window's context, from the first frame on (screens make textures through it).
    egui: Option<egui::Context>,
    updates_open: bool,
    shutdown_summary: Option<String>,
    /// The text scale the native minimum window size was last set for.
    applied_scale: f32,
    // Dropped last: the engine's shutdown and thread join run after the request workers stop.
    control: Option<Box<dyn HostControl>>,
}

/// The run status a context lends while Jobs, which holds it, is the screen called.
static NO_JOB_STATUS: BTreeMap<String, String> = BTreeMap::new();

/// The migrated screens: `screens!(Id => field, ...; Id => field, ...)` binds each [`ScreenId`]
/// to the `Shell` field holding that screen; the ones after the `;` live inside another screen
/// and are reached by ID only. Settings is bound in the macro too: its context lends the Indexing
/// screen's state to draw the sources section. Indexing (`projects.indexing`) is bound in the macro itself: every
/// other screen's context reads it. So is Chat: every other screen's context reads its router
/// catalog, and its own context lends it the watcher lists its sidebar shows. And so is Jobs:
/// every other screen's context reads its projects' run status.
macro_rules! screens {
    ($($id:ident => $($field:ident).+),* $(,)? ; $($nid:ident => $($nfield:ident).+),* $(,)?) => {
        impl Shell {
            fn screens(&self) -> Vec<&dyn DynScreen> {
                vec![
                    $(&self.$($field).+,)*
                    &self.projects.indexing,
                    &self.chat,
                    &self.jobs,
                    &self.decisions,
                    &self.work,
                    &self.settings,
                ]
            }
            fn screens_mut(&mut self) -> Vec<&mut dyn DynScreen> {
                vec![
                    $(&mut self.$($field).+,)*
                    &mut self.projects.indexing,
                    &mut self.chat,
                    &mut self.jobs,
                    &mut self.decisions,
                    &mut self.work,
                    &mut self.settings,
                ]
            }
            fn screen(&self, id: ScreenId) -> Option<&dyn DynScreen> {
                match id {
                    $(ScreenId::$id => Some(&self.$($field).+),)*
                    $(ScreenId::$nid => Some(&self.$($nfield).+),)*
                    ScreenId::Indexing => Some(&self.projects.indexing),
                    ScreenId::Chat => Some(&self.chat),
                    ScreenId::Jobs => Some(&self.jobs),
                    ScreenId::Decisions => Some(&self.decisions),
                    ScreenId::Work => Some(&self.work),
                    ScreenId::Settings => Some(&self.settings),
                }
            }
            /// The screen `id` with a context over the rest of the shell.
            fn screen_mut(&mut self, id: ScreenId) -> Option<(&mut dyn DynScreen, Cx<'_>)> {
                let online = self.online();
                // Work holds the archive counts every other context lends, so its own context
                // lends none (it reads its own).
                if id == ScreenId::Work {
                    let cx = Cx {
                        client: self.client.as_mut(),
                        online,
                        status: &mut self.status,
                        route: self.route,
                        routes: &mut self.routes,
                        scrolls: &mut self.scrolls,
                        dialogs: &mut *self.dialogs,
                        selected_project: self.selected_project.clone(),
                        projects: ProjectLists {
                            list: &self.projects.list,
                            repositories: &self.projects.repositories,
                            members: &self.projects.members,
                            members_of: self.projects.members_of.as_deref(),
                            indexing: Some(&self.projects.indexing),
                        },
                        catalog: &self.chat.routing.catalog.items,
                        job_status: &self.jobs.project_status,
                        archive_counts: &NO_ARCHIVE_COUNTS,
                        ctx: self.egui.clone(),
                        watchers: Some(&self.watchers),
                        completions: None,
                        indexing: None,
                        effects: Vec::new(),
                    };
                    return Some((&mut self.work, cx));
                }
                let mut cx = Cx {
                    client: self.client.as_mut(),
                    online,
                    status: &mut self.status,
                    route: self.route,
                    routes: &mut self.routes,
                    scrolls: &mut self.scrolls,
                    dialogs: &mut *self.dialogs,
                    selected_project: self.selected_project.clone(),
                    projects: ProjectLists {
                        list: &self.projects.list,
                        repositories: &self.projects.repositories,
                        members: &self.projects.members,
                        members_of: self.projects.members_of.as_deref(),
                        indexing: None,
                    },
                    catalog: &[],
                    job_status: &NO_JOB_STATUS,
                    archive_counts: &self.work.archive_counts,
                    ctx: self.egui.clone(),
                    watchers: None,
                    completions: None,
                    indexing: None,
                    effects: Vec::new(),
                };
                match id {
                    $(ScreenId::$id => {
                        cx.projects.indexing = Some(&self.projects.indexing);
                        cx.catalog = &self.chat.routing.catalog.items;
                        cx.job_status = &self.jobs.project_status;
                        Some((&mut self.$($field).+, cx))
                    })*
                    $(ScreenId::$nid => {
                        cx.projects.indexing = Some(&self.projects.indexing);
                        cx.catalog = &self.chat.routing.catalog.items;
                        cx.job_status = &self.jobs.project_status;
                        Some((&mut self.$($nfield).+, cx))
                    })*
                    // The Indexing screen's own context does not hold its state.
                    ScreenId::Indexing => {
                        cx.catalog = &self.chat.routing.catalog.items;
                        cx.job_status = &self.jobs.project_status;
                        Some((&mut self.projects.indexing, cx))
                    }
                    // Nor does Chat's: it reads its own catalog.
                    ScreenId::Chat => {
                        cx.projects.indexing = Some(&self.projects.indexing);
                        cx.job_status = &self.jobs.project_status;
                        cx.watchers = Some(&self.watchers);
                        Some((&mut self.chat, cx))
                    }
                    // Decisions keeps the shell's completion cards current.
                    ScreenId::Decisions => {
                        cx.projects.indexing = Some(&self.projects.indexing);
                        cx.catalog = &self.chat.routing.catalog.items;
                        cx.job_status = &self.jobs.project_status;
                        cx.completions = Some(&mut self.completions);
                        Some((&mut self.decisions, cx))
                    }
                    // Nor does Jobs': it reads its own run status.
                    ScreenId::Jobs => {
                        cx.projects.indexing = Some(&self.projects.indexing);
                        cx.catalog = &self.chat.routing.catalog.items;
                        Some((&mut self.jobs, cx))
                    }
                    // Settings draws the indexing sources: its context holds that state.
                    ScreenId::Settings => {
                        cx.projects.indexing = None;
                        cx.catalog = &self.chat.routing.catalog.items;
                        cx.job_status = &self.jobs.project_status;
                        cx.indexing = Some(&mut self.projects.indexing);
                        Some((&mut self.settings, cx))
                    }
                    ScreenId::Work => None,
                }
            }
        }
    };
}
/// The archive counts the Work screen's own context lends (it reads its own).
static NO_ARCHIVE_COUNTS: BTreeMap<String, (usize, usize)> = BTreeMap::new();
screens!(
    Help => help,
    Usage => usage,
    Graph => graph,
    Databases => databases,
    Conventions => conventions,
    Artifacts => artifacts,
    Resources => resources,
    Browser => browser,
    Projects => projects.screen,
    Watchers => watchers,
    AddOns => addons,
    Knowledge => knowledge,
    Purge => purge,
    Placeholder => placeholder,
    ;
    // Inside another registered screen: reached by its ID, not iterated (Roles and skills keep
    // no pending, purge or close state, and Settings holds and draws them).
    Instructions => settings.instructions,
);

impl Shell {
    pub fn new(init: ShellInit) -> Self {
        let memory = init
            .memory_path
            .as_deref()
            .map(Memory::load)
            .unwrap_or_default();
        let (client, offline, hints) = match init.backend {
            Ok(b) => {
                let hints = b.hints();
                (Some(Client::new(b)), None, hints)
            }
            Err(reason) => (None, Some(reason), None),
        };
        let chat = Chat::from_memory(&memory);
        let mut settings = SettingsScreen::default();
        settings.restore(&memory.settings);
        settings.scrolls = Scrolls::restoring(&memory.scroll);
        let mut shell = Self {
            client,
            offline,
            dialogs: init.dialogs,
            memory_path: init.memory_path,
            version: init.version,
            route: memory.route,
            intelligence_open: memory.intelligence_open,
            routes: memory.routes.clone(),
            chat,
            settings,
            help: Default::default(),
            knowledge: KnowledgeScreen::new(),
            conventions: crate::conventions_screen::ConventionsScreen::default(),
            artifacts: Default::default(),
            resources: Default::default(),
            browser: Default::default(),
            addons: Default::default(),
            projects: Default::default(),
            purge: project_purge::PurgeUi {
                reviews: memory.purge_reviews.clone(),
                ..Default::default()
            },
            placeholder: Default::default(),
            jobs: jobs::JobsUi::default(),
            work: work::WorkUi::default(),
            usage: crate::usage_screen::UsageScreen::default(),
            graph: crate::graph_screen::GraphScreen::default(),
            databases: Default::default(),
            watchers: Default::default(),
            selected_project: memory.selected_project.clone(),
            completions: Completions::default(),
            decisions: decisions::DecisionsUi::default(),
            scrolls: Scrolls::restoring(&memory.scroll),
            lifecycle: Lifecycle::Open,
            status: None,
            hints,
            hint_refresh_due: false,
            editors_changed: true,
            started: false,
            saved: Some(memory.clone()),
            last_save: Instant::now(),
            last_poll: Instant::now(),
            egui: None,
            updates_open: false,
            shutdown_summary: None,
            applied_scale: 0.0,
            control: init.control,
        };
        shell.restore_pending(memory.pending);
        shell
    }

    // -----------------------------------------------------------------------------------------
    // Accessors (tests and the binary)

    pub fn route(&self) -> Route {
        self.route
    }
    pub fn coordinator_open(&self) -> bool {
        self.chat.coordinator_open
    }
    pub fn lifecycle(&self) -> &Lifecycle {
        &self.lifecycle
    }
    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }
    pub fn route_memory(&self, route: Route) -> RouteMemory {
        self.routes.get(&route).cloned().unwrap_or_default()
    }
    /// How the runtime shutdown ended, once the close lifecycle finished it.
    pub fn shutdown_summary(&self) -> Option<&str> {
        self.shutdown_summary.as_deref()
    }
    /// The recorded vertical offset of the list `salt`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn scroll_offset(&self, salt: &str) -> Option<f32> {
        [&self.scrolls, &self.chat.scrolls, &self.settings.scrolls]
            .into_iter()
            .find_map(|s| s.offset(salt))
    }
    pub fn intelligence_open(&self) -> bool {
        self.intelligence_open
    }
    /// Blocks until every request has its reply queued (tests; never called by the frame loop).
    pub fn wait_idle(&self) -> bool {
        self.client
            .as_ref()
            .is_none_or(|c| c.wait_idle(Duration::from_secs(30)))
    }

    fn online(&self) -> Result<(), String> {
        match (&self.offline, &self.lifecycle) {
            (Some(r), _) => Err(r.clone()),
            (None, Lifecycle::Open) => Ok(()),
            (None, _) => Err("DevWorks is closing.".into()),
        }
    }

    fn backend(&self) -> Option<Arc<dyn Backend>> {
        self.client.as_ref().map(|c| c.backend().clone())
    }

    fn announce(&mut self, text: impl Into<String>) {
        self.status = Some(text.into());
    }

    /// Runs `f` on screen `id`, then the effects it asked for; `false` when no screen is registered under `id`.
    fn with_screen(&mut self, id: ScreenId, f: impl FnOnce(&mut dyn DynScreen, &mut Cx)) -> bool {
        let Some((screen, mut cx)) = self.screen_mut(id) else {
            return false;
        };
        f(screen, &mut cx);
        let effects = cx.into_effects();
        self.run_effects(effects);
        true
    }
    /// Runs `f` on the registered screen of type `S`, then the effects it asked for.
    fn with_typed<S: crate::screen::Screen>(&mut self, f: impl FnOnce(&mut S, &mut Cx)) {
        self.with_screen(S::ID, |s, cx| {
            f(
                s.as_any_mut()
                    .downcast_mut()
                    .expect("the registry holds S under S::ID"),
                cx,
            )
        });
    }
    /// Opens Settings on the embedding setup of Provider connections.
    pub(crate) fn open_embedding_setup(&mut self) {
        self.navigate(Route::Settings);
        self.with_typed(|s: &mut SettingsScreen, cx| s.show_embedding_setup(cx));
    }
    fn run_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Navigate(route) => self.navigate(route),
                Effect::OpenWork(id) => self.open_work(&id),
                Effect::OpenSession(id) => self.open_session(id),
                Effect::LoadProjects => self.load_projects(),
                Effect::LoadRepositories => self.load_repositories(),
                Effect::LoadMoreProjects => self.load_more_projects(),
                Effect::LoadMoreRepositories => self.load_more_repositories(),
                Effect::LoadMoreMembers => self.load_more_members(),
                Effect::ProjectChanged(p) => {
                    self.projects.mutations += 1;
                    let id = p.id.0.as_str();
                    if matches!(p.lifecycle, LifeState::Deleted)
                        && self.selected_project.as_deref() == Some(id)
                    {
                        self.select_project(None);
                    }
                    // Its lifecycle changed: the work screens re-read it (archived work is
                    // closed to change), and an archive count is taken afresh next time.
                    self.work.archive_counts.remove(id);
                    if self.work.of.as_deref() == Some(id) {
                        self.load_work();
                    }
                }
                Effect::SelectProject(p) => self.select_project(p),
                Effect::OpenJobs(p) => {
                    self.jobs.set_scope(Some(p));
                    self.navigate(Route::Jobs);
                }
                Effect::LoadProjectJobs(p) => {
                    self.with_typed(|j: &mut jobs::JobsUi, cx| j.load_project_jobs(cx, &p))
                }
                Effect::CountArchiveWork(p) => {
                    self.with_typed(|w: &mut work::WorkUi, cx| w.count_archive_work(cx, &p))
                }
                Effect::ForgetArchiveCount(p) => self.work.forget_archive_count(&p),
                Effect::ReviewPurge(p) => self.start_purge_review(p),
                Effect::OpenGraph(id, name) => self.open_repository_graph(&id, &name),
                Effect::OpenGraphNode { repository, node } => {
                    self.open_graph_node(&repository, &node)
                }
                Effect::FindDatabaseTable(table) => {
                    self.with_typed(|d: &mut crate::databases_screen::DatabasesScreen, _| {
                        d.tab = 0;
                        d.filter = table;
                        d.kind = "Table".into();
                        d.object_id = None;
                        d.parent = None;
                    });
                    self.navigate(Route::Databases);
                }
                Effect::LoadRouterCatalog => self.load_router_catalog(),
                Effect::Index(action) => {
                    self.with_typed(|s: &mut indexing::IndexingScreen, cx| s.act(cx, vec![action]))
                }
                Effect::ConfirmOutcomes => self.confirm_outcomes(),
                Effect::OpenEmbeddingSetup => self.open_embedding_setup(),
                Effect::OpenInstructions => {
                    self.with_typed(|s: &mut SettingsScreen, _| s.show_instructions());
                    self.navigate(Route::Settings);
                }
                Effect::ArtifactsChanged => self
                    .with_typed(|a: &mut crate::artifacts_screen::ArtifactsScreen, _| a.changed()),
                Effect::OpenConnections => {
                    self.with_typed(|s: &mut SettingsScreen, cx| s.show_connections_topic(cx))
                }
                Effect::OpenHelp(screen) => self.open_help_context(&screen),
                Effect::InspectApproval(id) => {
                    self.with_typed(|d: &mut decisions::DecisionsUi, cx| d.inspect_approval(cx, id))
                }
                Effect::SelectKnowledge(id) => {
                    self.with_typed(|k: &mut KnowledgeScreen, cx| k.select(cx, id));
                }
                Effect::Watch(actions) => self.watch_act(actions),
                Effect::OpenGitSession(id) => {
                    self.with_typed(|c: &mut Chat, cx| {
                        c.notice = None;
                        c.open_session(cx, id);
                    });
                    self.navigate(Route::Chat);
                }
                Effect::SetRoute(route) => self.route = route,
                Effect::OpenPrices(models) => {
                    self.with_typed(|u: &mut crate::usage_screen::UsageScreen, cx| {
                        u.show_prices(models);
                        cx.routes.entry(Route::Usage).or_default().tab = 2;
                        cx.navigate(Route::Usage);
                    });
                }
                Effect::OpenNotifications => self.decisions.notifications_open = true,
                Effect::InvalidateRouting => self.invalidate_routing(),
                Effect::ProjectList(result) => self.projects_listed(result),
                Effect::RepositoryList(result) => self.repositories_listed(result),
                Effect::ProjectMembers(project, result) => self.members_listed(project, result),
                Effect::ConfirmPurge => self.confirm_purge(),
            }
        }
    }
    /// The screen drawn on the current route: one hosted there, or the route's owner.
    fn shown_screen(&self) -> ScreenId {
        let default_memory = RouteMemory::default();
        let mem = self.routes.get(&self.route).unwrap_or(&default_memory);
        self.screens()
            .into_iter()
            .find(|s| s.hosted(self.route, mem))
            .map_or(self.route.screen().owner, |s| s.id())
    }

    // -----------------------------------------------------------------------------------------
    // Navigation

    /// Navigates to `route`. Returning Home closes the coordinator sidebar and shows the same
    /// conversation in the centre pane; navigating elsewhere never opens it (UI-13).
    pub fn navigate(&mut self, route: Route) {
        if route != Route::Databases {
            self.with_typed(|d: &mut crate::databases_screen::DatabasesScreen, _| d.left());
        }
        if route == Route::Chat {
            self.chat.coordinator_open = false;
        }
        self.route = route;
        self.load_route();
    }

    /// Shows conversation `id` on Home.
    fn open_session(&mut self, id: String) {
        self.with_typed(|c: &mut Chat, cx| c.open_session(cx, id));
        self.navigate(Route::Chat);
    }
    pub fn load_jobs(&mut self) {
        self.with_typed(|j: &mut jobs::JobsUi, cx| j.load_jobs(cx));
    }
    pub fn load_sessions(&mut self) {
        self.with_typed(|c: &mut Chat, cx| c.load_sessions(cx));
    }
    fn load_router_catalog(&mut self) {
        self.with_typed(|c: &mut Chat, cx| c.load_router_catalog(cx));
    }
    /// Reads the selected project's work lists again (the work screens are open).
    fn load_work(&mut self) {
        self.with_typed(|w: &mut work::WorkUi, cx| w.load_work(cx));
    }
    /// Opens a work item by ID: its project is selected and its screen shown with it selected.
    fn open_work(&mut self, id: &str) {
        self.with_typed(|w: &mut work::WorkUi, cx| w.open_work(cx, id));
    }
    fn invalidate_routing(&mut self) {
        self.with_typed(|c: &mut Chat, cx| c.invalidate_routing(cx));
    }
    /// Attaches the user's files to the active conversation's draft (dialog, drop and pasted
    /// paths). The shell never removes these files.
    pub fn attach(&mut self, paths: Vec<PathBuf>) {
        self.with_typed(|c: &mut Chat, cx| c.attach(cx, paths));
    }
    /// Attaches a pasted image: it is written as a PNG beside the store, attached, and the
    /// temporary file removed once the artifact service holds its copy.
    pub fn attach_pasted_image(&mut self, image: &egui::ColorImage) {
        self.with_typed(|c: &mut Chat, cx| c.attach_pasted_image(cx, image));
    }

    /// The toolbar chat icon: away from Home it toggles the coordinator sidebar; on Home the
    /// conversation is already in the centre, so it focuses the composer.
    pub fn toggle_coordinator(&mut self, ctx: &egui::Context) {
        self.with_typed(|c: &mut Chat, cx| c.toggle_coordinator(cx, ctx));
    }

    /// Tells the coordinator beside the screen which screen, project, item and selection the
    /// user is looking at (the conversation sends it once per change).
    fn append_coordinator_context_if_changed(&mut self) {
        use devworks_core::protocol::records::ScreenContext;
        use devworks_core::protocol::support::BoundedList;
        use devworks_core::protocol::{Id, ProjectId, Revision, RootId};
        let Some(backend) = self.backend() else {
            return;
        };
        let session = self
            .chat
            .selected
            .as_deref()
            .and_then(|value| Id::new(value).ok());
        let Some(session) = session.filter(|_| {
            (self.chat.coordinator_open || self.route == Route::Chat)
                && self.online().is_ok()
                && !self.purge.frozen
        }) else {
            backend.publish_current_screen(None, "");
            return;
        };
        let screen = self.route.screen();
        let (board, card) = if matches!(
            self.route,
            Route::Features | Route::Issues | Route::Overview
        ) {
            (self.work.board.as_deref(), self.work.card.as_deref())
        } else {
            (None, None)
        };
        let default_memory = RouteMemory::default();
        let screen_context = self.screen(self.shown_screen()).map(|s| {
            s.context(
                self.route,
                self.routes.get(&self.route).unwrap_or(&default_memory),
            )
        });
        let tab = screen_context.as_ref().map_or(0, |c| c.tab);
        let selection = screen_context
            .as_ref()
            .and_then(|c| c.selection.as_deref())
            .unwrap_or("");
        let knowledge_project = if self.route == Route::Knowledge {
            self.screen(ScreenId::Knowledge)
                .and_then(|s| s.scope())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let key = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            screen.id,
            self.selected_project.as_deref().unwrap_or(""),
            board.unwrap_or(""),
            card.unwrap_or(""),
            tab,
            selection,
            knowledge_project
        );
        let project = match self.route {
            Route::Knowledge => Some(knowledge_project.as_str()),
            _ if screen.group == NavGroup::Project => self.selected_project.as_deref(),
            _ => None,
        }
        .and_then(|value| Id::new(value).ok())
        .map(ProjectId);
        let mut selected_ids = Vec::new();
        for value in board.into_iter().chain(card).chain(Some(selection)) {
            if let Ok(id) = Id::new(value)
                && !selected_ids.contains(&id)
            {
                selected_ids.push(id);
            }
        }
        let root_id = board
            .or(card)
            .and_then(|value| Id::new(value).ok())
            .map(RootId);
        backend.publish_current_screen(
            Some((
                session,
                ScreenContext {
                    screen_id: Id::new(screen.id).expect("registered screen ID"),
                    project_id: project,
                    root_id,
                    selected_ids: BoundedList(selected_ids),
                    help_topic_id: Id::new(screen.help_topic).expect("registered Help topic"),
                    revision: Revision::new(1).expect("positive revision"),
                },
            )),
            &key,
        );
        if self.route == Route::Chat {
            return;
        }
        let context = format!(
            "Current screen: {}. {}\nProject: {}\nFeature: {}\nItem: {}\nSubview: {}\nSelection: {}\nKnowledge scope: {}\nLocal Help topic: {}",
            screen.title,
            screen.summary,
            self.selected_project.as_deref().unwrap_or("workspace"),
            board.unwrap_or("none"),
            card.unwrap_or("none"),
            screen.subviews.get(tab).copied().unwrap_or("default"),
            if selection.is_empty() {
                "none"
            } else {
                selection
            },
            if knowledge_project.is_empty() {
                "workspace"
            } else {
                &knowledge_project
            },
            screen.help_topic
        );
        self.with_typed(|c: &mut Chat, cx| c.notify_coordinator(cx, key, context));
    }

    fn load_route(&mut self) {
        let route = self.route;
        if route == Route::Jobs {
            // The job form's route selectors.
            self.load_router_catalog();
        }
        self.with_screen(route.screen().owner, |s, cx| s.load(cx, route));
    }

    // -----------------------------------------------------------------------------------------
    // Projects: the lists every screen reads and the navigation's project selection

    pub fn load_projects(&mut self) {
        self.projects.list_at = self.projects.mutations;
        // Once listed, a re-read covers every page shown (and a Load more in flight), so a
        // periodic or mutation re-read never collapses a list the user expanded.
        let fetch = if self.projects.list.loaded {
            self.projects.list.refresh()
        } else {
            self.projects.list.reload()
        };
        self.send_projects(fetch, "Loading projects");
    }
    pub fn load_more_projects(&mut self) {
        if let Some(fetch) = self.projects.list.more_page() {
            self.send_projects(fetch, "Loading more projects");
        }
    }
    fn send_projects(&mut self, fetch: Fetch, label: &str) {
        let cursor = fetch.cursor.clone();
        let sent = self.query(
            ProjectsSlot::ProjectsList.into(),
            label,
            "projects.list",
            move |b| ops::projects_list(b, None, cursor.as_deref()),
        );
        if !sent {
            self.projects.list.abandon(fetch.generation);
        }
    }
    pub fn load_repositories(&mut self) {
        // A re-read covers every page shown; the project's list was reset when the selection
        // changed, so it starts over then.
        let fetch = self.projects.repositories.reread();
        self.send_repositories(fetch, "Loading repositories");
        if let Some(p) = self.selected_project.clone() {
            let fetch = self.projects.members.reread();
            self.send_members(p, fetch, "Loading the project's repositories");
        }
    }
    pub fn load_more_repositories(&mut self) {
        if let Some(fetch) = self.projects.repositories.more_page() {
            self.send_repositories(fetch, "Loading more repositories");
        }
    }
    pub fn load_more_members(&mut self) {
        if let Some(p) = self.selected_project.clone()
            && let Some(fetch) = self.projects.members.more_page()
        {
            self.send_members(p, fetch, "Loading more of the project's repositories");
        }
    }
    fn send_repositories(&mut self, fetch: Fetch, label: &str) {
        let cursor = fetch.cursor.clone();
        let sent = self.query(
            ProjectsSlot::RepositoriesList.into(),
            label,
            "repositories.list",
            move |b| ops::repositories_list(b, None, cursor.as_deref()),
        );
        if !sent {
            self.projects.repositories.abandon(fetch.generation);
        }
    }
    fn send_members(&mut self, project: String, fetch: Fetch, label: &str) {
        let cursor = fetch.cursor.clone();
        let p = project.clone();
        let sent = self.query(
            ProjectsSlot::ProjectRepositories(project).into(),
            label,
            "repositories.list",
            move |b| ops::repositories_list(b, Some(&p), cursor.as_deref()),
        );
        if !sent {
            self.projects.members.abandon(fetch.generation);
        }
    }
    /// A project list reply, read: a list requested before a project mutation is requested
    /// again.
    fn projects_listed(&mut self, result: Result<Page<ProjectRecord>, String>) {
        if self.projects.list_at != self.projects.mutations {
            self.load_projects();
            return;
        }
        let applied = self.projects.list.apply_current(result);
        if let Applied::Continue(fetch) = applied {
            self.send_projects(fetch, "Loading projects");
            return;
        }
        if matches!(applied, Applied::Replaced | Applied::Appended) {
            // A selection that no longer exists (deleted elsewhere) is dropped.
            if let Some(s) = self.selected_project.clone()
                && !self.projects.list.more()
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
                self.with_screen(ScreenId::Browser, |s, cx| s.load(cx, Route::Browser));
            }
        }
    }
    fn repositories_listed(
        &mut self,
        result: Result<Page<devworks_core::protocol::records::RepositoryRecord>, String>,
    ) {
        if let Applied::Continue(fetch) = self.projects.repositories.apply_current(result) {
            self.send_repositories(fetch, "Loading repositories");
        }
    }
    fn members_listed(
        &mut self,
        project: String,
        result: Result<Page<devworks_core::protocol::records::RepositoryRecord>, String>,
    ) {
        // A reply for a project no longer selected is stale.
        if self.selected_project.as_deref() != Some(project.as_str()) {
            return;
        }
        match self.projects.members.apply_current(result) {
            Applied::Replaced | Applied::Appended => self.projects.members_of = Some(project),
            Applied::Continue(fetch) => {
                self.send_members(project, fetch, "Loading the project's repositories");
            }
            _ => {}
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
        self.with_typed(|g: &mut crate::graph_screen::GraphScreen, _| g.project_changed());
        self.projects.members.reset();
        self.projects.members_of = None;
        if self.route == Route::Repositories {
            self.load_repositories();
        }
        if self.route == Route::Browser {
            self.with_screen(ScreenId::Browser, |s, cx| s.load(cx, Route::Browser));
        }
        if matches!(
            self.route,
            Route::Overview | Route::Features | Route::Issues
        ) {
            self.load_work();
        }
    }

    /// The project selector of the left navigation.
    fn project_selector(&mut self, ui: &mut Ui) {
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
    // Requests

    /// Submits a query; `false` when nothing was submitted (offline, or closing: the shell
    /// admits nothing new once the window is closing).
    fn query(
        &mut self,
        slot: Slot,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) -> bool {
        let online = self.online().is_ok();
        crate::screen::submit_query(self.client.as_mut(), online, slot, label, op, frame)
    }
    pub fn load_settings(&mut self) {
        self.with_typed(|s: &mut SettingsScreen, cx| s.reload(cx));
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    fn drain(&mut self) {
        let Some(client) = &mut self.client else {
            return;
        };
        let replies = client.poll();
        if !replies.is_empty() {
            self.editors_changed = true;
        }
        for (slot, ticket, reply) in replies {
            // An earlier reply of this batch may have re-requested this slot (a message
            // acknowledgement re-reads the inbox): this reply is then stale.
            if self
                .client
                .as_ref()
                .is_some_and(|c| c.is_superseded(&slot, ticket))
            {
                continue;
            }
            self.apply(slot, reply);
        }
    }

    fn apply(&mut self, slot: Slot, reply: Reply) {
        let owner = slot.screen();
        self.with_screen(owner, |s, cx| s.apply(cx, slot, reply));
    }

    fn poll_hints(&mut self, ctx: &egui::Context) {
        let changed = self
            .hints
            .as_mut()
            .is_some_and(|rx| rx.has_changed().unwrap_or(false));
        if changed {
            if let Some(rx) = &mut self.hints {
                rx.borrow_and_update();
            }
            self.hint_refresh_due = true;
        }
        if self.hint_refresh_due {
            // A burst of hints re-reads the visible lists at most once per interval; the
            // refresh after the burst reads the state its last hint announced.
            let since = self.last_poll.elapsed();
            if since >= HINT_REFRESH_INTERVAL {
                self.hint_refresh_due = false;
                self.refresh_visible();
            } else {
                ctx.request_repaint_after(HINT_REFRESH_INTERVAL - since);
            }
        } else if self.last_poll.elapsed() > Duration::from_secs(10) {
            // A missed hint is repaired by a bounded periodic re-query of what is visible.
            self.refresh_visible();
        }
    }

    /// When a hint-driven refresh is waiting for its interval to end (tests step frames until
    /// then).
    #[cfg(any(test, feature = "test-support"))]
    pub fn hint_refresh_due_at(&self) -> Option<Instant> {
        self.hint_refresh_due
            .then(|| self.last_poll + HINT_REFRESH_INTERVAL)
    }

    /// Re-reads the visible screen's data: the periodic (and hint driven) refresh, also called
    /// directly by tests.
    pub fn refresh_visible(&mut self) {
        self.last_poll = Instant::now();
        if self.online().is_err() {
            return;
        }
        self.with_typed(|c: &mut Chat, cx| c.refresh_conversation(cx));
        self.with_typed(|d: &mut decisions::DecisionsUi, cx| d.refresh_decisions(cx));
        let route = self.route;
        self.with_screen(route.screen().owner, |s, cx| s.refresh(cx, route));
    }

    // -----------------------------------------------------------------------------------------
    // Lifecycle

    /// Begins the close lifecycle (the window's close button lands here).
    pub fn request_close(&mut self) {
        if self.lifecycle == Lifecycle::Open {
            for screen in self.screens_mut() {
                screen.closing();
            }
            if let Some(c) = &mut self.client {
                c.close();
            }
            self.lifecycle = Lifecycle::Quiescing {
                since: Instant::now(),
            };
            if let Some(c) = &mut self.control {
                c.begin_quiesce();
            }
            self.announce("Closing DevWorks: finishing requests in flight.");
        }
    }

    fn advance_lifecycle(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && self.lifecycle != Lifecycle::Closed {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.request_close();
        }
        match &self.lifecycle {
            Lifecycle::Open | Lifecycle::Closed => return,
            Lifecycle::Quiescing { since } => {
                let since = *since;
                let idle = self
                    .client
                    .as_ref()
                    .is_none_or(|c| c.activities().is_empty());
                let quiesced = self.control.as_mut().is_none_or(|c| c.quiesced());
                let bound = self
                    .control
                    .as_ref()
                    .map_or(QUIESCE_BOUND, |c| c.quiesce_bound());
                if (idle && quiesced) || since.elapsed() > bound {
                    self.lifecycle = Lifecycle::Persisting;
                }
            }
            Lifecycle::Persisting => {
                self.persist(true);
                match &mut self.control {
                    Some(c) => {
                        c.begin_shutdown();
                        self.lifecycle = Lifecycle::ShuttingDown;
                        self.announce("Closing DevWorks: stopping delivery and owned processes.");
                    }
                    None => {
                        self.lifecycle = Lifecycle::Exiting {
                            summary: "No runtime was running.".into(),
                        }
                    }
                }
            }
            Lifecycle::ShuttingDown => {
                if let Some(summary) = self.control.as_mut().and_then(|c| c.poll_shutdown()) {
                    self.shutdown_summary = Some(summary.clone());
                    self.lifecycle = Lifecycle::Exiting { summary };
                }
            }
            Lifecycle::Exiting { .. } => {
                self.lifecycle = Lifecycle::Closed;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    /// Called when the window is gone without the lifecycle (process exit paths): saves view
    /// state; the engine's own drop still performs its bounded shutdown.
    pub fn on_exit(&mut self) {
        for screen in self.screens_mut() {
            screen.closing();
        }
        self.persist(true);
        if let Some(c) = &mut self.client {
            c.close();
        }
        if !matches!(
            self.lifecycle,
            Lifecycle::Closed | Lifecycle::Exiting { .. }
        ) && let Some(c) = &mut self.control
        {
            c.begin_shutdown();
        }
    }

    pub fn memory(&self) -> Memory {
        let mut memory = Memory {
            version: crate::memory::VERSION,
            purge_live: None,
            purge_reviews: self.purge.reviews.clone(),
            purge_fences: self
                .saved
                .as_ref()
                .map(|m| m.purge_fences.clone())
                .unwrap_or_default(),
            route: self.route,
            intelligence_open: self.intelligence_open,
            selected_project: self.selected_project.clone(),
            routes: self.routes.clone(),
            settings: self.settings.memory(),
            scroll: [&self.scrolls, &self.chat.scrolls, &self.settings.scrolls]
                .into_iter()
                .flat_map(|s| s.offsets().iter().map(|(k, v)| (k.clone(), *v)))
                .collect(),
            pending: self.pending_memory(),
            ..Memory::default()
        };
        self.chat.save_memory(&mut memory);
        memory
    }

    /// Requests whose outcome is not known yet (including commands still running when the
    /// bounded quiesce gave up), with what their replies settle, so the next launch confirms
    /// them with their exact frames.
    fn pending_memory(&self) -> crate::memory::PendingMemory {
        use crate::memory::{ChatPendingMemory, PendingCommand, PendingMemory};
        let unconfirmed = self
            .client
            .as_ref()
            .map(Client::unconfirmed)
            .unwrap_or_default();
        let commands: Vec<PendingCommand> = unconfirmed
            .iter()
            .map(|u| PendingCommand {
                slot: u.slot.clone(),
                label: u.label.clone(),
                operation: u.operation().to_owned(),
                frame: u.frame().to_vec(),
                dispatch: u.dispatch,
            })
            .collect();
        let mut screens: BTreeMap<String, serde_json::Value> = self
            .screens()
            .into_iter()
            .filter_map(|screen| {
                let own: Vec<&Unresolved> = unconfirmed
                    .iter()
                    .filter(|u| u.slot.screen() == screen.id())
                    .collect();
                let saved = screen.pending_memory(&own)?;
                Some((screen.id().key().to_owned(), saved))
            })
            .collect();
        // The conversation, the work screens and Knowledge keep the named fields their state had
        // before they had their own screens.
        let chat: ChatPendingMemory = screens
            .remove(ScreenId::Chat.key())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let work: crate::memory::WorkPendingMemory = screens
            .remove(ScreenId::Work.key())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        PendingMemory {
            conventions: screens
                .remove(ScreenId::Conventions.key())
                .unwrap_or_default(),
            databases: screens
                .remove(ScreenId::Databases.key())
                .unwrap_or_default(),
            starting: chat.starting,
            submitted: chat.submitted,
            removed_while_sending: chat.removed_while_sending,
            removed: chat.removed,
            knowledge_import: screens
                .remove(ScreenId::Knowledge.key())
                .and_then(|v| serde_json::from_value(v).ok()),
            plans: work.plans,
            commands,
            screens,
        }
    }

    /// Puts back what [`Self::pending_memory`] kept; the first frame confirms it.
    fn restore_pending(&mut self, mut pending: crate::memory::PendingMemory) {
        // Conventions, Databases, Chat, Work and Knowledge keep the named fields their state had
        // before the per-screen map, so earlier files read back unchanged.
        let chat = crate::memory::ChatPendingMemory {
            starting: pending.starting.take(),
            submitted: pending.submitted.take(),
            removed_while_sending: std::mem::take(&mut pending.removed_while_sending),
            removed: std::mem::take(&mut pending.removed),
        };
        if let Ok(chat) = serde_json::to_value(chat) {
            pending
                .screens
                .insert(ScreenId::Chat.key().to_owned(), chat);
        }
        let work = crate::memory::WorkPendingMemory {
            plans: std::mem::take(&mut pending.plans),
        };
        if let Ok(work) = serde_json::to_value(work) {
            pending
                .screens
                .insert(ScreenId::Work.key().to_owned(), work);
        }
        for (id, saved) in [
            (ScreenId::Conventions, &mut pending.conventions),
            (ScreenId::Databases, &mut pending.databases),
        ] {
            if !saved.is_null() {
                pending
                    .screens
                    .insert(id.key().to_owned(), std::mem::take(saved));
            }
        }
        if let Some(import) = pending
            .knowledge_import
            .take()
            .and_then(|i| serde_json::to_value(i).ok())
        {
            pending
                .screens
                .insert(ScreenId::Knowledge.key().to_owned(), import);
        }
        for screen in self.screens_mut() {
            if let Some(saved) = pending.screens.get(screen.id().key()) {
                screen.restore_pending(saved);
            }
        }
        for c in pending.commands {
            let slot = c.slot.clone();
            let Some(u) =
                Unresolved::restored_dispatch(c.slot, c.label, &c.operation, c.frame, c.dispatch)
            else {
                continue;
            };
            for screen in self.screens_mut() {
                if screen.id() == slot.screen() {
                    screen.restore_in_flight(&u);
                }
            }
            if let Some(client) = &mut self.client {
                client.keep_unresolved(u);
            }
        }
        let ids: Vec<ScreenId> = self.screens().iter().map(|s| s.id()).collect();
        for id in ids {
            self.with_screen(id, |s, cx| s.restored(cx));
        }
    }

    fn save_memory(&mut self) {
        self.persist(false);
    }

    /// Writes view state when it changed (or always, when `force`).
    fn persist(&mut self, force: bool) {
        let memory = self.memory();
        if !force && self.saved.as_ref() == Some(&memory) {
            // Nothing to save; the frame loop publishes editor changes as they happen.
            self.last_save = Instant::now();
            return;
        }
        match self.publish_purge_client(true) {
            Ok(true) => {
                self.saved = Some(memory);
                self.last_save = Instant::now();
                return;
            }
            Err(e) => {
                self.announce(e);
                self.last_save = Instant::now();
                return;
            }
            Ok(false) => {}
        }
        if let Some(path) = &self.memory_path
            && memory.save(path).is_ok()
        {
            self.saved = Some(memory);
        }
        self.last_save = Instant::now();
    }

    // -----------------------------------------------------------------------------------------
    // Frame

    /// Renders one frame of the whole application into `ui` (the root viewport's UI).
    pub fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.egui = Some(ctx.clone());
        if let Some(b) = self.backend() {
            b.set_application_focus(ctx.input(|i| i.viewport().focused));
        }
        if !self.started {
            self.started = true;
            theme::apply(&ctx);
            if let Some(c) = &self.client {
                let c2 = ctx.clone();
                c.set_waker(move || {
                    let c3 = c2.clone();
                    Box::new(move || c3.request_repaint())
                });
            }
            self.load_sessions();
            self.with_typed(|d: &mut decisions::DecisionsUi, cx| {
                d.load_completion_notifications(cx)
            });
            // The navigation's project selector lists the projects on every screen.
            if self.route != Route::Portfolio && self.route != Route::Repositories {
                self.load_projects();
            }
            self.load_route();
            self.with_typed(|c: &mut Chat, cx| c.resume(cx));
            // Outcomes left unknown at the last close: confirmed with their exact frames.
            if self.unconfirmed_count() > 0 {
                self.confirm_outcomes();
            }
            self.with_typed(|c: &mut Chat, cx| c.store_waiting_drafts(cx));
        }
        self.apply_text_scale(&ctx);
        self.advance_lifecycle(&ctx);
        let input = ctx.input(|i| !i.events.is_empty() || !i.raw.dropped_files.is_empty());
        self.drain();
        // Building the snapshot reads every draft and editor, so it is published only when
        // input or a reply may have changed one, or while a purge review inspects it.
        if (self.editors_changed || input || self.purge.frozen || self.purge.project.is_some())
            && let Err(e) = self.publish_purge_client(false)
        {
            self.status = Some(e);
        }
        // This frame's widgets apply its input after the publish above: publish again next frame.
        self.editors_changed |= input;
        if self.lifecycle == Lifecycle::Open && !self.purge.frozen {
            self.poll_hints(&ctx);
            self.input(&ctx);
            self.with_typed(|c: &mut Chat, cx| c.frame(cx, &ctx));
        }
        if self.lifecycle == Lifecycle::Open {
            self.append_coordinator_context_if_changed();
            self.with_typed(|c: &mut Chat, cx| c.route_status_due(cx, &ctx));
        }
        if self.purge.frozen {
            ui.disable();
        }
        self.toolbar(ui);
        self.statusbar(ui);
        self.navigation(ui);
        if self.chat.coordinator_open && self.route != Route::Chat {
            self.with_typed(|c: &mut Chat, cx| c.coordinator_sidebar(cx, ui));
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(theme::BG).inner_margin(theme::GUTTER))
            .show(ui, |ui| self.content(ui));
        let route = self.route;
        self.overlays(&ctx);
        self.with_screen(ScreenId::Purge, |p, cx| p.ui(cx, route, ui));
        if self.lifecycle == Lifecycle::Open && self.last_save.elapsed() > Duration::from_secs(3) {
            self.save_memory();
        }
        // Periodic re-query of visible data (missed-hint repair) needs a frame now and then.
        ctx.request_repaint_after(Duration::from_secs(5));
    }

    /// Clamps the text scale to [`theme::MAX_TEXT_SCALE`] and keeps the native minimum window
    /// at [`theme::MIN_WINDOW`] logical points for the current scale (UI-07: layouts are
    /// designed and tested at that logical minimum).
    fn apply_text_scale(&mut self, ctx: &egui::Context) {
        let mut scale = ctx.zoom_factor();
        if scale > theme::MAX_TEXT_SCALE {
            scale = theme::MAX_TEXT_SCALE;
            ctx.set_zoom_factor(scale);
        }
        if (scale - self.applied_scale).abs() > f32::EPSILON {
            self.applied_scale = scale;
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(
                egui::vec2(theme::MIN_WINDOW[0], theme::MIN_WINDOW[1]) * scale,
            ));
        }
    }

    fn input(&mut self, ctx: &egui::Context) {
        let conversation_visible = self.chat.visible(self.route);
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() {
            if conversation_visible {
                self.attach(dropped);
            } else if self.route == Route::Knowledge && self.route_memory(Route::Knowledge).tab == 2
            {
                self.with_typed(|c: &mut crate::conventions_screen::ConventionsScreen, cx| {
                    c.import_dropped(cx, dropped)
                });
            } else if self.route == Route::Knowledge {
                // A file dropped on an entry view is previewed before import.
                self.with_typed(|k: &mut KnowledgeScreen, cx| k.import_files(cx, dropped));
            } else {
                self.announce("Open the workspace coordinator to attach dropped files.");
            }
        }
        self.with_typed(|c: &mut Chat, cx| c.paste(cx, ctx));
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        Panel::top("toolbar")
            .exact_size(TOOLBAR)
            .frame(
                Frame::new()
                    .fill(theme::BG)
                    .stroke(theme::soft_line())
                    .inner_margin(theme::margin(16, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new(self.route.nav_label()).strong());
                    widgets::caption(ui, "/  All projects");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(
                            ui,
                            widgets::Icon::Bell,
                            "Notifications",
                            "Notifications",
                            true,
                            Some(self.decisions.notifications_open),
                        )
                        .clicked()
                        {
                            self.decisions.notifications_open = !self.decisions.notifications_open;
                        }
                        ui.add_enabled(false, egui::Button::new("Find work, code, knowledge"))
                            .on_disabled_hover_text(
                                "Search arrives with the work, code and Knowledge indexes \
                                 (DW-12, DW-28, DW-30).",
                            );
                        let help = ui
                            .button("Help")
                            .on_hover_text("Open local Help for this screen");
                        help.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::Role::Button,
                                true,
                                "Help for current screen",
                            )
                        });
                        if help.clicked() {
                            let screen = self.help_topic();
                            self.open_help_context(screen);
                        }
                        let open = self.chat.coordinator_open;
                        let tip = if self.route == Route::Chat {
                            "The workspace conversation is open in the centre"
                        } else if open {
                            "Close the workspace coordinator sidebar"
                        } else {
                            "Open the workspace coordinator beside this screen"
                        };
                        let chat = widgets::icon_button(
                            ui,
                            widgets::Icon::Chat,
                            "Workspace coordinator",
                            tip,
                            true,
                            Some(open),
                        );
                        if chat.clicked() {
                            self.toggle_coordinator(&ctx);
                        }
                    });
                });
            });
    }

    /// The Help topic of the screen shown: its own, or the route's.
    fn help_topic(&self) -> &'static str {
        let default_memory = RouteMemory::default();
        let mem = self.routes.get(&self.route).unwrap_or(&default_memory);
        self.screen(self.shown_screen())
            .and_then(|s| s.help_topic(self.route, mem))
            .unwrap_or(self.route.screen().id)
    }

    fn statusbar(&mut self, ui: &mut Ui) {
        let activities = self
            .client
            .as_ref()
            .map(Client::activities)
            .unwrap_or_default();
        let runtime = match (&self.offline, &self.lifecycle) {
            (Some(_), _) => ("Runtime offline", Tone::Attention),
            (None, Lifecycle::Open) => ("Runtime running", Tone::Good),
            (None, _) => ("Closing", Tone::Attention),
        };
        let status = self.status.clone();
        let unconfirmed = self.unconfirmed_count();
        let mut confirm = false;
        Panel::bottom("statusbar")
            .exact_size(theme::STATUS_BAR)
            .frame(
                Frame::new()
                    .fill(theme::BG)
                    .stroke(theme::soft_line())
                    .inner_margin(theme::margin(12, 4)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    widgets::caption(ui, "Local workspace");
                    chip(ui, runtime.0, runtime.1);
                    // Before the (changing) activity list, so the control never moves under
                    // the pointer as requests come and go.
                    if unconfirmed > 0 {
                        chip(
                            ui,
                            format!(
                                "{unconfirmed} outcome{} not confirmed",
                                if unconfirmed == 1 { "" } else { "s" }
                            ),
                            Tone::Attention,
                        );
                        if ui
                            .small_button("Confirm outcomes")
                            .on_hover_text(
                                "Resubmit each unconfirmed request unchanged: a request that \
                                 was recorded is replayed, never repeated.",
                            )
                            .clicked()
                        {
                            confirm = true;
                        }
                    }
                    for a in activities.iter().take(3) {
                        ui.add(egui::Spinner::new().size(11.0));
                        let secs = a.elapsed.as_secs();
                        widgets::caption(
                            ui,
                            if secs >= 2 {
                                format!("{} ({secs} s)", a.label)
                            } else {
                                a.label.clone()
                            },
                        );
                    }
                    if activities.len() > 3 {
                        widgets::caption(ui, format!("+{} more", activities.len() - 3));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // A polite live region: status changes are announced (UI-07).
                        // Truncated to the space left, so it never covers the controls.
                        let r = ui.add(
                            egui::Label::new(
                                RichText::new(status.unwrap_or_default())
                                    .small()
                                    .color(theme::MUTED),
                            )
                            .truncate()
                            .selectable(false),
                        );
                        ui.ctx().accesskit_node_builder(r.id, |n| {
                            n.set_live(accesskit::Live::Polite);
                        });
                    });
                });
            });
        if confirm {
            self.confirm_outcomes();
        }
    }

    /// Commands and attachment saves whose outcome is not confirmed yet.
    pub fn unconfirmed_count(&self) -> usize {
        self.client.as_ref().map_or(0, |c| c.unresolved().len())
            + self
                .screens()
                .iter()
                .map(|s| s.unconfirmed_count())
                .sum::<usize>()
    }

    /// Resubmits every unconfirmed request with its identical frame (same request key).
    pub fn confirm_outcomes(&mut self) {
        if let Some(c) = &mut self.client {
            c.resubmit_unresolved();
        }
        // The resubmitted save runs again (and says so) until its outcome arrives.
        self.with_typed(|s: &mut SettingsScreen, _| s.outcomes_confirmed());
        self.with_typed(|c: &mut Chat, cx| c.confirm_attachments(cx));
        // A Knowledge import file's save: its identical frame.
        self.with_typed(|k: &mut KnowledgeScreen, cx| k.confirm_import(cx));
        self.with_typed(|c: &mut Chat, cx| c.confirm_removed(cx));
    }

    fn navigation(&mut self, ui: &mut Ui) {
        let mut go = None;
        Panel::left("navigation")
            .exact_size(theme::NAV)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(theme::BG)
                    .stroke(theme::soft_line())
                    .inner_margin(theme::margin(12, 14)),
            )
            .show(ui, |ui| {
                ui.label(RichText::new("DevWorks").size(18.0).strong());
                ui.add_space(10.0);
                let item = |ui: &mut Ui, r: Route, current: Route, go: &mut Option<Route>| {
                    let active = current == r
                        || (r == Route::Agents
                            && matches!(current, Route::Resources | Route::Browser))
                        || (r == Route::Repositories && current == Route::Indexing)
                        || (r == Route::Settings && current == Route::AddOns);
                    let resp = ui.add(
                        egui::Button::selectable(active, (r.nav_label(), egui::Atom::grow()))
                            .min_size(egui::vec2(ui.available_width(), 28.0)),
                    );
                    if resp.clicked() {
                        *go = Some(r);
                    }
                };
                let bottom_height = 4.0 * 32.0 + 64.0;
                let top_height = (ui.available_height() - bottom_height).max(120.0);
                ui.allocate_ui(egui::vec2(ui.available_width(), top_height), |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("nav-scroll")
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for r in NAV_ORDER
                                .iter()
                                .copied()
                                .filter(|r| r.screen().group == NavGroup::Global)
                            {
                                item(ui, r, self.route, &mut go);
                            }
                            ui.add_space(10.0);
                            widgets::caption(ui, "YOUR PROJECT");
                            self.project_selector(ui);
                            for r in NAV_ORDER
                                .iter()
                                .copied()
                                .filter(|r| r.screen().group == NavGroup::Project)
                            {
                                item(ui, r, self.route, &mut go);
                            }
                            ui.add_space(6.0);
                            let r = ui.add(
                                egui::Button::new(
                                    RichText::new("INTELLIGENCE      ")
                                        .small()
                                        .color(theme::DIM),
                                )
                                .frame(false),
                            );
                            let chevron = egui::Rect::from_min_size(
                                egui::pos2(r.rect.right() - 16.0, r.rect.top()),
                                egui::vec2(14.0, r.rect.height()),
                            );
                            widgets::paint_icon(
                                ui,
                                chevron,
                                if self.intelligence_open {
                                    widgets::Icon::ChevronDown
                                } else {
                                    widgets::Icon::ChevronRight
                                },
                                theme::DIM,
                            );
                            let open = self.intelligence_open;
                            r.widget_info(|| {
                                egui::WidgetInfo::selected(
                                    egui::Role::Button,
                                    true,
                                    open,
                                    "Intelligence",
                                )
                            });
                            if r.clicked() {
                                self.intelligence_open = !self.intelligence_open;
                            }
                            if self.intelligence_open {
                                for r in NAV_ORDER
                                    .iter()
                                    .copied()
                                    .filter(|r| r.screen().group == NavGroup::Intelligence)
                                {
                                    item(ui, r, self.route, &mut go);
                                }
                            }
                        });
                });
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    // The left-aligned footer: installed version and the update message.
                    let r = ui
                        .add(
                            egui::Button::new(
                                RichText::new("Updates: not checked")
                                    .small()
                                    .color(theme::ACCENT),
                            )
                            .frame(false),
                        )
                        .on_hover_text("Update discovery arrives with DW-58");
                    if r.clicked() {
                        self.updates_open = !self.updates_open;
                    }
                    ui.horizontal(|ui| {
                        let (dot, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 12.0), egui::Sense::hover());
                        widgets::paint_icon(ui, dot, widgets::Icon::Dot, theme::GREEN);
                        ui.label(RichText::new(format!("DevWorks {}", self.version)).small());
                    });
                    ui.add_space(6.0);
                    for r in NAV_ORDER
                        .iter()
                        .rev()
                        .copied()
                        .filter(|r| r.screen().group == NavGroup::Bottom)
                    {
                        item(ui, r, self.route, &mut go);
                    }
                });
            });
        if let Some(r) = go {
            self.navigate(r);
        }
    }

    fn content(&mut self, ui: &mut Ui) {
        let route = self.route;
        self.sync_watch_projects();
        self.with_screen(self.shown_screen(), |s, cx| s.ui(cx, route, ui));
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        let card_actions = self.completions.ui(ctx, TOOLBAR + 8.0);
        for a in card_actions {
            self.with_typed(|d: &mut decisions::DecisionsUi, cx| d.completion_action(cx, ctx, a));
        }
        if let Some(text) = self.completions.take_announcement() {
            self.announce(text);
        }
        self.with_typed(|d: &mut decisions::DecisionsUi, cx| d.notification_center(cx, ctx));
        if self.updates_open {
            let mut open = true;
            egui::Window::new("Updates")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(
                    egui::Align2::LEFT_BOTTOM,
                    egui::vec2(theme::NAV + 8.0, -36.0),
                )
                .show(ctx, |ui| {
                    ui.label(format!("Installed: DevWorks {}", self.version));
                    widgets::state_block(
                        ui,
                        &ViewState::Unavailable {
                            what: "update discovery, approval and installation".into(),
                            owner: "DW-58".into(),
                        },
                    );
                });
            self.updates_open = open;
        }
        if !matches!(self.lifecycle, Lifecycle::Open | Lifecycle::Closed) {
            let progress = self.control.as_ref().and_then(|c| c.progress());
            let step = match (&self.lifecycle, progress) {
                (Lifecycle::Exiting { summary }, _) => summary.clone(),
                (Lifecycle::Persisting, _) => "Saving the window state…".to_owned(),
                // The runtime's own shutdown stage, when a runtime is running.
                (Lifecycle::Quiescing { .. } | Lifecycle::ShuttingDown, Some(stage)) => stage,
                (Lifecycle::Quiescing { .. }, None) => "Finishing requests in flight…".to_owned(),
                (Lifecycle::ShuttingDown, None) => {
                    "Stopping delivery, collection and owned processes…".to_owned()
                }
                _ => String::new(),
            };
            egui::Modal::new(egui::Id::unique("closing")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Closing DevWorks");
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label(step);
                });
                widgets::caption(
                    ui,
                    "Nothing keeps running after the window closes. Interrupted work is \
                     recovered at the next launch.",
                );
            });
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) && self.chat.visible(self.route) {
            egui::Area::new(egui::Id::unique("drop-hint"))
                .order(egui::Order::Tooltip)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label("Drop to attach to the workspace conversation");
                    });
                });
        }
    }
}
