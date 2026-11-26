//! One desktop screen behind one trait. A screen owns its view state, its slot type (the
//! requests it waits for) and the reply, refresh, restore and purge handling of those slots;
//! the shell reaches it through [`DynScreen`] by the [`ScreenId`] a route or slot names.
use crate::backend::{Backend, Client, Reply, Slot, Unresolved};
use crate::memory::RouteMemory;
use crate::paged::{Fetch, Page, Paged};
use crate::project_purge::Live;
use crate::projects_screen::ProjectLists;
use crate::route::Route;
use crate::shell::Dialogs;
use crate::shell::indexing::IndexAction;
use crate::virtual_list::Scrolls;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::records::{ProjectRecord, RepositoryRecord, RouteCatalogModel};
use eframe::egui::Ui;
use serde_json::Value;
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;

/// The screen that owns a route or a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenId {
    Help,
    Usage,
    Graph,
    Databases,
    Instructions,
    Conventions,
    Artifacts,
    Resources,
    Agents,
    Browser,
    /// All projects and Repositories.
    Projects,
    Indexing,
    Watchers,
    /// Home and the coordinator sidebar: one conversation.
    Chat,
    /// Scheduled jobs.
    Jobs,
    /// Approvals, the notification center and completion cards.
    Decisions,
    /// Overview, Features and Issues.
    Work,
    /// Settings: categories, Provider connections, Agent tools and MCP servers.
    Settings,
    AddOns,
    Knowledge,
    /// The permanent project purge review (a window over every screen).
    Purge,
    /// Routes whose content belongs to a later item: a designed unavailable state.
    Placeholder,
}
impl ScreenId {
    /// The screen's key in persisted pending state (`PendingMemory::screens`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Usage => "usage",
            Self::Graph => "graph",
            Self::Databases => "databases",
            Self::Instructions => "instructions",
            Self::Conventions => "conventions",
            Self::Artifacts => "artifacts",
            Self::Resources => "resources",
            Self::Agents => "agents",
            Self::Browser => "browser",
            Self::Projects => "projects",
            Self::Indexing => "indexing",
            Self::Watchers => "watchers",
            Self::Chat => "chat",
            Self::Jobs => "jobs",
            Self::Decisions => "decisions",
            Self::Work => "work",
            Self::Settings => "settings",
            Self::AddOns => "addons",
            Self::Knowledge => "knowledge",
            Self::Purge => "purge",
            Self::Placeholder => "placeholder",
        }
    }
}

/// Work a screen asks of the shell. The shell runs it right after the screen call returns.
#[derive(Debug)]
pub enum Effect {
    Navigate(Route),
    /// Opens a work item (its ID).
    OpenWork(String),
    /// Shows a conversation (its session ID) on Home.
    OpenSession(String),
    LoadProjects,
    LoadRepositories,
    /// Reads the next page of the navigation's project list, of every registration, or of the
    /// selected project's repositories.
    LoadMoreProjects,
    LoadMoreRepositories,
    LoadMoreMembers,
    /// A project command succeeded with this record: the project lists, the navigation's
    /// selection and the work screens follow it.
    ProjectChanged(ProjectRecord),
    /// Selects a project in the navigation (`None` clears it).
    SelectProject(Option<String>),
    /// Opens Scheduled jobs scoped to a project (its ID).
    OpenJobs(String),
    /// Reads the run status of a project's scheduled jobs (its ID).
    LoadProjectJobs(String),
    /// Counts the running work an archive of a project stops (its ID).
    CountArchiveWork(String),
    /// Forgets a project's archive count (its ID): the next confirmation counts afresh.
    ForgetArchiveCount(String),
    /// Opens the permanent purge review of a deleted project.
    ReviewPurge(ProjectRecord),
    /// Opens a repository's graph (its ID and name).
    OpenGraph(String, String),
    OpenFleetAnalysis,
    /// Opens a repository's graph on the detail of one of its nodes.
    OpenGraphNode {
        repository: String,
        node: String,
    },
    /// Opens Databases searching for a table by name.
    FindDatabaseTable(String),
    LoadRouterCatalog,
    /// Work for the Indexing screen.
    Index(IndexAction),
    ConfirmOutcomes,
    /// Opens Settings on the embedding setup.
    OpenEmbeddingSetup,
    /// Opens Settings on the Instructions category.
    OpenInstructions,
    /// Opens Settings on Provider connections.
    OpenConnections,
    /// Opens Help on the topics of a screen or settings category (its ID).
    OpenHelp(String),
    /// Inspects an approval (its ID), after any navigation asked for before it.
    InspectApproval(String),
    /// Selects a Knowledge entry, after any navigation asked for before it.
    SelectKnowledge(devworks_core::protocol::Id),
    /// An artifact was stored or deleted: the Artifacts screen re-reads its list.
    ArtifactsChanged,
    /// Work for the watcher lists (a watcher section drawn by another screen).
    Watch(Vec<crate::watchers_screen::WatchAction>),
    /// Shows a source-control link's conversation (its session ID) on Home, with no earlier
    /// notice of that conversation.
    OpenGitSession(String),
    /// Settings or a connection changed: the router's selectors and availability are read again.
    InvalidateRouting,
    /// Shows a route without loading it (the screen has asked for what it needs itself).
    SetRoute(Route),
    /// Opens Usage on the prices of a model list (the model IDs the embedding setup priced).
    OpenPrices(String),
    /// Opens the notification center.
    OpenNotifications,
    /// The navigation's project list reply, read.
    ProjectList(Result<Page<ProjectRecord>, String>),
    /// The repository list reply, read.
    RepositoryList(Result<Page<RepositoryRecord>, String>),
    /// A reply with the repositories of a project (its ID).
    ProjectMembers(String, Result<Page<RepositoryRecord>, String>),
    /// Publishes the client state the purge reviews, then submits the confirmed purge.
    ConfirmPurge,
}

