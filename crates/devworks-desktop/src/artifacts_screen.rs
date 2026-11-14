//! Artifacts, Resources and Browser sessions: three lists, each bound to one query, with a
//! filter and a remembered selection in the route's memory.
use crate::backend::{Reply, screen_slot};
use crate::chat;
use crate::memory::RouteMemory;
use crate::ops::{self, Failure};
use crate::route::Route;
use crate::screen::{Cx, Screen, ScreenContext, ScreenId};
use crate::shell::{ListState, ListText, list_body};
use crate::widgets::{self, Header, ViewState};
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::records::{ArtifactRecord, BrowserSession, ItemState, ResourceRecord};
use eframe::egui::{self, Ui};
use std::collections::{BTreeMap, HashMap};

/// The Artifacts request the screen waits for. The name is persisted (see
/// [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ArtifactsSlot {
    ArtifactsList,
}
screen_slot!(ArtifactsSlot, Artifacts);

/// The Resources request the screen waits for. The name is persisted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ResourcesSlot {
    ResourcesList,
}
screen_slot!(ResourcesSlot, Resources);

/// The Browser sessions requests the screen waits for. The names are persisted.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BrowserSlot {
    /// The workspace's browser sessions.
    BrowserSessions,
    /// The browser sessions of one project (its ID).
    ProjectBrowserSessions(String),
    /// Closing a browser session (its ID).
    BrowserClose(String),
}
screen_slot!(BrowserSlot, Browser);

/// What each list screen reports to the coordinator: its route memory.
fn route_context(mem: &RouteMemory) -> ScreenContext {
    ScreenContext {
        tab: mem.tab,
        selection: mem.selection.clone(),
    }
}

#[derive(Default)]
pub struct ArtifactsScreen {
    pub list: ListState<ArtifactRecord>,
    /// Applied artifact mutations (attachment saves and deletions). A list query started
    /// before the latest one may miss it: its reply is dropped and the list re-queried, so it
    /// never overwrites newer records.
    pub mutations: u64,
    list_at: u64,
}

impl Screen for ArtifactsScreen {
    type Slot = ArtifactsSlot;
    const ID: ScreenId = ScreenId::Artifacts;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.load(cx);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !cx.pending(&ArtifactsSlot::ArtifactsList) {
            self.load(cx);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: ArtifactsSlot, reply: Reply) {
        match (slot, reply) {
            (ArtifactsSlot::ArtifactsList, Reply::Query(_)) if self.list_at != self.mutations => {
                self.load(cx);
            }
            (ArtifactsSlot::ArtifactsList, Reply::Query(q)) => match ops::artifacts(q) {
                Ok((items, more)) => {
                    self.list = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    };
                }
                Err(e) => self.list.error = Some(e.describe()),
            },
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.view(cx, ui);
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        self.list
            .items
            .retain(|r| !scope.deleted_artifact_ids.contains(&r.artifact.artifact_id));
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        route_context(mem)
    }
}

impl ArtifactsScreen {
    fn load(&mut self, cx: &mut Cx) {
        self.list_at = self.mutations;
        cx.query(
            ArtifactsSlot::ArtifactsList,
            "Loading artifacts",
            "artifacts.list",
            ops::workspace_artifacts,
        );
    }
    fn view(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let screen = Route::Artifacts.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut reload = false;
        let mut mem = cx.routes.remove(&Route::Artifacts).unwrap_or_default();
        let tab = mem.tab;
        let list = &self.list;
        let scrolls = &mut *cx.scrolls;
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
        cx.routes.insert(Route::Artifacts, mem);
        if reload {
            self.load(cx);
        }
    }
}

#[derive(Default)]
pub struct BrowserScreen {
    /// Every listed browser session: the workspace's, then each listed project's.
    pub list: ListState<BrowserSession>,
    /// The browser sessions per scope (`None`: the workspace), combined into `list`.
    parts: BTreeMap<Option<String>, ListState<BrowserSession>>,
}

