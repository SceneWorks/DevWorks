//! Native repository navigation retains the owner's pin through every drill-down.
use super::Shell;
use crate::{
    analysis_screen::{self, Action as AnalysisAction, Query as AnalysisQuery},
    backend::{Reply, Slot, TaskOutput, Unresolved},
    graph_ops::{self, GraphSlot},
    graph_screen::{self, Action, GraphScreen, Read},
    memory::RouteMemory,
    ops::{self, Failure},
    route::Route,
    screen::{Cx, Screen, ScreenContext, ScreenId, controller},
    widgets::Tone,
};
use devworks_core::ports::storage::ProjectPurgeClientScope;
use devworks_core::protocol::{
    CommandReply,
    data::{GraphResult, ViewSelection},
    results::DomainDecision,
};
use eframe::egui::Ui;

impl Shell {
    pub fn open_fleet_analysis(&mut self) {
        self.route = Route::Repositories;
        self.with_typed(|graph: &mut GraphScreen, cx| {
            let project = cx.selected_project.clone();
            let rows = if project.is_some() {
                &cx.projects.members
            } else {
                &cx.projects.repositories
            };
            let choices: std::collections::BTreeMap<_, _> = rows
                .items
                .iter()
                .map(|repo| (repo.id.as_str().to_owned(), repo.name.0.clone()))
                .collect();
            let generation = graph.analysis.generation.wrapping_add(1);
            *graph = GraphScreen {
                opened: true,
                analysis_fleet: true,
                name: "Fleet analysis".into(),
                project,
                sequence: graph.sequence.wrapping_add(1),
                published_repositories: choices.keys().cloned().collect(),
                repository_choices: choices,
                analysis: analysis_screen::AnalysisScreen {
                    generation,
                    ..Default::default()
                },
                ..Default::default()
            };
            GraphCtl { graph, cx }.read_analysis(AnalysisQuery::Fleet, None);
        });
    }

    pub fn open_repository_graph(&mut self, repository: &str, name: &str) {
        self.route = Route::Repositories;
        self.with_typed(|s: &mut super::indexing::IndexingScreen, cx| {
            s.act(
                cx,
                vec![super::indexing::IndexAction::Status(repository.into())],
            )
        });
        self.with_typed(|graph: &mut GraphScreen, cx| {
            GraphCtl { graph, cx }.open(repository, name)
        });
    }
}

impl Shell {
    /// Opens a repository's graph on the detail of one of its nodes (a use another screen
    /// found). The repository's name is not known there, so its ID names it.
    pub fn open_graph_node(&mut self, repository: &str, node: &str) {
        self.open_repository_graph(repository, repository);
        let node = node.to_owned();
        self.with_typed(|graph: &mut GraphScreen, cx| {
            GraphCtl { graph, cx }.open_graph_read(Read::node("Node details", &node), true)
        });
    }
}

controller!(GraphCtl, graph: GraphScreen);

/// The repository graph is drawn inside Repositories while it is open.
impl Screen for GraphScreen {
    type Slot = GraphSlot;
    const ID: ScreenId = ScreenId::Graph;
    fn load(&mut self, _cx: &mut Cx, _route: Route) {}
    fn refresh(&mut self, _cx: &mut Cx, _route: Route) {}
    fn apply(&mut self, cx: &mut Cx, slot: GraphSlot, reply: Reply) {
        if let Some((slot, reply)) = (GraphCtl { graph: self, cx }).apply_graph(slot, reply) {
            cx.unhandled(&slot, reply);
        }
    }
    fn ui(&mut self, cx: &mut Cx, _route: Route, ui: &mut Ui) {
        GraphCtl { graph: self, cx }.graph_screen(ui);
    }
    fn hosted(&self, route: Route, _mem: &RouteMemory) -> bool {
        route == Route::Repositories && self.opened
    }
    fn help_topic(&self, _route: Route, _mem: &RouteMemory) -> Option<&'static str> {
        Some("code-exploration")
    }
    fn restore_in_flight(&mut self, slot: &GraphSlot, _u: &Unresolved) {
        if matches!(slot, GraphSlot::GraphTrust(_)) {
            self.trust_busy = true;
            self.trust_unknown = true;
        }
    }
    fn remove_purged(&mut self, scope: &ProjectPurgeClientScope) {
        if self.project.as_deref() == Some(scope.project_id.0.as_str()) {
            *self = Default::default();
        }
    }
    fn context(&self, _route: Route, mem: &RouteMemory) -> ScreenContext {
        ScreenContext {
            tab: mem.tab,
            selection: self
                .selected
                .as_ref()
                .map(|v| v.node_id.as_str().to_owned()),
        }
    }
}

