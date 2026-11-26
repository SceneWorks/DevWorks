//! Agents & sessions: the selected project's coordinators and workers (`agents.list`), each
//! root's current step (`runs.inspect`), and the selected agent's transcript and assignment
//! messages (`agents.inbox`).
use crate::backend::{Reply, screen_slot};
use crate::memory::RouteMemory;
use crate::ops;
use crate::paged::{Applied, Fetch, ListText, Paged, list_body};
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId};
use crate::widgets::{self, Header, ViewState};
use crate::work_ops as wops;
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::records::{AgentMessage, AgentSummary, MessageClass, RootSnapshot};
use devworks_core::protocol::{OperationState, Phase};
use eframe::egui::{self, Ui};
use std::collections::{BTreeMap, BTreeSet};

/// The Agents requests the screen waits for. The names are persisted (see
/// [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum AgentsSlot {
    /// One project's assignments (its ID, and whether only the live ones).
    AgentsList(String, bool),
    /// The run snapshot of a root an agent works on (the root's ID).
    AgentsRun(String),
    /// The messages of one assignment (its ID).
    AgentsMessages(String),
}
screen_slot!(AgentsSlot, Agents);

#[derive(Default)]
pub struct AgentsScreen {
    pub list: Paged<AgentSummary>,
    /// The project and tab (`live_only`) the list was read for.
    scope: Option<(String, bool)>,
    /// Each listed root's run snapshot: its activation phase is the work's current step.
    runs: BTreeMap<String, RootSnapshot>,
    /// The selected assignment's messages, by assignment ID.
    messages: Option<(String, Paged<AgentMessage>)>,
}

impl Screen for AgentsScreen {
    type Slot = AgentsSlot;
    const ID: ScreenId = ScreenId::Agents;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.load(cx);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        let pending = self
            .scope
            .as_ref()
            .is_some_and(|(p, live)| cx.pending(&AgentsSlot::AgentsList(p.clone(), *live)));
        if !pending {
            self.load(cx);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: AgentsSlot, reply: Reply) {
        match (slot, reply) {
            // A reply for a project or tab no longer shown is dropped.
            (AgentsSlot::AgentsList(p, live), Reply::Query(_))
                if self.scope.as_ref() != Some(&(p.clone(), live)) => {}
            (AgentsSlot::AgentsList(p, live), Reply::Query(q)) => {
                let applied = self
                    .list
                    .apply_current(ops::agents_page(q).map_err(|e| e.describe()));
                if let Applied::Continue(fetch) = applied {
                    Self::send(cx, &mut self.list, p, live, fetch);
                } else {
                    self.inspect_roots(cx);
                }
            }
            (AgentsSlot::AgentsRun(root), Reply::Query(q)) => {
                if let Ok(s) = wops::run_snapshot(q) {
                    self.runs.insert(root, s);
                }
            }
            (AgentsSlot::AgentsMessages(a), Reply::Query(q)) => {
                if let Some((shown, list)) = &mut self.messages
                    && *shown == a
                    && let Applied::Continue(fetch) =
                        list.apply_current(ops::agent_messages_page(q).map_err(|e| e.describe()))
                {
                    Self::send_messages(cx, list, a, fetch);
                }
            }
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.view(cx, ui);
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        if self
            .scope
            .as_ref()
            .is_some_and(|(p, _)| p == scope.project_id.0.as_str())
        {
            *self = Self::default();
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: mem.selection.clone(),
        }
    }
}

impl AgentsScreen {
    /// The selected project's assignments for the shown tab (Active: only the live ones);
    /// another project or tab starts from its first page.
    fn load(&mut self, cx: &mut Cx) {
        let live = cx.routes.get(&Route::Agents).is_none_or(|m| m.tab == 0);
        let scope = cx.selected_project().map(|p| (p.to_owned(), live));
        if scope != self.scope {
            self.scope = scope.clone();
            self.list = Default::default();
            self.runs.clear();
            self.messages = None;
        }
        let Some((p, live)) = scope else {
            return;
        };
        let fetch = self.list.reread();
        Self::send(cx, &mut self.list, p, live, fetch);
        if let Some((a, list)) = &mut self.messages {
            let fetch = list.reread();
            Self::send_messages(cx, list, a.clone(), fetch);
        }
    }
    fn send(
        cx: &mut Cx,
        list: &mut Paged<AgentSummary>,
        project: String,
        live: bool,
        fetch: Fetch,
    ) {
        let p = project.clone();
        cx.paged(
            list,
            fetch,
            AgentsSlot::AgentsList(project, live),
            "Loading agents",
            "agents.list",
            move |b, cursor| ops::agents_list(b, &p, live, cursor),
        );
    }
    fn send_messages(
        cx: &mut Cx,
        list: &mut Paged<AgentMessage>,
        assignment: String,
        fetch: Fetch,
    ) {
        let a = assignment.clone();
        cx.paged(
            list,
            fetch,
            AgentsSlot::AgentsMessages(assignment),
            "Loading agent messages",
            "agents.inbox",
            move |b, cursor| ops::agent_messages(b, &a, cursor),
        );
    }
    /// Reads the run snapshot of every listed root, once per complete list read.
    fn inspect_roots(&mut self, cx: &mut Cx) {
        let roots: BTreeSet<String> = self
            .list
            .items
            .iter()
            .map(|a| a.root_id.0.as_str().to_owned())
            .collect();
        for root in roots {
            if cx.pending(&AgentsSlot::AgentsRun(root.clone())) {
                continue;
            }
            let r = root.clone();
            cx.query(
                AgentsSlot::AgentsRun(root),
                "Reading run state",
                "runs.inspect",
                move |b| wops::run_inspect(b, &r),
            );
        }
    }
    /// Reads the messages of the selected assignment (once per selection).
    fn select(&mut self, cx: &mut Cx, assignment: &str) {
        if self.messages.as_ref().is_some_and(|(a, _)| a == assignment) {
            return;
        }
        let mut list = Paged::default();
        let fetch = list.reload();
        Self::send_messages(cx, &mut list, assignment.to_owned(), fetch);
        self.messages = Some((assignment.to_owned(), list));
    }

