//! The shell: navigation, the integrated page layout, the shared conversation and its
//! app-level sidebar, Settings, the completion overlay, status and the close lifecycle. It holds
//! view state only; every record comes through [`Client`] from the runtime's gateway.
use crate::attachments;
use crate::backend::{Backend, Client, Reply, Slot, TaskOutput, Unresolved, is_unknown};
use crate::chat::Chat;
use crate::knowledge_screen::{self, KnowledgeAction, KnowledgeScreen};
use crate::mcp_panel::McpAction;
use crate::memory::{Memory, RouteMemory};
use crate::notifications::{CardAction, Completions};
use crate::ops::{self, Failure};
use crate::projects_screen::ProjectLists;
use crate::route::{NAV_ORDER, NavGroup, Route};
use crate::screen::{Cx, DynScreen, Effect, ScreenId};
use crate::settings_screen::{KEEP_AWAKE, SettingsAction, SettingsScreen};
use crate::theme;
use crate::virtual_list::{self, Scrolls};
use crate::web_panel::WebAction;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{ConnectionLifecycle, ItemState, LifeState, ProjectRecord};
use devworks_core::protocol::{DomainErrorCode, Id};
use devworks_core::settings::validate::codes;
use devworks_core::settings::view::Health;
use eframe::egui::{self, Align, Frame, Layout, Panel, RichText, Ui, ViewportCommand, accesskit};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub mod chat;
mod conventions;
mod databases;
pub mod decisions;
pub mod graph;
pub mod help;
pub mod indexing;
mod instructions;
pub mod jobs;
mod project_purge;
pub mod projects;
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

/// A list bound to one query.
pub struct ListState<T> {
    pub items: Vec<T>,
    pub loaded: bool,
    pub error: Option<String>,
    pub more: bool,
}
impl<T> Default for ListState<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            loaded: false,
            error: None,
            more: false,
        }
    }
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
    purge_reviews: BTreeMap<String, serde_json::Value>,
    last_save: Instant,
    last_poll: Instant,
    /// The window's context, from the first frame on (screens make textures through it).
    egui: Option<egui::Context>,
    /// The drafts the pending `settings.validate` checked; a report for other drafts is stale.
    validated_drafts: Option<BTreeMap<String, serde_json::Value>>,
    /// The settings revision each pending `settings.explain_source` was asked at; a chain read
    /// before the view moved to another revision is stale.
    explained_at: BTreeMap<String, Option<i64>>,
    notifications_open: bool,
    updates_open: bool,
    shutdown_summary: Option<String>,
    /// The text scale the native minimum window size was last set for.
    applied_scale: f32,
    // Dropped last: the engine's shutdown and thread join run after the request workers stop.
    control: Option<Box<dyn HostControl>>,
}