impl GraphCtl<'_, '_> {
    fn open(&mut self, repository: &str, name: &str) {
        let sequence = self.graph.sequence.wrapping_add(1);
        self.graph.analysis.invalidate();
        let analysis_generation = self.graph.analysis.generation;
        *self.graph = graph_screen::GraphScreen {
            opened: true,
            repository: repository.into(),
            name: name.into(),
            repository_choices: std::collections::BTreeMap::from([(
                repository.into(),
                name.into(),
            )]),
            published_repositories: std::collections::BTreeSet::from([repository.into()]),
            project: self.selected_project.clone(),
            sequence,
            analysis: analysis_screen::AnalysisScreen {
                generation: analysis_generation,
                ..Default::default()
            },
            depth: 3,
            relationship_depth: 3,
            line_start: "1".into(),
            line_end: "200".into(),
            ..Default::default()
        };
        self.open_graph_read(Read::overview(repository), false);
    }
    fn open_graph_read(&mut self, read: Read, remember: bool) {
        if remember && let Some(mut old) = self.graph.read.take() {
            // Back navigation re-reads its cursor under the retained pin; do not retain every
            // potentially large source or property page in the breadcrumb stack.
            old.page = None;
            old.module_pages.clear();
            self.graph.history.push(graph_screen::Breadcrumb {
                read: old,
                repository: self.graph.repository.clone(),
                name: self.graph.name.clone(),
                selected: self.graph.selected.clone(),
                pin: self.graph.pin.clone(),
                tab: self.graph.tab,
            });
        }
        if read.view_override.is_some() {
            self.graph.pin = None;
        }
        self.graph.tab = match read.operation {
            "code.overview" | "code.architecture" => 0,
            "repositories.nodes" | "code.search" | "code.node_detail" | "code.source"
            | "code.snippet" => 1,
            _ => 2,
        };
        self.graph.read = Some(read);
        self.read_graph();
    }
    fn read_graph(&mut self) {
        let Some(read) = self.graph.read.clone() else {
            return;
        };
        self.graph.sequence = self.graph.sequence.wrapping_add(1);
        let generation = self.graph.sequence;
        let scope = self.graph.scope();
        let view = read
            .view_override
            .clone()
            .map(|view| Some(Some(view)))
            .unwrap_or_else(|| self.graph.view());
        let cursor = read.cursors.last().cloned().flatten();
        self.graph.loading = true;
        self.graph.error = None;
        if !self.query(
            GraphSlot::GraphRead(generation),
            "Reading repository graph",
            read.operation,
            move |b| {
                graph_ops::read(
                    b,
                    read.operation,
                    scope,
                    view,
                    read.query,
                    cursor.as_deref(),
                )
            },
        ) {
            self.graph.loading = false;
            self.graph.error = Some(
                "The repository graph cannot be read while DevWorks is offline or closing.".into(),
            );
        }
    }
    fn apply_graph(&mut self, slot: GraphSlot, reply: Reply) -> Option<(GraphSlot, Reply)> {
        match (slot, reply) {
            (GraphSlot::AnalysisRead(generation), Reply::Query(outcome)) => {
                let state = &mut self.graph.analysis;
                if self.graph.opened && state.generation == generation {
                    state.loading = false;
                    match ops::query_result(outcome) {
                        Ok((result, revision)) => {
                            state.result = Some(result);
                            state.revision = Some(revision);
                        }
                        Err(error) => state.error = Some(error.describe()),
                    }
                }
            }
            (GraphSlot::AnalysisCommand(key), Reply::Command(outcome)) => {
                if self.graph.opened && key == self.analysis_key() {
                    let notice = match &outcome {
                        Ok(CommandReply::Recorded { receipt, .. }) => match &receipt.decision {
                            DomainDecision::Accepted { operation_id } => {
                                format!("Analysis queued as {}", operation_id.0.as_str())
                            }
                            DomainDecision::NoChange { .. } => {
                                "No saved requests needed attention.".into()
                            }
                            _ => "Analysis request recorded.".into(),
                        },
                        _ => ops::command_result(outcome).err().map_or_else(
                            || "Analysis request recorded.".into(),
                            |error| error.describe(),
                        ),
                    };
                    self.read_analysis(AnalysisQuery::Status, None);
                    self.graph.analysis.notice = Some(notice);
                }
            }
            (GraphSlot::RepositoryDocumentation(generation), Reply::Task(result)) => {
                let state = &mut self.graph.analysis;
                if self.graph.opened && state.generation == generation {
                    state.loading = false;
                    match result {
                        Ok(TaskOutput::RepositoryDocumentation(notice)) => {
                            state.notice = Some(notice)
                        }
                        Err(error) => state.error = Some(error),
                        _ => return None,
                    }
                }
            }
            (GraphSlot::AnalysisEvidence(generation), Reply::Task(result)) => {
                let state = &mut self.graph.analysis;
                if self.graph.opened && state.generation == generation {
                    match result {
                        Ok(TaskOutput::AnalysisEvidence(text)) => state.evidence = Some(text),
                        Err(error) => state.error = Some(error),
                        _ => return None,
                    }
                }
            }
            (GraphSlot::GraphRead(generation), _) if generation != self.graph.sequence => {}
            (GraphSlot::GraphRead(_), Reply::Query(outcome)) => {
                self.graph.loading = false;
                match graph_ops::page(outcome) {
                    Ok(page) => {
                        // An operation cannot silently replace the captured set under a reader.
                        if let (Some(old), Some(new)) = (&self.graph.pin, &page.view)
                            && old.pin_id != new.pin_id
                        {
                            self.graph.error = Some(
                                "The graph view changed. Refresh explicitly to read a new capture."
                                    .into(),
                            );
                            return None;
                        }
                        if self.graph.pin.is_none() {
                            self.graph.pin = page.view.clone();
                        }
                        if let Some(view) = &page.view {
                            for repository in &view.views {
                                let id = repository.snapshot.repository_id.as_str().to_owned();
                                self.graph
                                    .repository_choices
                                    .entry(id.clone())
                                    .or_insert(id);
                            }
                        }
                        if let Some(scope) = &page.scope_coverage {
                            for gap in &scope.unavailable_repositories.0 {
                                let id = gap.repository_id.as_str().to_owned();
                                self.graph
                                    .repository_choices
                                    .entry(id.clone())
                                    .or_insert(format!("{id} · {}", gap.reason.0));
                            }
                        }
                        if let GraphResult::RepositoryOverview { value } = &page.result {
                            for repository in &value.repositories.0 {
                                self.graph.repository_choices.insert(
                                    repository.repository_id.as_str().into(),
                                    repository.name.0.clone(),
                                );
                            }
                        }
                        if let GraphResult::NodeDetail { value } = &page.result {
                            self.graph.selected = Some(value.node.clone());
                            self.graph.repository = value.node.repository_id.as_str().into();
                            self.graph.name = self
                                .projects
                                .repositories
                                .items
                                .iter()
                                .find(|repository| repository.id == value.node.repository_id)
                                .map_or_else(
                                    || self.graph.repository.clone(),
                                    |repository| repository.name.0.clone(),
                                );
                        }
                        if let Some(read) = &mut self.graph.read {
                            if matches!(
                                page.result,
                                GraphResult::Architecture { .. }
                                    | GraphResult::RepositoryOverview { .. }
                            ) {
                                if read.module_pages.iter().any(|(_, old)| {
                                    old.view.as_ref().map(|view| &view.pin_id)
                                        != page.view.as_ref().map(|view| &view.pin_id)
                                }) {
                                    self.graph.error=Some("The captured module view changed while paging. Refresh before reading more entries.".into());
                                    return None;
                                }
                                for item in graph_screen::module_items(&page) {
                                    if read.module_pages.iter().any(|(_, old)| {
                                        graph_screen::module_items(old).iter().any(|previous| {
                                            serde_json::to_value(&previous.module).ok()
                                                == serde_json::to_value(&item.module).ok()
                                                && graph_screen::module_scalar(previous)
                                                    != graph_screen::module_scalar(item)
                                        })
                                    }) {
                                        self.graph.error=Some("Module analysis changed while paging. Refresh the selected graph view \
                                            before combining more entries.".into());
                                        return None;
                                    }
                                }
                                let cursor = read.cursors.last().cloned().flatten();
                                if let Some((_, stored)) =
                                    read.module_pages.iter_mut().find(|(key, _)| *key == cursor)
                                {
                                    *stored = page.clone();
                                } else {
                                    read.module_pages.push((cursor, page.clone()));
                                }
                            }
                            if page.view.is_some() {
                                read.view_override = None;
                            }
                            read.page = Some(page);
                        }
                    }
                    Err(error) => self.graph.error = Some(error.describe()),
                }
            }
            (GraphSlot::GraphTrust(id), Reply::Command(outcome)) => {
                self.graph.trust_busy = false;
                match &outcome {
                    Ok(devworks_core::protocol::CommandReply::ReceiptPurged {
                        outcome, ..
                    }) => {
                        self.graph.trust_reference = Some(outcome.command_id.0.as_str().into());
                    }
                    Ok(devworks_core::protocol::CommandReply::Recorded { receipt, .. }) => {
                        self.graph.trust_reference = Some(receipt.command_id.0.as_str().into())
                    }
                    Ok(
                        devworks_core::protocol::CommandReply::Rejected { request_key, .. }
                        | devworks_core::protocol::CommandReply::CommandStatusUnknown {
                            request_key,
                            ..
                        },
                    ) => self.graph.trust_reference = Some(request_key.0.as_str().into()),
                    Err(_) => {}
                }
                match ops::command_result(outcome) {
                    Ok(_) => {
                        self.graph.trust_unknown = false;
                        self.graph.notice =
                            Some((Tone::Good, format!("Trust change confirmed for {id}.")));
                        if self.graph.opened
                            && self
                                .graph
                                .selected
                                .as_ref()
                                .is_some_and(|node| node.node_id.as_str() == id)
                        {
                            self.open_graph_read(Read::node("Node details", &id), false);
                        }
                    }
                    Err(Failure::Unknown) => {
                        self.graph.trust_unknown = true;
                        self.graph.trust_busy = true;
                        self.graph.notice=Some((Tone::Attention,"Trust change outcome is not confirmed. Confirm outcomes checks the same \
                            original request.".into()));
                    }
                    Err(error) => {
                        self.graph.trust_unknown = false;
                        self.graph.notice = Some((Tone::Attention, error.describe()));
                    }
                }
            }
            (slot, reply) => return Some((slot, reply)),
        }
        None
    }
    fn graph_screen(&mut self, ui: &mut Ui) {
        if let Some(super::indexing::StatusState::Ready(status)) = self
            .projects
            .indexing
            .and_then(|indexing| indexing.status.get(&self.graph.repository))
        {
            let mut choices: Vec<_> = status
                .current_view_ids
                .0
                .iter()
                .map(|id| {
                    (
                        id.as_str().into(),
                        format!("Indexed view · {}", id.as_str()),
                    )
                })
                .collect();
            if let Some(overlays) = &status.overlays {
                for overlay in &overlays.0 {
                    if let Some(view) = &overlay.view_id {
                        choices.retain(|(id, _)| id != view.as_str());
                        choices.push((
                            view.as_str().into(),
                            format!(
                                "{} · {:?} · {}",
                                overlay
                                    .head_ref
                                    .as_ref()
                                    .map_or("Worktree", |v| v.0.as_str()),
                                overlay.state,
                                view.as_str()
                            ),
                        ));
                    }
                }
            }
            self.graph.exact_choices = choices;
        }
        if let Some(pin) = &self.graph.pin {
            for view in &pin.views {
                let id = view.view_id.as_str().to_owned();
                if !self
                    .graph
                    .exact_choices
                    .iter()
                    .any(|(choice, _)| choice == &id)
                {
                    self.graph.exact_choices.push((
                        id,
                        format!(
                            "{} · captured head {}",
                            view.snapshot.repository_id.as_str(),
                            view.snapshot.head.hex
                        ),
                    ));
                }
            }
        }
        self.graph.trust_busy = self.client.as_ref().is_some_and(|client| {
            client
                .unresolved()
                .iter()
                .any(|pending| matches!(pending.slot, Slot::Graph(GraphSlot::GraphTrust(_))))
        }) || self.client.as_ref().is_some_and(|client| {
            client
                .activities()
                .iter()
                .any(|activity| activity.label == "Changing node trust")
        });
        let mut actions = Vec::new();
        let online = self.online().is_ok();
        self.graph.ui(ui, online, &mut actions);
        for action in actions {
            match action {
                Action::Close | Action::Registration => {
                    self.graph.opened = false;
                }
                Action::Indexing => self.navigate(Route::Indexing),
                Action::Analysis(action) => self.analysis_action(action),
                Action::Repository(repository) => {
                    self.open_graph_read(Read::overview(&repository), true);
                    self.graph.repository = repository;
                    self.graph.name = self
                        .projects
                        .repositories
                        .items
                        .iter()
                        .find(|repo| repo.id.as_str() == self.graph.repository)
                        .map_or_else(|| self.graph.repository.clone(), |repo| repo.name.0.clone());
                    self.graph.selected = None;
                    self.graph.tab = 0;
                }
                Action::Read(read) => self.open_graph_read(read, true),
                Action::Filter {
                    repository,
                    module,
                    kind,
                } => {
                    self.open_graph_read(
                        Read::nodes(&repository, module.as_deref(), kind.as_deref()),
                        true,
                    );
                    self.graph.repository = repository;
                    self.graph.node_kind = kind.unwrap_or_default();
                    self.graph.module = module.unwrap_or_default();
                    self.graph.selected = None;
                    self.graph.tab = 1;
                }
                Action::Node(node) => {
                    let id = node.node_id.clone();
                    self.open_graph_read(Read::node("Node details", id.as_str()), true);
                    self.graph.selected = Some(node);
                    self.graph.target_name.clear();
                }
                Action::FindTable(table) => self
                    .effects
                    .push(crate::screen::Effect::FindDatabaseTable(table)),
                Action::Endpoint { id, repository } => {
                    self.open_graph_read(Read::node("Related node", &id), true);
                    self.graph.selected = None;
                    self.graph.repository = repository;
                    self.graph.target_name.clear();
                }
                Action::Back => {
                    if let Some(crumb) = self.graph.history.pop() {
                        self.graph.repository = crumb.repository;
                        self.graph.name = crumb.name;
                        self.graph.selected = crumb.selected;
                        self.graph.pin = crumb.pin;
                        self.open_graph_read(crumb.read, false);
                        self.graph.tab = crumb.tab;
                    }
                }
                Action::RefreshView => {
                    self.graph.analysis.invalidate();
                    self.graph.pin = None;
                    self.graph.history.clear();
                    self.graph.selected = None;
                    self.open_graph_read(Read::overview(&self.graph.repository), false);
                }
                Action::Page(next) => {
                    if let Some(read) = &mut self.graph.read {
                        if next {
                            if let Some(cursor) =
                                read.page.as_ref().and_then(|page| page.next_cursor.clone())
                            {
                                read.cursors.push(Some(cursor));
                            } else {
                                continue;
                            }
                        } else if read.cursors.len() > 1 {
                            read.cursors.pop();
                        } else {
                            continue;
                        }
                    }
                    self.read_graph();
                }
                Action::Trust(node, trusted) => {
                    let id = node.node_id.as_str().to_owned();
                    let scope = self.graph.scope();
                    let pin = self.graph.pin.as_ref().map(|pin| pin.pin_id.clone());
                    if self.command(
                        GraphSlot::GraphTrust(id),
                        "Changing node trust",
                        "code.set_trust",
                        move |b| graph_ops::trust(b, &node, scope, pin, trusted),
                    ) {
                        self.graph.trust_busy = true;
                    }
                }
            }
        }
    }
}