/// What the coordinator is told about the visible screen.
#[derive(Debug, Default)]
pub struct ScreenContext {
    pub tab: usize,
    pub selection: Option<String>,
}

/// The shell state a screen may use during one call.
pub struct Cx<'a> {
    pub(crate) client: Option<&'a mut Client>,
    pub(crate) online: Result<(), String>,
    pub(crate) status: &'a mut Option<String>,
    pub(crate) route: Route,
    /// Each route's view memory (tab, filter, selection).
    pub(crate) routes: &'a mut BTreeMap<Route, RouteMemory>,
    pub(crate) scrolls: &'a mut Scrolls,
    pub(crate) dialogs: &'a mut dyn Dialogs,
    /// The project the navigation selected.
    pub(crate) selected_project: Option<String>,
    /// Projects and repositories as the shell last read them.
    pub(crate) projects: ProjectLists<'a>,
    /// The router catalog as the shell last read it.
    pub(crate) catalog: &'a [RouteCatalogModel],
    /// The run status line of each project's scheduled jobs, by project.
    pub(crate) job_status: &'a BTreeMap<String, String>,
    /// Each counted project's features and issues, and how many have agents working now.
    pub(crate) archive_counts: &'a BTreeMap<String, (usize, usize)>,
    /// The window's context, once a frame has run (textures are made through it).
    pub(crate) ctx: Option<eframe::egui::Context>,
    /// The watcher lists, for a screen that shows a watcher section (Chat's sidebar).
    pub(crate) watchers: Option<&'a crate::watchers_screen::WatchersScreen>,
    /// The completion cards the shell shows, for the screen that keeps them current.
    pub(crate) completions: Option<&'a mut crate::notifications::Completions>,
    /// The Indexing screen's state, for the screen that draws its sources section (Settings).
    pub(crate) indexing: Option<&'a mut crate::indexing_screen::IndexingScreen>,
    pub(crate) effects: Vec<Effect>,
}
impl Cx<'_> {
    /// The application, or why requests cannot be made now.
    pub fn online(&self) -> Result<(), &str> {
        self.online.as_ref().map(|_| ()).map_err(String::as_str)
    }
    pub fn backend(&self) -> Option<Arc<dyn Backend>> {
        self.client.as_ref().map(|c| c.backend().clone())
    }
    pub fn selected_project(&self) -> Option<&str> {
        self.selected_project.as_deref()
    }
    /// Whether a request for `slot` is in flight.
    pub fn pending<S: Clone + Into<Slot>>(&self, slot: &S) -> bool {
        let slot = slot.clone().into();
        self.client.as_ref().is_some_and(|c| c.is_pending(&slot))
    }
    /// Whether a command of `slot` is running or its outcome is still not confirmed.
    pub fn waiting<S: Clone + Into<Slot>>(&self, slot: &S) -> bool {
        let slot = slot.clone().into();
        self.client
            .as_ref()
            .is_some_and(|c| c.is_pending(&slot) || c.unresolved().iter().any(|u| u.slot == slot))
    }
    pub fn announce(&mut self, text: impl Into<String>) {
        *self.status = Some(text.into());
    }
    pub fn load_projects(&mut self) {
        self.effects.push(Effect::LoadProjects);
    }
    pub fn invalidate_routing(&mut self) {
        self.effects.push(Effect::InvalidateRouting);
    }
    pub fn load_router_catalog(&mut self) {
        self.effects.push(Effect::LoadRouterCatalog);
    }
    pub fn open_work(&mut self, id: &str) {
        self.effects.push(Effect::OpenWork(id.to_owned()));
    }
    pub fn open_session(&mut self, id: String) {
        self.effects.push(Effect::OpenSession(id));
    }
    pub fn index_status(&mut self, repository: String) {
        self.index(IndexAction::Status(repository));
    }
    pub fn index(&mut self, action: IndexAction) {
        self.effects.push(Effect::Index(action));
    }
    pub fn load_repositories(&mut self) {
        self.effects.push(Effect::LoadRepositories);
    }
    pub fn confirm_outcomes(&mut self) {
        self.effects.push(Effect::ConfirmOutcomes);
    }
    pub fn open_embedding_setup(&mut self) {
        self.effects.push(Effect::OpenEmbeddingSetup);
    }
    /// The route shown when the call started.
    pub fn route(&self) -> Route {
        self.route
    }
    pub fn navigate(&mut self, route: Route) {
        self.effects.push(Effect::Navigate(route));
    }
    pub fn into_effects(self) -> Vec<Effect> {
        self.effects
    }
    /// Submits a query; `false` when nothing was submitted.
    pub fn query(
        &mut self,
        slot: impl Into<Slot>,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) -> bool {
        let online = self.online.is_ok();
        submit_query(
            self.client.as_deref_mut(),
            online,
            slot.into(),
            label,
            op,
            frame,
        )
    }
    /// Submits the request `fetch` names for `list`, building the frame after its cursor; a
    /// request nothing took (offline, or closing) is abandoned so the list never waits for it.
    pub fn paged<T>(
        &mut self,
        list: &mut Paged<T>,
        fetch: Fetch,
        slot: impl Into<Slot>,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend, Option<&str>) -> Vec<u8>,
    ) -> bool {
        let cursor = fetch.cursor.clone();
        let sent = self.query(slot, label, op, move |b| frame(b, cursor.as_deref()));
        if !sent {
            list.abandon(fetch.generation);
        }
        sent
    }
    /// Submits a command; `false` when it was not submitted now.
    pub fn command(
        &mut self,
        slot: impl Into<Slot>,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) -> bool {
        let online = self.online.is_ok();
        submit_command(
            self.client.as_deref_mut(),
            online,
            self.status,
            slot.into(),
            label,
            op,
            frame,
        )
    }
    /// Reports a reply its screen has no handling for (`slot` is the screen's own slot).
    pub fn unhandled(&mut self, slot: &dyn Debug, reply: Reply) {
        *self.status = Some(match reply {
            Reply::Task(Err(e)) => format!("A request for {slot:?} failed: {e}"),
            _ => format!("Unexpected reply for {slot:?}"),
        });
    }
}

