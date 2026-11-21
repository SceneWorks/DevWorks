//! The conversation (Home and the coordinator sidebar): sessions, the transcript and its live
//! text, the draft and its attachments, routing selections, and what an unconfirmed message,
//! start or attachment keeps across a restart. Home is its route; the sidebar is drawn beside
//! any other route while it is open.
use crate::attachments;
use crate::backend::{Reply, TaskOutput, Unresolved, is_unknown};
use crate::chat::{
    self, AttachState, Attachment, Chat, ChatAction, ChatSlot, StartReason, Surface,
};
use crate::memory::{ChatPendingMemory, Memory, RemovedMemory, RouteMemory, SubmittedMemory};
use crate::ops::{self, Failure};
use crate::paged::Applied;
use crate::project_purge::Live;
use crate::route::Route;
use crate::screen::{Cx, Effect, Screen, ScreenContext, ScreenId, controller};
use crate::theme;
use crate::virtual_list::Scrolls;
use crate::widgets::{self, Header, Tone, chip};
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::DomainErrorCode;
use devworks_core::protocol::provider::ArtifactRef;
use eframe::egui::{self, Align, Event, Frame, Layout, Panel, RichText, Ui};
use std::path::PathBuf;
use std::time::{Duration, Instant};

controller!(ChatCtl, chat: Chat);

