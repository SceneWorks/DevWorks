//! The shell: navigation, the integrated page layout, the shared conversation and its
//! app-level sidebar, Settings, the completion overlay, status and the close lifecycle. It holds
//! view state only; every record comes through [`Client`] from the runtime's gateway.
use crate::attachments;
use crate::backend::{Backend, Client, Reply, Slot, TaskOutput, Unresolved, is_unknown};
use crate::chat::{self, AttachState, Attachment, Chat, ChatAction, StartReason, Surface};
use crate::memory::{Memory, RouteMemory};
use crate::notifications::{CardAction, Completions};
use crate::ops::{self, Failure};
use crate::route::{NAV_ORDER, NavGroup, Route};
use crate::settings_screen::{KEEP_AWAKE, SettingsAction, SettingsScreen};
use crate::theme;
use crate::virtual_list::{self, Scrolls};
use crate::widgets::{self, Header, Tone, ViewState, chip};
use devworks_core::protocol::DomainErrorCode;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{ArtifactRecord, ResourceRecord};
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

pub mod projects;

/// Native file and folder choosers (rfd); replaced by scripted choices in tests.
pub trait Dialogs {
    fn pick_files(&mut self) -> Vec<PathBuf>;
    fn pick_folder(&mut self, start: Option<&str>) -> Option<PathBuf>;
}

