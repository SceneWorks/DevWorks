//! The shared conversation components (UI-01..04, UI-08/09, UI-11..13): `ConversationView` and
//! `ChatComposer` are the same code on the Home screen and in the app-level coordinator sidebar,
//! parameterized by the selected session, so both surfaces show one conversation with one draft,
//! one set of pending attachments and one transcript.
//!
//! Bound services: `sessions.list` / `.start` / `.transcript` / `.append_message` and
//! `artifacts.save` (workspace scope, conversation-tagged). Not bound here, and shown honestly as
//! unavailable: the coordinator agent and inference (DW-16), router tiers, named models and
//! effort (DW-17), Current's app-wide work status (DW-12, DW-14) and conversation dates for
//! history grouping (the session record carries none yet).
use crate::memory::{DraftMemory, SidebarTab, StoredAttachment};
use crate::ops::Failure;
use crate::theme;
use crate::virtual_list::{self, Blocks, Scrolls};
use crate::widgets::{self, Tone, ViewState, chip};
use devworks_core::protocol::provider::{
    ArtifactRef, TranscriptBody, TranscriptEntry, TranscriptPage, UserContent,
};
use devworks_core::protocol::records::SessionRecord;
use eframe::egui::{self, Align, Key, Layout, Modifiers, RichText, TextureHandle, Ui};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Draft key of the conversation not started yet.
pub const NEW_DRAFT: &str = "new";
/// Accessible name and tooltip of the plus-only attachment control (UI-08).
pub const ATTACH_LABEL: &str = "Attach images or files";
pub const COMPOSER_LABEL: &str = "Message the workspace coordinator";
pub const SEND_LABEL: &str = "Send message";
/// The coordinator is not part of this build: say so wherever a reply would be expected.
pub const NO_COORDINATOR: &str = "The workspace coordinator is not connected in this build \
    (DW-16). Messages are saved to this conversation, but no model receives them and no reply \
    will appear.";

#[derive(Clone, Debug)]
pub enum AttachState {
    /// Waiting for the conversation to exist (its ID is the attachment's provenance tag).
    WaitingForConversation,
    Storing,
    Stored(ArtifactRef),
    Failed(String),
}

pub struct Attachment {
    pub local: u64,
    pub name: String,
    pub media_type: String,
    /// The picked or materialized file (absent once restored from memory).
    pub path: Option<PathBuf>,
    pub state: AttachState,
    pub preview: Option<TextureHandle>,
    pub preview_note: Option<String>,
}
impl Attachment {
    pub fn is_image(&self) -> bool {
        self.media_type.starts_with("image/")
    }
}

#[derive(Default)]
pub struct Draft {
    pub text: String,
    pub attachments: Vec<Attachment>,
}
impl Draft {
    pub fn stored(&self) -> Vec<ArtifactRef> {
        self.attachments
            .iter()
            .filter_map(|a| match &a.state {
                AttachState::Stored(r) => Some(r.clone()),
                _ => None,
            })
            .collect()
    }
    pub fn busy(&self) -> bool {
        self.attachments.iter().any(|a| {
            matches!(
                a.state,
                AttachState::Storing | AttachState::WaitingForConversation
            )
        })
    }
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.attachments.is_empty()
    }
    pub fn memory(&self) -> DraftMemory {
        DraftMemory {
            text: self.text.clone(),
            attachments: self
                .attachments
                .iter()
                .filter_map(|a| match &a.state {
                    AttachState::Stored(r) => Some(StoredAttachment {
                        name: a.name.clone(),
                        media_type: a.media_type.clone(),
                        artifact: serde_json::to_value(r).ok()?,
                    }),
                    _ => None,
                })
                .collect(),
        }
    }
}