/// Submits a query; `false` when nothing was submitted (offline, or closing: the shell admits
/// nothing new once the window is closing).
pub(crate) fn submit_query(
    client: Option<&mut Client>,
    online: bool,
    slot: Slot,
    label: &str,
    op: &'static str,
    frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
) -> bool {
    if !online {
        return false;
    }
    let Some(client) = client else {
        return false;
    };
    let backend = client.backend().clone();
    let frame = frame(&*backend);
    client.query(slot, label, op, frame)
}

/// Submits a command; `false` when it was not submitted now (offline or closing; a closed
/// client keeps it unresolved). A caller that marked state for it restores that state.
pub(crate) fn submit_command(
    client: Option<&mut Client>,
    online: bool,
    status: &mut Option<String>,
    slot: Slot,
    label: &str,
    op: &'static str,
    frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
) -> bool {
    if !online {
        return false;
    }
    let Some(client) = client else {
        return false;
    };
    // A slot whose earlier command has no definite outcome keeps it until Confirm outcomes
    // settles that exact frame: a new request there would replace it (and could commit a
    // second time under a fresh request key).
    if client.unresolved().iter().any(|u| u.slot == slot) {
        *status = Some(format!(
            "{label}: an earlier request here is not confirmed yet. Use Confirm outcomes \
             first; nothing is applied twice."
        ));
        return false;
    }
    let backend = client.backend().clone();
    let frame = frame(&*backend);
    client.command(slot, label, op, frame)
}