    fn view(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let screen = Route::Agents.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut reload = false;
        let mut more = false;
        let mut link = false;
        let mut open: Option<String> = None;
        let mut mem = cx.routes.remove(&Route::Agents).unwrap_or_default();
        let shown_tab = mem.tab;
        let project = cx.selected_project().map(str::to_owned);
        let Self {
            list,
            runs,
            messages,
            ..
        } = self;
        let scrolls = &mut *cx.scrolls;
        let mut st = (&mut reload, &mut mem, &mut more, &mut link, &mut open);
        let (_, tab) = widgets::pane(
            ui,
            Header {
                title: screen.title,
                summary: screen.summary,
                tabs: screen.subviews,
                tab: shown_tab,
            },
            &mut st,
            |ui, st| {
                if widgets::secondary_button(ui, "Refresh").clicked() {
                    *st.0 = true;
                }
                if let Some((label, _)) = screen.link
                    && widgets::secondary_button(ui, label).clicked()
                {
                    *st.3 = true;
                }
            },
            |ui, st, tab| {
                // Offline says why before anything that needs the runtime.
                if let Err(reason) = &online {
                    widgets::state_block(
                        ui,
                        &ViewState::Offline {
                            reason: reason.clone(),
                        },
                    );
                    return;
                }
                if project.is_none() {
                    widgets::state_block(
                        ui,
                        &ViewState::Empty {
                            title: "No project selected".into(),
                            body: "Select a project to see its coordinators and workers.".into(),
                        },
                    );
                    return;
                }
                let RouteMemory {
                    filter, selection, ..
                } = &mut *st.1;
                let roles: BTreeMap<String, String> = list
                    .items
                    .iter()
                    .map(|a| {
                        (
                            a.assignment_id.0.as_str().to_owned(),
                            format!("{} {}", kind(a), a.role_id.as_str()),
                        )
                    })
                    .collect();
                ui.columns(2, |cols| {
                    list_body(
                        &mut cols[0],
                        &online,
                        list,
                        ListText {
                            what: "agents",
                            empty_title: if tab == 0 {
                                "No agents are working"
                            } else {
                                "No agents yet"
                            },
                            empty_body: "Coordinators and workers appear here when work runs.",
                            id_salt: "agents-list",
                        },
                        st.0,
                        st.2,
                        scrolls,
                        |a: &AgentSummary| {
                            format!(
                                "{} {} {}",
                                a.role_id.as_str(),
                                a.work_id.0.as_str(),
                                a.root_id.0.as_str()
                            )
                        },
                        |a: &AgentSummary| a.assignment_id.0.as_str().to_owned(),
                        filter,
                        selection,
                        |ui, a: &AgentSummary, selected| {
                            let step = runs.get(a.root_id.0.as_str()).map_or("reading", step_label);
                            ui.selectable_label(
                                selected,
                                format!(
                                    "{}  ·  {}  ·  {}  ·  {}  ·  {}",
                                    kind(a),
                                    a.role_id.as_str(),
                                    a.work_id.0.as_str(),
                                    state_label(&a.state),
                                    step
                                ),
                            )
                            .clicked()
                        },
                    );
                    let selected = selection.as_ref().and_then(|id| {
                        list.items.iter().find(|a| a.assignment_id.0.as_str() == id)
                    });
                    let ui = &mut cols[1];
                    let Some(a) = selected else {
                        widgets::caption(ui, "Select an agent to see its transcript and messages.");
                        return;
                    };
                    egui::ScrollArea::vertical()
                        .id_salt("agent-detail")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.strong(format!("{} {}", kind(a), a.role_id.as_str()));
                            widgets::caption(
                                ui,
                                format!(
                                    "Assignment {} · work {} · root {}{}",
                                    a.assignment_id.0.as_str(),
                                    a.work_id.0.as_str(),
                                    a.root_id.0.as_str(),
                                    a.parent_id
                                        .as_ref()
                                        .map(|p| format!(" · reports to {}", p.0.as_str()))
                                        .unwrap_or_default()
                                ),
                            );
                            if widgets::secondary_button(ui, "Open transcript").clicked() {
                                *st.4 = Some(a.session_id.as_str().to_owned());
                            }
                            ui.label(egui::RichText::new("Messages").strong());
                            let Some((shown, m)) = messages
                                .as_ref()
                                .filter(|(shown, _)| shown == a.assignment_id.0.as_str())
                            else {
                                ui.spinner();
                                return;
                            };
                            let _ = shown;
                            if !m.loaded {
                                ui.spinner();
                            } else if let Some(e) = &m.error {
                                widgets::caption(ui, format!("Messages could not be read: {e}"));
                            } else if m.items.is_empty() {
                                widgets::caption(ui, "No messages for this assignment yet.");
                            }
                            for message in &m.items {
                                let name = |id: &str| {
                                    roles.get(id).cloned().unwrap_or_else(|| id.to_owned())
                                };
                                let from = message
                                    .from_assignment_id
                                    .as_ref()
                                    .map_or("You".to_owned(), |f| name(f.0.as_str()));
                                let to = name(message.to_assignment_id.0.as_str());
                                widgets::caption(
                                    ui,
                                    format!(
                                        "{} · from {from} to {to}",
                                        class_label(&message.message_class)
                                    ),
                                );
                                ui.label(message.content.0.as_str());
                                ui.add_space(4.0);
                            }
                        });
                });
            },
        );
        mem.tab = tab;
        let selection = mem.selection.clone();
        cx.routes.insert(Route::Agents, mem);
        if let Some(session) = open {
            cx.open_session(session);
        }
        if link && let Some((_, target)) = screen.link {
            cx.navigate(target);
        }
        if let Some(a) = selection
            && self
                .list
                .items
                .iter()
                .any(|x| x.assignment_id.0.as_str() == a)
        {
            self.select(cx, &a);
        }
        if reload || tab != shown_tab {
            self.load(cx);
        }
        if more && let (Some((p, live)), Some(fetch)) = (self.scope.clone(), self.list.more_page())
        {
            Self::send(cx, &mut self.list, p, live, fetch);
        }
    }
}

