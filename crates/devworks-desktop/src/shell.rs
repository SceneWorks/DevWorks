//! The shell: navigation, the integrated page layout, the shared conversation and its
//! app-level sidebar, Settings, the completion overlay, status and the close lifecycle. It holds
//! view state only; every record comes through [`Client`] from the runtime's gateway.
use crate::attachments;
use crate::backend::{Backend, Client, Reply, Slot, TaskOutput, Unresolved, is_unknown};
use crate::chat::{self, AttachState, Attachment, Chat, ChatAction, StartReason, Surface};
use crate::knowledge_screen::{self, KnowledgeAction, KnowledgeScreen};
use crate::memory::{Memory, RouteMemory};
use crate::notifications::{CardAction, Completions};
use crate::ops::{self, Failure};
use crate::route::{NAV_ORDER, NavGroup, Route};
use crate::settings_screen::{KEEP_AWAKE, SettingsAction, SettingsScreen};
use crate::theme;
use crate::virtual_list::{self, Scrolls};
use crate::web_panel::WebAction;
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    ArtifactRecord, BrowserSession, ConnectionLifecycle, ItemState, ResourceRecord,
};
use devworks_core::protocol::{DomainErrorCode, Id};
use devworks_core::settings::validate::codes;
use devworks_core::settings::view::Health;
use eframe::egui::{
    self, Align, Event, Frame, Layout, Panel, RichText, Ui, ViewportCommand, accesskit,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

mod conventions;
mod databases;
pub mod decisions;
pub mod graph;
pub mod help;
pub mod indexing;
pub mod jobs;
mod live_text;
mod project_purge;
pub mod projects;
mod settings_secrets;
pub mod usage;
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
    coordinator_open: bool,
    /// Last screen context attempted for one selected conversation while the sidebar is open.
    last_coordinator_notice: Option<(String, String)>,
    routes: BTreeMap<Route, RouteMemory>,
    pub chat: Chat,
    pub settings: SettingsScreen,
    pub help: crate::help_screen::HelpScreen,
    pub knowledge: KnowledgeScreen,
    pub conventions: crate::conventions_screen::ConventionsScreen,
    pub artifacts: ListState<ArtifactRecord>,
    pub resources: ListState<ResourceRecord>,
    /// Every listed browser session: the workspace's, then each listed project's.
    pub browser: ListState<BrowserSession>,
    /// The browser sessions per scope (`None`: the workspace), combined into `browser`.
    browser_parts: BTreeMap<Option<String>, ListState<BrowserSession>>,
    pub addons: crate::addons::AddonsScreen,
    /// All projects, the project selector and Repositories (DW-11).
    pub projects: projects::ProjectsUi,
    pub purge: project_purge::PurgeUi,
    pub jobs: jobs::JobsUi,
    /// Features, the board, Issues and Overview of the selected project (DW-12).
    pub work: work::WorkUi,
    pub usage: crate::usage_screen::UsageScreen,
    pub graph: crate::graph_screen::GraphScreen,
    pub databases: crate::databases_screen::DatabasesScreen,
    /// The project the navigation selected; its screens follow it.
    selected_project: Option<String>,
    pub completions: Completions,
    pub decisions: decisions::DecisionsUi,
    scrolls: Scrolls,
    lifecycle: Lifecycle,
    status: Option<String>,
    hints: Option<watch::Receiver<u64>>,
    started: bool,
    saved: Option<Memory>,
    purge_reviews: BTreeMap<String, serde_json::Value>,
    last_save: Instant,
    last_poll: Instant,
    last_preview_poll: Option<Instant>,
    preview_target: Option<String>,
    preview_generation: u64,
    restored_selection: Option<String>,
    /// Applied session mutations (start, message) and artifact mutations (save, delete). A
    /// list query started before the latest mutation may miss it: its reply is dropped and
    /// the list re-queried, so it never overwrites newer records.
    session_mutations: u64,
    artifact_mutations: u64,
    sessions_list_at: u64,
    artifacts_list_at: u64,
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
        let mut chat = Chat::new();
        chat.tab = memory.sidebar_tab;
        chat.history_query = memory.history_query.clone();
        for (key, d) in &memory.drafts {
            let mut draft = chat::Draft {
                text: d.text.clone(),
                attachments: Vec::new(),
            };
            for a in &d.attachments {
                let Ok(r) = serde_json::from_value::<ArtifactRef>(a.artifact.clone()) else {
                    continue;
                };
                let local = chat.next_local();
                draft.attachments.push(Attachment {
                    local,
                    name: a.name.clone(),
                    media_type: a.media_type.clone(),
                    path: None,
                    owned: false,
                    state: AttachState::Stored(r),
                    preview: None,
                    preview_note: None,
                });
            }
            // Attachments that never started storing: stored once their conversation is known.
            for a in &d.uploads {
                let local = chat.next_local();
                draft.attachments.push(Attachment {
                    local,
                    name: a.name.clone(),
                    media_type: a.media_type.clone(),
                    path: Some(a.path.clone()),
                    owned: a.owned,
                    state: AttachState::WaitingForConversation,
                    preview: None,
                    preview_note: None,
                });
            }
            // Saves whose outcome was unknown at close: confirmed with their exact frames.
            for a in &d.unconfirmed {
                let local = chat.next_local();
                draft.attachments.push(Attachment {
                    local,
                    name: a.name.clone(),
                    media_type: a.media_type.clone(),
                    path: a.path.clone(),
                    owned: a.owned,
                    state: AttachState::Unconfirmed(a.frame.clone()),
                    preview: None,
                    preview_note: None,
                });
            }
            // Saves that failed: shown failed with their file, retried on request.
            for a in &d.failed {
                let local = chat.next_local();
                draft.attachments.push(Attachment {
                    local,
                    name: a.name.clone(),
                    media_type: a.media_type.clone(),
                    path: a.path.clone(),
                    owned: a.owned,
                    state: AttachState::Failed(a.error.clone()),
                    preview: None,
                    preview_note: None,
                });
            }
            chat.drafts.insert(key.clone(), draft);
        }
        chat.scrolls = Scrolls::restoring(&memory.scroll);
        chat.routing.selections = memory.routing.clone();
        chat.routing.first_route = memory.routing_first.clone();
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
            // The sidebar never reopens on its own on the Home screen (UI-13).
            coordinator_open: memory.coordinator_open && memory.route != Route::Chat,
            last_coordinator_notice: None,
            routes: memory.routes.clone(),
            chat,
            settings,
            help: Default::default(),
            knowledge: KnowledgeScreen::new(),
            conventions: crate::conventions_screen::ConventionsScreen::default(),
            artifacts: ListState::default(),
            resources: ListState::default(),
            browser: ListState::default(),
            browser_parts: BTreeMap::new(),
            addons: Default::default(),
            projects: projects::ProjectsUi::default(),
            purge: Default::default(),
            jobs: jobs::JobsUi::default(),
            work: work::WorkUi::default(),
            usage: crate::usage_screen::UsageScreen::default(),
            graph: crate::graph_screen::GraphScreen::default(),
            databases: Default::default(),
            selected_project: memory.selected_project.clone(),
            completions: Completions::default(),
            decisions: decisions::DecisionsUi::default(),
            scrolls: Scrolls::restoring(&memory.scroll),
            lifecycle: Lifecycle::Open,
            status: None,
            hints,
            started: false,
            saved: Some(memory.clone()),
            purge_reviews: memory.purge_reviews.clone(),
            last_save: Instant::now(),
            last_poll: Instant::now(),
            last_preview_poll: None,
            preview_target: None,
            preview_generation: 0,
            restored_selection: memory.selected_session,
            session_mutations: 0,
            artifact_mutations: 0,
            sessions_list_at: 0,
            artifacts_list_at: 0,
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
        self.coordinator_open
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

    // -----------------------------------------------------------------------------------------
    // Navigation

    /// Navigates to `route`. Returning Home closes the coordinator sidebar and shows the same
    /// conversation in the centre pane; navigating elsewhere never opens it (UI-13).
    pub fn navigate(&mut self, route: Route) {
        if route != Route::Databases {
            self.databases.generated_key = None;
        }
        if route == Route::Chat {
            self.coordinator_open = false;
        }
        self.route = route;
        self.load_route();
    }

    /// The toolbar chat icon: away from Home it toggles the coordinator sidebar; on Home the
    /// conversation is already in the centre, so it focuses the composer.
    pub fn toggle_coordinator(&mut self, ctx: &egui::Context) {
        if self.route == Route::Chat {
            ctx.memory_mut(|m| m.request_focus(egui::Id::unique(("composer", "home"))));
            return;
        }
        self.coordinator_open = !self.coordinator_open;
        if self.coordinator_open {
            self.last_coordinator_notice = None;
            self.ensure_conversation_loaded();
        }
    }

    fn append_coordinator_context_if_changed(&mut self) {
        if !self.coordinator_open || self.route == Route::Chat || self.online().is_err() {
            return;
        }
        let Some(session) = self.chat.selected.clone() else {
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
        let tab = match self.route {
            Route::Approvals => self.decisions.tab,
            Route::Knowledge => self.knowledge.tab,
            Route::Databases => self.graph.tab,
            Route::Usage => self.usage.tab,
            _ => self.routes.get(&self.route).map_or(0, |view| view.tab),
        };
        let selection = match self.route {
            Route::Approvals => self.decisions.selected.as_ref().map(|v| v.id.as_str()),
            Route::Knowledge => self.knowledge.selected.as_ref().map(Id::as_str),
            Route::Databases => self.graph.selected.as_ref().map(|v| v.node_id.as_str()),
            Route::Jobs => self.jobs.selected.as_deref(),
            Route::AddOns => self.addons.selected.as_deref(),
            Route::Help => self.help.selected.as_ref().map(|(_, topic)| topic.as_str()),
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
        if self.last_coordinator_notice.as_ref() == Some(&(session.clone(), key.clone())) {
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
                knowledge_project
            },
            screen.help_topic
        );
        self.last_coordinator_notice = Some((session.clone(), key));
        let Some(backend) = self.backend() else {
            return;
        };
        match Id::new(session.clone()) {
            Ok(id) => match backend.append_coordinator_notice(&id, screen.id, &context) {
                Ok(()) => self.load_transcript(&session),
                Err(error) => self.status = Some(error),
            },
            Err(_) => self.status = Some("Coordinator conversation identity is invalid".into()),
        }
    }

    fn load_route(&mut self) {
        match self.route {
            Route::Chat => self.ensure_conversation_loaded(),
            Route::Usage => {
                self.usage.tab = self.routes.entry(Route::Usage).or_default().tab;
                self.load_usage();
                self.load_router_catalog();
                if !self.projects.list.loaded {
                    self.load_projects();
                }
            }
            Route::Approvals => {
                self.decisions.tab = self.routes.entry(Route::Approvals).or_default().tab;
                self.load_approvals();
            }
            Route::Settings if self.settings.view.is_none() && !self.settings.loading => {
                self.load_settings();
            }
            Route::Databases => self.load_databases(false),
            Route::Help if !self.help.loaded && !self.help.loading => self.refresh_help(),
            Route::Artifacts => self.load_artifacts(),
            Route::Resources => self.load_resources(),
            Route::Browser => self.load_browser(),
            Route::AddOns => {
                self.load_addons();
                if let Some(c) = &mut self.client {
                    c.task(
                        Slot::AddonsDiscovery,
                        "Checking runtimes and package indexes",
                        |b| b.refresh_packages().map(|()| TaskOutput::Refreshed),
                    );
                }
            }
            Route::Portfolio => {
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
            Route::Repositories => {
                self.load_projects();
                self.load_repositories();
                if self.projects.indexing.sources.is_none() {
                    self.load_index_sources();
                }
            }
            Route::Indexing => {
                if self.projects.indexing.sources.is_none() {
                    self.load_index_sources();
                }
                self.index_act(vec![indexing::IndexAction::Runs]);
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

    fn ensure_conversation_loaded(&mut self) {
        if !self.chat.sessions_loaded && !self.pending(&Slot::SessionsList) {
            self.load_sessions();
        }
        if !self.chat.routing.catalog_loaded && self.chat.routing.catalog_load.is_none() {
            self.load_router_catalog();
        }
        if let Some(id) = self.chat.selected.clone()
            && !self.chat.transcripts.contains_key(&id)
        {
            self.load_transcript(&id);
        }
    }

    fn pending(&self, slot: &Slot) -> bool {
        self.client.as_ref().is_some_and(|c| c.is_pending(slot))
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
        if self.online().is_err() {
            return false;
        }
        let Some(b) = self.backend() else {
            return false;
        };
        let frame = frame(&*b);
        self.client
            .as_mut()
            .is_some_and(|c| c.query(slot, label, op, frame))
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
        if self.online().is_err() {
            return false;
        }
        // A slot whose earlier command has no definite outcome keeps it until Confirm outcomes
        // settles that exact frame: a new request there would replace it (and could commit a
        // second time under a fresh request key).
        if self
            .client
            .as_ref()
            .is_some_and(|c| c.unresolved().iter().any(|u| u.slot == slot))
        {
            self.announce(format!(
                "{label}: an earlier request here is not confirmed yet. Use Confirm outcomes \
                 first; nothing is applied twice."
            ));
            return false;
        }
        let Some(b) = self.backend() else {
            return false;
        };
        let frame = frame(&*b);
        self.client
            .as_mut()
            .is_some_and(|c| c.command(slot, label, op, frame))
    }

    pub fn load_sessions(&mut self) {
        self.sessions_list_at = self.session_mutations;
        self.query(
            Slot::SessionsList,
            "Loading conversations",
            "sessions.list",
            ops::sessions_list,
        );
    }
    fn load_transcript(&mut self, id: &str) {
        let t = self.chat.transcripts.entry(id.to_owned()).or_default();
        t.loading = true;
        let after = t.cursor.clone();
        let sid = id.to_owned();
        self.query(
            Slot::Transcript(id.to_owned()),
            "Loading the conversation",
            "sessions.transcript",
            move |b| ops::transcript(b, &sid, after.as_deref()),
        );
    }
    fn start_session(&mut self, reason: StartReason) {
        if self.chat.starting.is_some() {
            return;
        }
        self.chat.starting = Some(reason);
        let n = self.chat.sessions.len() + 1;
        let title = format!("Conversation {n}");
        self.command(
            Slot::SessionStart,
            "Starting a conversation",
            "sessions.start",
            move |b| ops::session_start(b, &title),
        );
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
    }
    fn refresh_container_capabilities(&mut self) {
        self.query(
            Slot::ContainerCapabilities,
            "Checking container runtimes",
            "containers.capabilities",
            ops::container_capabilities,
        );
    }
    fn load_artifacts(&mut self) {
        self.artifacts_list_at = self.artifact_mutations;
        self.query(
            Slot::ArtifactsList,
            "Loading artifacts",
            "artifacts.list",
            ops::workspace_artifacts,
        );
    }
    /// The workspace's browser sessions and the selected project's, or every listed project's
    /// when none is selected: a person may see and interrupt each of them.
    pub(crate) fn load_browser(&mut self) {
        let projects: Vec<String> = match &self.selected_project {
            Some(p) => vec![p.clone()],
            None => {
                if !self.projects.list.loaded && !self.pending(&Slot::ProjectsList) {
                    self.load_projects();
                }
                self.projects
                    .list
                    .items
                    .iter()
                    .map(|p| p.id.0.as_str().to_owned())
                    .collect()
            }
        };
        self.browser_parts
            .retain(|scope, _| scope.as_ref().is_none_or(|p| projects.contains(p)));
        self.combine_browser();
        self.query(
            Slot::BrowserSessions,
            "Loading browser sessions",
            "browser.sessions",
            |b| ops::browser_sessions_list(b, None),
        );
        for p in projects {
            let q = p.clone();
            self.query(
                Slot::ProjectBrowserSessions(p),
                "Loading a project's browser sessions",
                "browser.sessions",
                move |b| ops::browser_sessions_list(b, Some(&q)),
            );
        }
    }

    /// Records one scope's page and rebuilds the combined list.
    fn browser_part(
        &mut self,
        scope: Option<String>,
        page: Result<(Vec<BrowserSession>, bool), Failure>,
    ) {
        let part = match page {
            Ok((items, more)) => ListState {
                items,
                loaded: true,
                error: None,
                more,
            },
            Err(e) => ListState {
                error: Some(e.describe()),
                ..ListState::default()
            },
        };
        self.browser_parts.insert(scope, part);
        self.combine_browser();
    }
    fn combine_browser(&mut self) {
        let parts = &self.browser_parts;
        self.browser = ListState {
            items: parts.values().flat_map(|p| p.items.clone()).collect(),
            loaded: !parts.is_empty() && parts.values().all(|p| p.loaded),
            error: parts.values().find_map(|p| p.error.clone()),
            more: parts.values().any(|p| p.more),
        };
    }
    /// The scope a listed browser session belongs to, for its row.
    fn browser_scope_labels(&self) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        for (scope, part) in &self.browser_parts {
            let label = match scope {
                None => "Workspace".to_owned(),
                Some(p) => self
                    .projects
                    .list
                    .items
                    .iter()
                    .find(|r| r.id.0.as_str() == p)
                    .map_or_else(|| p.clone(), |r| r.name.0.clone()),
            };
            for s in &part.items {
                out.insert(s.id.as_str().to_owned(), label.clone());
            }
        }
        out
    }

    /// Interrupts a browser session: Close stops its browser through the process owner.
    fn close_browser(&mut self, session: &BrowserSession) {
        let (id, revision) = (
            session.id.as_str().to_owned(),
            session.revision.as_str().to_owned(),
        );
        let sid = id.clone();
        self.command(
            Slot::BrowserClose(id),
            "Closing the browser session",
            "browser.sessions",
            move |b| ops::browser_close(b, &sid, &revision),
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
                        c.task(
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
                        self.addons.busy = c.task(
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
    fn load_resources(&mut self) {
        self.query(
            Slot::ResourcesList,
            "Loading resources",
            "resources.list",
            ops::resources_list,
        );
    }

    /// Drops the catalog and availability derived from routing settings, reloading what is
    /// visible.
    fn invalidate_routing(&mut self) {
        self.chat.routing.invalidate();
        // A load still paging may predate the change: it is replaced, not completed. So may an
        // availability query in flight for the visible conversation.
        if self.route == Route::Chat || self.coordinator_open || self.route == Route::Jobs {
            self.load_router_catalog();
            let key = self.chat.draft_key();
            if self.pending(&Slot::RouteStatus(key.clone())) {
                self.reask_route_status_of(key);
            }
        } else {
            self.chat.routing.catalog_load = None;
        }
    }

    /// Loads the whole catalog, page by page, as a new load (replacing one still paging).
    fn load_router_catalog(&mut self) {
        if self.online().is_err() {
            return;
        }
        let generation = self.chat.routing.begin_catalog_load();
        if !self.query(
            Slot::RouterCatalog(generation),
            "Loading models and tiers",
            "router.catalog",
            |b| ops::router_catalog(b, None),
        ) {
            // Never left "loading": the next visible load (or launch) starts it again.
            self.chat.routing.abandon_catalog_load(generation);
        }
    }

    /// Asks the router about the active conversation's selection (and pin).
    fn refresh_route_status(&mut self) -> bool {
        let key = self.chat.draft_key();
        self.refresh_route_status_of(key)
    }

    /// Asks the router about one conversation's selection (and pin), unless a query for that
    /// same selection is already in flight (its answer is as fresh). Periodic and hint-driven
    /// refreshes therefore never stack identical queries on a slow router, which would occupy
    /// every client worker and starve other requests (a message's save among them).
    fn refresh_route_status_of(&mut self, key: String) -> bool {
        let sel = self
            .chat
            .routing
            .selections
            .get(&key)
            .cloned()
            .unwrap_or_default();
        if self.pending(&Slot::RouteStatus(key.clone()))
            && self.chat.routing.status_asked.get(&key) == Some(&sel)
        {
            return true;
        }
        self.ask_route_status(key, sel)
    }

    /// Asks again even when a query for the same selection is in flight: that query may predate
    /// a change it cannot see (a route or pin just decided, routing settings just saved). The
    /// newer query supersedes it.
    fn reask_route_status_of(&mut self, key: String) -> bool {
        let sel = self
            .chat
            .routing
            .selections
            .get(&key)
            .cloned()
            .unwrap_or_default();
        self.ask_route_status(key, sel)
    }

    fn ask_route_status(&mut self, key: String, sel: crate::routing::Selection) -> bool {
        let session = (key != chat::NEW_DRAFT).then(|| key.clone());
        let asked = sel.clone();
        let submitted = self.query(
            Slot::RouteStatus(key.clone()),
            "Checking model availability",
            "router.availability",
            move |b| ops::router_availability(b, &sel.intent, &sel.effort, session.as_deref()),
        );
        if submitted {
            self.chat.routing.status_asked.insert(key, asked);
        }
        submitted
    }

    /// A new route or effort for the active conversation: the explicit `router.pin` change of a
    /// pinned conversation, otherwise the selection its first routed request uses.
    fn select_route(&mut self, sel: crate::routing::Selection) {
        let key = self.chat.draft_key();
        if self.chat.selection() == sel {
            return;
        }
        if self.chat.routing.selection_locked(&key) {
            self.chat.notice = Some((
                Tone::Attention,
                "The conversation's first route is still being saved; choose again once it \
                 is pinned."
                    .into(),
            ));
            return;
        }
        let pin = self
            .chat
            .routing
            .status
            .get(&key)
            .and_then(|s| s.availability.as_ref())
            .and_then(|a| a.pin.as_ref())
            .map(|p| p.pin_revision.get());
        match pin {
            Some(revision) if key != chat::NEW_DRAFT => {
                if self.chat.routing.pin_in_flight.contains(&key)
                    || self.pending(&Slot::RoutePin(key.clone()))
                {
                    self.chat.notice = Some((
                        Tone::Attention,
                        "The previous model change is still being saved; choose again once it \
                         is confirmed."
                            .into(),
                    ));
                    return;
                }
                let session = key.clone();
                let submitted = sel.clone();
                if self.command(
                    Slot::RoutePin(key.clone()),
                    "Changing the conversation's model",
                    "router.pin",
                    move |b| {
                        ops::router_pin(b, &session, revision, &submitted.intent, &submitted.effort)
                    },
                ) {
                    self.chat.routing.pin_in_flight.insert(key.clone());
                    self.chat.routing.selections.insert(key, sel);
                } else {
                    // Not submitted: the pin (and so the selection) is unchanged.
                    self.chat.notice = Some((
                        Tone::Attention,
                        "The model selection was not changed: DevWorks is offline or closing."
                            .into(),
                    ));
                }
            }
            _ => {
                self.chat.routing.selections.insert(key, sel);
                self.refresh_route_status();
            }
        }
    }

    /// Routes a conversation that has no pin yet with its selection, so the selection becomes
    /// its durable pin at its first message (ROUTE-14). With nothing eligible the router refuses
    /// and the selection stays as it is; the status line says why.
    fn route_first(&mut self, session: &str) {
        let pinned = self
            .chat
            .routing
            .status
            .get(session)
            .and_then(|s| s.availability.as_ref())
            .map(|a| a.pin.is_some());
        let Some(epoch) = self.chat.session(session).map(|r| r.context_epoch.get()) else {
            // Its record may not be on the listed page: read it, then route.
            if self.chat.routing.first_route.contains(session)
                && !self.pending(&Slot::RouteSession(session.to_owned()))
            {
                let sid = session.to_owned();
                self.query(
                    Slot::RouteSession(session.to_owned()),
                    "Reading the conversation",
                    "sessions.get",
                    move |b| ops::session_get(b, &sid),
                );
            }
            return;
        };
        // A route submitted for it is authoritative until its decided reply is applied. The
        // slot alone is not: a reply already collected in this frame's batch, but applied after
        // the reply that called us, is no longer pending.
        if self.chat.routing.first_in_flight.contains(session)
            || self.chat.routing.pin_in_flight.contains(session)
            || self.pending(&Slot::RouteFirst(session.to_owned()))
            || self.pending(&Slot::RoutePin(session.to_owned()))
        {
            return;
        }
        match pinned {
            Some(true) => {
                self.chat.routing.first_route.remove(session);
                return;
            }
            // Not known yet whether it is pinned: routed once the router says (this reply or
            // a later one, also after a restart), never over a pin it does not know about.
            None => {
                if self.chat.routing.first_route.insert(session.to_owned())
                    && !self.pending(&Slot::RouteStatus(session.to_owned()))
                {
                    self.refresh_route_status_of(session.to_owned());
                }
                return;
            }
            Some(false) => {
                self.chat.routing.first_route.remove(session);
            }
        }
        let sel = self
            .chat
            .routing
            .selections
            .get(session)
            .cloned()
            .unwrap_or_default();
        let sid = session.to_owned();
        if self.command(
            Slot::RouteFirst(session.to_owned()),
            "Routing the conversation",
            "router.route",
            move |b| ops::router_route(b, &sid, epoch, &sel.intent, &sel.effort),
        ) {
            self.chat.routing.first_in_flight.insert(session.to_owned());
        } else {
            // Not submitted (offline, or the window is closing): it keeps waiting in the
            // persisted list and is routed when the router next answers (or at the next launch).
            self.chat.routing.first_route.insert(session.to_owned());
        }
    }

    fn send(&mut self) {
        let Some(session) = self.chat.selected_session().cloned() else {
            // The first message starts the conversation, then is sent.
            if self.chat.draft().is_some_and(|d| !d.is_empty()) {
                self.chat.send_after_start = true;
                self.start_session(StartReason::ForDraft);
            }
            return;
        };
        let draft = self.chat.draft_mut();
        if draft.is_empty() || draft.busy() {
            return;
        }
        let (text, refs) = (draft.text.clone(), draft.stored());
        let submitted = draft.submission();
        let id = session.id.as_str().to_owned();
        let revision = session.revision.get();
        self.chat.sending = Some(id.clone());
        self.chat.submitted = Some(submitted);
        let sid = id.clone();
        self.command(
            Slot::Append(id),
            "Saving your message",
            "sessions.append_message",
            move |b| ops::append_message(b, &sid, revision, &text, &refs),
        );
    }

    /// Attaches the user's files to the active conversation's draft (dialog, drop and pasted
    /// paths). The shell never removes these files.
    pub fn attach(&mut self, paths: Vec<PathBuf>) {
        self.attach_files(paths.into_iter().map(|p| (p, false)).collect());
    }

    /// Attaches files; `owned` marks one the shell created itself (a pasted image).
    fn attach_files(&mut self, files: Vec<(PathBuf, bool)>) {
        if self.online().is_err() || files.is_empty() {
            return;
        }
        let waiting = self.chat.selected.is_none();
        let key = self.chat.draft_key();
        for (path, owned) in files {
            let local = self.chat.next_local();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
                .to_owned();
            let media = attachments::media_type(&path).to_owned();
            let note = (!attachments::previewable(&media)).then(|| {
                "No preview. The file is stored, not analyzed: nothing has read its content yet."
                    .to_owned()
            });
            self.chat.draft_mut().attachments.push(Attachment {
                local,
                name,
                media_type: media.clone(),
                path: Some(path.clone()),
                owned,
                // Storing once its save is submitted (below, or when its conversation starts).
                state: AttachState::WaitingForConversation,
                preview: None,
                preview_note: note,
            });
            if attachments::previewable(&media)
                && let Some(c) = &mut self.client
            {
                let p = path.clone();
                c.task(
                    Slot::Preview(key.clone(), local),
                    "Preparing a preview",
                    move |_| attachments::preview(&p),
                );
            }
        }
        if waiting {
            self.start_session(StartReason::ForDraft);
        } else {
            self.store_pending();
        }
    }

    /// Attaches a pasted image: it is written as a PNG beside the store, attached, and the
    /// temporary file removed once the artifact service holds its copy.
    pub fn attach_pasted_image(&mut self, image: &egui::ColorImage) {
        let Some(b) = self.backend() else { return };
        let session = self
            .chat
            .selected
            .as_deref()
            .map(devworks_core::protocol::Id::new)
            .transpose();
        let result = session.map_err(|e| e.to_string()).and_then(|session| {
            let png = attachments::encode_paste(image)?;
            b.stage_pasted_image(session.as_ref(), &png)
        });
        match result {
            Ok(path) => self.attach_files(vec![(path, true)]),
            Err(e) => self.announce(format!("The pasted image could not be attached: {e}")),
        }
    }

    /// Removes a pending attachment from its draft. One already stored is deleted through
    /// `artifacts.delete`; one still being stored is deleted when its save completes.
    fn remove_attachment(&mut self, local: u64) {
        let Some(a) = self.chat.take_attachment(local) else {
            return;
        };
        let owned = a.path.clone().filter(|_| a.owned);
        match a.state {
            // Nothing was admitted: only a file the shell created is left to remove.
            AttachState::WaitingForConversation | AttachState::Failed(_) => {
                self.remove_owned(owned.as_deref());
            }
            // Being prepared: its reply finds it removed and admits nothing.
            AttachState::Storing => {
                self.chat.removed.insert(
                    local,
                    chat::Removed {
                        path: owned,
                        frame: None,
                        in_flight: true,
                    },
                );
            }
            // Admitted: its reply decides (a stored copy is deleted, an unknown outcome kept).
            AttachState::Saving(frame) => {
                self.chat.removed.insert(
                    local,
                    chat::Removed {
                        path: owned,
                        frame: Some(frame),
                        in_flight: true,
                    },
                );
            }
            // It may have committed: confirmed by "Confirm outcomes", then deleted.
            AttachState::Unconfirmed(frame) => {
                self.chat.removed.insert(
                    local,
                    chat::Removed {
                        path: owned,
                        frame: Some(frame),
                        in_flight: false,
                    },
                );
            }
            AttachState::Stored(r) => {
                let in_flight = self
                    .chat
                    .submitted
                    .as_ref()
                    .is_some_and(|s| s.locals.contains(&local));
                if in_flight {
                    // The message being saved references it: decide once the append settles.
                    self.chat.removed_while_sending.push(r);
                } else {
                    self.delete_removed(r);
                }
            }
        }
    }

    /// Removes a file the shell created (recorded as owned when it was created).
    fn remove_owned(&self, path: Option<&std::path::Path>) {
        if let (Some(p), Some(b)) = (path, self.backend()) {
            attachments::remove_owned(p, &b.scratch_dir());
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

    /// Admits an attachment's save frame (prepared, or unconfirmed) under `slot`.
    fn submit_save(
        &mut self,
        slot: Slot,
        label: &str,
        frame: Vec<u8>,
        owned: Option<PathBuf>,
    ) -> bool {
        self.client.as_mut().is_some_and(|c| {
            c.task(slot, label, move |b| {
                attachments::save(b, frame, owned.as_deref())
            })
        })
    }

    fn delete_removed(&mut self, artifact: ArtifactRef) {
        // Cleanup the user already asked for, so it is not refused while closing: a closed
        // client keeps the frame unresolved, and the next launch submits it.
        if self.offline.is_some() {
            return;
        }
        let Some(b) = self.backend() else { return };
        let frame = ops::delete_unsent_attachment(&*b, &artifact);
        if let Some(c) = &mut self.client {
            c.command(
                Slot::DeleteAttachment(artifact.artifact_id.as_str().to_owned()),
                "Deleting a removed attachment",
                "artifacts.delete",
                frame,
            );
        }
    }

    /// Re-derives the previews of restored image attachments from their stored artifacts.
    fn restore_previews(&mut self) {
        let jobs: Vec<(String, u64, ArtifactRef)> = self
            .chat
            .drafts
            .iter()
            .flat_map(|(key, d)| {
                d.attachments.iter().filter_map(move |a| match &a.state {
                    AttachState::Stored(r)
                        if a.preview.is_none() && attachments::previewable(&a.media_type) =>
                    {
                        Some((key.clone(), a.local, r.clone()))
                    }
                    _ => None,
                })
            })
            .collect();
        if self.online().is_err() {
            return;
        }
        for (key, local, r) in jobs {
            if let Some(c) = &mut self.client {
                c.task(Slot::Preview(key, local), "Restoring a preview", move |b| {
                    attachments::preview_stored(b, &r)
                });
            }
        }
    }

    /// Starts storing every attachment of the selected conversation's draft that waits.
    /// Stores a failed attachment afresh from its file (a new save, never the failed one).
    fn retry_attachment(&mut self, local: u64) {
        let Some(a) = self.chat.attachment_mut(local) else {
            return;
        };
        if !matches!(a.state, AttachState::Failed(_)) || a.path.is_none() {
            return;
        }
        a.state = AttachState::WaitingForConversation;
        match self.chat.owner(local) {
            Some(key) if key != chat::NEW_DRAFT => self.store_waiting(&key),
            _ => self.start_session(StartReason::ForDraft),
        }
    }

    fn store_pending(&mut self) {
        if let Some(session) = self.chat.selected.clone() {
            self.store_waiting(&session);
        }
    }

    /// Starts storing every attachment of `session`'s draft that waits. One becomes Storing
    /// only once its save is submitted (closing refuses it: it stays waiting, and is kept).
    fn store_waiting(&mut self, session: &str) {
        let jobs: Vec<(u64, PathBuf, String, String)> = self
            .chat
            .drafts
            .get(session)
            .map(|d| {
                d.attachments
                    .iter()
                    .filter(|a| matches!(a.state, AttachState::WaitingForConversation))
                    .filter_map(|a| {
                        Some((
                            a.local,
                            a.path.clone()?,
                            a.name.clone(),
                            a.media_type.clone(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (local, path, name, media) in jobs {
            let sid = session.to_owned();
            let submitted = self.client.as_mut().is_some_and(|c| {
                c.task(
                    Slot::Attach(session.to_owned(), local),
                    &format!("Storing {name}"),
                    move |b| attachments::prepare(b, &path, &name, &media, &sid),
                )
            });
            if submitted && let Some(a) = self.chat.attachment_mut(local) {
                a.state = AttachState::Storing;
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Replies

    fn drain(&mut self, ctx: &egui::Context) {
        let Some(client) = &mut self.client else {
            return;
        };
        for (slot, reply) in client.poll() {
            self.apply(ctx, slot, reply);
        }
    }

    fn apply(&mut self, ctx: &egui::Context, slot: Slot, reply: Reply) {
        let Some((slot, reply)) = self.apply_conventions(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_databases(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_graph(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_purge(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_projects(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_indexing(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_jobs(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_work(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_usage(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_decisions(slot, reply) else {
            return;
        };
        let Some((slot, reply)) = self.apply_help(slot, reply) else {
            return;
        };
        match (slot, reply) {
            // Started before a session mutation was applied: it may lack that change.
            (Slot::SessionsList, Reply::Query(_))
                if self.sessions_list_at != self.session_mutations =>
            {
                self.load_sessions();
            }
            (Slot::SessionsList, Reply::Query(q)) => match ops::sessions(q) {
                Ok((items, more)) => {
                    self.chat.sessions = items;
                    self.chat.sessions_more = more;
                    self.chat.sessions_loaded = true;
                    self.chat.sessions_error = None;
                    self.reconcile_selection();
                    // A waiting first route needs its session's record (its context epoch).
                    let waiting: Vec<String> =
                        self.chat.routing.first_route.iter().cloned().collect();
                    for key in waiting {
                        self.route_first(&key);
                    }
                }
                Err(e) => self.chat.sessions_error = Some(e.describe()),
            },
            (Slot::RouterCatalog(generation), Reply::Query(q)) => match ops::route_models(q) {
                Ok((models, next)) => {
                    if let crate::routing::CatalogPage::More(cursor) =
                        self.chat.routing.catalog_page(generation, models, next)
                        && !self.query(
                            Slot::RouterCatalog(generation),
                            "Loading models and tiers",
                            "router.catalog",
                            move |b| ops::router_catalog(b, Some(&cursor)),
                        )
                    {
                        self.chat.routing.abandon_catalog_load(generation);
                    }
                }
                Err(e) => self.chat.routing.catalog_failed(
                    generation,
                    format!("Named models are unavailable: {}", e.describe()),
                ),
            },
            (Slot::RouteSession(id), Reply::Query(q)) => {
                if let Ok(record) = ops::session_record(q) {
                    self.chat.upsert(record);
                    if self.chat.routing.first_route.contains(&id) {
                        self.route_first(&id);
                    }
                }
            }
            (Slot::RouteStatus(key), Reply::Query(_))
                if self.pending(&Slot::RouteStatus(key.clone())) =>
            {
                // A newer query was issued after this reply was collected (by a reply applied
                // before it in the same batch): it may predate a route or pin just applied.
            }
            (Slot::RouteStatus(key), Reply::Query(q)) => {
                let status = match ops::availability(q) {
                    Ok(a) => {
                        // A pinned conversation's selection is its durable pin.
                        if let Some(pin) = &a.pin
                            && !self.chat.routing.pin_in_flight.contains(&key)
                            && !self.pending(&Slot::RoutePin(key.clone()))
                        {
                            self.chat.routing.selections.insert(
                                key.clone(),
                                crate::routing::Selection {
                                    intent: pin.intent.clone(),
                                    effort: pin.requested_effort.clone(),
                                },
                            );
                        }
                        crate::routing::Status {
                            availability: Some(a),
                            error: None,
                        }
                    }
                    Err(e) => crate::routing::Status {
                        availability: None,
                        error: Some(e.describe()),
                    },
                };
                let known = status.availability.is_some();
                self.chat.routing.status_asked.remove(&key);
                self.chat.routing.status.insert(key.clone(), status);
                if known && self.chat.routing.first_route.contains(&key) {
                    self.route_first(&key);
                }
            }
            (Slot::RouteFirst(key), Reply::Command(c)) => {
                if matches!(ops::routed(c), Err(Failure::Unknown)) {
                    // Confirmed with its identical frame by Confirm outcomes or the next launch;
                    // the selection stays locked until then.
                    return;
                }
                self.chat.routing.first_in_flight.remove(&key);
                // Decided: a wait recorded for it (it was also kept while the window closed) is
                // over, so it is never routed a second time.
                self.chat.routing.first_route.remove(&key);
                // Pinned, or refused because nothing is eligible: the status line says which. It
                // is reread even when not shown, so no older reply stands for it.
                self.chat.routing.status.remove(&key);
                self.reask_route_status_of(key);
            }
            (Slot::RoutePin(key), Reply::Command(c)) => {
                match ops::pinned(c) {
                    Ok(choice) => {
                        self.chat.routing.selections.insert(
                            key.clone(),
                            crate::routing::Selection {
                                intent: choice.intent,
                                effort: choice.requested_effort,
                            },
                        );
                        self.announce("The conversation's model selection was saved.");
                    }
                    Err(Failure::Unknown) => {
                        self.chat.notice = Some((
                            Tone::Attention,
                            "Not yet confirmed whether the model change was saved. Use Confirm \
                             outcomes to check again; it is never applied twice."
                                .into(),
                        ));
                        return;
                    }
                    Err(e) => {
                        self.chat.notice = Some(chat::failure_notice(
                            "The model selection was not changed",
                            &e,
                        ));
                        self.announce("The model selection was not changed.");
                    }
                }
                // Decided: the pin (or the unchanged one) is re-read, shown or not, and its
                // selection adopted.
                self.chat.routing.pin_in_flight.remove(&key);
                self.chat.routing.status.remove(&key);
                self.reask_route_status_of(key);
            }
            (Slot::SessionStart, Reply::Command(c)) => {
                let reason = self.chat.starting.take();
                match ops::started(c) {
                    Err(Failure::Unknown) => {
                        // Not known whether it started: keep waiting for it (no second start);
                        // "Confirm outcomes" resubmits the identical request.
                        self.chat.starting = reason;
                        self.chat.notice = Some((
                            Tone::Attention,
                            "Not yet confirmed whether the conversation started. Use Confirm \
                             outcomes to check again; nothing is started twice."
                                .into(),
                        ));
                        self.announce("Starting a conversation: outcome not confirmed.");
                    }
                    Ok(record) => {
                        self.session_mutations += 1;
                        self.chat.notice = None;
                        let id = record.id.as_str().to_owned();
                        self.chat.upsert(record);
                        self.chat.selected = Some(id.clone());
                        self.chat.adopt_new_draft(&id);
                        self.load_transcript(&id);
                        self.store_pending();
                        if reason == Some(StartReason::NewChat) {
                            self.announce("New conversation started.");
                        }
                        if std::mem::take(&mut self.chat.send_after_start) {
                            self.send();
                        }
                    }
                    Err(e) => {
                        self.chat.send_after_start = false;
                        for a in &mut self.chat.draft_mut().attachments {
                            if matches!(a.state, AttachState::WaitingForConversation) {
                                a.state =
                                    AttachState::Failed("no conversation could be started".into());
                            }
                        }
                        self.chat.notice =
                            Some(chat::failure_notice("Starting a conversation", &e));
                    }
                }
            }
            (Slot::Transcript(id), Reply::Query(q)) => {
                match ops::transcript_page(q) {
                    Ok(page) => {
                        let more = self
                            .chat
                            .transcripts
                            .entry(id.clone())
                            .or_default()
                            .apply(page);
                        let latest_notice=self.chat.transcripts.get(&id).and_then(|transcript|
                        transcript.entries.iter().rev().find_map(|entry|match &entry.body {
                            devworks_core::protocol::provider::TranscriptBody::HostNotice{artifact,..}=>Some(artifact.clone()),
                            _=>None,
                        }));
                        if let Some(artifact) = latest_notice
                            && !self
                                .chat
                                .notice_texts
                                .contains_key(artifact.version_id.as_str())
                            && let Some(backend) = self.backend()
                        {
                            if let Ok(text) = backend.read_coordinator_notice(&artifact) {
                                self.chat
                                    .notice_texts
                                    .insert(artifact.version_id.as_str().into(), text);
                            }
                        }
                        self.chat.reconcile_text_preview();
                        if more {
                            self.load_transcript(&id);
                        }
                    }
                    Err(e) => {
                        let t = self.chat.transcripts.entry(id).or_default();
                        t.loading = false;
                        t.error = Some(e.describe());
                    }
                }
            }
            (Slot::TextPreview(id, generation), Reply::Task(result)) => {
                self.apply_live_text(&id, generation, result);
            }
            (Slot::Append(id), Reply::Command(c)) => {
                self.chat.sending = None;
                let outcome = ops::appended(c);
                if matches!(outcome, Err(Failure::Unknown)) {
                    // Not known whether the message was saved: the composer keeps it as
                    // submitted until the identical request is confirmed.
                    self.chat.sending = Some(id.clone());
                    self.chat.notice = Some((
                        Tone::Attention,
                        "Not yet confirmed whether your message was saved. Use Confirm \
                         outcomes to check again; it is never saved twice."
                            .into(),
                    ));
                    self.announce("Your message: outcome not confirmed.");
                    return;
                }
                let submitted = self.chat.submitted.take();
                let removed = std::mem::take(&mut self.chat.removed_while_sending);
                if outcome.is_err() {
                    // Not committed (and should it ever have been, the artifact service refuses
                    // deleting an artifact a committed message references).
                    for r in removed {
                        self.delete_removed(r);
                    }
                }
                match outcome {
                    Ok(record) => {
                        self.session_mutations += 1;
                        self.chat.upsert(record);
                        if let (Some(d), Some(sent)) = (self.chat.drafts.get_mut(&id), &submitted) {
                            d.settle_sent(sent);
                        }
                        self.chat.notice = None;
                        self.announce("Message saved to the conversation. No coordinator is connected to reply (DW-16).");
                        self.load_transcript(&id);
                        self.route_first(&id);
                    }
                    Err(e) => {
                        if let Failure::Refused(f) = &e
                            && f.code == DomainErrorCode::RevisionConflict
                        {
                            self.load_sessions();
                        }
                        self.chat.notice =
                            Some(chat::failure_notice("Your message was not saved", &e));
                        self.announce("Your message was not saved; the draft is kept.");
                    }
                }
            }
            (Slot::Attach(key, local), Reply::Task(Ok(TaskOutput::Prepared { frame }))) => {
                if let Some(a) = self.chat.attachment_mut(local) {
                    let owned = a.path.clone().filter(|_| a.owned);
                    let label = format!("Storing {}", a.name);
                    // The frame is in the state before it is admitted: a close from here on
                    // keeps it, and the next launch confirms this same request.
                    a.state = AttachState::Saving(frame.clone());
                    // Not submitted (closing): nothing was admitted, and its upload ticket lives
                    // only in this process, so the next launch prepares a fresh one.
                    if !self.submit_save(Slot::Attach(key, local), &label, frame, owned)
                        && let Some(a) = self.chat.attachment_mut(local)
                    {
                        a.state = AttachState::Storing;
                    }
                } else if let Some(r) = self.chat.removed.remove(&local) {
                    // Removed while it was being prepared: nothing was admitted.
                    self.remove_owned(r.path.as_deref());
                }
            }
            (Slot::Attach(_, local), Reply::Task(t)) => {
                let state = match t {
                    Ok(TaskOutput::Stored { outcome, frame }) if is_unknown(&outcome) => {
                        AttachState::Unconfirmed(frame)
                    }
                    Ok(TaskOutput::Stored { outcome, .. }) => match ops::saved_artifact(outcome) {
                        Ok(record) => {
                            self.artifact_mutations += 1;
                            AttachState::Stored(record.artifact)
                        }
                        Err(e) => AttachState::Failed(e.describe()),
                    },
                    Ok(
                        TaskOutput::Preview { .. }
                        | TaskOutput::Prepared { .. }
                        | TaskOutput::SettingSecret { .. }
                        | TaskOutput::PackagePrepared { .. }
                        | TaskOutput::Refreshed
                        | TaskOutput::TextPreview(_)
                        | TaskOutput::Convention(_),
                    ) => AttachState::Failed("unexpected reply".into()),
                    Err(e) => AttachState::Failed(e),
                };
                let stored = matches!(state, AttachState::Stored(_));
                let unconfirmed = matches!(state, AttachState::Unconfirmed(_));
                match self.chat.attachment_mut(local) {
                    Some(a) => {
                        self.status = Some(if stored {
                            format!("{} stored.", a.name)
                        } else if unconfirmed {
                            format!("{}: not yet confirmed whether it was stored.", a.name)
                        } else {
                            format!("{} could not be stored.", a.name)
                        });
                        a.state = state;
                    }
                    // Removed while it was being stored: nothing the user removed survives.
                    None => match state {
                        // (The save removed the shell's own file once it was stored.)
                        AttachState::Stored(r) => {
                            self.chat.removed.remove(&local);
                            self.delete_removed(r);
                        }
                        // Kept, with its file, until "Confirm outcomes" resolves it.
                        AttachState::Unconfirmed(frame) => {
                            let r = self.chat.removed.entry(local).or_insert(chat::Removed {
                                path: None,
                                frame: None,
                                in_flight: false,
                            });
                            r.frame = Some(frame);
                            r.in_flight = false;
                        }
                        // Not saved: the shell's own file is no longer needed.
                        AttachState::Failed(_)
                        | AttachState::Storing
                        | AttachState::Saving(_)
                        | AttachState::WaitingForConversation => {
                            if let Some(r) = self.chat.removed.remove(&local) {
                                self.remove_owned(r.path.as_deref());
                            }
                        }
                    },
                }
            }
            (Slot::Preview(_, local), Reply::Task(t)) => {
                if let Some(a) = self.chat.attachment_mut(local) {
                    match t {
                        Ok(TaskOutput::Preview { size, rgba }) => {
                            let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                            a.preview = Some(ctx.load_texture(
                                format!("attachment-preview-{local}"),
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                        Ok(
                            TaskOutput::Stored { .. }
                            | TaskOutput::Prepared { .. }
                            | TaskOutput::SettingSecret { .. }
                            | TaskOutput::PackagePrepared { .. }
                            | TaskOutput::Refreshed
                            | TaskOutput::TextPreview(_)
                            | TaskOutput::Convention(_),
                        ) => {}
                        Err(e) => a.preview_note = Some(format!("No preview: {e}")),
                    }
                }
            }
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
            (Slot::SettingsExplain(key), Reply::Query(q)) => match ops::settings_chain(q) {
                Ok(chain) => {
                    if let Some(v) = &mut self.settings.view {
                        v.apply_explanation(&key, &chain);
                    }
                    self.settings.explained.insert(key);
                }
                Err(e) => self.settings.message = Some((Tone::Bad, e.describe())),
            },
            (Slot::DeleteAttachment(_), Reply::Command(c)) => match ops::deleted(c) {
                Ok(()) => {
                    self.artifact_mutations += 1;
                    self.announce("The removed attachment was deleted.");
                }
                Err(Failure::Unknown) => {
                    self.announce("Deleting a removed attachment: outcome not confirmed.")
                }
                Err(e) => self.announce(format!(
                    "The removed attachment could not be deleted: {}",
                    e.describe()
                )),
            },
            (Slot::ArtifactsList, Reply::Query(_))
                if self.artifacts_list_at != self.artifact_mutations =>
            {
                self.load_artifacts();
            }
            (Slot::ArtifactsList, Reply::Query(q)) => match ops::artifacts(q) {
                Ok((items, more)) => {
                    self.artifacts = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    };
                }
                Err(e) => self.artifacts.error = Some(e.describe()),
            },
            (Slot::BrowserSessions, Reply::Query(q)) => {
                self.browser_part(None, ops::browser_sessions(q));
            }
            (Slot::ProjectBrowserSessions(p), Reply::Query(q)) => {
                self.browser_part(Some(p), ops::browser_sessions(q));
            }
            (Slot::BrowserClose(_), Reply::Command(c)) => {
                match ops::browser_closed(c) {
                    Ok(true) => self.announce("The browser session is being closed."),
                    Ok(false) => self.announce(
                        "Close is already requested, or the browser session has stopped.",
                    ),
                    Err(Failure::Unknown) => {
                        self.announce("Closing the browser session: outcome not confirmed.")
                    }
                    Err(e) => self.announce(format!(
                        "The browser session could not be closed: {}",
                        e.describe()
                    )),
                }
                self.load_browser();
            }
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
            (Slot::ResourcesList, Reply::Query(q)) => match ops::resources(q) {
                Ok((items, more)) => {
                    self.resources = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    };
                }
                Err(e) => self.resources.error = Some(e.describe()),
            },
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

    /// Restores the selected conversation once sessions are listed: the restored one when it
    /// still exists, otherwise the most recent open one. A missing selection is dropped.
    fn reconcile_selection(&mut self) {
        if self.chat.starting.is_some() {
            // A start is pending: the new draft (and anything waiting in it) belongs to that
            // conversation, which its reply selects. A list reply never re-homes it.
            return;
        }
        let wanted = self
            .chat
            .selected
            .clone()
            .or_else(|| self.restored_selection.take());
        let exists = |id: &str| self.chat.sessions.iter().any(|s| s.id.as_str() == id);
        let pick = match wanted {
            Some(id) if exists(&id) => Some(id),
            _ => self
                .chat
                .sessions
                .iter()
                .filter(|s| !s.closed)
                .max_by_key(|s| chat::last_activity(s))
                .map(|s| s.id.as_str().to_owned()),
        };
        if pick != self.chat.selected
            || pick
                .as_ref()
                .is_some_and(|p| !self.chat.transcripts.contains_key(p))
        {
            self.chat.selected = pick.clone();
            if let Some(id) = pick {
                self.chat.adopt_new_draft(&id);
                self.load_transcript(&id);
            }
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

    fn poll_hints(&mut self) {
        let changed = self
            .hints
            .as_mut()
            .is_some_and(|rx| rx.has_changed().unwrap_or(false));
        if changed {
            if let Some(rx) = &mut self.hints {
                rx.borrow_and_update();
            }
            self.refresh_visible();
        } else if self.last_poll.elapsed() > Duration::from_secs(10) {
            // A missed hint is repaired by a bounded periodic re-query of what is visible.
            self.refresh_visible();
        }
    }

    fn refresh_visible(&mut self) {
        self.last_poll = Instant::now();
        if self.online().is_err() {
            return;
        }
        if (self.route == Route::Chat || self.coordinator_open)
            && let Some(id) = self.chat.selected.clone()
            && !self.pending(&Slot::Transcript(id.clone()))
        {
            self.load_transcript(&id);
        }
        if self.route == Route::Chat || self.coordinator_open || self.route == Route::Jobs {
            if self.route == Route::Chat || self.coordinator_open {
                self.refresh_route_status();
            }
            // Catalog edits and membership changes made elsewhere reach the selectors too.
            if self.chat.routing.catalog_load.is_none() {
                self.load_router_catalog();
            }
        }
        self.refresh_waiting_first_routes();
        if self.route == Route::Usage && self.usage.tab == 2 && !self.usage.prices_loading {
            self.refresh_visible_prices();
        }
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
        match self.route {
            Route::Databases
                if !self.databases.loading
                    && !self.databases.batch_scanning
                    && self.databases.connections.len() <= 100 =>
            {
                self.load_databases(false)
            }
            Route::Help if !self.help.loading => self.probe_help(),
            Route::Artifacts if !self.pending(&Slot::ArtifactsList) => self.load_artifacts(),
            Route::Resources if !self.pending(&Slot::ResourcesList) => self.load_resources(),
            Route::Browser if !self.pending(&Slot::BrowserSessions) => self.load_browser(),
            Route::Portfolio | Route::Repositories
                if !self.pending(&Slot::ProjectsList) && !self.pending(&Slot::RepositoriesList) =>
            {
                self.load_route();
            }
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

    fn chat_actions(&mut self, ctx: &egui::Context, actions: Vec<ChatAction>) {
        for a in actions {
            match a {
                ChatAction::NewChat => self.start_session(StartReason::NewChat),
                ChatAction::Select(id) => {
                    self.chat.selected = Some(id.clone());
                    self.chat.notice = None;
                    if self.chat.routing.status.contains_key(&id) {
                        // Shown as last known while its pin is read again.
                        self.refresh_route_status();
                    }
                    if !self.chat.transcripts.contains_key(&id) {
                        self.load_transcript(&id);
                    }
                }
                ChatAction::Send => self.send(),
                ChatAction::AttachDialog => {
                    let picked = self.dialogs.pick_files();
                    self.attach(picked);
                }
                ChatAction::Attach(paths) => self.attach(paths),
                ChatAction::Remove(local) => self.remove_attachment(local),
                ChatAction::OpenInstructions => {
                    self.settings.category =
                        Some(devworks_core::settings::catalog::Category::Instructions);
                    if let Some(v) = &mut self.settings.view {
                        v.set_category(self.settings.category);
                    }
                    self.navigate(Route::Settings);
                }
                ChatAction::RetrySessions => self.load_sessions(),
                ChatAction::RetryTranscript => {
                    if let Some(id) = self.chat.selected.clone() {
                        self.load_transcript(&id);
                    }
                }
                ChatAction::CloseSidebar => self.coordinator_open = false,
                ChatAction::ConfirmAttachment(local) => self.confirm_attachment(local),
                ChatAction::RetryAttachment(local) => self.retry_attachment(local),
                ChatAction::SetSelection(sel) => self.select_route(sel),
            }
        }
        let _ = ctx;
    }

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

    // -----------------------------------------------------------------------------------------
    // Lifecycle

    /// Begins the close lifecycle (the window's close button lands here).
    pub fn request_close(&mut self) {
        if self.lifecycle == Lifecycle::Open {
            self.cancel_convention_queries();
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
        self.cancel_convention_queries();
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
        Memory {
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
            coordinator_open: self.coordinator_open,
            selected_session: self.chat.selected.clone(),
            selected_project: self.selected_project.clone(),
            sidebar_tab: self.chat.tab,
            history_query: self.chat.history_query.clone(),
            drafts: self
                .chat
                .drafts
                .iter()
                .map(|(k, d)| (k.clone(), d.memory()))
                .filter(|(_, d)| {
                    !d.text.is_empty()
                        || !d.attachments.is_empty()
                        || !d.unconfirmed.is_empty()
                        || !d.uploads.is_empty()
                        || !d.failed.is_empty()
                })
                .collect(),
            routes: self.routes.clone(),
            settings: self.settings.memory(),
            scroll: [&self.scrolls, &self.chat.scrolls, &self.settings.scrolls]
                .into_iter()
                .flat_map(|s| s.offsets().iter().map(|(k, v)| (k.clone(), *v)))
                .collect(),
            pending: self.pending_memory(),
            routing: self.chat.routing.selections.clone(),
            routing_first: self.chat.routing.first_route.clone(),
        }
    }

    /// Requests whose outcome is not known yet (including commands still running when the
    /// bounded quiesce gave up), with what their replies settle, so the next launch confirms
    /// them with their exact frames.
    fn pending_memory(&self) -> crate::memory::PendingMemory {
        use crate::memory::{PendingCommand, PendingMemory, RemovedMemory, SubmittedMemory};
        let commands: Vec<PendingCommand> = self
            .client
            .as_ref()
            .map(|c| {
                c.unconfirmed()
                    .iter()
                    .map(|u| PendingCommand {
                        slot: u.slot.clone(),
                        label: u.label.clone(),
                        operation: u.operation().to_owned(),
                        frame: u.frame().to_vec(),
                        dispatch: u.dispatch,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let has = |f: &dyn Fn(&Slot) -> bool| commands.iter().any(|c| f(&c.slot));
        let submitted = match (&self.chat.submitted, &self.chat.sending) {
            (Some(sent), Some(id)) if has(&|s| *s == Slot::Append(id.clone())) => {
                let artifacts = self
                    .chat
                    .drafts
                    .get(id)
                    .map(|d| {
                        d.attachments
                            .iter()
                            .filter(|a| sent.locals.contains(&a.local))
                            .filter_map(|a| match &a.state {
                                AttachState::Stored(r) => Some(r.artifact_id.as_str().to_owned()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(SubmittedMemory {
                    text: sent.text.clone(),
                    artifacts,
                })
            }
            _ => None,
        };
        PendingMemory {
            conventions: self.conventions_pending_memory(),
            databases: self.databases.pending_memory(),
            starting: self
                .chat
                .starting
                .filter(|_| has(&|s| *s == Slot::SessionStart)),
            submitted,
            removed_while_sending: if self.chat.sending.is_some()
                && has(&|s| matches!(s, Slot::Append(_)))
            {
                self.chat
                    .removed_while_sending
                    .iter()
                    .filter_map(|r| serde_json::to_value(r).ok())
                    .collect()
            } else {
                Vec::new()
            },
            removed: self
                .chat
                .removed
                .values()
                .filter_map(|r| {
                    Some(RemovedMemory {
                        path: r.path.clone(),
                        frame: r.frame.clone()?,
                    })
                })
                .collect(),
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
        }
    }

    /// Puts back what [`Self::pending_memory`] kept; the first frame confirms it.
    fn restore_pending(&mut self, pending: crate::memory::PendingMemory) {
        self.restore_conventions_pending(&pending.conventions);
        self.databases.restore_pending(&pending.databases);
        if let Some(i) = &pending.knowledge_import {
            self.knowledge.import = Some(knowledge_screen::ImportStage::Unconfirmed {
                name: i.name.clone(),
                frame: i.frame.clone(),
            });
        }
        let mut confirming: Vec<String> = Vec::new();
        for c in pending.commands {
            let slot = c.slot.clone();
            let Some(u) =
                Unresolved::restored_dispatch(c.slot, c.label, &c.operation, c.frame, c.dispatch)
            else {
                continue;
            };
            match &slot {
                Slot::DatabaseCa | Slot::DatabaseCommand(_) => self.databases.busy = true,
                Slot::ConventionArtifact(key, _) | Slot::ConventionCommand(key, _) => {
                    if let Some(scope) = self
                        .conventions
                        .states
                        .keys()
                        .find(|s| s.key() == *key)
                        .cloned()
                    {
                        self.conventions.states.get_mut(&scope).expect("scope").busy = true;
                    }
                }
                Slot::GraphTrust(_) => {
                    self.graph.trust_busy = true;
                    self.graph.trust_unknown = true;
                }
                Slot::PriceOverride | Slot::PriceRefresh => {
                    self.restore_price_command(&slot, u.frame())
                }
                Slot::WorkPlan(id) => confirming.push(id.clone()),
                Slot::SessionStart => {
                    self.chat.starting = Some(pending.starting.unwrap_or(StartReason::ForDraft));
                }
                Slot::Append(id) => {
                    self.chat.sending = Some(id.clone());
                    if let Some(sent) = &pending.submitted {
                        let locals = self
                            .chat
                            .drafts
                            .get(id)
                            .map(|d| {
                                d.attachments
                                    .iter()
                                    .filter(|a| {
                                        matches!(&a.state, AttachState::Stored(r)
                                            if sent.artifacts.iter().any(|x| x == r.artifact_id.as_str()))
                                    })
                                    .map(|a| a.local)
                                    .collect()
                            })
                            .unwrap_or_default();
                        self.chat.submitted = Some(chat::Submitted {
                            text: sent.text.clone(),
                            locals,
                        });
                        self.chat.removed_while_sending = pending
                            .removed_while_sending
                            .iter()
                            .filter_map(|r| serde_json::from_value(r.clone()).ok())
                            .collect();
                    }
                }
                Slot::AddonsSave { .. } | Slot::AddonsCommand => self.addons.busy = true,
                Slot::Configure(n) => {
                    let p = &mut self.settings.connections;
                    p.sequence = p.sequence.max(*n);
                    p.restore_edit(*n, u.frame());
                    p.busy.push((*n, u.label.clone()));
                }
                Slot::HelpGenerate => {
                    self.help.generating = true;
                    self.help.generation_unknown = true;
                }
                Slot::SettingsSave => {
                    self.settings.saving = true;
                    self.settings.unconfirmed = true;
                }
                Slot::ProjectSettingsSave(p) => {
                    self.projects.base.entry(p.clone()).or_default().saving = true;
                }
                Slot::RouteFirst(id) => {
                    self.chat.routing.first_in_flight.insert(id.clone());
                }
                Slot::RoutePin(id) => {
                    self.chat.routing.pin_in_flight.insert(id.clone());
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
        self.resume_database_batch();
        for r in pending.removed {
            let local = self.chat.next_local();
            self.chat.removed.insert(
                local,
                chat::Removed {
                    path: r.path,
                    frame: Some(r.frame),
                    in_flight: false,
                },
            );
        }
    }

    fn save_memory(&mut self) {
        self.persist(false);
    }

    /// Writes view state when it changed (or always, when `force`).
    fn persist(&mut self, force: bool) {
        let memory = self.memory();
        match self.publish_purge_client(force || self.saved.as_ref() != Some(&memory)) {
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
            && (force || self.saved.as_ref() != Some(&memory))
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
            self.restore_previews();
            // First messages saved before their routing was known, at the last close.
            self.refresh_waiting_first_routes();
            // Outcomes left unknown at the last close: confirmed with their exact frames.
            if self.unconfirmed_count() > 0 {
                self.confirm_outcomes();
            }
            // Attachments that never started storing: their conversation's, or a new one's.
            let waiting: Vec<String> = self
                .chat
                .drafts
                .iter()
                .filter(|(_, d)| {
                    d.attachments
                        .iter()
                        .any(|a| matches!(a.state, AttachState::WaitingForConversation))
                })
                .map(|(k, _)| k.clone())
                .collect();
            for key in waiting {
                if key == chat::NEW_DRAFT {
                    self.start_session(StartReason::ForDraft);
                } else {
                    self.store_waiting(&key);
                }
            }
        }
        self.apply_text_scale(&ctx);
        self.advance_lifecycle(&ctx);
        self.drain(&ctx);
        if let Err(e) = self.publish_purge_client(false) {
            self.status = Some(e);
        }
        if self.lifecycle == Lifecycle::Open && !self.purge.frozen {
            self.poll_hints();
            self.input(&ctx);
            self.poll_live_text(&ctx);
        }
        if self.lifecycle == Lifecycle::Open {
            self.append_coordinator_context_if_changed();
            self.route_status_due(&ctx);
        }
        if self.purge.frozen {
            ui.disable();
        }
        self.toolbar(ui);
        self.statusbar(ui);
        self.navigation(ui);
        if self.coordinator_open && self.route != Route::Chat {
            self.coordinator_sidebar(ui);
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

    /// Asks again about conversations whose first message still waits to be routed.
    fn refresh_waiting_first_routes(&mut self) {
        let waiting: Vec<String> = self.chat.routing.first_route.iter().cloned().collect();
        for key in waiting {
            if !self.pending(&Slot::RouteStatus(key.clone()))
                && !self.pending(&Slot::RouteFirst(key.clone()))
            {
                self.refresh_route_status_of(key);
            }
        }
    }

    /// Asks about the visible conversation's selection when nothing is known yet, and again when
    /// the earliest cooldown it waits for has ended (never earlier).
    fn route_status_due(&mut self, ctx: &egui::Context) {
        if !(self.route == Route::Chat || self.coordinator_open) || self.online().is_err() {
            return;
        }
        let key = self.chat.draft_key();
        if self.pending(&Slot::RouteStatus(key.clone())) {
            return;
        }
        let retry = match self.chat.routing.status.get(&key) {
            None => {
                self.refresh_route_status();
                return;
            }
            Some(s) => s
                .availability
                .as_ref()
                .filter(|a| !a.available)
                .and_then(|a| a.retry_at_ms.as_ref().map(|t| t.get())),
        };
        if let Some(at) = retry {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            if now >= at {
                // Once per reported expiry: a router still reporting it (its clock may lag) is
                // asked again by the periodic refresh, not every frame.
                if self.chat.routing.rechecked.get(&key) != Some(&at) && self.refresh_route_status()
                {
                    self.chat.routing.rechecked.insert(key, at);
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis((at - now) as u64));
            }
        }
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

    fn composer_focused(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| {
            m.has_focus(egui::Id::unique(("composer", "home")))
                || m.has_focus(egui::Id::unique(("composer", "sidebar")))
        })
    }

    fn input(&mut self, ctx: &egui::Context) {
        let conversation_visible = self.route == Route::Chat || self.coordinator_open;
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
                self.import_dropped_convention(dropped);
            } else if self.route == Route::Knowledge {
                // A file dropped on an entry view is previewed before import.
                self.knowledge_actions(vec![KnowledgeAction::ImportFiles(dropped)]);
            } else {
                self.announce("Open the workspace coordinator to attach dropped files.");
            }
        }
        if self.composer_focused(ctx) {
            let mut images = Vec::new();
            let mut files = Vec::new();
            ctx.input_mut(|i| {
                i.events.retain(|e| match e {
                    Event::PasteImage(img) => {
                        images.push(img.clone());
                        false
                    }
                    Event::Paste(text) => match attachments::pasted_path(text) {
                        Some(p) => {
                            files.push(p);
                            false
                        }
                        None => true,
                    },
                    _ => true,
                });
            });
            for img in images {
                self.attach_pasted_image(&img);
            }
            self.attach(files);
        }
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
                        let open = self.coordinator_open;
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
        let attachments = self
            .chat
            .drafts
            .values()
            .flat_map(|d| d.attachments.iter())
            .filter(|a| matches!(a.state, AttachState::Unconfirmed(_)))
            .count();
        self.client.as_ref().map_or(0, |c| c.unresolved().len())
            + attachments
            + usize::from(self.knowledge.import_frame(true).is_some())
            + self
                .chat
                .removed
                .values()
                .filter(|r| r.frame.is_some() && !r.in_flight)
                .count()
    }

    /// Resubmits every unconfirmed request with its identical frame (same request key).
    pub fn confirm_outcomes(&mut self) {
        if let Some(c) = &mut self.client {
            c.resubmit_unresolved();
        }
        // The resubmitted save runs again (and says so) until its outcome arrives.
        self.settings.unconfirmed = false;
        let locals: Vec<u64> = self
            .chat
            .drafts
            .values()
            .flat_map(|d| d.attachments.iter())
            .filter(|a| matches!(a.state, AttachState::Unconfirmed(_)))
            .map(|a| a.local)
            .collect();
        for local in locals {
            self.confirm_attachment(local);
        }
        // A Knowledge import file's save: its identical frame (not submitted while closing: it
        // stays unconfirmed, its frame kept).
        if let Some((name, frame)) = self.knowledge.import_frame(true)
            && self.submit_import_save(frame.clone())
        {
            self.knowledge.import = Some(knowledge_screen::ImportStage::Storing { name, frame });
        }
        let removed: Vec<(u64, Vec<u8>, Option<PathBuf>)> = self
            .chat
            .removed
            .iter()
            .filter(|(_, r)| !r.in_flight)
            .filter_map(|(l, r)| Some((*l, r.frame.clone()?, r.path.clone())))
            .collect();
        for (local, frame, path) in removed {
            // A removed attachment, under its own local ID (which no draft holds any more):
            // confirmed, then deleted by its reply.
            let slot = Slot::Attach(String::new(), local);
            if self.submit_save(slot, "Confirming a removed attachment", frame, path)
                && let Some(r) = self.chat.removed.get_mut(&local)
            {
                r.in_flight = true;
            }
        }
    }

    fn confirm_attachment(&mut self, local: u64) {
        let Some(a) = self.chat.attachment_mut(local) else {
            return;
        };
        let AttachState::Unconfirmed(frame) = &a.state else {
            return;
        };
        let frame = frame.clone();
        let owned = a.path.clone().filter(|_| a.owned);
        // Its own draft's slot, wherever the user is now.
        let key = self.chat.owner(local).unwrap_or_default();
        let slot = Slot::Attach(key, local);
        // Not submitted (closing): it stays unconfirmed, its frame kept.
        if self.submit_save(slot, "Confirming an attachment", frame.clone(), owned)
            && let Some(a) = self.chat.attachment_mut(local)
        {
            a.state = AttachState::Saving(frame);
        }
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

    fn coordinator_sidebar(&mut self, ui: &mut Ui) {
        let online = self.online();
        let mut actions = Vec::new();
        // The screen beside it keeps at least MIN_SCREEN of the width, down to the minimum
        // window (where the sidebar narrows instead of crushing the screen).
        let widest = (ui.available_width() - theme::MIN_SCREEN).clamp(260.0, 560.0);
        Panel::right("coordinator-sidebar")
            .resizable(true)
            .default_size(380.0_f32.min(widest))
            .size_range(260.0_f32.min(widest)..=widest)
            .frame(
                Frame::new()
                    .fill(theme::PANEL)
                    .stroke(theme::line())
                    .inner_margin(theme::margin(14, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Workspace coordinator").strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let label = "Close the workspace coordinator sidebar";
                        let r = ui.small_button("×").on_hover_text(label);
                        r.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::Role::Button, true, label)
                        });
                        if r.clicked() {
                            actions.push(ChatAction::CloseSidebar);
                        }
                    });
                });
                widgets::caption(
                    ui,
                    match self.chat.selected_session() {
                        Some(s) => format!("{} · same conversation as Chat", s.title.0),
                        None => "The conversation selected on Chat".to_owned(),
                    },
                );
                widgets::divider(ui);
                ui.add_space(6.0);
                if let Some((tone, text)) = &self.chat.notice {
                    chip(ui, text.clone(), *tone);
                }
                chat::conversation(
                    ui,
                    &mut self.chat,
                    Surface::Sidebar,
                    online.as_ref().map(|_| ()).map_err(String::as_str),
                    &mut actions,
                );
            });
        let ctx = ui.ctx().clone();
        self.chat_actions(&ctx, actions);
    }

    fn content(&mut self, ui: &mut Ui) {
        match self.route {
            Route::Chat => self.home(ui),
            Route::Settings => {
                let online = self.online();
                let mut actions = Vec::new();
                // Repository indexing sources sit with the repository paths (General).
                let mut index_actions = Vec::new();
                let up = online.is_ok();
                let indexing = &mut self.projects.indexing;
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
                if self.projects.indexing.sources.is_none()
                    && self.projects.indexing.sources_error.is_none()
                    && online.is_ok()
                {
                    self.load_index_sources();
                }
                self.index_act(index_actions);
                let ctx = ui.ctx().clone();
                self.poll_connections(&ctx);
            }
            Route::Databases => self.databases_screen(ui),
            Route::Help => self.help_screen(ui),
            Route::Indexing => self.indexing_screen(ui),
            Route::Artifacts => self.artifacts_screen(ui),
            Route::Resources => self.resources_screen(ui),
            Route::Browser => self.browser_screen(ui),
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
            Route::Portfolio => self.portfolio_screen(ui),
            Route::Repositories => self.repositories_screen(ui),
            Route::Knowledge if self.routes.entry(Route::Knowledge).or_default().tab == 2 => {
                self.conventions_screen(ui)
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
            Route::Usage => self.usage_screen(ui),
            Route::Jobs => self.jobs_screen(ui),
            Route::Overview => self.overview_screen(ui),
            Route::Features => self.features_screen(ui),
            Route::Issues => self.issues_screen(ui),
            _ => self.pending_screen(ui),
        }
    }

    fn home(&mut self, ui: &mut Ui) {
        let online = self.online();
        let online_ref = online.as_ref().map(|_| ()).map_err(String::as_str);
        let mut actions: Vec<ChatAction> = Vec::new();
        let narrow = ui.available_width() < theme::NARROW + theme::SIDEBAR;
        let screen = Route::Chat.screen();
        let notice = self.chat.notice.clone();
        let conversation = |ui: &mut Ui, st: &mut (&mut Chat, &mut Vec<ChatAction>)| {
            widgets::pane(
                ui,
                Header {
                    title: screen.title,
                    summary: "All projects",
                    tabs: &[],
                    tab: 0,
                },
                st,
                |ui, st| chat::header_actions(ui, online_ref.is_ok(), st.1),
                |ui, st, _| {
                    if let Some((tone, text)) = &notice {
                        chip(ui, text.clone(), *tone);
                    }
                    chat::conversation(ui, st.0, Surface::Home, online_ref, st.1);
                },
            );
        };
        let mut st = (&mut self.chat, &mut actions);
        if narrow {
            let label = if st.0.narrow_sidebar_open {
                "Hide Current and Chat History"
            } else {
                "Show Current and Chat History"
            };
            if widgets::secondary_button(ui, label).clicked() {
                st.0.narrow_sidebar_open = !st.0.narrow_sidebar_open;
            }
            conversation(ui, &mut st);
            if st.0.narrow_sidebar_open {
                // A narrow layout overlays the right edge without changing the conversation.
                let rect = ui.max_rect();
                let width = theme::SIDEBAR.min(rect.width());
                egui::Area::new(egui::Id::unique("home-sidebar-overlay"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(rect.max.x - width, rect.min.y))
                    .show(ui.ctx(), |ui| {
                        Frame::new()
                            .fill(theme::PANEL)
                            .stroke(theme::line())
                            .corner_radius(theme::radius())
                            .inner_margin(theme::margin(14, 12))
                            .show(ui, |ui| {
                                ui.set_width(width - 28.0);
                                ui.set_height(rect.height() - 24.0);
                                chat::sidebar(ui, st.0, online_ref, st.1);
                            });
                    });
            }
        } else {
            widgets::with_right_sidebar(
                ui,
                theme::SIDEBAR,
                &mut st,
                |ui, st| conversation(ui, st),
                |ui, st| {
                    Frame::new()
                        .fill(theme::PANEL)
                        .stroke(theme::line())
                        .corner_radius(theme::radius())
                        .inner_margin(theme::margin(14, 12))
                        .show(ui, |ui| {
                            ui.set_min_size(ui.available_size());
                            chat::sidebar(ui, st.0, online_ref, st.1);
                        });
                },
            );
        }
        let ctx = ui.ctx().clone();
        self.chat_actions(&ctx, actions);
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

    fn artifacts_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Artifacts.screen();
        let online = self.online();
        let mut reload = false;
        let mut mem = self.routes.remove(&Route::Artifacts).unwrap_or_default();
        let tab = mem.tab;
        let list = &self.artifacts;
        let scrolls = &mut self.scrolls;
        let mut st = (&mut reload, &mut mem);
        let (_, tab) = widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: screen.subviews,
                tab,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    *st.0 = true;
                }
            },
            |ui, st, tab| {
                if tab == 1 {
                    widgets::state_block(
                        ui,
                        &ViewState::Unavailable {
                            what: "project artifacts".into(),
                            owner: "DW-11 (projects)".into(),
                        },
                    );
                    return;
                }
                let mem = &mut *st.1;
                let r = ui.add(
                    egui::TextEdit::singleline(&mut mem.filter)
                        .hint_text("Filter by name or tag")
                        .desired_width(280.0),
                );
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter artifacts")
                });
                let RouteMemory {
                    filter, selection, ..
                } = mem;
                list_body(
                    ui,
                    &online,
                    list,
                    ListText {
                        what: "workspace artifacts",
                        empty_title: "No workspace artifacts yet",
                        empty_body: "Attachments you add to a Home conversation are kept here.",
                        id_salt: "artifacts-list",
                    },
                    st.0,
                    scrolls,
                    |a: &ArtifactRecord| {
                        let tags = a.tags.as_ref().map(|t| t.0.join(", ")).unwrap_or_default();
                        format!("{} {} {}", a.name.0, a.artifact.media_type, tags)
                    },
                    |a: &ArtifactRecord| a.artifact.artifact_id.as_str().to_owned(),
                    filter,
                    selection,
                    |ui, a: &ArtifactRecord, selected| {
                        if let Some(source) = &a.tested_repository {
                            if let Some(url) = &source.repository_url {
                                ui.hyperlink_to(
                                    &source.head.hex,
                                    format!("{}/commit/{}", url.0, source.head.hex),
                                );
                            } else {
                                ui.monospace(&source.head.hex);
                            }
                            widgets::caption(
                                ui,
                                format!(
                                    "{} · {}",
                                    source.repository_id.as_str(),
                                    if source.dirty {
                                        "dirty at observation"
                                    } else {
                                        "clean at observation"
                                    }
                                ),
                            );
                        }
                        let r = ui.selectable_label(
                            selected,
                            format!(
                                "{}  ·  {}  ·  {}",
                                a.name.0,
                                a.artifact.media_type,
                                chat::bytes_label(a.artifact.byte_length.get())
                            ),
                        );
                        let r = match &a.tags {
                            Some(t) if !t.0.is_empty() => r.on_hover_text(t.0.join(", ")),
                            _ => r,
                        };
                        r.clicked()
                    },
                );
            },
        );
        mem.tab = tab;
        self.routes.insert(Route::Artifacts, mem);
        if reload {
            self.load_artifacts();
        }
    }

    fn browser_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Browser.screen();
        let online = self.online();
        let mut reload = false;
        let mut close: Option<BrowserSession> = None;
        let mut mem = self.routes.remove(&Route::Browser).unwrap_or_default();
        let scopes = self.browser_scope_labels();
        let list = &self.browser;
        let scrolls = &mut self.scrolls;
        let mut st = (&mut reload, &mut mem, &mut close);
        widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &[],
                tab: 0,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    *st.0 = true;
                }
                let selected =
                    st.1.selection
                        .as_ref()
                        .and_then(|id| list.items.iter().find(|s| s.id.as_str() == id));
                if let Some(s) = selected.filter(|s| {
                    matches!(
                        s.state,
                        ItemState::Healthy | ItemState::Partial | ItemState::Unavailable
                    )
                }) && widgets::secondary_button(ui, "Close session").clicked()
                {
                    *st.2 = Some(s.clone());
                }
            },
            |ui, st, _| {
                let mem = &mut *st.1;
                let RouteMemory {
                    filter, selection, ..
                } = mem;
                list_body(
                    ui,
                    &online,
                    list,
                    ListText {
                        what: "browser sessions",
                        empty_title: "No browser sessions",
                        empty_body: "Agents' owned browser sessions appear here; evidence they record is kept in Artifacts.",
                        id_salt: "browser-list",
                    },
                    st.0,
                    scrolls,
                    |s: &BrowserSession| s.id.as_str().to_owned(),
                    |s: &BrowserSession| s.id.as_str().to_owned(),
                    filter,
                    selection,
                    |ui, s: &BrowserSession, selected| {
                        let state = match s.state {
                            ItemState::Partial => "starting",
                            ItemState::Healthy => "running",
                            ItemState::Disabled => "closed",
                            ItemState::Failed => "failed",
                            ItemState::Unavailable => "stop not verified",
                            ItemState::Enabled => "enabled",
                        };
                        let scope = scopes.get(s.id.as_str()).map_or("", String::as_str);
                        if let Some(source) = &s.tested_repository {
                            if let Some(url) = &source.repository_url {
                                ui.hyperlink_to(
                                    &source.head.hex,
                                    format!("{}/commit/{}", url.0, source.head.hex),
                                );
                            }
                            widgets::caption(
                                ui,
                                format!(
                                    "{} · worktree {} · {} · {} · observed {}",
                                    source.repository_id.as_str(),
                                    source.worktree_id.as_str(),
                                    source.head.hex,
                                    if source.dirty { "dirty" } else { "clean" },
                                    source.observed_at_ms.as_str()
                                ),
                            );
                        } else {
                            widgets::caption(ui, "No tested repository revision recorded");
                        }
                        ui.selectable_label(
                            selected,
                            format!(
                                "{}  ·  {scope}  ·  {state}  ·  {} target(s)  ·  revision {}",
                                s.id.as_str(),
                                s.target_ids.0.len(),
                                s.revision.as_str()
                            ),
                        )
                        .clicked()
                    },
                );
            },
        );
        self.routes.insert(Route::Browser, mem);
        if let Some(s) = close {
            self.close_browser(&s);
        }
        if reload {
            self.load_browser();
        }
    }

    fn resources_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Resources.screen();
        let online = self.online();
        let mut reload = false;
        let mut link = false;
        let mut mem = self.routes.remove(&Route::Resources).unwrap_or_default();
        let list = &self.resources;
        let scrolls = &mut self.scrolls;
        let mut st = (&mut reload, &mut mem, &mut link);
        widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &[],
                tab: 0,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    *st.0 = true;
                }
                if let Some((label, _)) = screen.link
                    && widgets::secondary_button(ui, label).clicked()
                {
                    *st.2 = true;
                }
            },
            |ui, st, _| {
                let mem = &mut *st.1;
                let r = ui.add(
                    egui::TextEdit::singleline(&mut mem.filter)
                        .hint_text("Filter resources")
                        .desired_width(280.0),
                );
                r.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Filter resources")
                });
                let RouteMemory {
                    filter, selection, ..
                } = mem;
                list_body(
                    ui,
                    &online,
                    list,
                    ListText {
                        what: "resources",
                        empty_title: "No shared resources registered",
                        empty_body: "Machine resources are registered at boot.",
                        id_salt: "resources-list",
                    },
                    st.0,
                    scrolls,
                    |r: &ResourceRecord| r.canonical_key.0.clone(),
                    |r: &ResourceRecord| r.id.0.as_str().to_owned(),
                    filter,
                    selection,
                    |ui, r: &ResourceRecord, selected| {
                        ui.selectable_label(
                            selected,
                            format!(
                                "{}  ·  capacity {}  ·  {} holding  ·  {} waiting",
                                r.canonical_key.0,
                                r.capacity.get(),
                                r.holders.0.len(),
                                r.waiters.0.len()
                            ),
                        )
                        .clicked()
                    },
                );
            },
        );
        self.routes.insert(Route::Resources, mem);
        if link && let Some((_, target)) = screen.link {
            self.navigate(target);
        }
        if reload {
            self.load_resources();
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
        if ctx.input(|i| !i.raw.hovered_files.is_empty())
            && (self.route == Route::Chat || self.coordinator_open)
        {
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
struct ListText<'a> {
    what: &'a str,
    empty_title: &'a str,
    empty_body: &'a str,
    id_salt: &'a str,
}

/// The body of a list bound to one query: offline, failed, loading, empty and filtered states,
/// then the rows (virtualized) with the remembered selection.
#[allow(clippy::too_many_arguments)]
fn list_body<T>(
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