/// An incrementally projected transcript: pages are appended after the tail cursor, so a
/// refresh fetches only new entries.
#[derive(Default)]
pub struct Transcript {
    pub entries: Vec<TranscriptEntry>,
    pub cursor: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub loaded: bool,
    pub blocks: Blocks,
}
impl Transcript {
    /// Applies one page; returns whether more pages follow at once.
    pub fn apply(&mut self, page: TranscriptPage) -> bool {
        let last = self.entries.last().map_or(0, |e| e.sequence.get());
        self.entries
            .extend(page.entries.into_iter().filter(|e| e.sequence.get() > last));
        self.cursor = Some(page.next_cursor);
        self.loading = false;
        self.loaded = true;
        self.error = None;
        page.has_more
    }
}

/// Why a session is being started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartReason {
    NewChat,
    /// Started so a pending send or attachment has a conversation.
    ForDraft,
}

pub struct Chat {
    pub sessions: Vec<SessionRecord>,
    pub sessions_loaded: bool,
    pub sessions_error: Option<String>,
    pub sessions_more: bool,
    pub selected: Option<String>,
    pub tab: SidebarTab,
    pub history_query: String,
    pub drafts: BTreeMap<String, Draft>,
    pub transcripts: HashMap<String, Transcript>,
    pub starting: Option<StartReason>,
    /// Send once the started session exists.
    pub send_after_start: bool,
    pub sending: Option<String>,
    pub notice: Option<(Tone, String)>,
    /// The overlay sidebar is shown on narrow Home layouts.
    pub narrow_sidebar_open: bool,
    pub scrolls: Scrolls,
    next_local: u64,
}

impl Chat {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            sessions_loaded: false,
            sessions_error: None,
            sessions_more: false,
            selected: None,
            tab: SidebarTab::Current,
            history_query: String::new(),
            drafts: BTreeMap::new(),
            transcripts: HashMap::new(),
            starting: None,
            send_after_start: false,
            sending: None,
            notice: None,
            narrow_sidebar_open: false,
            scrolls: Scrolls::default(),
            next_local: 0,
        }
    }
    pub fn next_local(&mut self) -> u64 {
        self.next_local += 1;
        self.next_local
    }
    /// The draft key of the active conversation.
    pub fn draft_key(&self) -> String {
        self.selected
            .clone()
            .unwrap_or_else(|| NEW_DRAFT.to_owned())
    }
    pub fn draft(&self) -> Option<&Draft> {
        self.drafts.get(&self.draft_key())
    }
    pub fn draft_mut(&mut self) -> &mut Draft {
        let key = self.draft_key();
        self.drafts.entry(key).or_default()
    }
    pub fn session(&self, id: &str) -> Option<&SessionRecord> {
        self.sessions.iter().find(|s| s.id.as_str() == id)
    }
    pub fn selected_session(&self) -> Option<&SessionRecord> {
        self.selected.as_deref().and_then(|s| self.session(s))
    }
    /// Sessions matching the Chat History search (title words, case-insensitive).
    pub fn filtered(&self) -> Vec<&SessionRecord> {
        let terms: Vec<String> = self
            .history_query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.sessions
            .iter()
            .filter(|s| {
                let t = s.title.0.to_lowercase();
                terms.iter().all(|w| t.contains(w))
            })
            .collect()
    }
    /// Moves the not-yet-started draft onto a started session.
    pub fn adopt_new_draft(&mut self, session: &str) {
        if let Some(d) = self.drafts.remove(NEW_DRAFT) {
            let target = self.drafts.entry(session.to_owned()).or_default();
            if target.text.is_empty() {
                target.text = d.text;
            }
            target.attachments.extend(d.attachments);
        }
    }
    pub fn upsert(&mut self, record: SessionRecord) {
        match self.sessions.iter_mut().find(|s| s.id == record.id) {
            Some(s) => *s = record,
            None => self.sessions.push(record),
        }
    }
}

impl Default for Chat {
    fn default() -> Self {
        Self::new()
    }
}