/// Coordinator (no parent) or worker.
fn kind(a: &AgentSummary) -> &'static str {
    if a.parent_id.is_none() {
        "Coordinator"
    } else {
        "Worker"
    }
}

/// The root's current step: what its activation is doing.
fn step_label(s: &RootSnapshot) -> &'static str {
    match s.activation.as_ref().map(|a| &a.phase) {
        None => "not started",
        Some(Phase::Pending) => "starting",
        Some(Phase::Claimed) => "claimed by a runner",
        Some(Phase::Running) => "running",
        Some(Phase::Stopping) => "stopping",
        Some(Phase::Stopped) => "stopped",
    }
}

fn state_label(s: &OperationState) -> &'static str {
    match s {
        OperationState::IntentRecorded => "ready",
        OperationState::Dispatching => "working",
        OperationState::NeedsInput => "waiting",
        OperationState::ObservedComplete => "finished",
        OperationState::Failed => "failed",
        OperationState::CancelledBeforeDispatch => "superseded",
        _ => "other",
    }
}

fn class_label(c: &MessageClass) -> &'static str {
    match c {
        MessageClass::Instruction => "Instruction",
        MessageClass::Status => "Status",
        MessageClass::Question => "Question",
        MessageClass::Evidence => "Evidence",
    }
}