impl GraphCtl<'_, '_> {
    fn analysis_view(&self) -> Option<ViewSelection> {
        let repository_ids = self
            .graph
            .published_repositories
            .iter()
            .map(|id| crate::frame::id(id))
            .collect::<Option<Vec<_>>>()?;
        if self.graph.analysis_fleet {
            return Some(ViewSelection::Published { repository_ids });
        }
        self.graph
            .view()?
            .or(Some(ViewSelection::Published { repository_ids }))
    }

    fn measurement_view(&self, view: devworks_core::protocol::Id) -> Option<ViewSelection> {
        self.graph
            .pin
            .as_ref()
            .is_none_or(|pin| !pin.views.iter().any(|captured| captured.view_id == view))
            .then_some(ViewSelection::Exact {
                view_ids: vec![view],
            })
    }

    fn analysis_key(&self) -> String {
        format!(
            "{}/{}",
            self.graph.project.as_deref().unwrap_or("workspace"),
            self.graph.repository
        )
    }

    fn read_analysis(&mut self, query: AnalysisQuery, cursor: Option<String>) {
        let scope = self.graph.scope();
        let views = self.analysis_view();
        let repository = query
            .repository_filter()
            .then(|| self.graph.repository.clone());
        if !self.graph.analysis_fleet {
            self.graph.tab = 3;
        }
        let operation = query.operation();
        self.graph.analysis.invalidate();
        let generation = self.graph.analysis.generation;
        self.graph.analysis.query = Some(query.clone());
        self.graph.analysis.loading = self.query(
            GraphSlot::AnalysisRead(generation),
            "Reading analysis report",
            operation,
            move |backend| {
                query.frame(
                    backend,
                    scope,
                    views,
                    repository.as_deref(),
                    cursor.as_deref(),
                )
            },
        );
        if !self.graph.analysis.loading {
            self.graph.analysis.error = Some("DevWorks is offline or closing.".into());
        }
    }