/// What the user asked the conversation surfaces to do; the shell turns each into requests.
#[derive(Clone, Debug, PartialEq)]
pub enum ChatAction {
    NewChat,
    Select(String),
    Send,
    AttachDialog,
    Attach(Vec<PathBuf>),
    Remove(u64),
    OpenInstructions,
    RetrySessions,
    RetryTranscript,
    CloseSidebar,
}

/// Which surface renders the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Home,
    Sidebar,
}
impl Surface {
    fn salt(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Sidebar => "sidebar",
        }
    }
}

/// The conversation transcript and its composer.
pub fn conversation(
    ui: &mut Ui,
    chat: &mut Chat,
    surface: Surface,
    online: Result<(), &str>,
    actions: &mut Vec<ChatAction>,
) {
    let composer_height = if surface == Surface::Home {
        150.0
    } else {
        170.0
    };
    let transcript_height = (ui.available_height() - composer_height).max(80.0);
    ui.allocate_ui(egui::vec2(ui.available_width(), transcript_height), |ui| {
        ui.set_min_height(transcript_height);
        transcript_view(ui, chat, surface, online, actions);
    });
    composer(ui, chat, surface, online, actions);
}

fn transcript_view(
    ui: &mut Ui,
    chat: &mut Chat,
    surface: Surface,
    online: Result<(), &str>,
    actions: &mut Vec<ChatAction>,
) {
    if let Err(reason) = online {
        widgets::state_block(
            ui,
            &ViewState::Offline {
                reason: reason.to_owned(),
            },
        );
        return;
    }
    let Some(id) = chat.selected.clone() else {
        empty_invitation(ui, surface);
        return;
    };
    let Some(t) = chat.transcripts.get_mut(&id) else {
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "the conversation".into(),
            },
        );
        return;
    };
    if let Some(e) = &t.error {
        let state = if t.loaded {
            ViewState::Stale {
                what: "messages".into(),
            }
        } else {
            ViewState::Failed {
                what: "the conversation".into(),
                error: e.clone(),
            }
        };
        if widgets::state_block(ui, &state) == widgets::StateAction::Retry {
            actions.push(ChatAction::RetryTranscript);
        }
        if !t.loaded {
            return;
        }
    }
    if !t.loaded {
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "the conversation".into(),
            },
        );
        return;
    }
    if t.entries.is_empty() {
        empty_invitation(ui, surface);
        return;
    }
    let entries = &t.entries;
    let width = ui.available_width();
    t.blocks.show(
        ui,
        &mut chat.scrolls,
        &format!("transcript-{}-{id}", surface.salt()),
        entries.len() + 1,
        true,
        |ui, i| {
            ui.set_width(width - 8.0);
            if i == entries.len() {
                // The honest coordinator status sits after the latest message.
                widgets::caption(ui, NO_COORDINATOR);
                return;
            }
            entry_block(ui, &entries[i]);
        },
    );
}

fn empty_invitation(ui: &mut Ui, surface: Surface) {
    ui.add_space(if surface == Surface::Home { 48.0 } else { 16.0 });
    ui.vertical(|ui| {
        ui.label(
            RichText::new("What would you like to work on?")
                .size(if surface == Surface::Home { 24.0 } else { 17.0 })
                .strong(),
        );
        ui.label(
            RichText::new(
                "Plan a feature, check a pull request, or steer your agents. One conversation \
                 across all your projects.",
            )
            .color(theme::MUTED),
        );
        ui.add_space(8.0);
        widgets::caption(ui, NO_COORDINATOR);
    });
}

