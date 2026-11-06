//! Native repository navigation retains the owner's pin through every drill-down.
use super::Shell;
use crate::{
    backend::{Reply, Slot},
    graph_ops,
    graph_screen::{self, Action, Read},
    ops::{self, Failure},
    route::Route,
    widgets::Tone,
};
use devworks_core::protocol::data::GraphResult;
use eframe::egui::Ui;
use serde_json::json;

impl Shell {
    pub fn open_repository_graph(&mut self, repository: &str, name: &str) {
        let sequence = self.graph.sequence.wrapping_add(1);
        self.graph = graph_screen::GraphScreen {
            opened: true,
            repository: repository.into(),
            name: name.into(),
            project: self.selected_project.clone(),
            sequence,
            depth: 3,
            line_start: "1".into(),
            line_end: "200".into(),
            ..Default::default()
        };
        self.route = Route::Repositories;
        self.index_act(vec![super::indexing::IndexAction::Status(
            repository.into(),
        )]);
        self.open_graph_read(
            Read::new(
                "code.architecture",
                "Overview",
                json!({"kind":"Overview","repositoryId":repository}),
            ),
            false,
        );
    }
    fn open_graph_read(&mut self, read: Read, remember: bool) {
        if remember && let Some(old) = self.graph.read.take() {
            self.graph.history.push(graph_screen::Breadcrumb {
                read: old,
                repository: self.graph.repository.clone(),
                name: self.graph.name.clone(),
                selected: self.graph.selected.clone(),
                tab: self.graph.tab,
            });
        }
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
        let view = self.graph.view();
        let cursor = read.cursors.last().cloned().flatten();
        self.graph.loading = true;
        self.graph.error = None;
        if !self.query(
            Slot::GraphRead(generation),
            "Reading repository graph",
            read.operation,
            move |b| {
                if read.operation == "repositories.nodes" {
                    graph_ops::repository_nodes(b, scope, read.query, view, cursor.as_deref())
                } else {
                    graph_ops::read(b, scope, view, read.query, cursor.as_deref())
                }
            },
        ) {
            self.graph.loading = false;
            self.graph.error = Some(
                "The repository graph cannot be read while DevWorks is offline or closing.".into(),
            );
        }
    }
    pub(super) fn apply_graph(&mut self, slot: Slot, reply: Reply) -> Option<(Slot, Reply)> {
        match (slot, reply) {
            (Slot::GraphRead(generation), _) if generation != self.graph.sequence => {}
            (Slot::GraphRead(_), Reply::Query(outcome)) => {
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
                            if let Some(source) = &value.node.source {
                                self.graph.file = source.path.0.clone();
                            }
                        }
                        if let Some(read) = &mut self.graph.read {
                            read.page = Some(page);
                        }
                    }
                    Err(error) => self.graph.error = Some(error.describe()),
                }
            }
            (Slot::GraphTrust(id), Reply::Command(outcome)) => {
                self.graph.trust_busy = false;
                match &outcome {
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
                        if self
                            .graph
                            .selected
                            .as_ref()
                            .is_some_and(|node| node.node_id.as_str() == id)
                        {
                            self.open_graph_read(
                                Read::new(
                                    "code.node_detail",
                                    "Node details",
                                    json!({"kind":"Node","nodeId":id}),
                                ),
                                false,
                            );
                        }
                    }
                    Err(Failure::Unknown) => {
                        self.graph.trust_unknown = true;
                        self.graph.trust_busy = true;
                        self.graph.notice=Some((Tone::Attention,"Trust change outcome is not confirmed. Confirm outcomes checks the same original request.".into()));
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
    pub(super) fn graph_screen(&mut self, ui: &mut Ui) {
        if let Some(super::indexing::StatusState::Ready(status)) =
            self.projects.indexing.status.get(&self.graph.repository)
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
        self.graph.trust_busy = self.client.as_ref().is_some_and(|client| {
            client
                .unresolved()
                .iter()
                .any(|pending| matches!(pending.slot, Slot::GraphTrust(_)))
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
                Action::Repository(repository) => {
                    self.open_graph_read(
                        Read::new(
                            "code.architecture",
                            "Overview",
                            json!({"kind":"Overview","repositoryId":repository}),
                        ),
                        true,
                    );
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
                    let mut query = json!({"repositoryId":repository});
                    if let Some(module) = &module {
                        query["internalModule"] = json!(module);
                    }
                    if let Some(kind) = &kind {
                        query["nodeKind"] = json!(kind);
                    }
                    self.open_graph_read(Read::new("repositories.nodes", "Nodes", query), true);
                    self.graph.repository = repository;
                    self.graph.node_kind = kind.unwrap_or_default();
                    self.graph.module = module.unwrap_or_default();
                    self.graph.selected = None;
                    self.graph.tab = 1;
                }
                Action::Node(node) => {
                    let id = node.node_id.clone();
                    self.open_graph_read(
                        Read::new(
                            "code.node_detail",
                            "Node details",
                            json!({"kind":"Node","nodeId":id}),
                        ),
                        true,
                    );
                    self.graph.selected = Some(node);
                    self.graph.target_name.clear();
                }
                Action::Endpoint { id, repository } => {
                    self.open_graph_read(
                        Read::new(
                            "code.node_detail",
                            "Related node",
                            json!({"kind":"Node","nodeId":id}),
                        ),
                        true,
                    );
                    self.graph.selected = None;
                    self.graph.repository = repository;
                    self.graph.target_name.clear();
                }
                Action::Back => {
                    if let Some(crumb) = self.graph.history.pop() {
                        self.graph.repository = crumb.repository;
                        self.graph.name = crumb.name;
                        self.graph.selected = crumb.selected;
                        self.graph.tab = crumb.tab;
                        self.open_graph_read(crumb.read, false);
                    }
                }
                Action::RefreshView => {
                    self.graph.pin = None;
                    self.graph.history.clear();
                    self.graph.selected = None;
                    self.open_graph_read(
                        Read::new(
                            "code.architecture",
                            "Overview",
                            json!({"kind":"Overview","repositoryId":self.graph.repository}),
                        ),
                        false,
                    );
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
                    let pin = self
                        .graph
                        .pin
                        .as_ref()
                        .map(|pin| pin.pin_id.as_str().to_owned());
                    if self.command(
                        Slot::GraphTrust(id),
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