/// The migrated screens: `screens!(Id => field, ...)` binds each [`ScreenId`] to the `Shell`
/// field holding that screen. Indexing (`projects.indexing`) is bound in the macro itself: every
/// other screen's context reads it. So is Chat: every other screen's context reads its router
/// catalog, and its own context lends it the watcher lists its sidebar shows.
macro_rules! screens {
    ($($id:ident => $($field:ident).+),* $(,)?) => {
        impl Shell {
            fn screens(&self) -> Vec<&dyn DynScreen> {
                vec![$(&self.$($field).+,)* &self.projects.indexing, &self.chat]
            }
            fn screens_mut(&mut self) -> Vec<&mut dyn DynScreen> {
                vec![$(&mut self.$($field).+,)* &mut self.projects.indexing, &mut self.chat]
            }
            fn screen(&self, id: ScreenId) -> Option<&dyn DynScreen> {
                match id {
                    $(ScreenId::$id => Some(&self.$($field).+),)*
                    ScreenId::Indexing => Some(&self.projects.indexing),
                    ScreenId::Chat => Some(&self.chat),
                    ScreenId::Legacy => None,
                }
            }
            /// The screen `id` with a context over the rest of the shell.
            fn screen_mut(&mut self, id: ScreenId) -> Option<(&mut dyn DynScreen, Cx<'_>)> {
                if id == ScreenId::Legacy {
                    return None;
                }
                let online = self.online();
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
                    job_status: &self.jobs.project_status,
                    archive_counts: &self.work.archive_counts,
                    ctx: self.egui.clone(),
                    watchers: None,
                    effects: Vec::new(),
                };
                match id {
                    $(ScreenId::$id => {
                        cx.projects.indexing = Some(&self.projects.indexing);
                        cx.catalog = &self.chat.routing.catalog;
                        Some((&mut self.$($field).+, cx))
                    })*
                    // The Indexing screen's own context does not hold its state.
                    ScreenId::Indexing => {
                        cx.catalog = &self.chat.routing.catalog;
                        Some((&mut self.projects.indexing, cx))
                    }
                    // Nor does Chat's: it reads its own catalog.
                    ScreenId::Chat => {
                        cx.projects.indexing = Some(&self.projects.indexing);
                        cx.watchers = Some(&self.watchers);
                        Some((&mut self.chat, cx))
                    }
                    ScreenId::Legacy => None,
                }
            }
        }
    };
}
screens!(
    Help => help,
    Usage => usage,
    Graph => graph,
    Databases => databases,
    Instructions => settings.instructions,
    Conventions => conventions,
    Artifacts => artifacts,
    Resources => resources,
    Browser => browser,
    Projects => projects.screen,
    Watchers => watchers,
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
            purge: Default::default(),
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
            purge_reviews: memory.purge_reviews.clone(),
            last_save: Instant::now(),
            last_poll: Instant::now(),
            egui: None,
            validated_drafts: None,
            explained_at: BTreeMap::new(),
            notifications_open: false,
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

    /// Runs `f` on screen `id`, then the effects it asked for; `false` for a legacy screen.
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
    fn run_effects(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Navigate(route) => self.navigate(route),
                Effect::OpenWork(id) => self.open_work(&id),
                Effect::OpenSession(id) => self.open_session(id),
                Effect::LoadProjects => self.load_projects(),
                Effect::LoadRepositories => self.load_repositories(),
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
                Effect::LoadProjectJobs(p) => self.load_project_jobs(&p),
                Effect::CountArchiveWork(p) => self.count_archive_work(&p),
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
                    self.settings.category =
                        Some(devworks_core::settings::catalog::Category::Instructions);
                    if let Some(v) = &mut self.settings.view {
                        v.set_category(self.settings.category);
                    }
                    self.navigate(Route::Settings);
                }
                Effect::ArtifactsChanged => self.artifacts.mutations += 1,
                Effect::Watch(actions) => self.watch_act(actions),
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
            self.databases.generated_key = None;
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
    pub fn load_sessions(&mut self) {
        self.with_typed(|c: &mut Chat, cx| c.load_sessions(cx));
    }
    fn load_transcript(&mut self, id: &str) {
        self.with_typed(|c: &mut Chat, cx| c.load_transcript(cx, id));
    }
    fn load_router_catalog(&mut self) {
        self.with_typed(|c: &mut Chat, cx| c.load_router_catalog(cx));
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
        if !self.chat.coordinator_open || self.route == Route::Chat {
            return;
        }
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
        let tab = match self.route {
            _ if screen_context.is_some() => screen_context.as_ref().map_or(0, |c| c.tab),
            Route::Approvals => self.decisions.tab,
            Route::Knowledge => self.knowledge.tab,
            _ => self.routes.get(&self.route).map_or(0, |view| view.tab),
        };
        let selection = match self.route {
            _ if screen_context.is_some() => {
                screen_context.as_ref().and_then(|c| c.selection.as_deref())
            }
            Route::Approvals => self.decisions.selected.as_ref().map(|v| v.id.as_str()),
            Route::Knowledge => self.knowledge.selected.as_ref().map(Id::as_str),
            Route::Jobs => self.jobs.selected.as_deref(),
            Route::AddOns => self.addons.selected.as_deref(),
            _ => self
                .routes
                .get(&self.route)
                .and_then(|view| view.selection.as_deref()),
        };
        let selection = selection.unwrap_or("");
        let knowledge_project = if self.route == Route::Knowledge {
            self.knowledge.project.as_deref().unwrap_or("")
        } else {
            ""
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
                knowledge_project
            },
            screen.help_topic
        );
        self.with_typed(|c: &mut Chat, cx| c.notify_coordinator(cx, key, context));
    }

    fn load_route(&mut self) {
        let route = self.route;
        if self.with_screen(route.screen().owner, |s, cx| s.load(cx, route)) {
            return;
        }
        match self.route {
            Route::Approvals => {
                self.decisions.tab = self.routes.entry(Route::Approvals).or_default().tab;
                self.load_approvals();
            }
            Route::Settings if self.settings.view.is_none() && !self.settings.loading => {
                self.load_settings();
            }
            Route::AddOns => {
                self.load_addons();
                if let Some(c) = &mut self.client {
                    c.network_task(
                        Slot::AddonsDiscovery,
                        "Checking runtimes and package indexes",
                        |b| b.refresh_packages().map(|()| TaskOutput::Refreshed),
                    );
                }
            }
            Route::Knowledge => {
                // Scope choices are the projects the user can see.
                if !self.projects.list.loaded && !self.pending(&Slot::ProjectsList) {
                    self.load_projects();
                }
                if self
                    .routes
                    .get(&Route::Knowledge)
                    .is_none_or(|r| r.tab != 2)
                    && !self.knowledge.loaded
                    && self.knowledge.listing.is_none()
                {
                    self.load_knowledge();
                }
            }
            Route::Jobs => {
                self.load_router_catalog();
                self.load_jobs();
                if !self.projects.list.loaded {
                    self.load_projects();
                }
            }
            Route::Overview | Route::Features | Route::Issues => {
                self.load_projects();
                self.load_work();
            }
            _ => {}
        }
    }

    fn pending(&self, slot: &Slot) -> bool {
        self.client.as_ref().is_some_and(|c| c.is_pending(slot))
    }
    /// Whether a command of `slot` is running or its outcome is still not confirmed.
    fn waiting(&self, slot: &Slot) -> bool {
        self.pending(slot)
            || self
                .client
                .as_ref()
                .is_some_and(|c| c.unresolved().iter().any(|u| &u.slot == slot))
    }

    // -----------------------------------------------------------------------------------------
    // Projects: the lists every screen reads and the navigation's project selection

    pub fn load_projects(&mut self) {
        self.projects.list_at = self.projects.mutations;
        self.query(
            Slot::ProjectsList,
            "Loading projects",
            "projects.list",
            |b| ops::projects_list(b, None),
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
        self.graph.opened = false;
        self.graph.sequence = self.graph.sequence.wrapping_add(1);
        self.graph.loading = false;
        self.projects.members = ListState::default();
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
    /// Submits a command; `false` when it was not submitted now (offline or closing; a closed
    /// client keeps it unresolved). A caller that marked state for it restores that state.
    fn command(
        &mut self,
        slot: Slot,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) -> bool {
        let online = self.online().is_ok();
        crate::screen::submit_command(
            self.client.as_mut(),
            online,
            &mut self.status,
            slot,
            label,
            op,
            frame,
        )
    }

    pub fn load_settings(&mut self) {
        if let Some(backend) = self.backend() {
            self.settings.connections.secret_store = Some(backend.secret_store());
        }
        self.settings.loading = true;
        self.query(
            Slot::SettingsRead,
            "Loading settings",
            "settings.read_effective",
            ops::settings_read,
        );
        self.refresh_container_capabilities();
        if !self.projects.list.loaded && !self.pending(&Slot::ProjectsList) {
            self.load_projects();
        }
        self.load_mcp_servers(None);
    }
    fn refresh_container_capabilities(&mut self) {
        self.query(
            Slot::ContainerCapabilities,
            "Checking container runtimes",
            "containers.capabilities",
            ops::container_capabilities,
        );
    }
    /// Loads the whole catalog, following its pages (see the `AddonsList` reply).
    fn load_addons(&mut self) {
        self.addons.catalog_pending = None;
        self.query(Slot::AddonsList, "Loading add-ons", "addons.catalog", |b| {
            crate::addons::catalog_frame(b, None)
        });
    }
    /// Loads a package's logs from the start, following their pages to the end.
    fn load_addon_logs(&mut self, package: String) {
        self.addons.logs_pending.insert(package.clone(), Vec::new());
        let p = package.clone();
        self.query(
            Slot::AddonsLogs(package),
            "Loading add-on logs",
            "addons.logs",
            move |b| crate::addons::logs_frame(b, &p, None),
        );
    }
    fn addons_actions(&mut self, actions: Vec<crate::addons::AddonsAction>) {
        use crate::addons::{self as a, AddonsAction};
        for action in actions {
            let registry = self.addons.registry_revision;
            let command = |op: &'static str,
                           cmd: serde_json::Value|
             -> (&'static str, serde_json::Value) { (op, cmd) };
            let lifecycle = match action {
                AddonsAction::Refresh => {
                    self.load_addons();
                    None
                }
                AddonsAction::Discover => {
                    if let Some(c) = &mut self.client {
                        c.network_task(
                            Slot::AddonsDiscovery,
                            "Checking runtimes and package indexes",
                            |b| b.refresh_packages().map(|()| TaskOutput::Refreshed),
                        );
                    }
                    None
                }
                AddonsAction::InstallOffer {
                    package,
                    digest,
                    update,
                } => {
                    if let Some(c) = &mut self.client {
                        self.addons.busy = c.network_task(
                            Slot::AddonsSave {
                                update: update.then_some(package.clone()),
                                registry,
                                download: None,
                            },
                            "Downloading the add-on",
                            move |b| {
                                let path = b.fetch_index_bundle(&package, &digest)?;
                                a::prepare_downloaded_install(b, path)
                            },
                        );
                    }
                    None
                }
                AddonsAction::Select(id) => {
                    self.addons.selected = Some(id);
                    None
                }
                AddonsAction::Logs(id) => {
                    self.load_addon_logs(id);
                    None
                }
                AddonsAction::OpenIndexing => {
                    self.navigate(Route::Indexing);
                    None
                }
                AddonsAction::InstallFromFile(update) => {
                    if let Some(path) = self.dialogs.pick_package()
                        && let Some(c) = &mut self.client
                    {
                        self.addons.busy = c.task(
                            Slot::AddonsSave {
                                update,
                                registry,
                                download: None,
                            },
                            "Preparing the add-on",
                            move |b| a::prepare_install_from_file(b, &path),
                        );
                    }
                    None
                }
                AddonsAction::Approve {
                    package,
                    bundle,
                    update,
                } => {
                    let bundle = serde_json::to_value(&bundle).unwrap_or_default();
                    let expected = registry.max(1).to_string();
                    Some(if update {
                        command(
                            "addons.update",
                            serde_json::json!({"kind": "Update", "packageId": package, "bundle": bundle, "expectedRegistryRevision": expected}),
                        )
                    } else {
                        command(
                            "addons.install",
                            serde_json::json!({"kind": "Install", "bundle": bundle, "expectedRegistryRevision": expected}),
                        )
                    })
                }
                AddonsAction::Enable(p) => Some(command(
                    "addons.enable",
                    serde_json::json!({"kind": "Enable", "packageId": p, "expectedRegistryRevision": registry.max(1).to_string()}),
                )),
                AddonsAction::Disable(p) => Some(command(
                    "addons.disable",
                    serde_json::json!({"kind": "Disable", "packageId": p, "expectedRegistryRevision": registry.max(1).to_string()}),
                )),
                AddonsAction::Uninstall(p) => Some(command(
                    "addons.uninstall",
                    serde_json::json!({"kind": "Uninstall", "packageId": p, "expectedRegistryRevision": registry.max(1).to_string()}),
                )),
                AddonsAction::Rollback {
                    package,
                    target,
                    generation,
                } => Some(command(
                    "addons.rollback",
                    serde_json::json!({"kind": "Rollback", "packageId": package, "targetManifestDigest": target, "expectedRegistryRevision": registry.max(1).to_string(), "expectedStateGeneration": generation.max(1).to_string()}),
                )),
                AddonsAction::TrustPublisher {
                    package,
                    revision,
                    trust,
                } => {
                    self.addons.busy = true;
                    self.command(
                        Slot::AddonsCommand,
                        "Saving publisher trust",
                        "addons.configure",
                        move |b| a::trust_frame(b, &package, revision, trust),
                    );
                    None
                }
                AddonsAction::Configure {
                    package,
                    revision,
                    schema,
                    value,
                } => {
                    self.addons.busy = true;
                    self.command(
                        Slot::AddonsCommand,
                        "Saving the add-on configuration",
                        "addons.configure",
                        move |b| a::configure_frame(b, &package, revision, schema, value),
                    );
                    None
                }
                AddonsAction::ProjectEnablement {
                    package,
                    revision,
                    disabled,
                } => {
                    self.addons.busy = true;
                    self.command(
                        Slot::AddonsCommand,
                        "Saving project enablement",
                        "addons.configure",
                        move |b| a::project_frame(b, &package, revision, &disabled),
                    );
                    None
                }
            };
            if let Some((op, cmd)) = lifecycle {
                self.addons.busy = true;
                self.command(Slot::AddonsCommand, "Updating the add-on", op, move |b| {
                    a::lifecycle_frame(b, cmd)
                });
            }
        }
    }

    /// Starts a new Knowledge listing (first page) and refreshes the retrieval mode. A listing
    /// that could not be submitted is not left in progress.
    fn load_knowledge(&mut self) {
        let (op, body) = self.knowledge.begin_listing(false);
        let generation = self.knowledge.generation;
        if !self.query(
            Slot::KnowledgeList(generation),
            "Loading Knowledge",
            op,
            move |b| knowledge_screen::query(b, op, body),
        ) {
            self.knowledge.listing = None;
        }
        let body = self.knowledge.diagnostics_body();
        self.query(
            Slot::KnowledgeDiagnostics,
            "Checking Knowledge",
            "knowledge.diagnostics",
            move |b| knowledge_screen::query(b, "knowledge.diagnostics", body),
        );
    }
    /// Gives the Knowledge screen the projects the user can see (active and archived, from
    /// `projects.list`) and follows the navigation's project selection; a changed scope lists
    /// again.
    fn sync_knowledge_scope(&mut self, actions: &mut Vec<KnowledgeAction>) {
        use devworks_core::protocol::records::LifeState;
        if !self.projects.list.loaded {
            return;
        }
        let projects: Vec<(String, String)> = self
            .projects
            .list
            .items
            .iter()
            .filter(|p| matches!(p.lifecycle, LifeState::Active | LifeState::Archived))
            .map(|p| {
                let name = match p.lifecycle {
                    LifeState::Archived => format!("{} (archived)", p.name.0),
                    _ => p.name.0.clone(),
                };
                (p.id.0.as_str().to_owned(), name)
            })
            .collect();
        let mut changed = self.knowledge.set_projects(projects);
        let nav = self.selected_project().map(str::to_owned);
        if self.knowledge.followed.as_ref() != Some(&nav) {
            self.knowledge.followed = Some(nav.clone());
            changed |= self.knowledge.follow_navigation(nav.as_deref());
        }
        if changed && self.knowledge.loaded {
            actions.push(KnowledgeAction::Search);
        }
    }
    /// Stores the first of `paths` as the user's artifact (like a chat attachment), then
    /// previews its import. One import at a time; nothing is imported before Confirm.
    fn import_knowledge_file(&mut self, paths: Vec<PathBuf>) {
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "import.json".into());
        if !self.knowledge.begin_import(&name) {
            self.knowledge.message = Some((
                widgets::Tone::Attention,
                "An import is already in progress.".into(),
            ));
            return;
        }
        let media = if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            "application/json"
        } else {
            attachments::media_type(&path)
        };
        let submitted = self.online().is_ok()
            && self.client.as_mut().is_some_and(|c| {
                c.task(
                    Slot::KnowledgeImportSave,
                    "Storing the import file",
                    move |b| attachments::prepare_file(b, &path, &name, media, "knowledge-import"),
                )
            });
        if !submitted {
            self.knowledge.import = Some(knowledge_screen::ImportStage::Failed {
                name: "the file".into(),
                error: "DevWorks is offline or closing.".into(),
            });
        }
    }
    fn read_knowledge(&mut self, claim: devworks_core::protocol::Id) {
        let body = self.knowledge.read_body(&claim);
        self.query(
            Slot::KnowledgeRead(claim.as_str().to_owned()),
            "Loading a Knowledge entry",
            "knowledge.read",
            move |b| knowledge_screen::query(b, "knowledge.read", body),
        );
    }
    /// Submits one Knowledge write at a time; what it took from the screen's forms
    /// (`submitted`) stays until its outcome is recorded.
    fn knowledge_write(
        &mut self,
        label: &str,
        op: &'static str,
        body: serde_json::Value,
        submitted: knowledge_screen::Submitted,
    ) {
        if self.knowledge.writing.is_some() {
            return;
        }
        self.knowledge.writing = Some(label.to_owned());
        self.knowledge.submitted = Some(submitted);
        if !self.command(Slot::KnowledgeWrite, label, op, move |b| {
            knowledge_screen::command(b, op, body)
        }) {
            // Refused because an earlier write is unresolved: that one stays the write in
            // flight until Confirm outcomes settles it.
            let held = self.client.as_ref().and_then(|c| {
                c.unresolved()
                    .iter()
                    .find(|u| u.slot == Slot::KnowledgeWrite)
                    .map(|u| u.label.clone())
            });
            self.knowledge.message = Some((
                widgets::Tone::Attention,
                match &held {
                    Some(earlier) => format!(
                        "{earlier}: the outcome is not confirmed yet. Use Confirm outcomes \
                         first; Knowledge changes wait until it is known."
                    ),
                    None => format!("{label} was not sent: DevWorks is offline or closing."),
                },
            ));
            self.knowledge.writing = held;
            self.knowledge.submitted = None;
        }
    }
    fn knowledge_actions(&mut self, actions: Vec<KnowledgeAction>) {
        for a in actions {
            match a {
                KnowledgeAction::Search => self.load_knowledge(),
                KnowledgeAction::More => {
                    if self.knowledge.listing.is_some() || self.knowledge.cursor.is_none() {
                        continue;
                    }
                    let (op, body) = self.knowledge.begin_listing(true);
                    let generation = self.knowledge.generation;
                    if !self.query(
                        Slot::KnowledgeList(generation),
                        "Loading more Knowledge",
                        op,
                        move |b| knowledge_screen::query(b, op, body),
                    ) {
                        self.knowledge.listing = None;
                    }
                }
                KnowledgeAction::Select(id) => {
                    self.knowledge.selected = Some(id.clone());
                    self.knowledge.detail = None;
                    self.knowledge.detail_error = None;
                    self.read_knowledge(id);
                }
                KnowledgeAction::Save(draft) => match self.knowledge.save_body(&draft) {
                    Ok((op, body)) => self.knowledge_write(
                        "Entry",
                        op,
                        body,
                        knowledge_screen::Submitted::Draft(draft),
                    ),
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::PreviewSource => match self.knowledge.preview_body() {
                    Ok((source, body)) => {
                        self.knowledge.source_preview = None;
                        self.query(
                            Slot::KnowledgePreview(source),
                            "Previewing a cleanup",
                            "knowledge.diagnostics",
                            move |b| knowledge_screen::query(b, "knowledge.diagnostics", body),
                        );
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::ImportDialog => {
                    let picked = self.dialogs.pick_files();
                    self.import_knowledge_file(picked);
                }
                KnowledgeAction::ImportFiles(paths) => self.import_knowledge_file(paths),
                KnowledgeAction::ConfirmImport => match self.knowledge.import_body() {
                    Ok(body) => {
                        if self.command(
                            Slot::KnowledgeImport,
                            "Importing Knowledge",
                            "knowledge.import_user_data",
                            move |b| {
                                knowledge_screen::command(b, "knowledge.import_user_data", body)
                            },
                        ) {
                            self.knowledge.import_started();
                        } else {
                            self.knowledge.message = Some((
                                widgets::Tone::Attention,
                                "The import was not sent: DevWorks is offline or closing.".into(),
                            ));
                        }
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::CancelImport => {
                    if !self.knowledge.import.as_ref().is_some_and(|s| s.busy()) {
                        self.knowledge.import = None;
                    }
                }
                KnowledgeAction::DeleteSource => match self.knowledge.delete_source_body() {
                    Ok((op, body)) => self.knowledge_write(
                        "Delete by source",
                        op,
                        body,
                        knowledge_screen::Submitted::Nothing,
                    ),
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
                KnowledgeAction::Change(change) => match self.knowledge.change_body(change) {
                    Ok((op, body)) => {
                        let submitted = self.knowledge.change_submission(change);
                        self.knowledge_write(change.label(), op, body, submitted)
                    }
                    Err(m) => self.knowledge.message = Some((widgets::Tone::Attention, m)),
                },
            }
        }
    }

    /// Admits a Knowledge import file's exact save frame (prepared, or unconfirmed).
    fn submit_import_save(&mut self, frame: Vec<u8>) -> bool {
        self.client.as_mut().is_some_and(|c| {
            c.task(
                Slot::KnowledgeImportSave,
                "Storing the import file",
                move |b| attachments::save(b, frame, None),
            )
        })
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
        if owner != ScreenId::Legacy {
            self.with_screen(owner, |s, cx| s.apply(cx, slot, reply));
            return;
        }
        let Some((slot, reply)) = self.apply_purge(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_jobs(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_work(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_decisions(slot, reply) else {
            return;
        };
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
                        self.with_screen(ScreenId::Browser, |s, cx| s.load(cx, Route::Browser));
                    }
                }
                Err(e) => self.projects.list.error = Some(e.describe()),
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
            // Started before a session mutation was applied: it may lack that change.
            (Slot::ContainerCapabilities, Reply::Query(q)) => {
                match ops::container_capability_entries(q) {
                    Ok(entries) => {
                        self.settings.container_capabilities = Some(entries);
                        self.settings.container_error = None;
                    }
                    Err(error) => self.settings.container_error = Some(error.describe()),
                }
            }
            (Slot::SettingsRead, Reply::Query(q)) => match ops::settings_entries(q) {
                Ok((revision, entries)) => {
                    self.settings.loaded(revision, &entries);
                    self.check_settings_health(revision);
                }
                Err(e) => {
                    self.settings.loading = false;
                    // The save committed but its result could not be read back: keep every
                    // draft (a later save of an already-saved value is a no-op change).
                    self.settings.after_save = false;
                    self.settings.saving = false;
                    self.settings.submitted.clear();
                    self.settings.error = Some(e.describe());
                }
            },
            (Slot::SettingsValidate, Reply::Query(_))
                if self.validated_drafts.take() != Some(self.settings.drafts()) =>
            {
                // The drafts changed while it ran: its findings describe other values.
                self.settings.validating = false;
                self.settings.message = Some((
                    Tone::Neutral,
                    "Values changed while validating; validate again.".into(),
                ));
            }
            (Slot::SettingsValidate, Reply::Query(q)) => {
                self.settings.validating = false;
                match ops::settings_report(q) {
                    Ok(report) => {
                        let valid = report.valid;
                        if let Some(v) = &mut self.settings.view {
                            v.apply_report(&report);
                        }
                        self.settings.message = Some(if valid {
                            (Tone::Good, "The changes are valid.".into())
                        } else {
                            (Tone::Bad, "Some values need attention.".into())
                        });
                    }
                    Err(e) => self.settings.message = Some((Tone::Bad, e.describe())),
                }
            }
            (Slot::SettingsHealth, Reply::Query(q)) => {
                if let Ok(report) = ops::settings_report(q) {
                    for (key, _) in self.settings.directories() {
                        let unavailable = report
                            .issues
                            .0
                            .iter()
                            .find(|i| i.path.0 == key && i.code.0 == codes::PATH_UNAVAILABLE);
                        let health = match unavailable {
                            Some(i) => Health::Unavailable(i.message.0.clone()),
                            None => Health::Available,
                        };
                        self.settings.set_health(key, health);
                    }
                }
            }
            (Slot::SettingsSave, Reply::Command(c)) => {
                let outcome = ops::settings_saved(c);
                self.settings.unconfirmed = matches!(outcome, Err(Failure::Unknown));
                match outcome {
                    Err(Failure::Unknown) => {
                        // Still "saving": the identical request confirms it, never a new one.
                        self.settings.message = Some((
                            Tone::Attention,
                            "Not yet confirmed whether the settings were saved. Use Confirm \
                             outcomes to check again."
                                .into(),
                        ));
                        self.announce("Settings: outcome not confirmed.");
                    }
                    Ok(()) => {
                        // Tier membership and other routing settings may have changed: the
                        // selectors' catalog and every availability are read again.
                        self.invalidate_routing();
                        // Still saving until the follow-up read settles the drafts.
                        self.settings.after_save = true;
                        self.settings.message = Some((Tone::Good, "Settings saved.".into()));
                        self.announce("Settings saved.");
                        self.load_settings();
                    }
                    Err(e) => {
                        self.settings.saving = false;
                        self.settings.submitted.clear();
                        let conflict = matches!(&e, Failure::Refused(f) if f.code == DomainErrorCode::RevisionConflict);
                        self.settings.message = Some((
                            Tone::Bad,
                            if conflict {
                                "Settings changed elsewhere; your changes are kept for review."
                                    .into()
                            } else {
                                e.describe()
                            },
                        ));
                        self.announce("Settings were not saved.");
                        if conflict {
                            self.load_settings();
                        }
                    }
                }
            }
            (Slot::SettingsExplain(key), Reply::Query(_))
                if self.explained_at.get(&key).copied().flatten() != self.settings_revision() =>
            {
                // The view moved to another revision meanwhile: explain it at that one.
                self.explain(key);
            }
            (Slot::WebFetch, Reply::Query(q)) => {
                let web = &mut self.settings.web;
                web.fetching = false;
                match ops::web_document(q) {
                    Ok(doc) => {
                        web.document = Some(doc);
                        web.error = None;
                    }
                    Err(e) => web.error = Some(e.describe()),
                }
                self.web_action(WebAction::LoadActivity);
            }
            (Slot::WebSearch, Reply::Query(q)) => {
                let web = &mut self.settings.web;
                web.searching = false;
                match ops::web_results(q) {
                    Ok(results) => {
                        web.results = Some(results);
                        web.error = None;
                    }
                    Err(e) => web.error = Some(e.describe()),
                }
                self.web_action(WebAction::LoadActivity);
            }
            (Slot::WebActivity, Reply::Query(q)) => {
                let web = &mut self.settings.web;
                web.activity_loading = false;
                web.activity = Some(ops::web_entries(q).map_err(|e| e.describe()));
            }
            (Slot::McpServers(scope, _), Reply::Query(_)) if scope != self.mcp_scope_key() => {}
            (Slot::McpServers(_, cursor), Reply::Query(q)) => match ops::mcp_server_page(q) {
                Ok((servers, next)) => {
                    self.settings
                        .mcp
                        .loaded_servers(servers, cursor.is_some(), next.is_none());
                    if let Some(next) = next {
                        self.load_mcp_servers(Some(next));
                    } else {
                        self.settings.mcp.loading = false;
                        self.settings.mcp.last_refresh = Some(Instant::now());
                        if let Some(server) = self.settings.mcp.selected.as_ref().and_then(|id| {
                            self.settings
                                .mcp
                                .servers
                                .iter()
                                .find(|server| &server.id == id)
                        }) && matches!(server.state, ItemState::Failed)
                        {
                            self.settings.mcp.error = Some(server.last_failure.as_ref().map_or_else(
                                || "The MCP server operation failed. Check its configuration and retry.".into(),
                                |failure| failure.0.clone(),
                            ));
                        }
                        if let Some(server) = self.settings.mcp.selected.as_ref().and_then(|id| {
                            self.settings
                                .mcp
                                .servers
                                .iter()
                                .find(|server| &server.id == id)
                        }) {
                            self.load_mcp_capabilities(server.id.clone(), None);
                        }
                    }
                }
                Err(error) => {
                    self.settings.mcp.loading = false;
                    self.settings.mcp.error = Some(error.describe());
                }
            },
            (Slot::McpContent(scope, _), Reply::Task(_)) if scope != self.mcp_scope_key() => {}
            (Slot::McpContent(_, server), Reply::Task(t)) => {
                if let Ok(server) = Id::new(server) {
                    self.settings.mcp.opened(
                        &server,
                        match t {
                            Ok(TaskOutput::McpContent(text)) => Ok(text),
                            Ok(_) => Err("unexpected reply".into()),
                            Err(error) => Err(error),
                        },
                    );
                }
            }
            (Slot::McpCapabilities(scope, _, _), Reply::Query(_))
                if scope != self.mcp_scope_key() => {}
            (Slot::McpCapabilities(_, server, cursor), Reply::Query(q)) => {
                match ops::mcp_capability_page(q) {
                    Ok((capabilities, next)) => {
                        if let Ok(id) = Id::new(server) {
                            self.settings
                                .mcp
                                .loaded_catalog(&id, capabilities, cursor.is_some());
                            if let Some(next) = next {
                                self.load_mcp_capabilities(id, Some(next));
                            }
                        }
                    }
                    Err(error) => self.settings.mcp.error = Some(error.describe()),
                }
            }
            (Slot::McpCommand, Reply::Task(Ok(TaskOutput::Prepared { frame }))) => {
                if !self.client.as_mut().is_some_and(|client| {
                    client.command(Slot::McpCommand, "Updating MCP", "mcp.configure", frame)
                }) {
                    self.settings.mcp.busy = false;
                    self.settings.mcp.error =
                        Some("MCP configuration could not be submitted.".into());
                }
            }
            (Slot::McpCommand, Reply::Task(Err(error))) => {
                self.settings.mcp.busy = false;
                self.settings.mcp.pending_server = None;
                self.settings.mcp.error = Some(error);
            }
            (Slot::McpRelease, Reply::Task(Err(error))) => {
                self.settings.mcp.error = Some(format!(
                    "The removed MCP token could not be deleted: {error}"
                ));
            }
            (Slot::McpRelease, Reply::Task(Ok(_))) => {}
            (Slot::McpCommand, Reply::Command(c)) => {
                self.settings.mcp.busy = false;
                match ops::mcp_command_status(c) {
                    Ok((message, operation, configured)) => {
                        self.settings.mcp.message = Some(message);
                        self.settings.mcp.error = None;
                        let reconnect = self.settings.mcp.reconnect.take();
                        // A removed server's tokens are no longer referenced once the removal
                        // is saved, whether or not its session has closed yet.
                        if let Some(handles) = self.settings.mcp.removing.take() {
                            if self.settings.mcp.pending_server.as_ref()
                                == self.settings.mcp.selected.as_ref()
                            {
                                self.settings.mcp.selected = None;
                                self.settings.mcp.capabilities.clear();
                            }
                            self.release_mcp_credentials(handles);
                        }
                        if let Some(server) = &configured {
                            self.release_dropped_mcp_credentials(server);
                            self.settings.mcp.saved(server);
                        }
                        if let Some(server) = &configured {
                            self.settings.mcp.selected = Some(server.id.clone());
                        }
                        self.settings.mcp.pending_operation = operation;
                        if self.settings.mcp.pending_operation.is_none() {
                            self.settings.mcp.pending_server = None;
                        }
                        self.load_mcp_servers(None);
                        if let Some(server) =
                            configured.filter(|s| reconnect.as_ref() == Some(&s.id))
                        {
                            self.mcp_action(McpAction::Connect(server.id, server.revision.get()));
                        }
                    }
                    Err(error) => {
                        self.settings.mcp.reconnect = None;
                        self.settings.mcp.removing = None;
                        self.settings.mcp.keep_selection = false;
                        self.settings.mcp.pending_server = None;
                        self.settings.mcp.pending_operation = None;
                        self.settings.mcp.error = Some(error.describe());
                    }
                }
            }
            (Slot::McpOperation(scope), Reply::Query(_)) if scope != self.mcp_scope_key() => {}
            (Slot::McpOperation(_), Reply::Query(q)) => match ops::query_result(q) {
                Ok((
                    devworks_core::protocol::results::OperationResult::HistoryOperationStatus {
                        value,
                    },
                    _,
                )) => {
                    if self
                        .settings
                        .mcp
                        .observed_operation(&value.operation_id, value.state)
                    {
                        self.load_mcp_servers(None);
                    }
                }
                Ok(_) => {
                    self.settings.mcp.error =
                        Some("Unexpected MCP operation status response".into())
                }
                Err(error) => self.settings.mcp.error = Some(error.describe()),
            },
            (Slot::SettingsExplain(key), Reply::Query(q)) => match ops::settings_chain(q) {
                Ok(chain) => {
                    if let Some(v) = &mut self.settings.view {
                        v.apply_explanation(&key, &chain);
                    }
                    self.settings.explained.insert(key);
                }
                Err(e) => self.settings.message = Some((Tone::Bad, e.describe())),
            },
            (Slot::AddonsList, Reply::Query(q)) => match crate::addons::catalog(q) {
                Ok((items, registry, next)) => {
                    let (mut all, first) = self
                        .addons
                        .catalog_pending
                        .take()
                        .unwrap_or_else(|| (Vec::new(), registry));
                    let pages = all.len() / usize::from(crate::ops::PAGE_LIMIT) + 1;
                    if registry != first {
                        // The registry changed between pages: read it again from the start.
                        self.load_addons();
                    } else if let Some(cursor) =
                        next.filter(|_| pages < crate::addons::CATALOG_PAGES)
                    {
                        all.extend(items);
                        self.addons.catalog_pending = Some((all, first));
                        self.query(
                            Slot::AddonsList,
                            "Loading add-ons",
                            "addons.catalog",
                            move |b| crate::addons::catalog_frame(b, Some(&cursor)),
                        );
                    } else {
                        all.extend(items);
                        self.addons.items = all;
                        self.addons.registry_revision = registry;
                        self.addons.loaded = true;
                        self.addons.error = None;
                    }
                }
                Err(e) => {
                    self.addons.catalog_pending = None;
                    self.addons.error = Some(e.describe());
                }
            },
            (Slot::AddonsLogs(id), Reply::Query(q)) => match crate::addons::logs(q) {
                Ok((entries, next)) => {
                    let mut all = self.addons.logs_pending.remove(&id).unwrap_or_default();
                    all.extend(entries);
                    if let Some((p, d)) = self.addons.awaiting.clone()
                        && p == id
                        && let Some(e) = all.iter().find(|e| e.id.as_str() == d)
                    {
                        self.addons.config_errors.insert(p, e.message.0.clone());
                        self.addons.awaiting = None;
                    }
                    // Only the newest entries are kept while the log is followed to its end.
                    if all.len() > crate::addons::LOGS_KEPT {
                        all.drain(..all.len() - crate::addons::LOGS_KEPT);
                    }
                    match next {
                        Some(cursor) => {
                            self.addons.logs_pending.insert(id.clone(), all);
                            self.query(
                                Slot::AddonsLogs(id.clone()),
                                "Loading add-on logs",
                                "addons.logs",
                                move |b| crate::addons::logs_frame(b, &id, Some(&cursor)),
                            );
                        }
                        None => {
                            self.addons.logs.insert(id, all);
                        }
                    }
                }
                Err(e) => {
                    self.addons.logs_pending.remove(&id);
                    self.addons.notice = Some(e.describe());
                }
            },
            (Slot::AddonsCommand, Reply::Command(outcome)) => {
                self.addons.busy = is_unknown(&outcome);
                // A configuration refused by the package's schema names the diagnostic that
                // carries its field errors: fetch it for the form.
                if let Some(package) = crate::addons::configured_package(&outcome) {
                    self.addons.config_errors.remove(&package);
                }
                if let Some((package, diagnostic)) = crate::addons::configuration_refusal(&outcome)
                {
                    self.addons.awaiting = Some((package.clone(), diagnostic));
                    self.load_addon_logs(package);
                }
                self.addons.notice = Some(crate::addons::describe(outcome));
                self.load_addons();
            }
            (Slot::AddonsDiscovery, Reply::Task(t)) => {
                if let Err(e) = t {
                    self.addons.notice = Some(e);
                }
                self.load_addons();
            }
            (
                Slot::AddonsSave {
                    update, registry, ..
                },
                Reply::Task(Ok(TaskOutput::PackagePrepared { frame, download })),
            ) => {
                if let Some(client) = &mut self.client {
                    client.command(
                        Slot::AddonsSave {
                            update,
                            registry,
                            download: Some(download.into_path()),
                        },
                        "Saving the add-on bundle",
                        "artifacts.save",
                        frame,
                    );
                }
            }
            (
                Slot::AddonsSave {
                    update,
                    registry,
                    download,
                },
                Reply::Task(Ok(TaskOutput::Prepared { frame })),
            ) => {
                if let Some(client) = &mut self.client {
                    client.command(
                        Slot::AddonsSave {
                            update,
                            registry,
                            download,
                        },
                        "Saving the add-on bundle",
                        "artifacts.save",
                        frame,
                    );
                }
            }
            (
                Slot::AddonsSave {
                    update,
                    registry,
                    download,
                },
                Reply::Command(outcome),
            ) => {
                let unknown = is_unknown(&outcome);
                if !unknown && let (Some(path), Some(backend)) = (&download, self.backend()) {
                    attachments::remove_owned(path, &backend.scratch_dir().join("packages"));
                }
                match ops::saved_artifact(outcome) {
                    Ok(saved) => {
                        if let Some(backend) = self.backend() {
                            let (op, frame) = crate::addons::install_saved(
                                &*backend,
                                &saved.artifact,
                                update.as_deref(),
                                registry,
                            );
                            if let Some(client) = &mut self.client {
                                client.command(
                                    Slot::AddonsCommand,
                                    "Installing the add-on",
                                    op,
                                    frame,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        self.addons.busy = unknown;
                        self.addons.notice = Some(e.describe());
                    }
                }
            }
            (Slot::AddonsSave { .. }, Reply::Task(t)) => {
                self.addons.busy = false;
                self.addons.notice = Some(match t {
                    Err(e) => e,
                    Ok(_) => "unexpected reply".into(),
                });
            }
            (Slot::KnowledgeList(generation), Reply::Query(q)) => {
                if self.knowledge.listed(generation, q) {
                    self.load_knowledge();
                }
            }
            (Slot::KnowledgeRead(claim), Reply::Query(q)) => self.knowledge.read_reply(&claim, q),
            (Slot::KnowledgeDiagnostics, Reply::Query(q)) => self.knowledge.diagnostics_reply(q),
            (Slot::KnowledgeImportSave, Reply::Task(r)) => match self.knowledge.import_saved(r) {
                knowledge_screen::ImportNext::Store(frame) => {
                    if !self.submit_import_save(frame) {
                        // Not admitted: nothing was stored.
                        self.knowledge.import = Some(knowledge_screen::ImportStage::Failed {
                            name: "the file".into(),
                            error: "DevWorks is offline or closing.".into(),
                        });
                    }
                }
                knowledge_screen::ImportNext::Preview(body) => {
                    let submitted = self.query(
                        Slot::KnowledgeImportPreview,
                        "Previewing an import",
                        "knowledge.diagnostics",
                        move |b| knowledge_screen::query(b, "knowledge.diagnostics", body),
                    );
                    if !submitted {
                        self.knowledge.import = None;
                    }
                }
                knowledge_screen::ImportNext::Nothing => {}
            },
            (Slot::KnowledgeImportPreview, Reply::Query(q)) => self.knowledge.import_previewed(q),
            (Slot::KnowledgeImport, Reply::Command(c)) => {
                if self.knowledge.import_done(c) {
                    self.load_knowledge();
                }
            }
            (Slot::KnowledgePreview(source), Reply::Query(q)) => {
                self.knowledge.preview_reply(&source, q)
            }
            (Slot::KnowledgeWrite, Reply::Command(c)) => {
                if self.knowledge.written(c) {
                    self.load_knowledge();
                    if let Some(id) = self.knowledge.selected.clone() {
                        self.read_knowledge(id);
                    }
                }
            }
            (Slot::EmbeddingSpaces(g), Reply::Query(q)) => {
                let panel = &mut self.settings.connections.embedding_spaces;
                if g != panel.generation {
                    return;
                }
                panel.loading = false;
                match ops::embedding_space_page(q) {
                    Ok(page) => panel.loaded(page),
                    Err(error) => panel.error = Some(error.describe()),
                }
            }
            (Slot::Connections(g), Reply::Query(q)) => {
                if g != self.settings.connections.generation {
                    return;
                }
                match ops::connection_records(q) {
                    Ok((items, next)) => {
                        self.settings.connections.gathering.extend(items);
                        match next {
                            Some(cursor) => {
                                if !self.query(
                                    Slot::Connections(g),
                                    "Loading provider connections",
                                    "router.connections",
                                    move |b| ops::router_connections(b, Some(&cursor)),
                                ) {
                                    self.settings.connections.loading = false;
                                }
                            }
                            None => {
                                let all = std::mem::take(&mut self.settings.connections.gathering);
                                self.settings.connections.loaded(all);
                            }
                        }
                    }
                    Err(e) => {
                        self.settings.connections.loading = false;
                        self.settings.connections.error = Some(e.describe());
                    }
                }
            }
            (Slot::ConnectionCatalog(g), Reply::Query(q)) => {
                if g != self.settings.connections.catalog_generation {
                    return;
                }
                match ops::route_models(q) {
                    Ok((items, next)) => {
                        self.settings.connections.catalog_gathering.extend(items);
                        match next {
                            Some(cursor) => {
                                self.query(
                                    Slot::ConnectionCatalog(g),
                                    "Loading models",
                                    "router.catalog",
                                    move |b| ops::router_catalog_all(b, Some(&cursor)),
                                );
                            }
                            None => {
                                let all = std::mem::take(
                                    &mut self.settings.connections.catalog_gathering,
                                );
                                self.settings.connections.catalog_loaded(all);
                            }
                        }
                    }
                    Err(e) => self.settings.connections.catalog_error = Some(e.describe()),
                }
            }
            (Slot::Configure(n), Reply::Task(Ok(TaskOutput::Prepared { frame }))) => {
                self.settings.connections.prepared_edit(n, &frame);
                let label = self
                    .settings
                    .connections
                    .busy
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, label)| label.clone())
                    .unwrap_or_else(|| "Configuring provider".into());
                // Preparation has no router effects. The client owns the exact frame before
                // admission, including when a close has already stopped new requests.
                if let Some(client) = &mut self.client {
                    client.command(Slot::Configure(n), &label, "router.configure", frame);
                }
            }
            (Slot::Configure(n), Reply::Command(outcome)) => {
                let unknown = is_unknown(&outcome);
                let result = ops::configured(outcome);
                let p = &mut self.settings.connections;
                let label = p
                    .busy
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, label)| label.clone())
                    .unwrap_or_default();
                if !unknown {
                    p.busy.retain(|(k, _)| *k != n);
                }
                p.edit_finished(n, result.is_ok(), unknown);
                p.message = Some(match result {
                    Ok(true) => (Tone::Good, format!("{label}: done.")),
                    Ok(false) => (Tone::Neutral, format!("{label}: nothing changed.")),
                    Err(e) => (Tone::Bad, format!("{label}: {}", e.describe())),
                });
                self.load_connections();
                self.invalidate_routing();
            }
            (Slot::Configure(n), Reply::Task(t)) => {
                // A task failure before preparation has no router effects. A failed command
                // worker, however, leaves its exact frame unresolved in the client.
                let unconfirmed = self
                    .client
                    .as_ref()
                    .is_some_and(|c| c.unconfirmed().iter().any(|u| u.slot == Slot::Configure(n)));
                let p = &mut self.settings.connections;
                p.edit_finished(n, false, unconfirmed);
                let label = p
                    .busy
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, label)| label.clone())
                    .unwrap_or_default();
                if !unconfirmed {
                    p.busy.retain(|(k, _)| *k != n);
                }
                p.message = Some((
                    if unconfirmed {
                        Tone::Attention
                    } else {
                        Tone::Bad
                    },
                    match t {
                        Err(e) => format!(
                            "{label}: {e}{}",
                            if unconfirmed {
                                ". The original change is awaiting confirmation."
                            } else {
                                ""
                            }
                        ),
                        Ok(_) => format!("{label}: unexpected reply."),
                    },
                ));
            }
            (Slot::SettingsSecret(key), Reply::Task(result)) => {
                self.setting_secret_prepared(&key, result)
            }
            (slot, Reply::Task(Err(e))) => {
                self.announce(format!("A request for {slot:?} failed: {e}"));
            }
            (slot, _) => self.announce(format!("Unexpected reply for {slot:?}")),
        }
    }

    fn settings_revision(&self) -> Option<i64> {
        self.settings.view.as_ref().map(|v| v.revision)
    }

    fn explain(&mut self, key: String) {
        self.explained_at
            .insert(key.clone(), self.settings_revision());
        let k = key.clone();
        self.query(
            Slot::SettingsExplain(key),
            "Explaining a setting",
            "settings.explain_source",
            move |b| ops::settings_explain(b, &k),
        );
    }

    fn check_settings_health(&mut self, revision: i64) {
        if let Some(b) = self.backend() {
            self.settings.set_health(KEEP_AWAKE, b.keep_awake_health());
        }
        let dirs = self.settings.directories();
        if dirs.is_empty() {
            return;
        }
        self.query(
            Slot::SettingsHealth,
            "Checking directories",
            "settings.validate",
            move |b| {
                let entries: Vec<(&str, serde_json::Value)> =
                    dirs.iter().map(|(k, v)| (*k, v.clone())).collect();
                ops::settings_validate_values(b, revision, &entries)
            },
        );
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
    pub fn hint_refresh_due_at(&self) -> Option<Instant> {
        self.hint_refresh_due
            .then(|| self.last_poll + HINT_REFRESH_INTERVAL)
    }

    fn refresh_visible(&mut self) {
        self.last_poll = Instant::now();
        if self.online().is_err() {
            return;
        }
        self.with_typed(|c: &mut Chat, cx| c.refresh_conversation(cx));
        self.load_completion_notifications();
        if self.route == Route::Approvals
            && !self.pending(&Slot::ApprovalsList)
            && !self.pending(&Slot::ReviewQueue)
        {
            self.load_approvals();
        }
        if self.notifications_open && !self.pending(&Slot::NotificationsList) {
            self.load_notification_history();
        }
        // Background discovery commits independently of the manual refresh command. Read the
        // completed inventory on normal hints/periodic repair, including while Settings is open.
        // Loading keeps unsaved model edits and leaves unknown configure outcomes untouched.
        if self.route == Route::Settings
            && self.settings.show_connections
            && !self.settings.connections.loading
        {
            self.load_connections();
        }
        let route = self.route;
        if self.with_screen(route.screen().owner, |s, cx| s.refresh(cx, route)) {
            return;
        }
        match self.route {
            // Only an unpaged listing is re-read in place: a re-read of a list the user paged
            // through would drop the pages they loaded.
            Route::Knowledge
                if self
                    .routes
                    .get(&Route::Knowledge)
                    .is_none_or(|r| r.tab != 2)
                    && self.knowledge.loaded
                    && self.knowledge.listing.is_none()
                    && self.knowledge.items.len() <= knowledge_screen::PAGE as usize =>
            {
                self.load_knowledge();
            }
            // Run status moves while the screen is open (claims, runs, cancellations).
            Route::Jobs if !self.pending(&Slot::JobsList) && self.jobs.list_refresh.is_none() => {
                self.load_jobs();
            }
            // Changes an agent made reach the work screens (the lists are read in full again).
            Route::Overview | Route::Features | Route::Issues if !self.work_loading() => {
                self.load_work();
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------------------------
    // Actions

    fn settings_actions(&mut self, actions: Vec<SettingsAction>) {
        for a in actions {
            match a {
                SettingsAction::Reload => self.load_settings(),
                SettingsAction::RefreshContainers => self.refresh_container_capabilities(),
                SettingsAction::Validate => {
                    let Some(b) = self.backend() else { continue };
                    let Some(frame) = self
                        .settings
                        .view
                        .as_ref()
                        .and_then(|v| ops::settings_submit(&*b, v, true))
                    else {
                        continue;
                    };
                    self.settings.validating = true;
                    self.validated_drafts = Some(self.settings.drafts());
                    if let Some(c) = &mut self.client {
                        c.query(
                            Slot::SettingsValidate,
                            "Validating settings",
                            "settings.validate",
                            frame,
                        );
                    }
                }
                SettingsAction::Save => {
                    let Some(b) = self.backend() else { continue };
                    let Some(frame) = self
                        .settings
                        .view
                        .as_ref()
                        .and_then(|v| ops::settings_submit(&*b, v, false))
                    else {
                        continue;
                    };
                    self.settings.saving = true;
                    self.settings.submitted = self
                        .settings
                        .view
                        .as_ref()
                        .map(|v| {
                            v.dirty_keys()
                                .into_iter()
                                .filter_map(|k| Some((k.to_owned(), v.item(k)?.draft.clone()?)))
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(c) = &mut self.client {
                        c.command(
                            Slot::SettingsSave,
                            "Saving settings",
                            "settings.save_revision",
                            frame,
                        );
                    }
                }
                SettingsAction::StoreSecret { key, value } => {
                    self.store_setting_secret(&key, value)
                }
                SettingsAction::Explain(key) => self.explain(key),
                SettingsAction::ChooseDirectory(key) => {
                    let start = self
                        .settings
                        .view
                        .as_ref()
                        .and_then(|v| v.item(&key))
                        .and_then(|i| i.shown().as_str().map(str::to_owned));
                    if let Some(p) = self.dialogs.pick_folder(start.as_deref()) {
                        self.settings.choose(&key, p.display().to_string());
                    }
                }
                SettingsAction::OpenAddOns => self.navigate(Route::AddOns),
                SettingsAction::OpenJobs => self.navigate(Route::Jobs),
                SettingsAction::OpenNotifications => {
                    self.notifications_open = true;
                }
                SettingsAction::OpenConnections => self.load_connections(),
                SettingsAction::OpenHelp(topic) => self.open_help_context(&topic),
                SettingsAction::Connection(a) => self.connection_action(a),
                SettingsAction::Web(action) => self.web_action(action),
                SettingsAction::Instructions(action) => self.with_typed(
                    |p: &mut crate::instructions_screen::InstructionsPanel, cx| p.act(cx, action),
                ),
                SettingsAction::Mcp(action) => self.mcp_action(action),
            }
        }
    }

    fn mcp_scope_key(&self) -> String {
        self.settings.mcp.project.as_ref().map_or_else(
            || "workspace".to_owned(),
            |project| format!("project:{}", project.0.as_str()),
        )
    }

    fn load_mcp_servers(&mut self, cursor: Option<String>) {
        if cursor.is_none() {
            self.settings.mcp.loading = true;
        }
        let next = cursor.clone();
        let scope = self.settings.mcp.project.clone();
        let scope_key = self.mcp_scope_key();
        if !self.query(
            Slot::McpServers(scope_key, cursor),
            "Loading MCP servers",
            "mcp.servers",
            move |backend| ops::mcp_servers(backend, scope.as_ref(), next.as_deref()),
        ) {
            self.settings.mcp.loading = false;
        }
    }

    fn load_mcp_capabilities(&mut self, server_id: Id, cursor: Option<String>) {
        let Some(server) = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|server| server.id == server_id)
        else {
            return;
        };
        let revision = server.catalog_revision.get();
        let scope = self.settings.mcp.project.clone();
        let slot = Slot::McpCapabilities(
            self.mcp_scope_key(),
            server_id.as_str().to_owned(),
            cursor.clone(),
        );
        self.query(
            slot,
            "Loading MCP capabilities",
            "mcp.capabilities",
            move |backend| {
                ops::mcp_capabilities(
                    backend,
                    scope.as_ref(),
                    &server_id,
                    revision,
                    cursor.as_deref(),
                )
            },
        );
    }

    /// A removed or replaced bearer token leaves its stored secret unreferenced once the new
    /// configuration is saved; delete it then.
    fn release_dropped_mcp_credentials(
        &mut self,
        saved: &devworks_core::protocol::records::McpServer,
    ) {
        let Some(old) = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|server| server.id == saved.id)
        else {
            return;
        };
        let kept = crate::mcp_panel::credential_handles(saved);
        let dropped: Vec<Id> = crate::mcp_panel::credential_handles(old)
            .into_iter()
            .filter(|handle| !kept.contains(handle))
            .collect();
        self.release_mcp_credentials(dropped);
    }

    /// Delete the stored secrets behind MCP handles no saved server references any more.
    fn release_mcp_credentials(&mut self, dropped: Vec<Id>) {
        if dropped.is_empty() {
            return;
        }
        if let Some(client) = self.client.as_mut() {
            client.task(
                Slot::McpRelease,
                "Removing MCP credential",
                move |backend| {
                    backend.release_mcp_credentials(dropped)?;
                    // Nothing to submit: the release reply only reports a failure.
                    Ok(TaskOutput::Prepared { frame: Vec::new() })
                },
            );
        }
    }

    fn mcp_action(&mut self, action: McpAction) {
        if let McpAction::Sampling(id, ..) = &action {
            self.settings.mcp.reconnect = Some(id.clone());
        }
        match action {
            McpAction::Reload => self.load_mcp_servers(None),
            McpAction::Catalog(id, _revision) => self.load_mcp_capabilities(id, None),
            McpAction::Open(id, catalog_revision, request) => {
                let server = id.clone();
                let scope = self.settings.mcp.project.clone();
                let slot = Slot::McpContent(self.mcp_scope_key(), id.as_str().to_owned());
                let submitted = self.online().is_ok()
                    && self.client.as_mut().is_some_and(|client| {
                        client.network_task(slot, "Reading MCP content", move |backend| {
                            ops::mcp_content(
                                backend,
                                scope.as_ref(),
                                &id,
                                catalog_revision,
                                &request,
                            )
                            .map(TaskOutput::McpContent)
                        })
                    });
                if !submitted {
                    self.settings
                        .mcp
                        .opened(&server, Err("the app is offline or closing".into()));
                }
            }
            other => {
                let (operation, body, secret, pending_server) = match other {
                    McpAction::Configure { body, secret } => ("mcp.configure", body, secret, None),
                    McpAction::Connect(id, revision) => (
                        "mcp.connect",
                        serde_json::json!({"scope":ops::mcp_scope(self.settings.mcp.project.as_ref()),"serverId":id,"expectedRevision":revision.to_string()}),
                        None,
                        Some(id),
                    ),
                    McpAction::Disable(id, revision) => (
                        "mcp.disable",
                        serde_json::json!({"scope":ops::mcp_scope(self.settings.mcp.project.as_ref()),"serverId":id,"expectedRevision":revision.to_string()}),
                        None,
                        Some(id),
                    ),
                    McpAction::Remove(id, revision, handles) => {
                        self.settings.mcp.removing = Some(handles);
                        (
                            "mcp.remove",
                            serde_json::json!({"scope":ops::mcp_scope(self.settings.mcp.project.as_ref()),"serverId":id,"expectedRevision":revision.to_string()}),
                            None,
                            Some(id),
                        )
                    }
                    McpAction::Discover(id, revision) => (
                        "mcp.discover",
                        serde_json::json!({"scope":ops::mcp_scope(self.settings.mcp.project.as_ref()),"serverId":id,"expectedCatalogRevision":revision.to_string()}),
                        None,
                        Some(id),
                    ),
                    McpAction::Select(id, revision, catalog, tools)
                    | McpAction::Sampling(id, revision, catalog, tools) => (
                        "mcp.select_tools",
                        serde_json::json!({"scope":ops::mcp_scope(self.settings.mcp.project.as_ref()),"serverId":id,"expectedRevision":revision.to_string(),"expectedCatalogRevision":catalog.to_string(),"toolIds":tools}),
                        None,
                        None,
                    ),
                    McpAction::Classify(id, revision, tool, read_only) => {
                        let mut body = serde_json::json!({"scope":ops::mcp_scope(self.settings.mcp.project.as_ref()),"serverId":id,"expectedRevision":revision.to_string(),"toolId":tool});
                        if let Some(read_only) = read_only {
                            body["readOnly"] = serde_json::json!(read_only);
                        }
                        ("mcp.classify_tool", body, None, None)
                    }
                    McpAction::Reload | McpAction::Catalog(_, _) | McpAction::Open(..) => {
                        unreachable!()
                    }
                };
                let submitted = if let Some(secret) = secret {
                    self.online().is_ok()
                        && self.client.as_mut().is_some_and(|client| {
                            client.task(
                                Slot::McpCommand,
                                "Storing MCP credential",
                                move |backend| {
                                    let handle = backend.stage_mcp_api_key(secret)?;
                                    let mut body = body;
                                    body["transport"]["credentialRef"] = serde_json::json!(handle);
                                    Ok(TaskOutput::Prepared {
                                        frame: ops::mcp_command(backend, operation, body),
                                    })
                                },
                            )
                        })
                } else {
                    self.command(
                        Slot::McpCommand,
                        "Updating MCP",
                        operation,
                        move |backend| ops::mcp_command(backend, operation, body),
                    )
                };
                if !submitted {
                    self.settings.mcp.busy = false;
                    self.settings.mcp.reconnect = None;
                    self.settings.mcp.removing = None;
                    self.settings.mcp.error = Some("MCP command could not be submitted.".into());
                } else {
                    self.settings.mcp.pending_server = pending_server;
                    self.settings.mcp.pending_operation = None;
                    self.settings.mcp.last_operation_poll = None;
                }
            }
        }
    }

    /// Runs a web fetch, search or activity load through the same use cases agents call.
    fn web_action(&mut self, action: WebAction) {
        match action {
            WebAction::Fetch(url) => {
                if !self.query(Slot::WebFetch, "Fetching a page", "web.fetch", |b| {
                    ops::web_fetch(b, &url)
                }) {
                    self.settings.web.fetching = false;
                }
            }
            WebAction::Search { query, provider } => {
                if !self.query(Slot::WebSearch, "Searching the web", "web.search", |b| {
                    ops::web_search(b, &query, &provider)
                }) {
                    self.settings.web.searching = false;
                }
            }
            WebAction::LoadActivity => {
                if !self.query(
                    Slot::WebActivity,
                    "Loading web activity",
                    "workspace.logs",
                    ops::web_activity,
                ) {
                    self.settings.web.activity_loading = false;
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Settings → Provider connections (DW-18)

    /// Loads every connection and the whole catalog, page by page, as new loads.
    fn load_connections(&mut self) {
        if let Some(b) = self.backend() {
            self.settings.connections.secret_store = Some(b.secret_store());
        }
        let p = &mut self.settings.connections;
        p.generation += 1;
        p.gathering.clear();
        p.loading = true;
        p.last_load = Some(Instant::now());
        p.catalog_generation += 1;
        p.catalog_gathering.clear();
        let (g, cg) = (p.generation, p.catalog_generation);
        if !self.query(
            Slot::Connections(g),
            "Loading provider connections",
            "router.connections",
            |b| ops::router_connections(b, None),
        ) {
            self.settings.connections.loading = false;
        }
        self.query(
            Slot::ConnectionCatalog(cg),
            "Loading models",
            "router.catalog",
            |b| ops::router_catalog_all(b, None),
        );
        self.load_embedding_spaces(None);
    }

    pub(crate) fn open_embedding_setup(&mut self) {
        self.navigate(Route::Settings);
        self.settings.show_connections = true;
        self.settings.connections.embedding_spaces.open = true;
        self.load_connections();
    }
    fn load_embedding_spaces(&mut self, cursor: Option<String>) {
        let panel = &mut self.settings.connections.embedding_spaces;
        panel.generation += 1;
        panel.loading = true;
        panel.last_load = Some(Instant::now());
        let generation = panel.generation;
        if !self.query(
            Slot::EmbeddingSpaces(generation),
            "Loading embedding spaces",
            "router.embedding_spaces",
            move |b| ops::router_embedding_spaces(b, cursor.as_deref()),
        ) {
            self.settings.connections.embedding_spaces.loading = false;
        }
    }

    fn connection_action(&mut self, action: crate::connections_screen::ConnectionAction) {
        use crate::connections_screen::ConnectionAction as A;
        let saving_editor = matches!(&action, A::ConfigureEdit { .. });
        match action {
            A::Reload => self.load_connections(),
            A::EmbeddingSetup => self.open_embedding_setup(),
            A::EmbeddingPrice(model) => {
                self.usage.price_models = model;
                self.usage.price_providers.clear();
                self.usage.price_snapshots.clear();
                self.usage.history = false;
                self.routes.entry(Route::Usage).or_default().tab = 2;
                self.navigate(Route::Usage);
            }
            A::EmbeddingSpaces { cursor } => self.load_embedding_spaces(cursor),
            A::UseSessionOnly => {
                if let Some(b) = self.backend() {
                    match b.use_session_only_secrets() {
                        Ok(()) => {
                            self.settings.connections.message = Some((
                                Tone::Attention,
                                "Credentials are kept in memory for this session only; sign in \
                                 again after a restart."
                                    .into(),
                            ));
                            self.settings.connections.secret_store = Some(b.secret_store());
                        }
                        Err(e) => self.settings.connections.message = Some((Tone::Bad, e)),
                    }
                }
            }
            A::Configure {
                label,
                action,
                key,
                code,
            }
            | A::ConfigureEdit {
                label,
                action,
                key,
                code,
            } => {
                if self.online().is_err() {
                    self.settings.connections.message =
                        Some((Tone::Bad, "DevWorks is offline or closing.".into()));
                    return;
                }
                let p = &mut self.settings.connections;
                p.sequence += 1;
                let n = p.sequence;
                if saving_editor {
                    p.preparing_edit(n, &action);
                }
                let submitted = self.client.as_mut().is_some_and(|c| {
                    c.task(Slot::Configure(n), &label, move |b| {
                        let mut action = action;
                        // The key goes to the OS secret store first; only its handle travels.
                        if let Some(key) = key {
                            let handle = b
                                .stage_api_key(key)
                                .map_err(|e| format!("the secret store refused the key: {e}"))?;
                            action["credentialRef"] =
                                serde_json::Value::String(handle.as_str().to_owned());
                        }
                        if let Some((flow, pasted)) = code {
                            b.submit_sign_in_code(&flow, pasted)?;
                        }
                        Ok(TaskOutput::Prepared {
                            frame: ops::router_configure(b, action),
                        })
                    })
                });
                if submitted {
                    self.settings.connections.busy.push((n, label));
                } else {
                    self.settings.connections.edit_finished(n, false, false);
                    self.settings.connections.message =
                        Some((Tone::Bad, "The change was not submitted.".into()));
                }
            }
        }
    }

    /// Reloads the connections panel while a sign-in is open (a device code finishes in the
    /// background), at most every few seconds.
    fn poll_connections(&mut self, ctx: &egui::Context) {
        let p = &self.settings.connections;
        let signing_in = p.records.iter().any(|r| {
            r.lifecycle.as_ref().is_some_and(|l| {
                matches!(
                    l,
                    ConnectionLifecycle::Authorizing | ConnectionLifecycle::Refreshing
                )
            })
        });
        let qualifying = p.embedding_spaces.qualifying();
        if !self.settings.show_connections || (!signing_in && !qualifying) {
            return;
        }
        let due = !p.loading && p.last_load.is_none_or(|t| t.elapsed() >= CONNECTION_POLL);
        if due {
            self.load_connections();
        }
        ctx.request_repaint_after(CONNECTION_POLL);
    }

    fn poll_mcp(&mut self, ctx: &egui::Context) {
        let Some(operation) = self.settings.mcp.pending_operation.clone() else {
            return;
        };
        let interval = Duration::from_secs(2);
        let slot = Slot::McpOperation(self.mcp_scope_key());
        if !self.pending(&slot)
            && self
                .settings
                .mcp
                .last_operation_poll
                .is_none_or(|last| last.elapsed() >= interval)
        {
            self.settings.mcp.last_operation_poll = Some(Instant::now());
            self.query(slot, "Checking MCP operation", "history.operation_status", move |backend| {
                serde_json::json!({"protocol":ops::PROTOCOL,"queryId":backend.token("q"),"query":{"kind":"history.operation_status","operationId":operation}}).to_string().into_bytes()
            });
        }
        ctx.request_repaint_after(interval);
    }

    // -----------------------------------------------------------------------------------------
    // Lifecycle

    /// Begins the close lifecycle (the window's close button lands here).
    pub fn request_close(&mut self) {
        if self.lifecycle == Lifecycle::Open {
            self.conventions.cancel_queries();
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
        self.conventions.cancel_queries();
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
            purge_reviews: self.purge_reviews.clone(),
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
        // The conversation keeps the named fields its state had before it had its own screen.
        let chat: ChatPendingMemory = screens
            .remove(ScreenId::Chat.key())
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
            knowledge_import: self
                .knowledge
                .import_frame(false)
                .map(|(name, frame)| crate::memory::UnconfirmedImport { name, frame }),
            plans: self
                .work
                .plan_saving
                .values()
                .filter_map(|f| serde_json::to_value(f).ok())
                .collect(),
            commands,
            screens,
        }
    }

    /// Puts back what [`Self::pending_memory`] kept; the first frame confirms it.
    fn restore_pending(&mut self, mut pending: crate::memory::PendingMemory) {
        // Conventions, Databases and Chat keep the named fields their state had before the
        // per-screen map, so earlier files read back unchanged.
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
        if let Some(i) = &pending.knowledge_import {
            self.knowledge.import = Some(knowledge_screen::ImportStage::Unconfirmed {
                name: i.name.clone(),
                frame: i.frame.clone(),
            });
        }
        for screen in self.screens_mut() {
            if let Some(saved) = pending.screens.get(screen.id().key()) {
                screen.restore_pending(saved);
            }
        }
        let mut confirming: Vec<String> = Vec::new();
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
            match &slot {
                Slot::WorkPlan(id) => confirming.push(id.clone()),
                Slot::AddonsSave { .. } | Slot::AddonsCommand => self.addons.busy = true,
                Slot::Configure(n) => {
                    let p = &mut self.settings.connections;
                    p.sequence = p.sequence.max(*n);
                    p.restore_edit(*n, u.frame());
                    p.busy.push((*n, u.label.clone()));
                }
                Slot::SettingsSave => {
                    self.settings.saving = true;
                    self.settings.unconfirmed = true;
                }
                // Knowledge changes stay blocked until the earlier write's outcome is known.
                Slot::KnowledgeWrite => {
                    self.knowledge.writing = Some(u.label.clone());
                }
                _ => {}
            }
            if let Some(client) = &mut self.client {
                client.keep_unresolved(u);
            }
        }
        let plans = pending
            .plans
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();
        self.restore_plan_saves(plans, &confirming);
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
            self.load_completion_notifications();
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
        self.overlays(&ctx);
        self.purge_overlay(&ctx);
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
                self.knowledge_actions(vec![KnowledgeAction::ImportFiles(dropped)]);
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
                            Some(self.notifications_open),
                        )
                        .clicked()
                        {
                            self.notifications_open = !self.notifications_open;
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
                            let screen = if self.route == Route::Repositories && self.graph.opened {
                                "code-exploration"
                            } else if self.route == Route::Settings {
                                self.settings.help_topic()
                            } else if self.route == Route::Knowledge
                                && self
                                    .routes
                                    .get(&Route::Knowledge)
                                    .is_some_and(|r| r.tab == 2)
                            {
                                "knowledge-conventions"
                            } else {
                                self.route.screen().id
                            };
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
            + self.chat.unconfirmed_count()
            + usize::from(self.knowledge.import_frame(true).is_some())
    }

    /// Resubmits every unconfirmed request with its identical frame (same request key).
    pub fn confirm_outcomes(&mut self) {
        if let Some(c) = &mut self.client {
            c.resubmit_unresolved();
        }
        // The resubmitted save runs again (and says so) until its outcome arrives.
        self.settings.unconfirmed = false;
        self.with_typed(|c: &mut Chat, cx| c.confirm_attachments(cx));
        // A Knowledge import file's save: its identical frame (not submitted while closing: it
        // stays unconfirmed, its frame kept).
        if let Some((name, frame)) = self.knowledge.import_frame(true)
            && self.submit_import_save(frame.clone())
        {
            self.knowledge.import = Some(knowledge_screen::ImportStage::Storing { name, frame });
        }
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
        if self.with_screen(self.shown_screen(), |s, cx| s.ui(cx, route, ui)) {
            return;
        }
        match self.route {
            Route::Settings => {
                let online = self.online();
                let mut actions = Vec::new();
                self.settings.mcp.projects = self.projects.list.items.clone();
                // Repository indexing sources sit with the repository paths (General).
                let mut index_actions = Vec::new();
                let up = online.is_ok();
                let indexing = &mut self.projects.indexing;
                let load_sources = indexing.sources.is_none() && indexing.sources_error.is_none();
                let mut general = |ui: &mut Ui| {
                    ui.add_space(8.0);
                    indexing::sources_panel(ui, indexing, up, &mut index_actions);
                    if widgets::secondary_button(ui, "Open indexing runs").clicked() {
                        index_actions.push(indexing::IndexAction::Runs);
                    }
                };
                self.settings.ui(
                    ui,
                    online.as_ref().map(|_| ()).map_err(String::as_str),
                    &mut actions,
                    &mut general,
                );
                self.settings_actions(actions);
                if index_actions
                    .iter()
                    .any(|a| matches!(a, indexing::IndexAction::Runs))
                {
                    self.route = Route::Indexing;
                }
                if load_sources && online.is_ok() {
                    index_actions.insert(0, indexing::IndexAction::LoadSources);
                }
                self.with_typed(|s: &mut indexing::IndexingScreen, cx| s.act(cx, index_actions));
                let ctx = ui.ctx().clone();
                self.poll_connections(&ctx);
                self.poll_mcp(&ctx);
            }
            Route::AddOns => {
                let online = self.online();
                let mut actions = Vec::new();
                self.addons.ui(
                    ui,
                    online.as_ref().map(|_| ()).map_err(String::as_str),
                    &mut actions,
                );
                self.addons_actions(actions);
            }
            Route::Knowledge => {
                let online = self.online();
                let mut actions = Vec::new();
                self.sync_knowledge_scope(&mut actions);
                // The selected subview is route memory, kept across navigation and restarts.
                self.knowledge.tab = self.routes.entry(Route::Knowledge).or_default().tab;
                self.knowledge.ui(
                    ui,
                    online.as_ref().map(|_| ()).map_err(String::as_str),
                    &mut actions,
                );
                self.routes.entry(Route::Knowledge).or_default().tab = self.knowledge.tab;
                self.knowledge_actions(actions);
            }
            Route::Approvals => self.approvals_screen(ui),
            Route::Jobs => self.jobs_screen(ui),
            Route::Overview => self.overview_screen(ui),
            Route::Features => self.features_screen(ui),
            Route::Issues => self.issues_screen(ui),
            _ => self.pending_screen(ui),
        }
    }

    /// A screen whose content belongs to a later item: the full shell, integrated header,
    /// subviews, summary strip and inner sidebar, with a designed unavailable state.
    fn pending_screen(&mut self, ui: &mut Ui) {
        let route = self.route;
        let screen = route.screen();
        let online = self.online();
        let mut link = false;
        let tab = self.routes.entry(route).or_default().tab;
        let primary = |ui: &mut Ui, link: &mut bool| -> usize {
            widgets::pane(
                ui,
                Header {
                    title: screen.title,
                    summary: screen.summary,
                    tabs: screen.subviews,
                    tab,
                },
                link,
                |ui, link| {
                    if let Some((label, _)) = screen.link
                        && widgets::secondary_button(ui, label).clicked()
                    {
                        *link = true;
                    }
                    if route == Route::Approvals {
                        ui.add_enabled(false, egui::Button::new("All projects"))
                            .on_disabled_hover_text(
                                "Project filtering applies once projects exist (DW-11).",
                            );
                    }
                },
                |ui, _, tab| {
                    if !screen.summary_strip.is_empty() {
                        let items: Vec<(&str, Option<String>)> =
                            screen.summary_strip.iter().map(|l| (*l, None)).collect();
                        let owner = screen.pending.map_or("", |p| p.owner);
                        widgets::summary_strip(ui, &items, &format!("not available until {owner}"));
                        ui.add_space(10.0);
                    }
                    let state = match (&online, screen.pending) {
                        (Err(reason), _) => ViewState::Offline {
                            reason: reason.clone(),
                        },
                        (Ok(()), Some(p)) => ViewState::Unavailable {
                            what: match screen.subviews.get(tab) {
                                Some(s) => format!("{s}: {}", p.what),
                                None => p.what.to_owned(),
                            },
                            owner: p.owner.to_owned(),
                        },
                        (Ok(()), None) => ViewState::Empty {
                            title: "Nothing here yet".into(),
                            body: String::new(),
                        },
                    };
                    widgets::state_block(ui, &state);
                },
            )
            .1
        };
        let new_tab = match screen.inner_sidebar {
            Some(title) => {
                let mut st = (&mut link, tab);
                widgets::with_right_sidebar(
                    ui,
                    theme::SIDEBAR,
                    &mut st,
                    |ui, st| st.1 = primary(ui, st.0),
                    |ui, _| {
                        widgets::secondary_pane(ui, title, |ui| {
                            widgets::state_block(
                                ui,
                                &ViewState::Empty {
                                    title: "Nothing selected".into(),
                                    body: "Select an item to see its detail here.".into(),
                                },
                            );
                        });
                    },
                );
                st.1
            }
            None => primary(ui, &mut link),
        };
        self.routes.entry(route).or_default().tab = new_tab;
        if link && let Some((_, target)) = screen.link {
            self.navigate(target);
        }
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        let card_actions = self.completions.ui(ctx, TOOLBAR + 8.0);
        for a in card_actions {
            self.completion_action(ctx, a);
        }
        if let Some(text) = self.completions.take_announcement() {
            self.announce(text);
        }
        self.notification_center(ctx);
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

/// Texts of a bound list's states.
pub(crate) struct ListText<'a> {
    pub what: &'a str,
    pub empty_title: &'a str,
    pub empty_body: &'a str,
    pub id_salt: &'a str,
}

/// The body of a list bound to one query: offline, failed, loading, empty and filtered states,
/// then the rows (virtualized) with the remembered selection.
#[allow(clippy::too_many_arguments)]
pub(crate) fn list_body<T>(
    ui: &mut Ui,
    online: &Result<(), String>,
    list: &ListState<T>,
    text: ListText<'_>,
    reload: &mut bool,
    scrolls: &mut Scrolls,
    haystack: impl Fn(&T) -> String,
    key: impl Fn(&T) -> String,
    filter: &str,
    selection: &mut Option<String>,
    mut row: impl FnMut(&mut Ui, &T, bool) -> bool,
) {
    let what = text.what;
    if let Err(reason) = online {
        widgets::state_block(
            ui,
            &ViewState::Offline {
                reason: reason.clone(),
            },
        );
        return;
    }
    if let Some(e) = &list.error {
        let state = if list.loaded {
            ViewState::Stale { what: what.into() }
        } else {
            ViewState::Failed {
                what: what.into(),
                error: e.clone(),
            }
        };
        if widgets::state_block(ui, &state) == widgets::StateAction::Retry {
            *reload = true;
        }
        if !list.loaded {
            return;
        }
    }
    if !list.loaded {
        widgets::state_block(ui, &ViewState::Loading { what: what.into() });
        return;
    }
    let terms: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    let rows: Vec<&T> = list
        .items
        .iter()
        .filter(|i| {
            let t = haystack(i).to_lowercase();
            terms.iter().all(|w| t.contains(w))
        })
        .collect();
    if rows.is_empty() {
        let none = list.items.is_empty();
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: if none {
                    text.empty_title.into()
                } else {
                    "Nothing matches the filter".into()
                },
                body: if none {
                    text.empty_body.into()
                } else {
                    "Clear or change the filter.".into()
                },
            },
        );
        return;
    }
    widgets::caption(
        ui,
        format!(
            "{} of {} shown{}",
            rows.len(),
            list.items.len(),
            if list.more { " · more exist" } else { "" }
        ),
    );
    if list.more {
        widgets::state_block(
            ui,
            &ViewState::Partial {
                what: what.into(),
                missing: "entries beyond the first page".into(),
            },
        );
    }
    let selected = selection.clone();
    let mut picked = None;
    virtual_list::rows(ui, scrolls, text.id_salt, 28.0, rows.len(), |ui, i| {
        let item = rows[i];
        let k = key(item);
        if row(ui, item, selected.as_deref() == Some(k.as_str())) {
            picked = Some(k);
        }
    });
    if let Some(p) = picked {
        *selection = Some(p);
    }
}
