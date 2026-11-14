//! Watchers (WATCH-09): the Watchers screen lists every watcher; the chat sidebar's Current tab
//! lists the selected conversation's and the feature detail the selected feature's. Each row
//! shows who owns the watcher (project, feature, work item, assignment and agent), its source,
//! state, elapsed time, last poll and output tail, and can cancel it. Lists poll while shown,
//! since watchers change without a runtime hint.
use crate::{
    backend::{Reply, Unresolved},
    memory::RouteMemory,
    ops::Failure,
    route::Route,
    screen::{Cx, Screen, ScreenContext, ScreenId},
    theme,
    watch_ops::{self, WatchScope, WatchSlot, WatchView},
    widgets::{self, StateAction, Tone, ViewState},
};
use devworks_core::protocol::DomainErrorCode;
use devworks_core::protocol::records::{
    WatchOutcomeKind, WatchSourceKind, WatcherRecord, WatcherState,
};
use eframe::egui::{self, RichText, Ui};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// How often a shown list is read again.
pub const WATCH_POLL: Duration = Duration::from_secs(3);
/// Output lines a row shows (the newest).
const TAIL_LINES: usize = 8;

/// One shown list of watchers.
#[derive(Default)]
pub struct WatchList {
    pub scope: Option<WatchScope>,
    pub items: Vec<WatcherRecord>,
    pub next_cursor: Option<String>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
    pub generation: u64,
    pub loaded_at: Option<Instant>,
}

#[derive(Default)]
pub struct WatchersScreen {
    pub all: WatchList,
    pub session: WatchList,
    pub feature: WatchList,
    /// Watchers whose cancel is in flight.
    pub cancelling: BTreeSet<String>,
    pub notice: Option<(Tone, String)>,
    /// Project names by ID, for attribution.
    pub project_names: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub enum WatchAction {
    /// Show `scope` in a view (read it when it is not what the view shows).
    Show(WatchView, WatchScope),
    Refresh(WatchView),
    More(WatchView),
    /// Cancel a watcher; `true` also stops the process it launched.
    Cancel(WatcherRecord, bool),
}

impl WatchersScreen {
    fn list(&self, view: WatchView) -> &WatchList {
        match view {
            WatchView::All => &self.all,
            WatchView::Session => &self.session,
            WatchView::Feature => &self.feature,
        }
    }
    fn list_mut(&mut self, view: WatchView) -> &mut WatchList {
        match view {
            WatchView::All => &mut self.all,
            WatchView::Session => &mut self.session,
            WatchView::Feature => &mut self.feature,
        }
    }

    /// Runs actions from any of the views.
    pub fn act(&mut self, cx: &mut Cx, actions: Vec<WatchAction>) {
        for action in actions {
            match action {
                WatchAction::Show(view, scope) => {
                    let list = self.list_mut(view);
                    if list.scope.as_ref() != Some(&scope) {
                        // Another conversation or feature: its rows replace the shown ones.
                        *list = WatchList {
                            scope: Some(scope),
                            generation: list.generation,
                            ..WatchList::default()
                        };
                        self.load(cx, view, false);
                    }
                }
                WatchAction::Refresh(view) => {
                    if !self.list(view).loading {
                        self.load(cx, view, false);
                    }
                }
                WatchAction::More(view) => self.load(cx, view, true),
                WatchAction::Cancel(watcher, stop_process) => {
                    let id = watcher.id.as_str().to_owned();
                    self.notice = None;
                    if cx.command(
                        WatchSlot::WatcherCancel(id.clone()),
                        "Cancelling the watcher",
                        "watch.cancel",
                        move |b| watch_ops::cancel(b, &watcher, stop_process),
                    ) {
                        self.cancelling.insert(id);
                    }
                }
            }
        }
    }

