//! Help requests use the same five gateway operations as agent documentation tools.
use super::Shell;
use crate::{
    backend::{Reply, Unresolved},
    help_ops::{self, HelpSlot},
    help_screen::{HelpAction, HelpScope, HelpScreen},
    memory::RouteMemory,
    ops::Failure,
    paged::{Applied, Page},
    route::Route,
    screen::{Cx, Screen, ScreenContext, ScreenId},
    widgets::Tone,
};
use devworks_core::protocol::OperationState;
use devworks_core::protocol::records::{HelpAvailability, HelpReadView, HelpSnapshot};
use eframe::egui::Ui;
use std::time::Duration;

/// The catalog snapshot a topic page was read from: a page that continues another one must come
/// from the same.
fn snapshot_fence(snapshot: &HelpSnapshot) -> String {
    format!(
        "{:?}|{:?}|{}",
        snapshot.help_revision, snapshot.registry_revision, snapshot.app_version.0
    )
}

impl Shell {
    /// Opens Help on the topics of `screen` (a screen or settings-category ID).
    pub fn open_help_context(&mut self, screen: &str) {
        self.with_typed(|help: &mut HelpScreen, cx| help.open_context(cx, screen));
        self.route = Route::Help;
    }
}

impl Screen for HelpScreen {
    type Slot = HelpSlot;
    const ID: ScreenId = ScreenId::Help;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        if !self.topics.loaded && !self.topics.loading() {
            self.refresh_help(cx);
        }
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !self.topics.loading() {
            self.probe_help(cx);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: HelpSlot, reply: Reply) {
        self.apply_reply(cx, slot, reply);
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.help_screen(cx, ui);
    }
    fn restore_in_flight(&mut self, slot: &HelpSlot, _u: &Unresolved) {
        if *slot == HelpSlot::HelpGenerate {
            self.generating = true;
            self.generation_unknown = true;
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: self.selected.as_ref().map(|(_, topic)| topic.clone()),
        }
    }
}