/// The platform dialogs.
pub struct NativeDialogs;
impl Dialogs for NativeDialogs {
    fn pick_files(&mut self) -> Vec<PathBuf> {
        rfd::FileDialog::new()
            .set_title("Attach images or files")
            .pick_files()
            .unwrap_or_default()
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
    routes: BTreeMap<Route, RouteMemory>,
    pub chat: Chat,
    pub settings: SettingsScreen,
    pub artifacts: ListState<ArtifactRecord>,
    pub resources: ListState<ResourceRecord>,
    /// All projects, the project selector and Repositories (DW-11).
    pub projects: projects::ProjectsUi,
    /// The project the navigation selected; its screens follow it.
    selected_project: Option<String>,
    pub completions: Completions,
    scrolls: Scrolls,
    lifecycle: Lifecycle,
    status: Option<String>,
    hints: Option<watch::Receiver<u64>>,
    started: bool,
    saved: Option<Memory>,
    last_save: Instant,
    last_poll: Instant,
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
            routes: memory.routes.clone(),
            chat,
            settings,
            artifacts: ListState::default(),
            resources: ListState::default(),
            projects: projects::ProjectsUi::default(),
            selected_project: memory.selected_project.clone(),
            completions: Completions::default(),
            scrolls: Scrolls::restoring(&memory.scroll),
            lifecycle: Lifecycle::Open,
            status: None,
            hints,
            started: false,
            saved: Some(memory.clone()),
            last_save: Instant::now(),
            last_poll: Instant::now(),
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
            self.ensure_conversation_loaded();
        }
    }

    fn load_route(&mut self) {
        match self.route {
            Route::Chat => self.ensure_conversation_loaded(),
            Route::Settings if self.settings.view.is_none() && !self.settings.loading => {
                self.load_settings();
            }
            Route::Artifacts => self.load_artifacts(),
            Route::Resources => self.load_resources(),
            Route::Portfolio => {
                self.load_projects();
                self.load_deleted_projects();
                self.load_repositories();
            }
            Route::Repositories => {
                self.load_projects();
                self.load_repositories();
            }
            _ => {}
        }
    }

    fn ensure_conversation_loaded(&mut self) {
        if !self.chat.sessions_loaded && !self.pending(&Slot::SessionsList) {
            self.load_sessions();
        }
        if !self.chat.routing.catalog_loaded && !self.pending(&Slot::RouterCatalog) {
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

    fn query(
        &mut self,
        slot: Slot,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) {
        if self.online().is_err() {
            return;
        }
        let Some(b) = self.backend() else { return };
        let frame = frame(&*b);
        if let Some(c) = &mut self.client {
            c.query(slot, label, op, frame);
        }
    }
    fn command(
        &mut self,
        slot: Slot,
        label: &str,
        op: &'static str,
        frame: impl FnOnce(&dyn Backend) -> Vec<u8>,
    ) {
        if self.online().is_err() {
            return;
        }
        let Some(b) = self.backend() else { return };
        let frame = frame(&*b);
        if let Some(c) = &mut self.client {
            c.command(slot, label, op, frame);
        }
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
        self.settings.loading = true;
        self.query(
            Slot::SettingsRead,
            "Loading settings",
            "settings.read_effective",
            ops::settings_read,
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
    fn load_resources(&mut self) {
        self.query(
            Slot::ResourcesList,
            "Loading resources",
            "resources.list",
            ops::resources_list,
        );
    }

    fn load_router_catalog(&mut self) {
        self.query(
            Slot::RouterCatalog,
            "Loading models and tiers",
            "router.catalog",
            ops::router_catalog,
        );
    }

    /// Asks the router about the active conversation's selection (and pin).
    fn refresh_route_status(&mut self) {
        // A newer query supersedes one still running (its selection may be stale).
        let key = self.chat.draft_key();
        let sel = self.chat.selection();
        let session = (key != chat::NEW_DRAFT).then(|| key.clone());
        self.query(
            Slot::RouteStatus(key),
            "Checking model availability",
            "router.availability",
            move |b| ops::router_availability(b, &sel.intent, &sel.effort, session.as_deref()),
        );
    }

    /// A new route or effort for the active conversation: the explicit `router.pin` change of a
    /// pinned conversation, otherwise the selection its first routed request uses.
    fn select_route(&mut self, sel: crate::routing::Selection) {
        let key = self.chat.draft_key();
        if self.chat.selection() == sel {
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
                if self.pending(&Slot::RoutePin(key.clone())) {
                    self.chat.notice = Some((
                        Tone::Attention,
                        "The previous model change is still being saved; choose again once it \
                         is confirmed."
                            .into(),
                    ));
                    return;
                }
                self.chat
                    .routing
                    .selections
                    .insert(key.clone(), sel.clone());
                let session = key.clone();
                self.command(
                    Slot::RoutePin(key),
                    "Changing the conversation's model",
                    "router.pin",
                    move |b| ops::router_pin(b, &session, revision, &sel.intent, &sel.effort),
                );
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
        let Some(record) = self.chat.session(session) else {
            return;
        };
        if pinned != Some(false)
            || self.pending(&Slot::RouteFirst(session.to_owned()))
            || self.pending(&Slot::RoutePin(session.to_owned()))
        {
            return;
        }
        let epoch = record.context_epoch.get();
        let sel = self
            .chat
            .routing
            .selections
            .get(session)
            .cloned()
            .unwrap_or_default();
        let sid = session.to_owned();
        self.command(
            Slot::RouteFirst(session.to_owned()),
            "Routing the conversation",
            "router.route",
            move |b| ops::router_route(b, &sid, epoch, &sel.intent, &sel.effort),
        );
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
        let name = format!("{}.png", b.token("pasted-image"));
        match attachments::materialize_paste(image, &b.scratch_dir(), &name) {
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
        let Some((slot, reply)) = self.apply_projects(slot, reply) else {
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
                }
                Err(e) => self.chat.sessions_error = Some(e.describe()),
            },
            (Slot::RouterCatalog, Reply::Query(q)) => match ops::route_models(q) {
                Ok(models) => {
                    self.chat.routing.catalog = models;
                    self.chat.routing.catalog_loaded = true;
                    self.chat.routing.catalog_error = None;
                }
                Err(e) => {
                    self.chat.routing.catalog_loaded = true;
                    self.chat.routing.catalog_error =
                        Some(format!("Named models are unavailable: {}", e.describe()));
                }
            },
            (Slot::RouteStatus(key), Reply::Query(q)) => {
                let status = match ops::availability(q) {
                    Ok(a) => {
                        // A pinned conversation's selection is its durable pin.
                        if let Some(pin) = &a.pin
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
                self.chat.routing.status.insert(key, status);
            }
            (Slot::RouteFirst(key), Reply::Command(c)) => {
                if matches!(ops::routed(c), Err(Failure::Unknown)) {
                    // Confirmed with its identical frame by Confirm outcomes or the next launch.
                    return;
                }
                // Pinned, or refused because nothing is eligible: the status line says which.
                self.chat.routing.status.remove(&key);
                if self.chat.draft_key() == key {
                    self.refresh_route_status();
                }
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
                // The pin (or the unchanged one) is re-read; its selection is adopted.
                self.chat.routing.status.remove(&key);
                if self.chat.draft_key() == key {
                    self.refresh_route_status();
                }
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
            (Slot::Transcript(id), Reply::Query(q)) => match ops::transcript_page(q) {
                Ok(page) => {
                    let more = self
                        .chat
                        .transcripts
                        .entry(id.clone())
                        .or_default()
                        .apply(page);
                    if more {
                        self.load_transcript(&id);
                    }
                }
                Err(e) => {
                    let t = self.chat.transcripts.entry(id).or_default();
                    t.loading = false;
                    t.error = Some(e.describe());
                }
            },
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
                    Ok(TaskOutput::Preview { .. } | TaskOutput::Prepared { .. }) => {
                        AttachState::Failed("unexpected reply".into())
                    }
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
                        Ok(TaskOutput::Stored { .. } | TaskOutput::Prepared { .. }) => {}
                        Err(e) => a.preview_note = Some(format!("No preview: {e}")),
                    }
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
        if self.route == Route::Chat || self.coordinator_open {
            self.refresh_route_status();
        }
        match self.route {
            Route::Artifacts if !self.pending(&Slot::ArtifactsList) => self.load_artifacts(),
            Route::Resources if !self.pending(&Slot::ResourcesList) => self.load_resources(),
            Route::Portfolio | Route::Repositories
                if !self.pending(&Slot::ProjectsList) && !self.pending(&Slot::RepositoriesList) =>
            {
                self.load_route();
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
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Lifecycle

    /// Begins the close lifecycle (the window's close button lands here).
    pub fn request_close(&mut self) {
        if self.lifecycle == Lifecycle::Open {
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
            commands,
        }
    }

    /// Puts back what [`Self::pending_memory`] kept; the first frame confirms it.
    fn restore_pending(&mut self, pending: crate::memory::PendingMemory) {
        for c in pending.commands {
            let slot = c.slot.clone();
            let Some(u) = Unresolved::restored(c.slot, c.label, &c.operation, c.frame) else {
                continue;
            };
            match &slot {
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
                Slot::SettingsSave => {
                    self.settings.saving = true;
                    self.settings.unconfirmed = true;
                }
                Slot::ProjectSettingsSave(p) => {
                    self.projects.base.entry(p.clone()).or_default().saving = true;
                }
                _ => {}
            }
            if let Some(client) = &mut self.client {
                client.keep_unresolved(u);
            }
        }
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
            // The navigation's project selector lists the projects on every screen.
            if self.route != Route::Portfolio && self.route != Route::Repositories {
                self.load_projects();
            }
            self.load_route();
            self.restore_previews();
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
        if self.lifecycle == Lifecycle::Open {
            self.poll_hints();
            self.input(&ctx);
        }
        if self.lifecycle == Lifecycle::Open {
            self.route_status_due(&ctx);
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
        if self.lifecycle == Lifecycle::Open && self.last_save.elapsed() > Duration::from_secs(3) {
            self.save_memory();
        }
        // Periodic re-query of visible data (missed-hint repair) needs a frame now and then.
        ctx.request_repaint_after(Duration::from_secs(5));
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
                self.refresh_route_status();
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
                        if ui.button("Help").clicked() {
                            self.navigate(Route::Help);
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
                        || (r == Route::Agents && current == Route::Resources)
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
                self.settings.ui(
                    ui,
                    online.as_ref().map(|_| ()).map_err(String::as_str),
                    &mut actions,
                );
                self.settings_actions(actions);
            }
            Route::Artifacts => self.artifacts_screen(ui),
            Route::Resources => self.resources_screen(ui),
            Route::Portfolio => self.portfolio_screen(ui),
            Route::Repositories => self.repositories_screen(ui),
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

    fn resources_screen(&mut self, ui: &mut Ui) {
        let screen = Route::Resources.screen();
        let online = self.online();
        let mut reload = false;
        let mut mem = self.routes.remove(&Route::Resources).unwrap_or_default();
        let list = &self.resources;
        let scrolls = &mut self.scrolls;
        let mut st = (&mut reload, &mut mem);
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
        if reload {
            self.load_resources();
        }
    }

    fn overlays(&mut self, ctx: &egui::Context) {
        let card_actions = self.completions.ui(ctx, TOOLBAR + 8.0);
        for a in card_actions {
            if let CardAction::Open(id) = a {
                self.announce(format!(
                    "Opening completed work {id} arrives with its work screens (DW-12)."
                ));
            }
        }
        if let Some(text) = self.completions.take_announcement() {
            self.announce(text);
        }
        if self.notifications_open {
            let mut open = true;
            egui::Window::new("Notifications")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, TOOLBAR + 4.0))
                .show(ctx, |ui| {
                    ui.label(format!(
                        "{} completion card{} on screen.",
                        self.completions.cards().len(),
                        if self.completions.cards().len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ));
                    widgets::state_block(
                        ui,
                        &ViewState::Unavailable {
                            what: "notification history, read state and filters".into(),
                            owner: "DW-14".into(),
                        },
                    );
                });
            self.notifications_open = open;
        }
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