    fn load(&mut self, cx: &mut Cx, view: WatchView, append: bool) {
        let list = self.list_mut(view);
        let Some(scope) = list.scope.clone() else {
            return;
        };
        let cursor = if append {
            let Some(cursor) = list.next_cursor.clone() else {
                return;
            };
            Some(cursor)
        } else {
            None
        };
        list.generation = list.generation.wrapping_add(1);
        let generation = list.generation;
        // The shown rows stay until the reply replaces them.
        if cx.query(
            WatchSlot::WatcherList(view, generation, append),
            "Loading watchers",
            "watch.list",
            move |b| watch_ops::list(b, &scope, cursor.as_deref()),
        ) {
            list.loading = true;
        } else {
            list.loaded_at = Some(Instant::now());
        }
    }

    fn reload_shown(&mut self, cx: &mut Cx) {
        for view in [WatchView::All, WatchView::Session, WatchView::Feature] {
            if self.list(view).scope.is_some() {
                self.load(cx, view, false);
            }
        }
    }

    fn apply_reply(&mut self, cx: &mut Cx, slot: WatchSlot, reply: Reply) {
        match (slot, reply) {
            (WatchSlot::WatcherList(view, generation, append), Reply::Query(outcome)) => {
                let list = self.list_mut(view);
                if generation != list.generation {
                    return;
                }
                list.loading = false;
                list.loaded_at = Some(Instant::now());
                match watch_ops::page(outcome) {
                    Ok(page) => {
                        if append {
                            list.items.extend(page.items.0);
                        } else {
                            list.items = page.items.0;
                        }
                        list.next_cursor = page.next_cursor.map(|c| c.0);
                        list.loaded = true;
                        list.error = None;
                    }
                    Err(e) => list.error = Some(e.describe()),
                }
            }
            (WatchSlot::WatcherCancel(id), Reply::Command(outcome)) => {
                self.cancelling.remove(&id);
                self.notice = Some(match watch_ops::cancelled(outcome) {
                    Ok(w) if matches!(w.state, WatcherState::Stopping) => (
                        Tone::Good,
                        "Stop requested; the watcher ends once its process has stopped.".into(),
                    ),
                    Ok(_) => (Tone::Good, "Watcher cancelled.".into()),
                    Err(Failure::Refused(f)) if f.code == DomainErrorCode::RevisionConflict => (
                        Tone::Attention,
                        "The watcher changed meanwhile; it was reloaded.".into(),
                    ),
                    Err(Failure::Refused(f)) if f.code == DomainErrorCode::Conflict => (
                        Tone::Attention,
                        "The watcher has already ended or has no process to stop.".into(),
                    ),
                    Err(e) => (Tone::Bad, e.describe()),
                });
                self.reload_shown(cx);
            }
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }

    /// The list of `view` showing `scope`: rows, paging and retry. Pushes what the user asked
    /// for, and a read when the scope changed or the shown rows are older than [`WATCH_POLL`].
    pub fn section(
        &self,
        ui: &mut Ui,
        view: WatchView,
        scope: WatchScope,
        online: Result<(), &str>,
        actions: &mut Vec<WatchAction>,
    ) {
        let list = self.list(view);
        if list.scope.as_ref() != Some(&scope) {
            actions.push(WatchAction::Show(view, scope));
        } else if online.is_ok()
            && !list.loading
            && list.loaded_at.is_none_or(|at| at.elapsed() >= WATCH_POLL)
        {
            actions.push(WatchAction::Refresh(view));
        }
        ui.ctx().request_repaint_after(WATCH_POLL);
        if let Err(reason) = online {
            widgets::state_block(
                ui,
                &ViewState::Offline {
                    reason: reason.to_owned(),
                },
            );
            return;
        }
        if let Some(error) = &list.error {
            let state = ViewState::Failed {
                what: "watchers".into(),
                error: error.clone(),
            };
            if widgets::state_block(ui, &state) == StateAction::Retry {
                actions.push(WatchAction::Refresh(view));
            }
            return;
        }
        if !list.loaded {
            widgets::state_block(
                ui,
                &ViewState::Loading {
                    what: "watchers".into(),
                },
            );
            return;
        }
        if list.items.is_empty() {
            widgets::state_block(
                ui,
                &ViewState::Empty {
                    title: "No watchers".into(),
                    body: match view {
                        WatchView::All => "No agent is watching anything.",
                        WatchView::Session => "This conversation's agent is watching nothing.",
                        WatchView::Feature => "No agent on this feature is watching anything.",
                    }
                    .into(),
                },
            );
            return;
        }
        for watcher in &list.items {
            self.row(ui, watcher, actions);
            ui.add_space(6.0);
        }
        if list.next_cursor.is_some()
            && ui
                .add_enabled(!list.loading, egui::Button::new("More watchers"))
                .clicked()
        {
            actions.push(WatchAction::More(view));
        }
    }

    fn row(&self, ui: &mut Ui, w: &WatcherRecord, actions: &mut Vec<WatchAction>) {
        egui::Frame::new()
            .fill(theme::RAISED)
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(source_label(w)).strong());
                    let (text, tone) = state_label(w);
                    widgets::chip(ui, text, tone);
                });
                widgets::caption(ui, self.owner_line(w));
                widgets::caption(ui, timing_line(w));
                if let Some(tail) = w.output_tail.as_ref().filter(|t| !t.0.trim().is_empty()) {
                    let lines: Vec<&str> = tail.0.lines().collect();
                    let shown = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
                    let more = lines.len() > TAIL_LINES || w.output_truncated;
                    widgets::caption(
                        ui,
                        if more {
                            "Output (latest lines)"
                        } else {
                            "Output"
                        },
                    );
                    ui.label(RichText::new(shown).monospace().small().color(theme::MUTED));
                }
                if matches!(w.state, WatcherState::Active) {
                    let busy = self.cancelling.contains(w.id.as_str());
                    ui.horizontal_wrapped(|ui| {
                        if busy {
                            ui.add_enabled(false, egui::Button::new("Cancelling…"));
                            return;
                        }
                        if ui.button("Cancel watcher").clicked() {
                            actions.push(WatchAction::Cancel(w.clone(), false));
                        }
                        if w.process.is_some() && ui.button("Cancel and stop process").clicked() {
                            actions.push(WatchAction::Cancel(w.clone(), true));
                        }
                    });
                }
            });
    }

    fn owner_line(&self, w: &WatcherRecord) -> String {
        let mut parts = Vec::new();
        if let Some(project) = &w.project_id {
            let id = project.0.as_str();
            parts.push(format!(
                "Project {}",
                self.project_names.get(id).map_or(id, String::as_str)
            ));
        } else {
            parts.push("No project".to_owned());
        }
        match &w.owner {
            Some(owner) => {
                if let Some(feature) = &owner.feature_title {
                    parts.push(format!("Feature “{}”", feature.0));
                }
                if let Some(work) = &owner.work_title
                    && owner.work_id != owner.feature_id
                {
                    parts.push(format!("Work “{}”", work.0));
                }
                if let Some(assignment) = &owner.assignment_id {
                    parts.push(format!("Assignment {}", assignment.0.as_str()));
                }
                parts.push(format!("Agent {}", owner.agent_id.0.as_str()));
            }
            None => parts.push(format!("Session {}", w.session_id.as_str())),
        }
        parts.join(" · ")
    }
}

