//! The shared conversation components (UI-01..04, UI-08/09, UI-11..13): `ConversationView` and
//! `ChatComposer` are the same code on the Home screen and in the app-level coordinator sidebar,
//! parameterized by the selected session, so both surfaces show one conversation with one draft,
//! one set of pending attachments and one transcript.
//!
//! Bound services: `sessions.list` / `.start` / `.transcript` / `.append_message` and
//! `artifacts.save` (workspace scope, conversation-tagged), and `router.catalog`, `.availability`
//! and `.pin` for the route and effort selectors ([`crate::routing`]). Not bound here, and shown
//! honestly as unavailable: the coordinator agent and inference (DW-16), Current's app-wide
//! work status (DW-12, DW-14) and conversation dates for history grouping (the session record
//! carries none yet).
use crate::memory::{
    DraftMemory, FailedAttachment, PendingUpload, SidebarTab, StoredAttachment,
    UnconfirmedAttachment,
};
use crate::ops::Failure;
use crate::routing::{self, Routing, Selection};
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

/// A removed attachment whose save may have committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    /// A file the shell created for it (a pasted image), removed once it is resolved.
    pub path: Option<PathBuf>,
    /// Its save's frame (`None` while it is still being prepared: nothing is admitted).
    pub frame: Option<Vec<u8>>,
    /// That frame is admitted and its outcome not back yet.
    pub in_flight: bool,
}

/// Draft key of the conversation not started yet.
pub const NEW_DRAFT: &str = "new";
/// Widest text column of an attachment chip.
pub const ATTACHMENT_CHIP_WIDTH: f32 = 180.0;
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
    /// Its save is being prepared (hashing, upload ticket); nothing is admitted yet.
    Storing,
    /// Its exact `artifacts.save` frame is admitted (or about to be): kept here first, so a
    /// close at any point confirms this same request at the next launch.
    Saving(Vec<u8>),
    Stored(ArtifactRef),
    /// The save's outcome is not known yet; confirmed by resubmitting this exact frame.
    Unconfirmed(Vec<u8>),
    Failed(String),
}

pub struct Attachment {
    pub local: u64,
    pub name: String,
    pub media_type: String,
    /// The picked or materialized file (absent once restored from memory).
    pub path: Option<PathBuf>,
    /// The shell created `path` (a pasted image). Only such a file is ever removed, and its
    /// ownership is recorded here when it is created, never inferred from where it is.
    pub owned: bool,
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

/// What one `sessions.append_message` submitted from a draft: its text and the local IDs of
/// the stored attachments it carried. The composer stays editable while it is in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submitted {
    pub text: String,
    pub locals: Vec<u64>,
}