/// A screen's controller for one call: the screen's state under the name of its shell field,
/// and the shell context, to which other field and method lookups fall through.
macro_rules! controller {
    ($name:ident, $field:ident: $screen:ty) => {
        pub(crate) struct $name<'s, 'a> {
            pub $field: &'s mut $screen,
            pub cx: &'s mut $crate::screen::Cx<'a>,
        }
        impl<'a> std::ops::Deref for $name<'_, 'a> {
            type Target = $crate::screen::Cx<'a>;
            fn deref(&self) -> &Self::Target {
                self.cx
            }
        }
        impl std::ops::DerefMut for $name<'_, '_> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                self.cx
            }
        }
        // Called on the controller itself, so a request's arguments may still read the screen
        // (a call through `DerefMut` would borrow the whole controller first).
        #[allow(dead_code)]
        impl $name<'_, '_> {
            fn query(
                &mut self,
                slot: impl Into<$crate::backend::Slot>,
                label: &str,
                op: &'static str,
                frame: impl FnOnce(&dyn $crate::backend::Backend) -> Vec<u8>,
            ) -> bool {
                self.cx.query(slot, label, op, frame)
            }
            fn command(
                &mut self,
                slot: impl Into<$crate::backend::Slot>,
                label: &str,
                op: &'static str,
                frame: impl FnOnce(&dyn $crate::backend::Backend) -> Vec<u8>,
            ) -> bool {
                self.cx.command(slot, label, op, frame)
            }
        }
    };
}
pub(crate) use controller;

/// The slot type of a screen that makes no requests: no slot of it exists.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NoSlot {}
impl From<NoSlot> for Slot {
    fn from(slot: NoSlot) -> Self {
        match slot {}
    }
}
impl TryFrom<Slot> for NoSlot {
    type Error = Slot;
    fn try_from(slot: Slot) -> Result<Self, Slot> {
        Err(slot)
    }
}

/// One screen. `Slot` variant names are persisted in `ui-state.json`: they stay unique across
/// every screen's slot type and never change.
pub trait Screen: 'static {
    type Slot: Into<Slot> + TryFrom<Slot, Error = Slot> + Debug;
    const ID: ScreenId;
    /// The screen was opened (`route` is one of the routes it owns).
    fn load(&mut self, cx: &mut Cx, route: Route);
    /// A hint or the periodic repair re-reads what the open screen shows.
    fn refresh(&mut self, cx: &mut Cx, route: Route);
    fn apply(&mut self, cx: &mut Cx, slot: Self::Slot, reply: Reply);
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui);
    /// The window is closing or has closed: stop work the screen runs itself.
    fn closing(&mut self) {}
    /// Requests of the screen whose outcome is not confirmed beyond the client's own commands
    /// (Confirm outcomes resubmits them).
    fn unconfirmed_count(&self) -> usize {
        0
    }
    /// The scope its content shows (Knowledge's project), told to the coordinator beside it.
    fn scope(&self) -> Option<String> {
        None
    }
    /// The Help topic of the content it shows on `route`, when that is not the route's own.
    fn help_topic(&self, _route: Route, _mem: &RouteMemory) -> Option<&'static str> {
        None
    }
    /// What the screen keeps for its unconfirmed commands (`unresolved`) across a restart.
    fn pending_memory(&self, _unresolved: &[&Unresolved]) -> Option<Value> {
        None
    }
    fn restore_pending(&mut self, _saved: &Value) {}
    /// A command restored unconfirmed at launch: mark the state its reply settles.
    fn restore_in_flight(&mut self, _slot: &Self::Slot, _u: &Unresolved) {}
    /// Pending state and unconfirmed commands are all restored: resume what they left running.
    fn restored(&mut self, _cx: &mut Cx) {}
    /// Whether the screen is shown on `route`, which another screen owns, in place of the
    /// owner's content.
    fn hosted(&self, _route: Route, _mem: &RouteMemory) -> bool {
        false
    }
    /// Unsaved editor state a project purge must see.
    fn purge_live(&self, _out: &mut Live) {}
    /// Drops state of a purged project.
    fn remove_purged(&mut self, _scope: &ProjectPurgeClientScope) {}
    fn context(&self, route: Route, mem: &RouteMemory) -> ScreenContext;
}