fn source_label(w: &WatcherRecord) -> String {
    let mut label = match w.source_kind {
        WatchSourceKind::Process => match &w.process {
            Some(p) => format!("Process {}", p.executable_ref.as_str()),
            None => "Process".to_owned(),
        },
        WatchSourceKind::Probe => match &w.probe {
            Some(p) => format!(
                "Probe every {} · {} runs",
                duration(p.interval_ms.get()),
                p.runs.get()
            ),
            None => "Probe".to_owned(),
        },
        WatchSourceKind::Timer => "Timer".to_owned(),
        WatchSourceKind::File => "File".to_owned(),
    };
    if let Some(matches) = &w.matches {
        label.push_str(&format!(" · {} matches", matches.get()));
    }
    label
}

fn state_label(w: &WatcherRecord) -> (String, Tone) {
    match w.state {
        WatcherState::Active => ("Watching".into(), Tone::Accent),
        WatcherState::Stopping => ("Stopping".into(), Tone::Attention),
        WatcherState::Cancelled => ("Cancelled".into(), Tone::Neutral),
        WatcherState::Settled => match w.outcome.as_ref().map(|o| &o.kind) {
            Some(WatchOutcomeKind::Succeeded) => ("Succeeded".into(), Tone::Good),
            Some(WatchOutcomeKind::Failed) => ("Failed".into(), Tone::Bad),
            Some(WatchOutcomeKind::Cancelled) => ("Cancelled".into(), Tone::Neutral),
            Some(WatchOutcomeKind::TimedOut) => ("Timed out".into(), Tone::Attention),
            Some(WatchOutcomeKind::Interrupted) => ("Interrupted".into(), Tone::Attention),
            Some(WatchOutcomeKind::Lost) => ("Lost".into(), Tone::Bad),
            Some(WatchOutcomeKind::Uncertain) | None => ("Ended".into(), Tone::Attention),
        },
    }
}