impl Draft {
    /// What sending the draft now would submit.
    pub fn submission(&self) -> Submitted {
        Submitted {
            text: self.text.clone(),
            locals: self
                .attachments
                .iter()
                .filter(|a| matches!(a.state, AttachState::Stored(_)))
                .map(|a| a.local)
                .collect(),
        }
    }
    /// Removes what `sent` saved, keeping every edit made while it was in flight: attachments
    /// added since stay, and text typed after the submitted text stays. Text that no longer
    /// starts with the submitted text was rewritten by the user and is kept whole.
    pub fn settle_sent(&mut self, sent: &Submitted) {
        self.attachments.retain(|a| !sent.locals.contains(&a.local));
        if let Some(rest) = self.text.strip_prefix(sent.text.as_str()) {
            self.text = rest.trim_start().to_owned();
        }
    }
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
                AttachState::Storing
                    | AttachState::Saving(_)
                    | AttachState::WaitingForConversation
                    | AttachState::Unconfirmed(_)
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
            unconfirmed: self
                .attachments
                .iter()
                .filter_map(|a| match &a.state {
                    // Admitted or about to be: confirmed with this frame at the next launch.
                    AttachState::Saving(frame) | AttachState::Unconfirmed(frame) => {
                        Some(UnconfirmedAttachment {
                            name: a.name.clone(),
                            media_type: a.media_type.clone(),
                            path: a.path.clone(),
                            owned: a.owned,
                            frame: frame.clone(),
                        })
                    }
                    _ => None,
                })
                .collect(),
            uploads: self
                .attachments
                .iter()
                // Never admitted: stored afresh at the next launch.
                .filter(|a| {
                    matches!(
                        a.state,
                        AttachState::WaitingForConversation | AttachState::Storing
                    )
                })
                .filter_map(|a| {
                    Some(PendingUpload {
                        name: a.name.clone(),
                        media_type: a.media_type.clone(),
                        path: a.path.clone()?,
                        owned: a.owned,
                    })
                })
                .collect(),
            failed: self
                .attachments
                .iter()
                .filter_map(|a| match &a.state {
                    AttachState::Failed(error) => Some(FailedAttachment {
                        name: a.name.clone(),
                        media_type: a.media_type.clone(),
                        path: a.path.clone(),
                        owned: a.owned,
                        error: error.clone(),
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Volatile text, never persisted as part of the transcript or draft.
    pub text_preview: Option<devworks_engine::providers::live::LiveTurn>,
    pub starting: Option<StartReason>,
    /// Send once the started session exists.
    pub send_after_start: bool,
    pub sending: Option<String>,
    /// What the in-flight append submitted (cleared from the draft only on success).
    pub submitted: Option<Submitted>,
    /// Submitted attachments the user removed while the append was in flight: deleted only if
    /// the append did not commit (a committed message references them).
    pub removed_while_sending: Vec<ArtifactRef>,
    /// Attachments removed while their save was running or its outcome was unknown, by local
    /// ID: confirmed (by "Confirm outcomes"), then deleted, so a save that committed never
    /// leaves an artifact behind.
    pub removed: BTreeMap<u64, Removed>,
    /// The bounded composer height each surface measured last frame (Home, sidebar).
    pub composer_heights: [f32; 2],
    /// The composer's height besides its scrolling draft and attachment areas (controls,
    /// metadata line, margins), measured last frame: the draft area gets the rest of the bound.
    pub composer_overheads: [f32; 2],
    /// The height each surface's conversation (transcript and composer) had last frame.
    pub conversation_heights: [f32; 2],
    pub notice: Option<(Tone, String)>,
    /// The overlay sidebar is shown on narrow Home layouts.
    pub narrow_sidebar_open: bool,
    pub scrolls: Scrolls,
    /// The composer's route and effort selection, catalog and status (shared by both surfaces).
    pub routing: Routing,
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
            text_preview: None,
            starting: None,
            send_after_start: false,
            sending: None,
            submitted: None,
            removed_while_sending: Vec::new(),
            removed: BTreeMap::new(),
            composer_heights: [150.0, 170.0],
            composer_overheads: [110.0, 110.0],
            conversation_heights: [0.0, 0.0],
            notice: None,
            narrow_sidebar_open: false,
            scrolls: Scrolls::default(),
            routing: Routing::default(),
            next_local: 0,
        }
    }
    /// A finalized preview remains visible until the canonical transcript catches up.
    pub fn reconcile_text_preview(&mut self) {
        let Some(turn) = &self.text_preview else {
            return;
        };
        let t = self.transcripts.get(turn.session_id.as_str());
        let epoch = self
            .sessions
            .iter()
            .find(|s| s.id == turn.session_id)
            .map_or(turn.context_epoch, |s| s.context_epoch.get());
        let changed_epoch = t.is_some_and(|t| {
            t.entries.iter().any(|e| match &e.body {
                TranscriptBody::ContextEpochChanged { next_epoch, .. } => {
                    next_epoch.get() > turn.context_epoch
                }
                TranscriptBody::NativeItem { binding, .. } => {
                    binding.context_epoch.get() > turn.context_epoch
                }
                _ => false,
            })
        });
        let caught_up = turn.finalized_through.is_some_and(|through| {
            t.and_then(|t| t.entries.last())
                .is_some_and(|e| e.sequence.get() >= through)
        });
        if self.selected.as_deref() != Some(turn.session_id.as_str())
            || epoch > turn.context_epoch
            || changed_epoch
            || caught_up
        {
            self.text_preview = None;
        }
    }
    pub fn apply_text_preview(
        &mut self,
        incoming: Option<devworks_engine::providers::live::LiveTurn>,
    ) {
        match incoming {
            Some(turn) if self.selected.as_deref() == Some(turn.session_id.as_str()) => {
                let older = self.text_preview.as_ref().is_some_and(|old| {
                    old.session_id == turn.session_id
                        && (turn.context_epoch < old.context_epoch
                            || (turn.attempt_id == old.attempt_id && turn.revision < old.revision))
                });
                if !older {
                    self.text_preview = Some(turn);
                }
            }
            None if self
                .text_preview
                .as_ref()
                .is_some_and(|t| t.finalized_through.is_none()) =>
            {
                self.text_preview = None;
            }
            _ => {}
        }
        self.reconcile_text_preview();
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
    /// The pending attachment with this local ID, in whichever draft holds it.
    pub fn attachment_mut(&mut self, local: u64) -> Option<&mut Attachment> {
        self.drafts
            .values_mut()
            .flat_map(|d| d.attachments.iter_mut())
            .find(|a| a.local == local)
    }
    pub fn session(&self, id: &str) -> Option<&SessionRecord> {
        self.sessions.iter().find(|s| s.id.as_str() == id)
    }
    pub fn selected_session(&self) -> Option<&SessionRecord> {
        self.selected.as_deref().and_then(|s| self.session(s))
    }
    /// Sessions matching the Chat History search (title words, case-insensitive).
    /// Sessions matching the Chat History search, most recently active first.
    pub fn filtered(&self) -> Vec<&SessionRecord> {
        let terms: Vec<String> = self
            .history_query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut out: Vec<&SessionRecord> = self
            .sessions
            .iter()
            .filter(|s| {
                let t = s.title.0.to_lowercase();
                terms.iter().all(|w| t.contains(w))
            })
            .collect();
        // Stable: sessions without a time keep their listed (newest-started-first) order.
        out.sort_by_key(|s| std::cmp::Reverse(last_activity(s)));
        out
    }
    /// The active conversation's route and effort selection.
    pub fn selection(&self) -> Selection {
        self.routing
            .selections
            .get(&self.draft_key())
            .cloned()
            .unwrap_or_default()
    }
    /// Moves the not-yet-started draft onto a started session.
    pub fn adopt_new_draft(&mut self, session: &str) {
        // The new chat's route and effort selection becomes the session's.
        if let Some(sel) = self.routing.selections.remove(NEW_DRAFT) {
            self.routing
                .selections
                .entry(session.to_owned())
                .or_insert(sel);
        }
        self.routing.status.remove(NEW_DRAFT);
        if let Some(d) = self.drafts.remove(NEW_DRAFT) {
            let target = self.drafts.entry(session.to_owned()).or_default();
            // Both texts are kept: neither the session's draft nor the new one is dropped.
            if target.text.trim().is_empty() {
                target.text = d.text;
            } else if !d.text.trim().is_empty() {
                target.text = format!("{}\n\n{}", target.text, d.text);
            }
            target.attachments.extend(d.attachments);
        }
    }
    /// The key of the draft holding the attachment with this local ID.
    pub fn owner(&self, local: u64) -> Option<String> {
        self.drafts
            .iter()
            .find(|(_, d)| d.attachments.iter().any(|a| a.local == local))
            .map(|(k, _)| k.clone())
    }
    /// Takes the attachment with this local ID out of whichever draft holds it.
    pub fn take_attachment(&mut self, local: u64) -> Option<Attachment> {
        self.drafts.values_mut().find_map(|d| {
            let i = d.attachments.iter().position(|a| a.local == local)?;
            Some(d.attachments.remove(i))
        })
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

/// A session's last activity (start, newest message or end), when the record carries it.
pub fn last_activity(s: &SessionRecord) -> Option<i64> {
    s.last_activity_ms
        .as_ref()
        .or(s.created_at_ms.as_ref())
        .map(|c| c.get())
}

/// A Chat History date group, by the user's local calendar day of a session's last activity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DateGroup {
    Today,
    Yesterday,
    /// Two to seven days ago.
    Previous7Days,
    /// Eight to thirty days ago.
    Previous30Days,
    /// Older, by calendar month.
    Month {
        year: i16,
        month: i8,
    },
    /// The record carries no time.
    Undated,
}
impl DateGroup {
    pub fn label(&self) -> String {
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        match self {
            Self::Today => "Today".into(),
            Self::Yesterday => "Yesterday".into(),
            Self::Previous7Days => "Previous 7 days".into(),
            Self::Previous30Days => "Previous 30 days".into(),
            Self::Month { year, month } => {
                format!("{} {year}", MONTHS[(*month as usize).clamp(1, 12) - 1])
            }
            Self::Undated => "Undated".into(),
        }
    }
}

/// The group of a time (`at_ms`, UTC milliseconds) seen from `now` in its time zone. A time
/// later than now (another machine's clock) counts as today.
pub fn date_group(now: &jiff::Zoned, at_ms: Option<i64>) -> DateGroup {
    let Some(at) = at_ms.and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok()) else {
        return DateGroup::Undated;
    };
    let at = at.to_zoned(now.time_zone().clone());
    let days = at
        .date()
        .until(now.date())
        .map(|span| span.get_days())
        .unwrap_or(0);
    match days {
        ..=0 => DateGroup::Today,
        1 => DateGroup::Yesterday,
        2..=7 => DateGroup::Previous7Days,
        8..=30 => DateGroup::Previous30Days,
        _ => DateGroup::Month {
            year: at.year(),
            month: at.month(),
        },
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
    /// Resubmit an attachment's unconfirmed save frame.
    ConfirmAttachment(u64),
    RetryAttachment(u64),
    /// A new route (tier, named model or Open Router Auto) or effort for the active
    /// conversation.
    SetSelection(Selection),
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
    fn index(self) -> usize {
        match self {
            Self::Home => 0,
            Self::Sidebar => 1,
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
    // The composer is bounded (its draft and attachment rows scroll) and the transcript gets the
    // rest, sized from the composer's measured height so the action row is always on screen.
    let available = ui.available_height();
    chat.conversation_heights[surface.index()] = available;
    let bound = composer_bound(available);
    let measured = chat.composer_heights[surface.index()];
    let transcript_height = (available - measured).max(0.0);
    // Exactly that rect, clipped: taller content (an empty or error state) scrolls inside it
    // and never pushes the composer down.
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), transcript_height),
        egui::Sense::hover(),
    );
    let mut transcript = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    transcript.set_clip_rect(rect.intersect(ui.clip_rect()));
    transcript_view(&mut transcript, chat, surface, online, actions);
    composer(ui, chat, surface, online, actions, bound);
}

/// The most height a composer may take of `available`: at most two thirds, so the
/// conversation stays visible, and never less than its fixed controls need.
pub fn composer_bound(available: f32) -> f32 {
    (available * 0.66).max(110.0)
}

fn transcript_view(
    ui: &mut Ui,
    chat: &mut Chat,
    surface: Surface,
    online: Result<(), &str>,
    actions: &mut Vec<ChatAction>,
) {
    let showing_entries = online.is_ok()
        && chat
            .selected
            .as_ref()
            .and_then(|id| chat.transcripts.get(id))
            .is_some_and(|t| t.loaded && !t.entries.is_empty());
    if showing_entries {
        transcript_state(ui, chat, surface, online, actions);
    } else {
        // A state block scrolls within the transcript's height (the entries scroll themselves).
        egui::ScrollArea::vertical()
            .id_salt(("transcript-state", surface.salt()))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                transcript_state(ui, chat, surface, online, actions)
            });
    }
}

fn transcript_state(
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
    let preview = chat
        .text_preview
        .as_ref()
        .filter(|p| p.session_id.as_str() == id);
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
    if !t.loaded && preview.is_none() {
        widgets::state_block(
            ui,
            &ViewState::Loading {
                what: "the conversation".into(),
            },
        );
        return;
    }
    if t.entries.is_empty() && preview.is_none() {
        empty_invitation(ui, surface);
        return;
    }
    // The atomic transcript commit can precede the registry's final sequence marker.
    // Keep one copy of this attempt's text throughout that handover.
    let entries: Vec<_> = t.entries.iter().filter(|entry| !matches!(
        (&entry.body, preview),
        (TranscriptBody::AssistantAttempt { attempt_id, .. }, Some(p)) if *attempt_id == p.attempt_id
    )).collect();
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
                if let Some(preview) = preview {
                    egui::Frame::new()
                        .fill(theme::PANEL)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            widgets::caption(ui, "Streaming response");
                            ui.label(&preview.text);
                        });
                } else {
                    widgets::caption(ui, NO_COORDINATOR);
                }
                return;
            }
            entry_block(ui, entries[i]);
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
    bound: f32,
) {
    let start = ui.cursor().min.y;
    let overhead = chat.composer_overheads[surface.index()];
    let mut scrolled = 0.0;
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
            let selection = chat.selection();
            let draft = chat.draft_mut();
            // The draft and attachment areas scroll within what the measured overhead (the
            // action row, metadata line and margins) leaves of the bound.
            let row = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
            // At least one row of draft; the two areas share exactly what is left.
            let room = (bound - overhead).max(row);
            let attachments_height = if draft.attachments.is_empty() {
                0.0
            } else {
                (room * 0.4).min(96.0)
            };
            if !draft.attachments.is_empty() {
                // Attachment chips wrap inside a bounded, vertically scrolling area.
                scrolled += egui::ScrollArea::vertical()
                    .id_salt(("composer-attachments", surface.salt()))
                    .max_height(attachments_height)
                    .min_scrolled_height(attachments_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width());
                        // Chips are framed groups, which egui does not wrap: lay them out in
                        // rows of as many fixed-width slots as fit.
                        let slot = ATTACHMENT_CHIP_WIDTH + 96.0;
                        let per_row = ((ui.available_width() / slot) as usize).max(1);
                        for row in draft.attachments.chunks(per_row) {
                            ui.horizontal(|ui| {
                                for a in row {
                                    ui.scope(|ui| {
                                        ui.set_max_width(slot);
                                        attachment_chip(ui, a, actions);
                                    });
                                }
                            });
                        }
                    })
                    .inner_rect
                    .height();
            }
            let input_height = room - attachments_height;
            scrolled += egui::ScrollArea::vertical()
                .id_salt(("composer-input", surface.salt()))
                .max_height(input_height)
                // egui's default minimum (64) would let a small window's draft area overflow.
                .min_scrolled_height(input_height.min(row * 2.0))
                .stick_to_bottom(true)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.set_max_width(ui.available_width());
                    let input = egui::TextEdit::multiline(&mut draft.text)
                        .id(input_id)
                        .hint_text("Ask about any project, feature, PR, or agent…")
                        .desired_rows(if surface == Surface::Home { 2 } else { 3 })
                        .desired_width(f32::INFINITY)
                        .frame(egui::Frame::NONE);
                    let response = ui.add_enabled(online.is_ok(), input);
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::Role::TextInput,
                            online.is_ok(),
                            COMPOSER_LABEL,
                        )
                    });
                })
                .inner_rect
                .height();
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
                // A conversation's selection may be its durable pin: it is changed only once
                // the router said what it is pinned to (never over an unknown pin).
                let key = chat.draft_key();
                // Nor while its first route, which carries the current selection, is saved.
                let known = (key == NEW_DRAFT
                    || chat
                        .routing
                        .status
                        .get(&key)
                        .is_some_and(|s| s.availability.is_some()))
                    && !chat.routing.selection_locked(&key);
                selectors(
                    ui,
                    &chat.routing,
                    &selection,
                    surface,
                    online.is_ok() && known,
                    actions,
                );
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
        let key = chat.draft_key();
        widgets::caption(
            ui,
            routing::status_line(
                &chat.routing.catalog,
                &chat.selection(),
                chat.routing.status.get(&key),
            ),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            widgets::caption(ui, "Cmd / Ctrl + Enter to send");
        });
    });
    // Measure for the next frame: the transcript is sized from the total, the draft area from
    // the overhead. A change repaints once more so both converge immediately.
    let total = ui.cursor().min.y - start;
    let i = surface.index();
    let (old_total, old_overhead) = (chat.composer_heights[i], chat.composer_overheads[i]);
    chat.composer_heights[i] = total;
    chat.composer_overheads[i] = (total - scrolled).max(0.0);
    if (total - old_total).abs() > 0.5 || (chat.composer_overheads[i] - old_overhead).abs() > 0.5 {
        ui.ctx().request_repaint();
    }
}