fn entry_block(ui: &mut Ui, entry: &TranscriptEntry) {
    let (who, fill) = match &entry.body {
        TranscriptBody::UserMessage { .. } => ("You", theme::RAISED),
        TranscriptBody::AssistantAttempt { .. } => ("Coordinator", theme::PANEL),
        TranscriptBody::HostNotice { .. } => ("DevWorks", theme::PANEL),
        _ => ("Activity", theme::PANEL),
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(who).strong());
                widgets::caption(ui, format!("#{}", entry.sequence.get()));
            });
            match &entry.body {
                TranscriptBody::UserMessage { content } => {
                    for c in content {
                        match c {
                            UserContent::Text { text } => {
                                ui.label(text);
                            }
                            UserContent::Attachment { artifact } => {
                                chip(
                                    ui,
                                    format!(
                                        "Attachment · {} · {}",
                                        artifact.media_type,
                                        bytes_label(artifact.byte_length.get())
                                    ),
                                    Tone::Accent,
                                );
                            }
                        }
                    }
                }
                TranscriptBody::AssistantAttempt {
                    visible_text,
                    interrupted,
                    ..
                } => {
                    ui.label(visible_text);
                    if *interrupted {
                        chip(ui, "Interrupted", Tone::Attention);
                    }
                }
                TranscriptBody::ToolCallRecorded { .. } => {
                    ui.label(RichText::new("Tool call recorded").color(theme::MUTED));
                }
                TranscriptBody::ToolResponseRecorded { .. } => {
                    ui.label(RichText::new("Tool result recorded").color(theme::MUTED));
                }
                TranscriptBody::HostNotice { .. } => {
                    ui.label(RichText::new("Host notice").color(theme::MUTED));
                }
                TranscriptBody::NativeItem { .. } => {
                    ui.label(RichText::new("Provider continuation material").color(theme::DIM));
                }
                TranscriptBody::ContextEpochChanged { reason, .. } => {
                    ui.label(
                        RichText::new(format!("Context changed: {reason:?}")).color(theme::MUTED),
                    );
                }
                TranscriptBody::OperationUpdate { operation_id, .. } => {
                    ui.label(
                        RichText::new(format!("Operation {} updated", operation_id.0.as_str()))
                            .color(theme::MUTED),
                    );
                }
            }
        });
}

pub fn bytes_label(n: i64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / f64::from(1 << 20)),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// The composer: pending attachments, the input, the controls aligned with Send along its
/// bottom edge, and the scope / pinned model / shortcut line below its border (UI-02).
fn composer(
    ui: &mut Ui,
    chat: &mut Chat,
    surface: Surface,
    online: Result<(), &str>,
    actions: &mut Vec<ChatAction>,
) {
    let enabled = online.is_ok() && chat.sending.is_none();
    let input_id = egui::Id::unique(("composer", surface.salt()));
    // The send shortcut is taken before the multiline input can turn it into a newline.
    let focused = ui.ctx().memory(|m| m.has_focus(input_id));
    if enabled && focused && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
        actions.push(ChatAction::Send);
    }
    egui::Frame::new()
        .fill(theme::RAISED)
        .stroke(theme::line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let draft = chat.draft_mut();
            if !draft.attachments.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for a in &draft.attachments {
                        attachment_chip(ui, a, actions);
                    }
                });
            }
            let input = egui::TextEdit::multiline(&mut draft.text)
                .id(input_id)
                .hint_text("Ask about any project, feature, PR, or agent…")
                .desired_rows(if surface == Surface::Home { 2 } else { 3 })
                .desired_width(f32::INFINITY)
                .frame(egui::Frame::NONE);
            let response = ui.add_enabled(online.is_ok(), input);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, online.is_ok(), COMPOSER_LABEL)
            });
            ui.horizontal_wrapped(|ui| {
                if widgets::icon_button(
                    ui,
                    widgets::Icon::Plus,
                    ATTACH_LABEL,
                    ATTACH_LABEL,
                    online.is_ok(),
                    None,
                )
                .clicked()
                {
                    actions.push(ChatAction::AttachDialog);
                }
                ui.add_enabled_ui(false, |ui| {
                    egui::ComboBox::from_id_salt(("model", surface.salt()))
                        .selected_text("Model: not configured")
                        .show_ui(ui, |_| {})
                        .response
                        .on_disabled_hover_text(
                            "Router tiers and named models arrive with DW-17; until then no \
                             model is selected or pinned.",
                        )
                        .widget_info_labeled("Model or tier");
                    egui::ComboBox::from_id_salt(("effort", surface.salt()))
                        .selected_text("Effort: default")
                        .show_ui(ui, |_| {})
                        .response
                        .on_disabled_hover_text("Effort selection arrives with DW-17.")
                        .widget_info_labeled("Effort");
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let draft = chat.draft();
                    let ready = draft.is_some_and(|d| !d.is_empty() && !d.busy());
                    let why = if online.is_err() {
                        "The runtime is offline."
                    } else if chat.sending.is_some() {
                        "Saving the previous message…"
                    } else if draft.is_some_and(Draft::busy) {
                        "Waiting for attachments to be stored."
                    } else {
                        "Write a message or attach a file first."
                    };
                    let send = ui
                        .add_enabled(enabled && ready, egui::Button::new("Send"))
                        .on_disabled_hover_text(why)
                        .widget_info_labeled(SEND_LABEL);
                    if send.clicked() {
                        actions.push(ChatAction::Send);
                    }
                });
            });
        });
    ui.horizontal_wrapped(|ui| {
        widgets::caption(ui, "All projects");
        widgets::caption(ui, "Pinned: no model (DW-17)");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            widgets::caption(ui, "Cmd / Ctrl + Enter to send");
        });
    });
}