/// Elapsed time, and for an open watcher how long before this read it last polled its source.
fn timing_line(w: &WatcherRecord) -> String {
    let elapsed = w.elapsed_ms.get();
    let mut line = format!("Elapsed {}", duration(elapsed));
    if matches!(w.state, WatcherState::Active | WatcherState::Stopping) {
        let read_at = w.created_at_ms.get() + elapsed;
        match &w.last_polled_at_ms {
            Some(at) => line.push_str(&format!(
                " · last poll {} ago",
                duration((read_at - at.get()).max(0))
            )),
            None => line.push_str(" · not polled since DevWorks started"),
        }
    }
    line
}

/// `1 h 02 m`, `3 m 05 s` or `4 s`.
pub fn duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} m {:02} s", s / 60, s % 60),
        _ => format!("{} h {:02} m", s / 3600, s / 60 % 60),
    }
}

impl Screen for WatchersScreen {
    type Slot = WatchSlot;
    const ID: ScreenId = ScreenId::Watchers;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.act(cx, vec![WatchAction::Show(WatchView::All, WatchScope::All)]);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        self.act(cx, vec![WatchAction::Refresh(WatchView::All)]);
    }
    fn apply(&mut self, cx: &mut Cx, slot: WatchSlot, reply: Reply) {
        self.apply_reply(cx, slot, reply);
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        let screen = Route::Watchers.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut actions = Vec::new();
        let mut st = (&*self, &mut actions);
        widgets::pane(
            ui,
            widgets::Header {
                title: screen.title,
                summary: screen.summary,
                tabs: &[],
                tab: 0,
            },
            &mut st,
            |ui, st| {
                if ui
                    .add_enabled(online.is_ok(), egui::Button::new("Refresh"))
                    .clicked()
                {
                    st.1.push(WatchAction::Refresh(WatchView::All));
                }
            },
            |ui, st, _| {
                if let Some((tone, text)) = &st.0.notice {
                    widgets::chip(ui, text.clone(), *tone);
                }
                egui::ScrollArea::vertical()
                    .id_salt("watchers-list")
                    .show(ui, |ui| {
                        st.0.section(
                            ui,
                            WatchView::All,
                            WatchScope::All,
                            online.as_ref().map(|_| ()).map_err(String::as_str),
                            st.1,
                        );
                    });
            },
        );
        self.act(cx, actions);
    }
    fn restore_in_flight(&mut self, slot: &WatchSlot, _u: &Unresolved) {
        if let WatchSlot::WatcherCancel(id) = slot {
            self.cancelling.insert(id.clone());
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::duration;

    #[test]
    fn durations_read_in_the_largest_two_units() {
        assert_eq!(duration(4_200), "4 s");
        assert_eq!(duration(185_000), "3 m 05 s");
        assert_eq!(duration(3_720_000), "1 h 02 m");
        assert_eq!(duration(-5), "0 s");
    }
}