    fn analysis_action(&mut self, action: AnalysisAction) {
        match action {
            AnalysisAction::Read(query, cursor) => self.read_analysis(query, cursor),
            AnalysisAction::Command(command) => {
                let Some(revision) = self.graph.analysis.revision else {
                    return;
                };
                let scope = self.graph.scope();
                let views = if matches!(
                    command,
                    analysis_screen::Command::AnalyzeProjects
                        | analysis_screen::Command::Synthesize
                ) {
                    self.graph
                        .pin
                        .as_ref()
                        .and_then(|pin| {
                            pin.views.iter().find(|view| {
                                view.snapshot.repository_id.as_str() == self.graph.repository
                            })
                        })
                        .map_or_else(
                            || {
                                crate::frame::id(&self.graph.repository).map(|repository| {
                                    ViewSelection::Published {
                                        repository_ids: vec![repository],
                                    }
                                })
                            },
                            |view| {
                                Some(ViewSelection::Exact {
                                    view_ids: vec![view.view_id.clone()],
                                })
                            },
                        )
                } else {
                    self.analysis_view()
                };
                if self.command(
                    GraphSlot::AnalysisCommand(self.analysis_key()),
                    "Requesting analysis",
                    command.operation(),
                    move |backend| command.frame(backend, scope, views, revision),
                ) {
                    self.graph.analysis.loading = true;
                    self.graph.analysis.notice = Some("Submitting analysis request…".into());
                }
            }
            AnalysisAction::OpenFile {
                repository,
                view,
                path,
            } => {
                let source = graph_screen::file_source(repository.clone(), path, 0, 0);
                let mut read = Read::code("code.source", "Captured source", source);
                read.view_override = self.measurement_view(view);
                self.open_graph_read(read, true);
                self.graph.repository = repository.as_str().into();
                self.graph.selected = None;
            }
            AnalysisAction::OpenRepository(repository) => {
                let id = repository.as_str();
                let name = self
                    .graph
                    .repository_choices
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| id.to_owned());
                if self.graph.analysis_fleet {
                    self.open(id, &name);
                } else {
                    self.open_graph_read(Read::overview(id), true);
                    self.graph.repository = id.into();
                    self.graph.name = name;
                    self.graph.selected = None;
                }
            }
            AnalysisAction::OpenEndpoint {
                repository,
                view,
                node,
            } => {
                let mut read = Read::node("Related node", node.as_str());
                read.view_override = self.measurement_view(view);
                self.open_graph_read(read, true);
                self.graph.repository = repository.as_str().into();
                self.graph.selected = None;
            }
            AnalysisAction::OpenWork(id) => self.open_work(id.as_str()),
            AnalysisAction::GenerateDocumentation {
                run,
                binding,
                revision,
                delivery,
            } => {
                let Some(project) = self.graph.project.as_deref().and_then(crate::frame::id) else {
                    self.graph.analysis.error = Some(
                        "Open the repository from its project to generate its documentation."
                            .into(),
                    );
                    return;
                };
                let generation = self.graph.analysis.generation;
                if self.online().is_ok()
                    && let Some(client) = self.client.as_mut()
                {
                    let project = devworks_core::protocol::ProjectId(project);
                    client.task(
                        GraphSlot::RepositoryDocumentation(generation).into(),
                        "Generating repository documentation",
                        move |backend| {
                            backend
                                .generate_repository_documentation(
                                    &run, &project, &binding, &revision, delivery,
                                )
                                .map(TaskOutput::RepositoryDocumentation)
                        },
                    );
                    self.graph.analysis.loading = true;
                }
            }
            AnalysisAction::Evidence(report) => {
                let generation = self.graph.analysis.generation;
                if self.online().is_ok()
                    && let Some(client) = self.client.as_mut()
                {
                    client.task(
                        GraphSlot::AnalysisEvidence(generation).into(),
                        "Reading report evidence",
                        move |backend| {
                            let bytes = ops::read_artifact_bytes(backend, &report)?;
                            let mut text = String::from_utf8(bytes)
                                .map_err(|_| "The report is not UTF-8.".to_owned())?;
                            const DISPLAY_BYTES: usize = 65_536;
                            if text.len() > DISPLAY_BYTES {
                                text.truncate(text.floor_char_boundary(DISPLAY_BYTES));
                                text.push_str("\nReport preview truncated.");
                            }
                            Ok(TaskOutput::AnalysisEvidence(text))
                        },
                    );
                }
            }
        }
    }
}