/// [`Screen`] without its slot type, for the shell's registry.
pub trait DynScreen {
    fn id(&self) -> ScreenId;
    fn load(&mut self, cx: &mut Cx, route: Route);
    fn refresh(&mut self, cx: &mut Cx, route: Route);
    /// Applies a reply for one of this screen's slots.
    fn apply(&mut self, cx: &mut Cx, slot: Slot, reply: Reply);
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui);
    fn pending_memory(&self, unresolved: &[&Unresolved]) -> Option<Value>;
    fn restore_pending(&mut self, saved: &Value);
    fn restore_in_flight(&mut self, u: &Unresolved);
    fn restored(&mut self, cx: &mut Cx);
    fn hosted(&self, route: Route, mem: &RouteMemory) -> bool;
    fn purge_live(&self, out: &mut Live);
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope);
    fn context(&self, route: Route, mem: &RouteMemory) -> ScreenContext;
    fn closing(&mut self);
    fn scope(&self) -> Option<String>;
    fn unconfirmed_count(&self) -> usize;
    fn help_topic(&self, route: Route, mem: &RouteMemory) -> Option<&'static str>;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
impl<S: Screen> DynScreen for S {
    fn id(&self) -> ScreenId {
        S::ID
    }
    fn load(&mut self, cx: &mut Cx, route: Route) {
        Screen::load(self, cx, route);
    }
    fn refresh(&mut self, cx: &mut Cx, route: Route) {
        Screen::refresh(self, cx, route);
    }
    fn apply(&mut self, cx: &mut Cx, slot: Slot, reply: Reply) {
        match S::Slot::try_from(slot) {
            Ok(slot) => Screen::apply(self, cx, slot, reply),
            Err(slot) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, route: Route, ui: &mut Ui) {
        Screen::ui(self, cx, route, ui);
    }
    fn pending_memory(&self, unresolved: &[&Unresolved]) -> Option<Value> {
        Screen::pending_memory(self, unresolved)
    }
    fn restore_pending(&mut self, saved: &Value) {
        Screen::restore_pending(self, saved);
    }
    fn restore_in_flight(&mut self, u: &Unresolved) {
        if let Ok(slot) = S::Slot::try_from(u.slot.clone()) {
            Screen::restore_in_flight(self, &slot, u);
        }
    }
    fn restored(&mut self, cx: &mut Cx) {
        Screen::restored(self, cx);
    }
    fn hosted(&self, route: Route, mem: &RouteMemory) -> bool {
        Screen::hosted(self, route, mem)
    }
    fn purge_live(&self, out: &mut Live) {
        Screen::purge_live(self, out);
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        Screen::remove_purged(self, scope);
    }
    fn context(&self, route: Route, mem: &RouteMemory) -> ScreenContext {
        Screen::context(self, route, mem)
    }
    fn closing(&mut self) {
        Screen::closing(self);
    }
    fn scope(&self) -> Option<String> {
        Screen::scope(self)
    }
    fn unconfirmed_count(&self) -> usize {
        Screen::unconfirmed_count(self)
    }
    fn help_topic(&self, route: Route, mem: &RouteMemory) -> Option<&'static str> {
        Screen::help_topic(self, route, mem)
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