impl Screen for Chat {
    type Slot = ChatSlot;
    const ID: ScreenId = ScreenId::Chat;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        ChatCtl { chat: self, cx }.ensure_conversation_loaded();
    }
    /// The shell refreshes the conversation on every route it is visible on
    /// ([`Chat::refresh_conversation`]), Home included.
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: ChatSlot, reply: Reply) {
        if let Some((slot, reply)) = (ChatCtl { chat: self, cx }).apply_chat(slot, reply) {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        ChatCtl { chat: self, cx }.home(ui);
    }
    /// What the unconfirmed start, message and removed attachments settle, kept with their
    /// exact frames so the next launch confirms them.
    fn pending_memory(&self, unresolved: &[&Unresolved]) -> Option<serde_json::Value> {
        let has = |f: &dyn Fn(&ChatSlot) -> bool| {
            unresolved
                .iter()
                .any(|u| ChatSlot::try_from(u.slot.clone()).is_ok_and(|s| f(&s)))
        };
        let submitted = match (&self.submitted, &self.sending) {
            (Some(sent), Some(id)) if has(&|s| *s == ChatSlot::Append(id.clone())) => {
                let artifacts = self
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
        let pending = ChatPendingMemory {
            starting: self
                .starting
                .filter(|_| has(&|s| *s == ChatSlot::SessionStart)),
            submitted,
            removed_while_sending: if self.sending.is_some()
                && has(&|s| matches!(s, ChatSlot::Append(_)))
            {
                self.removed_while_sending
                    .iter()
                    .filter_map(|r| serde_json::to_value(r).ok())
                    .collect()
            } else {
                Vec::new()
            },
            removed: self
                .removed
                .values()
                .filter_map(|r| {
                    Some(RemovedMemory {
                        path: r.path.clone(),
                        frame: r.frame.clone()?,
                    })
                })
                .collect(),
        };
        serde_json::to_value(pending).ok()
    }
    fn restore_pending(&mut self, saved: &serde_json::Value) {
        let Ok(pending) = serde_json::from_value::<ChatPendingMemory>(saved.clone()) else {
            return;
        };
        for r in &pending.removed {
            let local = self.next_local();
            self.removed.insert(
                local,
                chat::Removed {
                    path: r.path.clone(),
                    frame: Some(r.frame.clone()),
                    in_flight: false,
                },
            );
        }
        self.restoring = Some(pending);
    }
    fn restore_in_flight(&mut self, slot: &ChatSlot, _u: &Unresolved) {
        let pending = self.restoring.clone().unwrap_or_default();
        match slot {
            ChatSlot::SessionStart => {
                self.starting = Some(pending.starting.unwrap_or(StartReason::ForDraft));
            }
            ChatSlot::Append(id) => {
                self.sending = Some(id.clone());
                if let Some(sent) = &pending.submitted {
                    let locals = self
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
                    self.submitted = Some(chat::Submitted {
                        text: sent.text.clone(),
                        locals,
                    });
                    self.removed_while_sending = pending
                        .removed_while_sending
                        .iter()
                        .filter_map(|r| serde_json::from_value(r.clone()).ok())
                        .collect();
                }
            }
            ChatSlot::RouteFirst(id) => {
                self.routing.first_in_flight.insert(id.clone());
            }
            ChatSlot::RoutePin(id) => {
                self.routing.pin_in_flight.insert(id.clone());
            }
            _ => {}
        }
    }
    fn restored(&mut self, _cx: &mut Cx) {
        self.restoring = None;
    }
    fn purge_live(&self, _out: &mut Live) {}
    fn unconfirmed_count(&self) -> usize {
        Chat::unconfirmed_count(self)
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        let session = |id: &str| scope.session_ids.iter().any(|x| x.as_str() == id);
        self.text_preview = None;
        self.routing.status.retain(|id, _| !session(id));
        self.routing.rechecked.retain(|id, _| !session(id));
        self.routing.first_in_flight.retain(|id| !session(id));
        self.routing.pin_in_flight.retain(|id| !session(id));
        self.routing.status_asked.retain(|id, _| !session(id));
        self.sessions.items.retain(|r| !session(r.id.as_str()));
        self.drafts.retain(|id, _| !session(id));
        self.transcripts.retain(|id, _| !session(id));
        if self.selected.as_deref().is_some_and(session) {
            self.selected = None;
            self.text_preview = None;
        }
        self.routing.selections.retain(|id, _| !session(id));
        self.routing.first_route.retain(|id| !session(id));
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

/// What the shell asks of the conversation outside the [`Screen`] calls.
impl Chat {
    /// The conversation as the last close left it: drafts and their attachments, the selection
    /// (chosen once the list arrives), scroll offsets, routing selections and the sidebar.
    pub(crate) fn from_memory(memory: &Memory) -> Self {
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
        // The sidebar never reopens on its own on the Home screen (UI-13).
        chat.coordinator_open = memory.coordinator_open && memory.route != Route::Chat;
        chat.restored_selection = memory.selected_session.clone();
        chat
    }
    /// Puts the conversation's view state into `m` (scroll offsets are merged by the shell).
    pub(crate) fn save_memory(&self, m: &mut Memory) {
        m.coordinator_open = self.coordinator_open;
        m.selected_session = self.selected.clone();
        m.sidebar_tab = self.tab;
        m.history_query = self.history_query.clone();
        m.drafts = self
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
            .collect();
        m.routing = self.routing.selections.clone();
        m.routing_first = self.routing.first_route.clone();
    }
    /// Whether the conversation is shown on `route` (Home, or the open sidebar).
    pub(crate) fn visible(&self, route: Route) -> bool {
        route == Route::Chat || self.coordinator_open
    }
    pub(crate) fn load_sessions(&mut self, cx: &mut Cx) {
        ChatCtl { chat: self, cx }.load_sessions();
    }
    pub(crate) fn load_transcript(&mut self, cx: &mut Cx, id: &str) {
        ChatCtl { chat: self, cx }.load_transcript(id);
    }
    /// Shows conversation `id` (the shell then navigates Home).
    pub(crate) fn open_session(&mut self, cx: &mut Cx, id: String) {
        self.selected = Some(id.clone());
        self.load_transcript(cx, &id);
    }
    pub(crate) fn load_router_catalog(&mut self, cx: &mut Cx) {
        ChatCtl { chat: self, cx }.load_router_catalog();
    }
    /// Routing settings changed: what the selectors show is read again.
    pub(crate) fn invalidate_routing(&mut self, cx: &mut Cx) {
        ChatCtl { chat: self, cx }.invalidate_routing();
    }
    /// Launch: previews of restored attachments, and first messages saved before their routing
    /// was known at the last close.
    pub(crate) fn resume(&mut self, cx: &mut Cx) {
        let mut ctl = ChatCtl { chat: self, cx };
        ctl.restore_previews();
        ctl.refresh_waiting_first_routes();
    }
    /// Launch: attachments that never started storing, in their conversation or a new one.
    pub(crate) fn store_waiting_drafts(&mut self, cx: &mut Cx) {
        let waiting: Vec<String> = self
            .drafts
            .iter()
            .filter(|(_, d)| {
                d.attachments
                    .iter()
                    .any(|a| matches!(a.state, AttachState::WaitingForConversation))
            })
            .map(|(k, _)| k.clone())
            .collect();
        let mut ctl = ChatCtl { chat: self, cx };
        for key in waiting {
            if key == chat::NEW_DRAFT {
                ctl.start_session(StartReason::ForDraft);
            } else {
                ctl.store_waiting(&key);
            }
        }
    }
    /// A hint or the periodic repair: the visible transcript, its routing status and the
    /// catalog are read again, and waiting first routes asked about.
    pub(crate) fn refresh_conversation(&mut self, cx: &mut Cx) {
        let route = cx.route();
        let visible = self.visible(route);
        let mut ctl = ChatCtl { chat: self, cx };
        if visible
            && let Some(id) = ctl.chat.selected.clone()
            && !ctl.pending(&ChatSlot::Transcript(id.clone()))
        {
            ctl.load_transcript(&id);
        }
        if visible && let Some(id) = ctl.chat.selected.clone() {
            ctl.refresh_workspace_reply(&id);
        }
        if visible || route == Route::Jobs {
            if visible {
                ctl.refresh_route_status();
            }
            // Catalog edits and membership changes made elsewhere reach the selectors too.
            if !ctl.chat.routing.catalog.loading() {
                ctl.load_router_catalog();
            }
        }
        ctl.refresh_waiting_first_routes();
    }
    /// The toolbar chat icon: away from Home it toggles the coordinator sidebar; on Home the
    /// conversation is already in the centre, so it focuses the composer.
    pub(crate) fn toggle_coordinator(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        if cx.route() == Route::Chat {
            ctx.memory_mut(|m| m.request_focus(egui::Id::unique(("composer", "home"))));
            return;
        }
        self.coordinator_open = !self.coordinator_open;
        if self.coordinator_open {
            self.last_coordinator_notice = None;
            ChatCtl { chat: self, cx }.ensure_conversation_loaded();
        }
    }
    /// Tells the coordinator which screen the user is on (`key` identifies that context), once
    /// per change while the sidebar is open beside another route.
    pub(crate) fn notify_coordinator(&mut self, cx: &mut Cx, key: String, context: String) {
        if !self.coordinator_open || cx.route() == Route::Chat || cx.online().is_err() {
            return;
        }
        let Some(session) = self.selected.clone() else {
            return;
        };
        if self.last_coordinator_notice.as_ref() == Some(&(session.clone(), key.clone())) {
            return;
        }
        let Some(backend) = cx.backend() else {
            return;
        };
        let screen = cx.route().screen();
        match devworks_core::protocol::Id::new(session.clone()) {
            Ok(id) => match backend.append_coordinator_notice(&id, screen.id, &context) {
                Ok(()) => {
                    self.last_coordinator_notice = Some((session.clone(), key));
                    self.load_transcript(cx, &session);
                }
                Err(error) => cx.announce(error),
            },
            Err(_) => cx.announce("Coordinator conversation identity is invalid"),
        }
    }
    /// The frame's conversation input: images and file paths pasted into a composer.
    pub(crate) fn paste(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        let mut ctl = ChatCtl { chat: self, cx };
        if !ctl.composer_focused(ctx) {
            return;
        }
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
            ctl.attach_pasted_image(&img);
        }
        ctl.attach(files);
    }
    pub(crate) fn attach(&mut self, cx: &mut Cx, paths: Vec<PathBuf>) {
        ChatCtl { chat: self, cx }.attach(paths);
    }
    pub(crate) fn attach_pasted_image(&mut self, cx: &mut Cx, image: &egui::ColorImage) {
        ChatCtl { chat: self, cx }.attach_pasted_image(image);
    }
    pub(crate) fn frame(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        ChatCtl { chat: self, cx }.poll_live_text(ctx);
    }
    pub(crate) fn route_status_due(&mut self, cx: &mut Cx, ctx: &egui::Context) {
        ChatCtl { chat: self, cx }.route_status_due(ctx);
    }
    pub(crate) fn coordinator_sidebar(&mut self, cx: &mut Cx, ui: &mut Ui) {
        ChatCtl { chat: self, cx }.coordinator_sidebar(ui);
    }
    /// Attachments and removed attachments whose save outcome is not confirmed.
    pub(crate) fn unconfirmed_count(&self) -> usize {
        self.drafts
            .values()
            .flat_map(|d| d.attachments.iter())
            .filter(|a| matches!(a.state, AttachState::Unconfirmed(_)))
            .count()
            + self
                .removed
                .values()
                .filter(|r| r.frame.is_some() && !r.in_flight)
                .count()
    }
    /// Confirm outcomes: every unconfirmed attachment save, with its identical frame.
    pub(crate) fn confirm_attachments(&mut self, cx: &mut Cx) {
        let locals: Vec<u64> = self
            .drafts
            .values()
            .flat_map(|d| d.attachments.iter())
            .filter(|a| matches!(a.state, AttachState::Unconfirmed(_)))
            .map(|a| a.local)
            .collect();
        let mut ctl = ChatCtl { chat: self, cx };
        for local in locals {
            ctl.confirm_attachment(local);
        }
    }
    /// Confirm outcomes: every removed attachment whose save may have committed (deleted by
    /// its reply once confirmed).
    pub(crate) fn confirm_removed(&mut self, cx: &mut Cx) {
        let removed: Vec<(u64, Vec<u8>, Option<PathBuf>)> = self
            .removed
            .iter()
            .filter(|(_, r)| !r.in_flight)
            .filter_map(|(l, r)| Some((*l, r.frame.clone()?, r.path.clone())))
            .collect();
        let mut ctl = ChatCtl { chat: self, cx };
        for (local, frame, path) in removed {
            // A removed attachment, under its own local ID (which no draft holds any more):
            // confirmed, then deleted by its reply.
            let slot = ChatSlot::Attach(String::new(), local);
            if ctl.submit_save(slot, "Confirming a removed attachment", frame, path)
                && let Some(r) = ctl.chat.removed.get_mut(&local)
            {
                r.in_flight = true;
            }
        }
    }
}

impl ChatCtl<'_, '_> {
    fn ensure_conversation_loaded(&mut self) {
        if !self.chat.sessions.loaded && !self.pending(&ChatSlot::SessionsList) {
            self.load_sessions();
        }
        if !self.chat.routing.catalog_loaded && !self.chat.routing.catalog.loading() {
            self.load_router_catalog();
        }
        if let Some(id) = self.chat.selected.clone()
            && !self.chat.transcripts.contains_key(&id)
        {
            self.load_transcript(&id);
        }
    }

    fn load_sessions(&mut self) {
        self.chat.sessions_list_at = self.chat.session_mutations;
        // A re-read covers every page already shown (or asked for), so loaded older
        // conversations, and a selection among them, survive it.
        let fetch = self.chat.sessions.refresh();
        self.send_sessions(fetch, "Loading conversations");
    }
    fn send_sessions(&mut self, fetch: crate::paged::Fetch, label: &str) {
        self.cx.paged(
            &mut self.chat.sessions,
            fetch,
            ChatSlot::SessionsList,
            label,
            "sessions.list",
            ops::sessions_list,
        );
    }
    fn load_more_sessions(&mut self) {
        if let Some(fetch) = self.chat.sessions.more_page() {
            self.send_sessions(fetch, "Loading older conversations");
        }
    }

    fn load_transcript(&mut self, id: &str) {
        let t = self.chat.transcripts.entry(id.to_owned()).or_default();
        t.loading = true;
        let after = t.cursor.clone();
        let sid = id.to_owned();
        self.query(
            ChatSlot::Transcript(id.to_owned()),
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
        let n = self.chat.sessions.items.len() + 1;
        let title = format!("Conversation {n}");
        self.command(
            ChatSlot::SessionStart,
            "Starting a conversation",
            "sessions.start",
            move |b| ops::session_start(b, &title),
        );
    }

    /// Drops the catalog and availability derived from routing settings, reloading what is
    /// visible.
    fn invalidate_routing(&mut self) {
        self.chat.routing.invalidate();
        // A load still paging may predate the change: it is replaced, not completed. So may an
        // availability query in flight for the visible conversation.
        if self.route == Route::Chat || self.chat.coordinator_open || self.route == Route::Jobs {
            self.load_router_catalog();
            let key = self.chat.draft_key();
            if self.pending(&ChatSlot::RouteStatus(key.clone())) {
                self.reask_route_status_of(key);
            }
        } else {
            // Nothing in flight is wanted any more: the next visible load starts afresh.
            self.chat
                .routing
                .catalog
                .abandon(self.chat.routing.catalog.generation());
        }
    }

    /// Loads the whole catalog, page by page, as a new load (replacing one still paging).
    fn load_router_catalog(&mut self) {
        if self.online().is_err() {
            return;
        }
        // Every page is read before the shown catalog changes; a load still paging is replaced.
        let fetch = self.chat.routing.catalog.reload_all();
        self.send_catalog(fetch);
    }
    fn send_catalog(&mut self, fetch: crate::paged::Fetch) {
        // Never left "loading" when nothing was submitted: the next visible load (or launch)
        // starts it again.
        let slot = ChatSlot::RouterCatalog(fetch.generation);
        self.cx.paged(
            &mut self.chat.routing.catalog,
            fetch,
            slot,
            "Loading models and tiers",
            "router.catalog",
            ops::router_catalog,
        );
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
        if self.pending(&ChatSlot::RouteStatus(key.clone()))
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
            ChatSlot::RouteStatus(key.clone()),
            "Checking model availability",
            "router.availability",
            move |b| {
                ops::router_availability_for(
                    b,
                    &sel.intent,
                    &sel.effort,
                    session.as_deref(),
                    &["tools"],
                )
            },
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
        if self.chat.route_selection_locked(&key) {
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
                    || self.pending(&ChatSlot::RoutePin(key.clone()))
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
                    ChatSlot::RoutePin(key.clone()),
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
                && !self.pending(&ChatSlot::RouteSession(session.to_owned()))
            {
                let sid = session.to_owned();
                self.query(
                    ChatSlot::RouteSession(session.to_owned()),
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
            || self.pending(&ChatSlot::RouteFirst(session.to_owned()))
            || self.pending(&ChatSlot::RoutePin(session.to_owned()))
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
                    && !self.pending(&ChatSlot::RouteStatus(session.to_owned()))
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
        if matches!(
            sel.intent,
            devworks_core::protocol::records::RouteIntent::DevWorksAuto {}
        ) {
            // The saved native reply owns classification from the first message.
            return;
        }
        let sid = session.to_owned();
        if self.command(
            ChatSlot::RouteFirst(session.to_owned()),
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
        let selection = self
            .chat
            .routing
            .selections
            .get(&id)
            .cloned()
            .unwrap_or_default();
        let route = devworks_core::protocol::provider::ConversationRoutePreference {
            intent: selection.intent,
            effort: selection.effort,
        };
        let revision = session.revision.get();
        self.chat.sending = Some(id.clone());
        self.chat.submitted = Some(submitted);
        let sid = id.clone();
        self.command(
            ChatSlot::Append(id),
            "Saving your message",
            "sessions.append_message",
            move |b| ops::append_message_with_route(b, &sid, revision, &text, &refs, Some(route)),
        );
    }

    /// Attaches the user's files to the active conversation's draft (dialog, drop and pasted
    /// paths). The shell never removes these files.
    fn attach(&mut self, paths: Vec<PathBuf>) {
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
                    ChatSlot::Preview(key.clone(), local).into(),
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
    fn attach_pasted_image(&mut self, image: &egui::ColorImage) {
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

    /// Admits an attachment's save frame (prepared, or unconfirmed) under `slot`.
    fn submit_save(
        &mut self,
        slot: ChatSlot,
        label: &str,
        frame: Vec<u8>,
        owned: Option<PathBuf>,
    ) -> bool {
        self.client.as_mut().is_some_and(|c| {
            c.task(slot.into(), label, move |b| {
                attachments::save(b, frame, owned.as_deref())
            })
        })
    }

    fn delete_removed(&mut self, artifact: ArtifactRef) {
        // Cleanup the user already asked for, so it is not refused while closing: a closed
        // client keeps the frame unresolved, and the next launch submits it. Offline, there is
        // no client.
        let Some(b) = self.backend() else { return };
        let frame = ops::delete_unsent_attachment(&*b, &artifact);
        if let Some(c) = &mut self.client {
            c.command(
                ChatSlot::DeleteAttachment(artifact.artifact_id.as_str().to_owned()).into(),
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
                c.task(
                    ChatSlot::Preview(key, local).into(),
                    "Restoring a preview",
                    move |b| attachments::preview_stored(b, &r),
                );
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
                    ChatSlot::Attach(session.to_owned(), local).into(),
                    &format!("Storing {name}"),
                    move |b| attachments::prepare(b, &path, &name, &media, &sid),
                )
            });
            if submitted && let Some(a) = self.chat.attachment_mut(local) {
                a.state = AttachState::Storing;
            }
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
            .or_else(|| self.chat.restored_selection.take());
        let exists = |id: &str| self.chat.sessions.items.iter().any(|s| s.id.as_str() == id);
        let pick = match wanted {
            Some(id) if exists(&id) => Some(id),
            _ => self
                .chat
                .sessions
                .items
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
                ChatAction::OpenInstructions => self.cx.effects.push(Effect::OpenInstructions),
                ChatAction::RetrySessions => self.load_sessions(),
                ChatAction::MoreSessions => self.load_more_sessions(),
                ChatAction::RetryTranscript => {
                    if let Some(id) = self.chat.selected.clone() {
                        self.load_transcript(&id);
                    }
                }
                ChatAction::CloseSidebar => self.chat.coordinator_open = false,
                ChatAction::ConfirmAttachment(local) => self.confirm_attachment(local),
                ChatAction::RetryAttachment(local) => self.retry_attachment(local),
                ChatAction::SetSelection(sel) => self.select_route(sel),
            }
        }
        let _ = ctx;
    }

    /// Asks again about conversations whose first message still waits to be routed.
    fn refresh_waiting_first_routes(&mut self) {
        let waiting: Vec<String> = self.chat.routing.first_route.iter().cloned().collect();
        for key in waiting {
            if !self.pending(&ChatSlot::RouteStatus(key.clone()))
                && !self.pending(&ChatSlot::RouteFirst(key.clone()))
            {
                self.refresh_route_status_of(key);
            }
        }
    }

    /// Asks about the visible conversation's selection when nothing is known yet, and again when
    /// the earliest cooldown it waits for has ended (never earlier).
    fn route_status_due(&mut self, ctx: &egui::Context) {
        if !(self.route == Route::Chat || self.chat.coordinator_open) || self.online().is_err() {
            return;
        }
        let key = self.chat.draft_key();
        if self.pending(&ChatSlot::RouteStatus(key.clone())) {
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

    fn composer_focused(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| {
            m.has_focus(egui::Id::unique(("composer", "home")))
                || m.has_focus(egui::Id::unique(("composer", "sidebar")))
        })
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
        let slot = ChatSlot::Attach(key, local);
        // Not submitted (closing): it stays unconfirmed, its frame kept.
        if self.submit_save(slot, "Confirming an attachment", frame.clone(), owned)
            && let Some(a) = self.chat.attachment_mut(local)
        {
            a.state = AttachState::Saving(frame);
        }
    }

    fn coordinator_sidebar(&mut self, ui: &mut Ui) {
        let online = self.online().map_err(str::to_owned);
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
                if let Some((tone, text)) = self.chat.coordinator_reply_notice() {
                    chip(ui, text, tone);
                }
                chat::conversation(
                    ui,
                    &mut *self.chat,
                    Surface::Sidebar,
                    online.as_ref().map(|_| ()).map_err(String::as_str),
                    &mut actions,
                );
            });
        let ctx = ui.ctx().clone();
        self.chat_actions(&ctx, actions);
    }

    fn home(&mut self, ui: &mut Ui) {
        let online = self.online().map_err(str::to_owned);
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
                    if let Some((tone, text)) = st.0.coordinator_reply_notice() {
                        chip(ui, text, tone);
                    }
                    chat::conversation(ui, st.0, Surface::Home, online_ref, st.1);
                },
            );
        };
        let watch = self.cx.watchers;
        let mut watch_actions = Vec::new();
        let mut watchers = |ui: &mut Ui, session: &str| {
            let Some(watch) = watch else { return };
            watch.section(
                ui,
                crate::watch_ops::WatchView::Session,
                crate::watch_ops::WatchScope::Session(session.to_owned()),
                online_ref,
                &mut watch_actions,
            );
        };
        let mut st = (&mut *self.chat, &mut actions);
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
                                chat::sidebar(ui, st.0, online_ref, st.1, &mut watchers);
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
                            chat::sidebar(ui, st.0, online_ref, st.1, &mut watchers);
                        });
                },
            );
        }
        let ctx = ui.ctx().clone();
        self.chat_actions(&ctx, actions);
        if !watch_actions.is_empty() {
            self.cx.effects.push(Effect::Watch(watch_actions));
        }
    }

    fn refresh_workspace_reply(&mut self, session: &str) {
        let slot = ChatSlot::WorkspaceReplyStatus(session.to_owned());
        if self.pending(&slot) {
            return;
        }
        if let Some(client) = &mut self.client
            && let Ok(session) = devworks_core::protocol::Id::new(session)
        {
            client.task(
                slot.into(),
                "Reading the coordinator reply status",
                move |b| {
                    b.workspace_reply_status(&session)
                        .map(TaskOutput::WorkspaceReplyStatus)
                },
            );
        }
    }

    fn poll_live_text(&mut self, ctx: &egui::Context) {
        let target = (self.route == Route::Chat || self.chat.coordinator_open)
            .then(|| self.chat.selected.clone())
            .flatten();
        if target != self.chat.preview_target {
            self.chat.preview_target = target.clone();
            self.chat.preview_generation = self.chat.preview_generation.wrapping_add(1);
            self.chat.last_preview_poll = None;
            self.chat.text_preview = None;
        }
        self.chat.reconcile_text_preview();
        let Some(id) = target else { return };
        let interval = if self.chat.text_preview.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(500)
        };
        ctx.request_repaint_after(interval);
        let slot = ChatSlot::TextPreview(id.clone(), self.chat.preview_generation);
        if self.pending(&slot)
            || self
                .chat
                .last_preview_poll
                .is_some_and(|t| t.elapsed() < interval)
            || self.client.is_none()
        {
            return;
        }
        self.chat.last_preview_poll = Some(Instant::now());
        if let Some(client) = &mut self.client {
            client.task(slot.into(), "Reading the live response", move |b| {
                b.text_preview(&devworks_core::protocol::Id::try_from(id).map_err(str::to_owned)?)
                    .map(TaskOutput::TextPreview)
            });
        }
    }

    fn apply_live_text(&mut self, id: &str, generation: u64, result: Result<TaskOutput, String>) {
        if generation != self.chat.preview_generation
            || self.chat.preview_target.as_deref() != Some(id)
            || self.chat.selected.as_deref() != Some(id)
        {
            return;
        }
        match result {
            Ok(TaskOutput::TextPreview(turn)) => self.chat.apply_text_preview(turn),
            _ => self.chat.text_preview = None,
        }
        if self
            .chat
            .text_preview
            .as_ref()
            .is_some_and(|t| t.finalized_through.is_some())
            && !self.pending(&ChatSlot::Transcript(id.to_owned()))
        {
            self.load_transcript(id);
        }
    }

    /// Applies a reply for one of the conversation's slots; others are handed back.
    fn apply_chat(&mut self, slot: ChatSlot, reply: Reply) -> Option<(ChatSlot, Reply)> {
        match (slot, reply) {
            (ChatSlot::SessionsList, Reply::Query(_))
                if self.chat.sessions_list_at != self.chat.session_mutations =>
            {
                self.load_sessions();
            }
            (ChatSlot::SessionsList, Reply::Query(q)) => match self
                .chat
                .sessions
                .apply_current_unique(ops::sessions(q).map_err(|e| e.describe()), |s| {
                    s.id.as_str().to_owned()
                }) {
                // Recency order moves between reads, so pages can overlap: a conversation
                // listed twice shows once.
                Applied::Continue(fetch) => self.send_sessions(fetch, "Loading conversations"),
                Applied::Replaced | Applied::Appended => {
                    self.reconcile_selection();
                    // A waiting first route needs its session's record (its context epoch).
                    let waiting: Vec<String> =
                        self.chat.routing.first_route.iter().cloned().collect();
                    for key in waiting {
                        self.route_first(&key);
                    }
                }
                _ => {}
            },
            (ChatSlot::RouterCatalog(generation), Reply::Query(q)) => {
                let reply = ops::route_models(q)
                    .map(|(items, next)| crate::paged::Page::new(items, next))
                    .map_err(|e| format!("Named models are unavailable: {}", e.describe()));
                let routing = &mut self.chat.routing;
                match routing.catalog.apply(generation, reply) {
                    Applied::Continue(fetch) => self.send_catalog(fetch),
                    Applied::Replaced => {
                        routing.catalog_loaded = true;
                        routing.catalog_error = None;
                    }
                    Applied::Failed => {
                        routing.catalog_loaded = true;
                        routing.catalog_error = routing.catalog.error.take();
                    }
                    Applied::Stale | Applied::Appended | Applied::Fenced => {}
                }
            }
            (ChatSlot::RouteSession(id), Reply::Query(q)) => {
                if let Ok(record) = ops::session_record(q) {
                    self.chat.upsert(record);
                    if self.chat.routing.first_route.contains(&id) {
                        self.route_first(&id);
                    }
                }
            }
            (ChatSlot::RouteStatus(key), Reply::Query(_))
                if self.pending(&ChatSlot::RouteStatus(key.clone())) =>
            {
                // A newer query was issued after this reply was collected (by a reply applied
                // before it in the same batch): it may predate a route or pin just applied.
            }
            (ChatSlot::RouteStatus(key), Reply::Query(q)) => {
                let status = match ops::availability(q) {
                    Ok(a) => {
                        // A pinned conversation's selection is its durable pin.
                        if let Some(pin) = &a.pin
                            && !self.chat.routing.pin_in_flight.contains(&key)
                            && !self.pending(&ChatSlot::RoutePin(key.clone()))
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
            (ChatSlot::RouteFirst(key), Reply::Command(c)) => {
                if matches!(ops::routed(c), Err(Failure::Unknown)) {
                    // Confirmed with its identical frame by Confirm outcomes or the next launch;
                    // the selection stays locked until then.
                    return None;
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
            (ChatSlot::RoutePin(key), Reply::Command(c)) => {
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
                        return None;
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
            (ChatSlot::SessionStart, Reply::Command(c)) => {
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
                        self.chat.session_mutations += 1;
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
            (ChatSlot::Transcript(id), Reply::Query(q)) => {
                match ops::transcript_page(q) {
                    Ok(page) => {
                        let more = self
                            .chat
                            .transcripts
                            .entry(id.clone())
                            .or_default()
                            .apply(page);
                        // The texts shown: every watcher notice's and the latest other one's.
                        let notices: Vec<_> = self
                            .chat
                            .transcripts
                            .get(&id)
                            .map(|transcript| {
                                let mut latest_other = false;
                                transcript
                                    .entries
                                    .iter()
                                    .rev()
                                    .filter_map(|entry| match &entry.body {
                                        body @ devworks_core::protocol::provider::TranscriptBody::HostNotice {
                                            artifact,
                                            ..
                                        } if chat::watcher_notice(body)
                                            || !std::mem::replace(&mut latest_other, true) =>
                                        {
                                            Some(artifact.clone())
                                        }
                                        _ => None,
                                    })
                                    .filter(|a| {
                                        !self.chat.notice_texts.contains_key(a.version_id.as_str())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if let Some(backend) = self.backend() {
                            for artifact in notices {
                                if let Ok(text) = backend.read_coordinator_notice(&artifact) {
                                    self.chat
                                        .notice_texts
                                        .insert(artifact.version_id.as_str().into(), text);
                                }
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
            (ChatSlot::WorkspaceReplyStatus(id), Reply::Task(result)) => match result {
                Ok(TaskOutput::WorkspaceReplyStatus(Some(status))) => {
                    self.chat.reply_status.insert(id, status);
                }
                Ok(TaskOutput::WorkspaceReplyStatus(None)) => {
                    self.chat.reply_status.remove(&id);
                }
                _ => {}
            },
            (ChatSlot::TextPreview(id, generation), Reply::Task(result)) => {
                self.apply_live_text(&id, generation, result);
            }
            (ChatSlot::Append(id), Reply::Command(c)) => {
                self.chat.sending = None;
                let queued = matches!(&c, Ok(devworks_core::protocol::CommandReply::Recorded {
                    receipt, ..
                }) if receipt.operation_id.is_some());
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
                    return None;
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
                        self.chat.session_mutations += 1;
                        self.chat.upsert(record);
                        if let (Some(d), Some(sent)) = (self.chat.drafts.get_mut(&id), &submitted) {
                            d.settle_sent(sent);
                        }
                        self.chat.notice = None;
                        self.load_transcript(&id);
                        if queued {
                            self.refresh_workspace_reply(&id);
                            self.announce("Message saved. Coordinator reply queued.");
                        } else {
                            self.announce(
                                "Message saved. The workspace coordinator is unavailable.",
                            );
                        }
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
            (ChatSlot::Attach(key, local), Reply::Task(Ok(TaskOutput::Prepared { frame }))) => {
                if let Some(a) = self.chat.attachment_mut(local) {
                    let owned = a.path.clone().filter(|_| a.owned);
                    let label = format!("Storing {}", a.name);
                    // The frame is in the state before it is admitted: a close from here on
                    // keeps it, and the next launch confirms this same request.
                    a.state = AttachState::Saving(frame.clone());
                    // Not submitted (closing): nothing was admitted, and its upload ticket lives
                    // only in this process, so the next launch prepares a fresh one.
                    if !self.submit_save(ChatSlot::Attach(key, local), &label, frame, owned)
                        && let Some(a) = self.chat.attachment_mut(local)
                    {
                        a.state = AttachState::Storing;
                    }
                } else if let Some(r) = self.chat.removed.remove(&local) {
                    // Removed while it was being prepared: nothing was admitted.
                    self.remove_owned(r.path.as_deref());
                }
            }
            (ChatSlot::Attach(_, local), Reply::Task(t)) => {
                let state = match t {
                    Ok(TaskOutput::Stored { outcome, frame }) if is_unknown(&outcome) => {
                        AttachState::Unconfirmed(frame)
                    }
                    Ok(TaskOutput::Stored { outcome, .. }) => match ops::saved_artifact(outcome) {
                        Ok(record) => {
                            self.cx.effects.push(Effect::ArtifactsChanged);
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
                        | TaskOutput::WorkspaceReplyStatus(_)
                        | TaskOutput::Convention(_)
                        | TaskOutput::Instruction(_)
                        | TaskOutput::McpContent(_)
                        | TaskOutput::SystemOneCompletion(_)
                        | TaskOutput::RepositoryDeletionPreview(_)
                        | TaskOutput::RepositoryDeleted(_),
                    ) => AttachState::Failed("unexpected reply".into()),
                    Err(e) => AttachState::Failed(e),
                };
                let stored = matches!(state, AttachState::Stored(_));
                let unconfirmed = matches!(state, AttachState::Unconfirmed(_));
                match self.chat.attachment_mut(local) {
                    Some(a) => {
                        *self.cx.status = Some(if stored {
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
            (ChatSlot::Preview(_, local), Reply::Task(t)) => {
                let ctx = self.cx.ctx.clone();
                if let Some(a) = self.chat.attachment_mut(local) {
                    match t {
                        Ok(TaskOutput::Preview { size, rgba }) => {
                            let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                            a.preview = ctx.map(|ctx| {
                                ctx.load_texture(
                                    format!("attachment-preview-{local}"),
                                    image,
                                    egui::TextureOptions::LINEAR,
                                )
                            });
                        }
                        Ok(
                            TaskOutput::Stored { .. }
                            | TaskOutput::Prepared { .. }
                            | TaskOutput::SettingSecret { .. }
                            | TaskOutput::PackagePrepared { .. }
                            | TaskOutput::Refreshed
                            | TaskOutput::TextPreview(_)
                            | TaskOutput::WorkspaceReplyStatus(_)
                            | TaskOutput::Convention(_)
                            | TaskOutput::Instruction(_)
                            | TaskOutput::McpContent(_)
                            | TaskOutput::SystemOneCompletion(_)
                            | TaskOutput::RepositoryDeletionPreview(_)
                            | TaskOutput::RepositoryDeleted(_),
                        ) => {}
                        Err(e) => a.preview_note = Some(format!("No preview: {e}")),
                    }
                }
            }
            (ChatSlot::DeleteAttachment(_), Reply::Command(c)) => match ops::deleted(c) {
                Ok(()) => {
                    self.cx.effects.push(Effect::ArtifactsChanged);
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
            other => return Some(other),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{NativeDialogs, Shell, ShellInit};
    use devworks_core::protocol::{
        AttemptId, Id, Revision,
        provider::{TranscriptBody, TranscriptEntry},
    };
    use devworks_engine::providers::live::LiveTurn;
    fn id(s: &str) -> Id {
        s.to_owned().try_into().unwrap()
    }
    fn turn(revision: u64, finalized_through: Option<i64>) -> LiveTurn {
        LiveTurn {
            session_id: id("current"),
            attempt_id: AttemptId(id("attempt")),
            context_epoch: 0,
            revision,
            text: format!("response {revision}"),
            finalized_through,
        }
    }
    /// Applies a live-text reply as the conversation's reply handling does.
    fn live(shell: &mut Shell, id: &str, generation: u64, result: Result<TaskOutput, String>) {
        shell.with_typed(|c: &mut Chat, cx| {
            ChatCtl { chat: c, cx }.apply_live_text(id, generation, result)
        });
    }
    #[test]
    fn live_text_reply_cannot_replace_a_newer_selection_or_clear_it_from_an_old_generation() {
        let mut shell = Shell::new(ShellInit {
            backend: Err("offline".into()),
            dialogs: Box::new(NativeDialogs),
            memory_path: None,
            control: None,
            version: "test".into(),
        });
        shell.chat.preview_target = Some("current".into());
        shell.chat.selected = Some("current".into());
        shell.chat.preview_generation = 3;
        live(
            &mut shell,
            "current",
            3,
            Ok(TaskOutput::TextPreview(Some(turn(2, None)))),
        );
        live(
            &mut shell,
            "current",
            2,
            Ok(TaskOutput::TextPreview(Some(turn(3, None)))),
        );
        live(&mut shell, "other", 3, Ok(TaskOutput::TextPreview(None)));
        live(
            &mut shell,
            "current",
            2,
            Err("old authorization refusal".into()),
        );
        assert_eq!(shell.chat.text_preview.as_ref().unwrap().revision, 2);
        live(
            &mut shell,
            "current",
            3,
            Ok(TaskOutput::TextPreview(Some(turn(1, None)))),
        );
        assert_eq!(shell.chat.text_preview.as_ref().unwrap().revision, 2);
        shell.chat.selected = Some("other".into());
        live(
            &mut shell,
            "current",
            3,
            Ok(TaskOutput::TextPreview(Some(turn(4, None)))),
        );
        shell.chat.reconcile_text_preview();
        assert!(shell.chat.text_preview.is_none());
    }
    #[test]
    fn final_preview_survives_registry_eviction_until_the_canonical_sequence_arrives() {
        let mut chat = Chat::new();
        chat.selected = Some("current".into());
        chat.apply_text_preview(Some(turn(1, None)));
        chat.apply_text_preview(None);
        assert!(
            chat.text_preview.is_none(),
            "dropped unfinished output is cleared"
        );
        chat.apply_text_preview(Some(turn(2, Some(5))));
        chat.apply_text_preview(None);
        assert!(
            chat.text_preview.is_some(),
            "final canonical handover cannot blank the response"
        );
        chat.transcripts
            .entry("current".into())
            .or_default()
            .entries
            .push(TranscriptEntry {
                entry_id: id("entry"),
                sequence: Revision::new(5).unwrap(),
                body: TranscriptBody::AssistantAttempt {
                    request_id: id("request"),
                    attempt_id: AttemptId(id("attempt")),
                    visible_text: "response 2".into(),
                    interrupted: false,
                },
            });
        chat.reconcile_text_preview();
        assert!(chat.text_preview.is_none());
    }
}