impl HelpScreen {
    fn open_context(&mut self, cx: &mut Cx, screen: &str) {
        if cx.route() != Route::Help {
            self.back = Some(cx.route());
        }
        self.scope = HelpScope::Screen(screen.to_owned());
        self.selected = None;
        self.read = None;
        self.read_error = None;
        self.read_generation = self.read_generation.wrapping_add(1);
        self.reading = false;
        self.load_help_topics(cx, false);
    }
    /// Reads the topics of the scope from the first page (it was opened, searched or changed).
    fn load_help_topics(&mut self, cx: &mut Cx, append: bool) {
        let fetch = if append {
            let Some(fetch) = self.topics.more_page() else {
                return;
            };
            fetch
        } else {
            self.topics.reload()
        };
        self.send_help_topics(cx, fetch);
    }
    fn send_help_topics(&mut self, cx: &mut Cx, fetch: crate::paged::Fetch) {
        let append = fetch.append();
        let scope = self.scope.clone();
        let operation = match scope {
            HelpScope::Search(_) => "help.search",
            HelpScope::Screen(_) => "help.topic_for_screen",
            HelpScope::Package { .. } => "help.package_guides",
        };
        self.topics.error = None;
        let slot = HelpSlot::HelpTopics(fetch.generation, append);
        cx.paged(
            &mut self.topics,
            fetch,
            slot,
            "Loading local Help topics",
            operation,
            move |b, cursor| match scope {
                HelpScope::Search(query) => help_ops::search(b, &query, cursor),
                HelpScope::Screen(screen) => help_ops::context(b, &screen, cursor),
                HelpScope::Package { id, version } => help_ops::guides(b, &id, &version, cursor),
            },
        );
    }
    fn read_help_passage(&mut self, cx: &mut Cx) {
        let Some((topic, version)) = self.selected.clone() else {
            return;
        };
        let cursor = self.read_cursors.last().cloned().flatten();
        self.read_generation = self.read_generation.wrapping_add(1);
        let generation = self.read_generation;
        self.reading = true;
        self.read_error = None;
        if !cx.query(
            HelpSlot::HelpRead(generation),
            "Reading local Help passage",
            "help.read",
            move |b| help_ops::read(b, &topic, &version, cursor.as_deref()),
        ) {
            self.reading = false;
        }
    }
    fn select_help_topic(&mut self, cx: &mut Cx, topic: String, version: String) {
        self.selected = Some((topic, version));
        self.read_cursors = vec![None];
        self.read = None;
        self.read_help_passage(cx);
    }
    /// Reads the same scope again: every page shown is re-read, so the rows the user loaded
    /// stay. A page from another catalog snapshot than the first ends the read as it ends
    /// paging, with the prompt to refresh.
    fn refresh_help(&mut self, cx: &mut Cx) {
        if !self.topics.loading() {
            let fetch = self.topics.reread();
            self.send_help_topics(cx, fetch);
        }
    }
    fn probe_help(&mut self, cx: &mut Cx) {
        // The probe reads the first page only to learn the catalog's state; it never replaces
        // the rows, but it supersedes a topics request still in flight.
        let fetch = self.topics.probe();
        let scope = self.scope.clone();
        let operation = match scope {
            HelpScope::Search(_) => "help.search",
            HelpScope::Screen(_) => "help.topic_for_screen",
            HelpScope::Package { .. } => "help.package_guides",
        };
        let slot = HelpSlot::HelpProbe(fetch.generation);
        cx.paged(
            &mut self.topics,
            fetch,
            slot,
            "Checking local Help status",
            operation,
            move |b, _| match scope {
                HelpScope::Search(query) => help_ops::search(b, &query, None),
                HelpScope::Screen(screen) => help_ops::context(b, &screen, None),
                HelpScope::Package { id, version } => help_ops::guides(b, &id, &version, None),
            },
        );
    }
    fn help_snapshot(&mut self, snapshot: HelpSnapshot) {
        if self.snapshot.as_ref().is_some_and(|old| {
            old.help_revision != snapshot.help_revision
                || old.registry_revision != snapshot.registry_revision
                || old.app_version.0 != snapshot.app_version.0
        }) {
            // Passage cursors belong to this catalog snapshot, not merely the topic version.
            // Keep the exact selection, but start its reread with a fresh cursor. A late reply
            // from the previous snapshot cannot overwrite the newly adopted one.
            self.read_cursors = vec![None];
            self.read_generation = self.read_generation.wrapping_add(1);
            self.reading = false;
            self.read_error = None;
        }
        if let Some(operation) = &snapshot.generation_operation {
            let complete = matches!(operation.state, OperationState::ObservedComplete);
            let terminal = complete
                || matches!(
                    operation.state,
                    OperationState::Failed
                        | OperationState::NeedsInput
                        | OperationState::CancelledBeforeDispatch
                );
            let id = operation.operation_id.0.as_str();
            // This is the owner's current global status. Keep the original Accepted receipt
            // reference in its separate notice; a different latest ID never proves its outcome.
            self.generation_operation = Some(id.to_owned());
            if !self.generation_unknown {
                self.generating = !terminal;
            }
        }
        self.snapshot = Some(snapshot);
    }
    fn apply_reply(&mut self, cx: &mut Cx, slot: HelpSlot, reply: Reply) {
        match (slot, reply) {
            (HelpSlot::HelpRead(generation), _) if generation != self.read_generation => {}
            (HelpSlot::HelpTopics(generation, _), Reply::Query(outcome)) => {
                match help_ops::topics(outcome) {
                    Ok(page) => {
                        let changed = self.snapshot.as_ref().is_some_and(|old| {
                            old.help_revision != page.snapshot.help_revision
                                || old.registry_revision != page.snapshot.registry_revision
                                || old.app_version.0 != page.snapshot.app_version.0
                        });
                        let rows = Page::new(page.items.0, page.next_cursor.map(|cursor| cursor.0))
                            .fenced(snapshot_fence(&page.snapshot));
                        match self.topics.apply(generation, Ok(rows)) {
                            Applied::Stale => return,
                            Applied::Fenced => {
                                self.topics.fail("Local documentation changed while paging. Refresh Help to load a consistent catalog.");
                                return;
                            }
                            Applied::Continue(fetch) => {
                                self.send_help_topics(cx, fetch);
                                return;
                            }
                            _ => {}
                        }
                        self.notice = (!matches!(page.availability, HelpAvailability::Available))
                            .then(|| {
                                format!(
                                    "Documentation {:?}: {}",
                                    page.availability,
                                    page.reason
                                        .as_ref()
                                        .map_or("No local documentation is available.", |reason| {
                                            reason.0.as_str()
                                        })
                                )
                            });
                        self.help_snapshot(page.snapshot);
                        if matches!(self.scope, HelpScope::Screen(_))
                            && self.selected.is_none()
                            && let Some(topic) = self.topics.items.first()
                        {
                            let (id, version) =
                                (topic.id.as_str().to_owned(), topic.version.0.clone());
                            self.select_help_topic(cx, id, version);
                        } else if changed && self.selected.is_some() && !self.reading {
                            // Ask the owner about the inspected exact version; never silently
                            // replace it with another installed package/app version.
                            self.read_help_passage(cx);
                        }
                    }
                    Err(error) => {
                        self.topics.apply(generation, Err(error.describe()));
                    }
                }
            }
            (HelpSlot::HelpProbe(generation), Reply::Query(outcome)) => {
                if !self.topics.end(generation) {
                    return;
                }
                match help_ops::topics(outcome) {
                    Ok(page) => {
                        let changed = self.snapshot.as_ref().is_some_and(|old| {
                            old.help_revision != page.snapshot.help_revision
                                || old.registry_revision != page.snapshot.registry_revision
                                || old.app_version.0 != page.snapshot.app_version.0
                        });
                        self.help_snapshot(page.snapshot.clone());
                        self.topics.error = None;
                        // Initial publication can finish after the first empty catalog reply.
                        // Adopt the owner's first results, while preserving any existing page
                        // or inspected version when later generations change the catalog.
                        let initial = self.topics.items.is_empty() && !page.items.0.is_empty();
                        if initial {
                            self.notice = None;
                            self.topics.adopt(
                                Page::new(page.items.0, page.next_cursor.map(|cursor| cursor.0))
                                    .fenced(snapshot_fence(&page.snapshot)),
                            );
                            if matches!(self.scope, HelpScope::Screen(_))
                                && self.selected.is_none()
                                && let Some(topic) = self.topics.items.first()
                            {
                                let (id, version) =
                                    (topic.id.as_str().to_owned(), topic.version.0.clone());
                                self.select_help_topic(cx, id, version);
                            }
                        }
                        if !matches!(page.availability, HelpAvailability::Available) {
                            self.notice = Some(format!(
                                "Documentation {:?}: {}",
                                page.availability,
                                page.reason.map_or_else(
                                    || "No local documentation is available.".into(),
                                    |reason| reason.0
                                )
                            ));
                        } else if changed && !initial {
                            self.notice = Some("Local documentation changed. Refresh Help for current search results; \
                                inspected topics keep their exact version references.".into());
                        }
                        if changed && self.selected.is_some() && !self.reading {
                            self.read_help_passage(cx);
                        }
                    }
                    Err(error) => self.topics.error = Some(error.describe()),
                }
            }
            (HelpSlot::HelpRead(_), Reply::Query(outcome)) => {
                self.reading = false;
                match help_ops::passage(outcome) {
                    Ok(view) => {
                        let snapshot = match &view {
                            HelpReadView::Available { snapshot, .. }
                            | HelpReadView::Unavailable { snapshot, .. } => snapshot.clone(),
                        };
                        self.help_snapshot(snapshot);
                        self.read = Some(view);
                    }
                    Err(error) => self.read_error = Some(error.describe()),
                }
            }
            (HelpSlot::HelpGenerate, Reply::Command(outcome)) => match help_ops::generated(outcome)
            {
                Ok(help_ops::Generated::Accepted(id)) => {
                    self.generation_unknown = false;
                    self.generating = true;
                    self.generation_operation = Some(id.0.as_str().to_owned());
                    self.generation_notice = Some((
                        Tone::Neutral,
                        format!(
                            "Capability documentation generation accepted. Reference {}.",
                            id.0.as_str()
                        ),
                    ));
                    self.refresh_help(cx);
                }
                Ok(help_ops::Generated::Recorded(operation)) => {
                    self.generation_unknown = false;
                    self.generating = !matches!(
                        operation.state,
                        OperationState::ObservedComplete
                            | OperationState::Failed
                            | OperationState::NeedsInput
                            | OperationState::CancelledBeforeDispatch
                    );
                    self.generation_operation = Some(operation.operation_id.0.as_str().to_owned());
                    self.generation_notice = Some((
                        Tone::Neutral,
                        format!("Capability documentation: {:?}.", operation.state),
                    ));
                    self.refresh_help(cx);
                }
                Err(Failure::Unknown) => {
                    self.generation_unknown = true;
                    self.generating = true;
                    self.generation_notice = Some((
                        Tone::Attention,
                        "Generation outcome not confirmed. Use Confirm outcomes to check the \
                        original request; it will not run twice."
                            .into(),
                    ));
                }
                Err(error) => {
                    self.generating = false;
                    self.generation_unknown = false;
                    self.generation_notice = Some((Tone::Attention, error.describe()));
                    self.refresh_help(cx);
                }
            },
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn help_screen(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let mut actions = Vec::new();
        HelpScreen::ui(self, ui, cx.online(), &mut actions);
        for action in actions {
            match action {
                HelpAction::Search => {
                    self.scope = HelpScope::Search(self.query.trim().to_owned());
                    self.load_help_topics(cx, false);
                }
                HelpAction::AllTopics => {
                    self.query.clear();
                    self.scope = HelpScope::default();
                    self.load_help_topics(cx, false);
                }
                HelpAction::MoreTopics => self.load_help_topics(cx, true),
                HelpAction::Select(topic, version) => self.select_help_topic(cx, topic, version),
                HelpAction::OpenVersion(topic, version) => {
                    if let HelpScope::Package { id, .. } = self.scope.clone() {
                        self.scope = HelpScope::Package {
                            id,
                            version: version.clone(),
                        };
                        self.load_help_topics(cx, false);
                    }
                    self.select_help_topic(cx, topic, version);
                }
                HelpAction::Guides(id, version) => {
                    self.scope = HelpScope::Package { id, version };
                    self.load_help_topics(cx, false);
                }
                HelpAction::NextPassage => {
                    if let Some(HelpReadView::Available { passage, .. }) = &self.read
                        && let Some(cursor) = &passage.next_cursor
                    {
                        self.read_cursors.push(Some(cursor.0.clone()));
                        self.read_help_passage(cx);
                    }
                }
                HelpAction::PreviousPassage => {
                    if self.read_cursors.len() > 1 {
                        self.read_cursors.pop();
                        self.read_help_passage(cx);
                    }
                }
                HelpAction::ReloadPassage => self.read_help_passage(cx),
                HelpAction::Refresh => self.refresh_help(cx),
                HelpAction::Back => {
                    if let Some(route) = self.back.take() {
                        cx.navigate(route);
                    }
                }
                HelpAction::Generate => {
                    if self.generating {
                        continue;
                    }
                    let Some(snapshot) = &self.snapshot else {
                        continue;
                    };
                    if !snapshot.can_generate {
                        continue;
                    }
                    let registry = snapshot.registry_revision.get();
                    let help = snapshot.help_revision.get();
                    self.generating = true;
                    self.generation_notice = None;
                    self.generation_operation = None;
                    if !cx.command(
                        HelpSlot::HelpGenerate,
                        "Generating capability documentation",
                        "help.generate_capability_docs",
                        move |b| help_ops::generate(b, registry, help),
                    ) {
                        self.generating = false;
                    }
                }
            }
        }
        // Only normal hints / the shell's bounded periodic refresh read pending operation facts.
        if self.generating && !self.generation_unknown {
            ui.ctx().request_repaint_after(Duration::from_secs(3));
        }
    }
}