/// The route selector (tiers first, then named models) and the separate effort selector.
fn selectors(
    ui: &mut Ui,
    r: &Routing,
    selection: &Selection,
    surface: Surface,
    enabled: bool,
    actions: &mut Vec<ChatAction>,
) {
    let current = routing::intent_label(&r.catalog, &selection.intent);
    let effort_text = format!("Effort: {}", routing::effort_label(&selection.effort));
    let mut picked = None;
    let route_hover = r
        .catalog_error
        .clone()
        .unwrap_or_else(|| "Tiers allocate round robin; a named model is used directly.".into());
    ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(("model", surface.salt()))
            .selected_text(current.clone())
            .show_ui(ui, |ui| {
                for (label, intent) in routing::options(&r.catalog) {
                    let on = routing::same_intent(&intent, &selection.intent);
                    if ui.selectable_label(on, label).clicked() && !on {
                        picked = Some(intent);
                    }
                }
            })
            .response
            .on_hover_text(route_hover)
            .widget_info(|| combo_info(routing::ROUTE_LABEL, enabled, &current));
        let mut effort = None;
        egui::ComboBox::from_id_salt(("effort", surface.salt()))
            .selected_text(effort_text.clone())
            .show_ui(ui, |ui| {
                for e in routing::efforts(&r.catalog, &selection.intent) {
                    let on = routing::effort_label(&e) == routing::effort_label(&selection.effort);
                    if ui.selectable_label(on, routing::effort_label(&e)).clicked() && !on {
                        effort = Some(e);
                    }
                }
            })
            .response
            .on_hover_text("Effort is separate from the route; an explicit effort is only sent to a model that declares it.")
            .widget_info(|| combo_info(routing::EFFORT_LABEL, enabled, &effort_text));
        if let Some(effort) = effort {
            actions.push(ChatAction::SetSelection(Selection {
                intent: selection.intent.clone(),
                effort,
            }));
        }
    });
    if let Some(intent) = picked {
        // Effort is kept independently of the route; one the new route does not declare is
        // shown as not eligible rather than changed silently.
        actions.push(ChatAction::SetSelection(Selection {
            intent,
            effort: selection.effort.clone(),
        }));
    }
}

