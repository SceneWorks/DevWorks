//! The shell: navigation, the integrated page layout, the shared conversation and its
//! app-level sidebar, Settings, the completion overlay, status and the close lifecycle. It holds
//! view state only; every record comes through [`Client`] from the runtime's gateway.
use crate::attachments;
use crate::backend::{Backend, Client, Reply, Slot, TaskOutput};
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
    notifications_open: bool,
    updates_open: bool,
    shutdown_summary: Option<String>,
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
                    state: AttachState::Stored(r),
                    preview: None,
                    preview_note: Some("Preview is not kept across restarts.".into()),
                });
            }
            chat.drafts.insert(key.clone(), draft);
        }
        chat.scrolls = Scrolls::restoring(&memory.scroll);
        let mut settings = SettingsScreen::default();
        settings.restore(&memory.settings);
        settings.scrolls = Scrolls::restoring(&memory.scroll);
        Self {
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
            notifications_open: false,
            updates_open: false,
            shutdown_summary: None,
            control: init.control,
        }
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
            _ => {}
        }
    }

    fn ensure_conversation_loaded(&mut self) {
        if !self.chat.sessions_loaded && !self.pending(&Slot::SessionsList) {
            self.load_sessions();
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
        let id = session.id.as_str().to_owned();
        let revision = session.revision.get();
        self.chat.sending = Some(id.clone());
        let sid = id.clone();
        self.command(
            Slot::Append(id),
            "Saving your message",
            "sessions.append_message",
            move |b| ops::append_message(b, &sid, revision, &text, &refs),
        );
    }

    /// Attaches files to the active conversation's draft (dialog, drop and paste all land here).
    pub fn attach(&mut self, paths: Vec<PathBuf>) {
        if self.online().is_err() || paths.is_empty() {
            return;
        }
        let waiting = self.chat.selected.is_none();
        let key = self.chat.draft_key();
        for path in paths {
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
                state: if waiting {
                    AttachState::WaitingForConversation
                } else {
                    AttachState::Storing
                },
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
            Ok(path) => self.attach(vec![path]),
            Err(e) => self.announce(format!("The pasted image could not be attached: {e}")),
        }
    }

    /// Starts storing every attachment of the selected conversation's draft that waits.
    fn store_pending(&mut self) {
        let Some(session) = self.chat.selected.clone() else {
            return;
        };
        let jobs: Vec<(u64, PathBuf, String, String)> = {
            let draft = self.chat.draft_mut();
            draft
                .attachments
                .iter_mut()
                .filter(|a| {
                    matches!(
                        a.state,
                        AttachState::Storing | AttachState::WaitingForConversation
                    )
                })
                .filter_map(|a| {
                    a.state = AttachState::Storing;
                    Some((
                        a.local,
                        a.path.clone()?,
                        a.name.clone(),
                        a.media_type.clone(),
                    ))
                })
                .collect()
        };
        let Some(b) = self.backend() else { return };
        let scratch = b.scratch_dir();
        for (local, path, name, media) in jobs {
            if self.pending(&Slot::Attach(session.clone(), local)) {
                continue;
            }
            let sid = session.clone();
            let materialized = path.starts_with(&scratch);
            if let Some(c) = &mut self.client {
                c.task(
                    Slot::Attach(session.clone(), local),
                    &format!("Storing {name}"),
                    move |b| attachments::store(b, &path, &name, &media, &sid, materialized),
                );
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
        match (slot, reply) {
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
            (Slot::SessionStart, Reply::Command(c)) => {
                let reason = self.chat.starting.take();
                match ops::started(c) {
                    Ok(record) => {
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
                match ops::appended(c) {
                    Ok(record) => {
                        self.chat.upsert(record);
                        if let Some(d) = self.chat.drafts.get_mut(&id) {
                            d.text.clear();
                            d.attachments.clear();
                        }
                        self.chat.notice = None;
                        self.announce("Message saved to the conversation. No coordinator is connected to reply (DW-16).");
                        self.load_transcript(&id);
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
            (Slot::Attach(key, local), Reply::Task(t)) => {
                let state = match t {
                    Ok(TaskOutput::Stored(c)) => match ops::saved_artifact(c) {
                        Ok(record) => AttachState::Stored(record.artifact),
                        Err(e) => AttachState::Failed(e.describe()),
                    },
                    Ok(TaskOutput::Preview { .. }) => {
                        AttachState::Failed("unexpected reply".into())
                    }
                    Err(e) => AttachState::Failed(e),
                };
                let stored = matches!(state, AttachState::Stored(_));
                if let Some(a) = self
                    .chat
                    .drafts
                    .get_mut(&key)
                    .and_then(|d| d.attachments.iter_mut().find(|a| a.local == local))
                {
                    self.status = Some(if stored {
                        format!("{} stored.", a.name)
                    } else {
                        format!("{} could not be stored.", a.name)
                    });
                    a.state = state;
                }
            }
            (Slot::Preview(key, local), Reply::Task(t)) => {
                let key_now = if self.chat.drafts.contains_key(&key) {
                    key
                } else {
                    self.chat.draft_key()
                };
                if let Some(a) = self
                    .chat
                    .drafts
                    .get_mut(&key_now)
                    .and_then(|d| d.attachments.iter_mut().find(|a| a.local == local))
                {
                    match t {
                        Ok(TaskOutput::Preview { size, rgba }) => {
                            let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                            a.preview = Some(ctx.load_texture(
                                format!("attachment-preview-{local}"),
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                        Ok(TaskOutput::Stored(_)) => {}
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
                    self.settings.after_save = false;
                    self.settings.error = Some(e.describe());
                }
            },
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
                self.settings.saving = false;
                match ops::settings_saved(c) {
                    Ok(()) => {
                        self.settings.after_save = true;
                        self.settings.message = Some((Tone::Good, "Settings saved.".into()));
                        self.announce("Settings saved.");
                        self.load_settings();
                    }
                    Err(e) => {
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
            (Slot::SettingsExplain(key), Reply::Query(q)) => match ops::settings_chain(q) {
                Ok(chain) => {
                    if let Some(v) = &mut self.settings.view {
                        v.apply_explanation(&key, &chain);
                    }
                    self.settings.explained.insert(key);
                }
                Err(e) => self.settings.message = Some((Tone::Bad, e.describe())),
            },
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
                .rev()
                .find(|s| !s.closed)
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
        match self.route {
            Route::Artifacts if !self.pending(&Slot::ArtifactsList) => self.load_artifacts(),
            Route::Resources if !self.pending(&Slot::ResourcesList) => self.load_resources(),
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
                ChatAction::Remove(local) => {
                    let d = self.chat.draft_mut();
                    d.attachments.retain(|a| a.local != local);
                }
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
                    if let Some(c) = &mut self.client {
                        c.command(
                            Slot::SettingsSave,
                            "Saving settings",
                            "settings.save_revision",
                            frame,
                        );
                    }
                }
                SettingsAction::Explain(key) => {
                    let k = key.clone();
                    self.query(
                        Slot::SettingsExplain(key),
                        "Explaining a setting",
                        "settings.explain_source",
                        move |b| ops::settings_explain(b, &k),
                    );
                }
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
                let idle = self
                    .client
                    .as_ref()
                    .is_none_or(|c| c.activities().is_empty());
                if idle || since.elapsed() > QUIESCE_BOUND {
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
            sidebar_tab: self.chat.tab,
            history_query: self.chat.history_query.clone(),
            drafts: self
                .chat
                .drafts
                .iter()
                .map(|(k, d)| (k.clone(), d.memory()))
                .filter(|(_, d)| !d.text.is_empty() || !d.attachments.is_empty())
                .collect(),
            routes: self.routes.clone(),
            settings: self.settings.memory(),
            scroll: [&self.scrolls, &self.chat.scrolls, &self.settings.scrolls]
                .into_iter()
                .flat_map(|s| s.offsets().iter().map(|(k, v)| (k.clone(), *v)))
                .collect(),
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
            self.load_route();
        }
        self.advance_lifecycle(&ctx);
        self.drain(&ctx);
        if self.lifecycle == Lifecycle::Open {
            self.poll_hints();
            self.input(&ctx);
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
        Panel::bottom("statusbar")
            .exact_size(28.0)
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
                        let r = ui.label(
                            RichText::new(status.unwrap_or_default())
                                .small()
                                .color(theme::MUTED),
                        );
                        ui.ctx().accesskit_node_builder(r.id, |n| {
                            n.set_live(accesskit::Live::Polite);
                        });
                    });
                });
            });
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
                            ui.add_enabled(false, egui::Button::new("No project selected"))
                                .on_disabled_hover_text(
                                    "Projects arrive with DW-11; project screens show what \
                                     they will hold until then.",
                                );
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
        Panel::right("coordinator-sidebar")
            .resizable(true)
            .default_size(380.0)
            .size_range(300.0..=560.0)
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
            let step = match &self.lifecycle {
                Lifecycle::Quiescing { .. } => "Finishing requests in flight…".to_owned(),
                Lifecycle::Persisting => "Saving the window state…".to_owned(),
                Lifecycle::ShuttingDown => {
                    "Stopping delivery, collection and owned processes (at most 30 s)…".to_owned()
                }
                Lifecycle::Exiting { summary } => summary.clone(),
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