impl Screen for BrowserScreen {
    type Slot = BrowserSlot;
    const ID: ScreenId = ScreenId::Browser;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.load(cx);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !cx.pending(&BrowserSlot::BrowserSessions) {
            self.load(cx);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: BrowserSlot, reply: Reply) {
        match (slot, reply) {
            (BrowserSlot::BrowserSessions, Reply::Query(q)) => {
                self.part(None, ops::browser_sessions(q));
            }
            (BrowserSlot::ProjectBrowserSessions(p), Reply::Query(q)) => {
                self.part(Some(p), ops::browser_sessions(q));
            }
            (BrowserSlot::BrowserClose(_), Reply::Command(c)) => {
                match ops::browser_closed(c) {
                    Ok(true) => cx.announce("The browser session is being closed."),
                    Ok(false) => cx.announce(
                        "Close is already requested, or the browser session has stopped.",
                    ),
                    Err(Failure::Unknown) => {
                        cx.announce("Closing the browser session: outcome not confirmed.")
                    }
                    Err(e) => cx.announce(format!(
                        "The browser session could not be closed: {}",
                        e.describe()
                    )),
                }
                self.load(cx);
            }
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.view(cx, ui);
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        self.parts
            .remove(&Some(scope.project_id.0.as_str().to_owned()));
        self.list = Default::default();
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        route_context(mem)
    }
}

impl BrowserScreen {
    /// The workspace's browser sessions and the selected project's, or every listed project's
    /// when none is selected: a person may see and interrupt each of them.
    fn load(&mut self, cx: &mut Cx) {
        let projects: Vec<String> = match &cx.selected_project {
            Some(p) => vec![p.clone()],
            None => {
                if !cx.projects.list.loaded && !cx.pending(&crate::backend::Slot::ProjectsList) {
                    cx.load_projects();
                }
                cx.projects
                    .list
                    .items
                    .iter()
                    .map(|p| p.id.0.as_str().to_owned())
                    .collect()
            }
        };
        self.parts
            .retain(|scope, _| scope.as_ref().is_none_or(|p| projects.contains(p)));
        self.combine();
        cx.query(
            BrowserSlot::BrowserSessions,
            "Loading browser sessions",
            "browser.sessions",
            |b| ops::browser_sessions_list(b, None),
        );
        for p in projects {
            let q = p.clone();
            cx.query(
                BrowserSlot::ProjectBrowserSessions(p),
                "Loading a project's browser sessions",
                "browser.sessions",
                move |b| ops::browser_sessions_list(b, Some(&q)),
            );
        }
    }

    /// Records one scope's page and rebuilds the combined list.
    fn part(&mut self, scope: Option<String>, page: Result<(Vec<BrowserSession>, bool), Failure>) {
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
        self.parts.insert(scope, part);
        self.combine();
    }
    fn combine(&mut self) {
        let parts = &self.parts;
        self.list = ListState {
            items: parts.values().flat_map(|p| p.items.clone()).collect(),
            loaded: !parts.is_empty() && parts.values().all(|p| p.loaded),
            error: parts.values().find_map(|p| p.error.clone()),
            more: parts.values().any(|p| p.more),
        };
    }
    /// The scope a listed browser session belongs to, for its row.
    fn scope_labels(&self, cx: &Cx) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (scope, part) in &self.parts {
            let label = match scope {
                None => "Workspace".to_owned(),
                Some(p) => cx
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
    fn close(&mut self, cx: &mut Cx, session: &BrowserSession) {
        let (id, revision) = (
            session.id.as_str().to_owned(),
            session.revision.as_str().to_owned(),
        );
        let sid = id.clone();
        cx.command(
            BrowserSlot::BrowserClose(id),
            "Closing the browser session",
            "browser.sessions",
            move |b| ops::browser_close(b, &sid, &revision),
        );
    }

    fn view(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let screen = Route::Browser.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut reload = false;
        let mut close: Option<BrowserSession> = None;
        let mut mem = cx.routes.remove(&Route::Browser).unwrap_or_default();
        let scopes = self.scope_labels(cx);
        let list = &self.list;
        let scrolls = &mut *cx.scrolls;
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
        cx.routes.insert(Route::Browser, mem);
        if let Some(s) = close {
            self.close(cx, &s);
        }
        if reload {
            self.load(cx);
        }
    }
}

#[derive(Default)]
pub struct ResourcesScreen {
    pub list: ListState<ResourceRecord>,
}

impl Screen for ResourcesScreen {
    type Slot = ResourcesSlot;
    const ID: ScreenId = ScreenId::Resources;
    fn load(&mut self, cx: &mut Cx, _route: Route) {
        self.load(cx);
    }
    fn refresh(&mut self, cx: &mut Cx, _route: Route) {
        if !cx.pending(&ResourcesSlot::ResourcesList) {
            self.load(cx);
        }
    }
    fn apply(&mut self, cx: &mut Cx, slot: ResourcesSlot, reply: Reply) {
        match (slot, reply) {
            (ResourcesSlot::ResourcesList, Reply::Query(q)) => match ops::resources(q) {
                Ok((items, more)) => {
                    self.list = ListState {
                        items,
                        loaded: true,
                        error: None,
                        more,
                    };
                }
                Err(e) => self.list.error = Some(e.describe()),
            },
            (slot, reply) => cx.unhandled(&slot, reply),
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        self.view(cx, ui);
    }
    fn remove_purged(&mut self, _scope: &ProjectPurgeClientScope) {
        self.list = Default::default();
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        route_context(mem)
    }
}

impl ResourcesScreen {
    fn load(&mut self, cx: &mut Cx) {
        cx.query(
            ResourcesSlot::ResourcesList,
            "Loading resources",
            "resources.list",
            ops::resources_list,
        );
    }

    fn view(&mut self, cx: &mut Cx, ui: &mut Ui) {
        let screen = Route::Resources.screen();
        let online = cx.online().map_err(str::to_owned);
        let mut reload = false;
        let mut link = false;
        let mut mem = cx.routes.remove(&Route::Resources).unwrap_or_default();
        let list = &self.list;
        let scrolls = &mut *cx.scrolls;
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
        cx.routes.insert(Route::Resources, mem);
        if link && let Some((_, target)) = screen.link {
            cx.navigate(target);
        }
        if reload {
            self.load(cx);
        }
    }
}