fn attachment_chip(ui: &mut Ui, a: &Attachment, actions: &mut Vec<ChatAction>) {
    egui::Frame::new()
        .fill(theme::PANEL)
        .stroke(theme::soft_line())
        .corner_radius(theme::radius())
        .inner_margin(theme::margin(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(t) = &a.preview {
                    ui.add(
                        egui::Image::new(t)
                            .max_size(egui::vec2(48.0, 48.0))
                            .alt_text(format!("Preview of {}", a.name)),
                    );
                }
                ui.vertical(|ui| {
                    ui.label(RichText::new(&a.name).small());
                    let (text, tone) = match &a.state {
                        AttachState::WaitingForConversation => {
                            ("Waiting for the conversation".to_owned(), Tone::Neutral)
                        }
                        AttachState::Storing => ("Storing…".to_owned(), Tone::Neutral),
                        AttachState::Stored(r) => (
                            format!("Stored · {}", bytes_label(r.byte_length.get())),
                            Tone::Good,
                        ),
                        AttachState::Failed(e) => (format!("Failed: {e}"), Tone::Bad),
                    };
                    chip(ui, text, tone);
                    if let Some(note) = &a.preview_note {
                        widgets::caption(ui, note.clone());
                    }
                });
                let label = format!("Remove attachment {}", a.name);
                if ui
                    .small_button("×")
                    .on_hover_text(&label)
                    .widget_info_labeled(&label)
                    .clicked()
                {
                    actions.push(ChatAction::Remove(a.local));
                }
            });
        });
}

/// The Home header: New Chat beside Coordinator instructions, reachable with either sidebar tab.
pub fn header_actions(ui: &mut Ui, online: bool, actions: &mut Vec<ChatAction>) {
    if widgets::secondary_button(ui, "Coordinator instructions").clicked() {
        actions.push(ChatAction::OpenInstructions);
    }
    if ui
        .add_enabled(online, egui::Button::new("New Chat"))
        .on_disabled_hover_text("The runtime is offline.")
        .clicked()
    {
        actions.push(ChatAction::NewChat);
    }
}

/// The Home right sidebar: Current first, Chat History second.
pub fn sidebar(
    ui: &mut Ui,
    chat: &mut Chat,
    online: Result<(), &str>,
    actions: &mut Vec<ChatAction>,
) {
    ui.horizontal(|ui| {
        if ui
            .selectable_label(chat.tab == SidebarTab::Current, "Current")
            .clicked()
        {
            chat.tab = SidebarTab::Current;
        }
        if ui
            .selectable_label(chat.tab == SidebarTab::History, "Chat History")
            .clicked()
        {
            chat.tab = SidebarTab::History;
        }
    });
    widgets::divider(ui);
    ui.add_space(6.0);
    match chat.tab {
        SidebarTab::Current => current(ui),
        SidebarTab::History => history(ui, chat, online, actions),
    }
}