/// A named selector with its current choice as its value.
fn combo_info(label: &str, enabled: bool, value: &str) -> egui::WidgetInfo {
    let mut info = egui::WidgetInfo::labeled(egui::Role::ComboBox, enabled, label);
    info.current_text_value = Some(value.to_owned());
    info
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
                    // A chip never grows wider than this: long names and notes wrap.
                    ui.set_max_width(ATTACHMENT_CHIP_WIDTH);
                    ui.add(egui::Label::new(RichText::new(&a.name).small()).wrap());
                    let (text, tone) = match &a.state {
                        AttachState::WaitingForConversation => {
                            ("Waiting for the conversation".to_owned(), Tone::Neutral)
                        }
                        AttachState::Storing | AttachState::Saving(_) => {
                            ("Storing…".to_owned(), Tone::Neutral)
                        }
                        AttachState::Stored(r) => (
                            format!("Stored · {}", bytes_label(r.byte_length.get())),
                            Tone::Good,
                        ),
                        AttachState::Unconfirmed(_) => {
                            ("Not confirmed".to_owned(), Tone::Attention)
                        }
                        AttachState::Failed(e) => (format!("Failed: {e}"), Tone::Bad),
                    };
                    chip(ui, text, tone);
                    if matches!(a.state, AttachState::Unconfirmed(_)) {
                        let label = format!("Confirm upload of {}", a.name);
                        if ui
                            .small_button("Confirm")
                            .on_hover_text(&label)
                            .widget_info_labeled(&label)
                            .clicked()
                        {
                            actions.push(ChatAction::ConfirmAttachment(a.local));
                        }
                    }
                    if matches!(a.state, AttachState::Failed(_)) && a.path.is_some() {
                        let label = format!("Retry upload of {}", a.name);
                        if ui
                            .small_button("Retry")
                            .on_hover_text(&label)
                            .widget_info_labeled(&label)
                            .clicked()
                        {
                            actions.push(ChatAction::RetryAttachment(a.local));
                        }
                    }
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
    // Rows interleave a header before the first session of each date group.
    enum Row {
        Group(String),
        Session(String, String, bool),
    }
    let now = jiff::Zoned::now();
    let mut rows: Vec<Row> = Vec::new();
    let mut current = None;
    for s in chat.filtered() {
        let group = date_group(&now, last_activity(s));
        if current.as_ref() != Some(&group) {
            rows.push(Row::Group(group.label()));
            current = Some(group);
        }
        rows.push(Row::Session(
            s.id.as_str().to_owned(),
            s.title.0.clone(),
            s.closed,
        ));
    }
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
        |ui, i| match &rows[i] {
            Row::Group(label) => {
                ui.label(RichText::new(label).small().strong().color(theme::DIM));
            }
            Row::Session(id, title, closed) => {
                ui.horizontal(|ui| {
                    let r = ui.selectable_label(selected.as_deref() == Some(id), title);
                    if r.clicked() {
                        actions.push(ChatAction::Select(id.clone()));
                    }
                    if *closed {
                        chip(ui, "Closed", Tone::Neutral);
                    }
                });
            }
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