fn current(ui: &mut Ui) {
    widgets::caption(ui, "Across all projects");
    for (title, what, owner) in [
        (
            "Needs you",
            "decisions and approvals waiting for you",
            "DW-14",
        ),
        (
            "Working",
            "active features, issues and agents",
            "DW-12, DW-15",
        ),
        ("Recently completed", "verified completions", "DW-12, DW-14"),
    ] {
        ui.add_space(6.0);
        ui.label(RichText::new(title).strong());
        widgets::state_block(
            ui,
            &ViewState::Unavailable {
                what: what.into(),
                owner: owner.into(),
            },
        );
    }
}

fn history(ui: &mut Ui, chat: &mut Chat, online: Result<(), &str>, actions: &mut Vec<ChatAction>) {
    ui.add(
        egui::TextEdit::singleline(&mut chat.history_query)
            .hint_text("Search conversations")
            .desired_width(f32::INFINITY),
    )
    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "Search conversations"));
    widgets::caption(
        ui,
        "Grouped by date once conversations record their times (DW-16).",
    );
    if let Err(reason) = online {
        widgets::state_block(
            ui,
            &ViewState::Offline {
                reason: reason.to_owned(),
            },
        );
        return;
    }
    if let Some(e) = &chat.sessions_error {
        let state = ViewState::Failed {
            what: "conversations".into(),
            error: e.clone(),
        };
        if widgets::state_block(ui, &state) == widgets::StateAction::Retry {
            actions.push(ChatAction::RetrySessions);
        }
        if !chat.sessions_loaded {
            return;
        }
    }
    if !chat.sessions_loaded {
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "conversations".into(),
            },
        );
        return;
    }
    let rows: Vec<(String, String, bool)> = chat
        .filtered()
        .into_iter()
        .map(|s| (s.id.as_str().to_owned(), s.title.0.clone(), s.closed))
        .collect();
    if rows.is_empty() {
        widgets::state_block(
            ui,
            &ViewState::Empty {
                title: if chat.sessions.is_empty() {
                    "No conversations yet".into()
                } else {
                    "No conversations match".into()
                },
                body: if chat.sessions.is_empty() {
                    "Start one with New Chat or by sending a message.".into()
                } else {
                    "Try another search.".into()
                },
            },
        );
        return;
    }
    let selected = chat.selected.clone();
    virtual_list::rows(
        ui,
        &mut chat.scrolls,
        "chat-history",
        30.0,
        rows.len(),
        |ui, i| {
            let (id, title, closed) = &rows[i];
            ui.horizontal(|ui| {
                let r = ui.selectable_label(selected.as_deref() == Some(id), title);
                if r.clicked() {
                    actions.push(ChatAction::Select(id.clone()));
                }
                if *closed {
                    chip(ui, "Closed", Tone::Neutral);
                }
            });
        },
    );
    if chat.sessions_more {
        widgets::caption(ui, "Older conversations are not listed yet.");
    }
}

/// Extension for naming icon-only and selector controls for screen readers.
trait Named {
    fn widget_info_labeled(self, label: &str) -> Self;
}
impl Named for egui::Response {
    fn widget_info_labeled(self, label: &str) -> Self {
        let (enabled, label) = (self.enabled(), label.to_owned());
        let typ = if self.sense.senses_click() {
            egui::Role::Button
        } else {
            egui::Role::Unknown
        };
        self.widget_info(|| egui::WidgetInfo::labeled(typ, enabled, label.clone()));
        self
    }
}

/// Converts a failure to the notice the chat surfaces show.
pub fn failure_notice(what: &str, f: &Failure) -> (Tone, String) {
    (Tone::Bad, format!("{what}: {}", f.describe()))
}
