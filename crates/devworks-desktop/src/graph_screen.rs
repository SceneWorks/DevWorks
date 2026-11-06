//! Repository graph presentation. Scope, source, ranking and eligibility come from the owner.
use crate::{
    theme,
    widgets::{self, Tone},
};
use devworks_core::protocol::{
    data::{GraphPage, GraphResult, ResolvedViewSet},
    records::*,
};
use eframe::egui::{self, RichText, Ui};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct Read {
    pub operation: &'static str,
    pub title: String,
    pub query: Value,
    pub cursors: Vec<Option<String>>,
    pub page: Option<GraphPage>,
}
#[derive(Clone)]
pub struct Breadcrumb {
    pub read: Read,
    pub repository: String,
    pub name: String,
    pub selected: Option<CodeNode>,
    pub tab: usize,
}
impl Read {
    pub fn new(operation: &'static str, title: impl Into<String>, query: Value) -> Self {
        Self {
            operation,
            title: title.into(),
            query,
            cursors: vec![None],
            page: None,
        }
    }
}
#[derive(Default)]
pub struct GraphScreen {
    pub opened: bool,
    pub repository: String,
    pub name: String,
    pub project: Option<String>,
    pub pin: Option<ResolvedViewSet>,
    pub view_mode: usize,
    pub exact_view: String,
    pub exact_choices: Vec<(String, String)>,
    pub read: Option<Read>,
    pub history: Vec<Breadcrumb>,
    pub selected: Option<CodeNode>,
    pub sequence: u64,
    pub loading: bool,
    pub error: Option<String>,
    pub notice: Option<(Tone, String)>,
    pub trust_busy: bool,
    pub trust_unknown: bool,
    pub trust_reference: Option<String>,
    pub filter: String,
    pub node_kind: String,
    pub module: String,
    pub file: String,
    pub line_start: String,
    pub line_end: String,
    pub target_name: String,
    pub depth: u16,
    pub direction: usize,
    pub edge_kind: String,
    pub tab: usize,
}

#[derive(Clone)]
pub enum Action {
    Close,
    Back,
    RefreshView,
    Read(Read),
    Node(CodeNode),
    Endpoint { id: String, repository: String },
    Repository(String),
    Page(bool),
    Trust(CodeNode, bool),
    Registration,
    Indexing,
}

fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(label);
    ui.add(
        egui::TextEdit::singleline(value)
            .id_salt(label)
            .hint_text(hint)
            .desired_width(220.0),
    )
    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, label));
}
fn selectable(ui: &mut Ui, text: impl AsRef<str>) {
    ui.add(egui::Label::new(text.as_ref()).selectable(true).wrap());
}
fn optional(ui: &mut Ui, label: &str, text: Option<&str>) {
    if let Some(text) = text {
        widgets::caption(ui, format!("{label}: {text}"));
    }
}

impl GraphScreen {
    pub fn view(&self) -> Option<Value> {
        if let Some(pin) = &self.pin {
            return Some(json!({"kind":"Pinned","pinId":pin.pin_id}));
        }
        match self.view_mode {
            1 => Some(json!({"kind":"Published","repositoryIds":[self.repository]})),
            2 => Some(json!({"kind":"Exact","viewIds":[self.exact_view]})),
            _ => None,
        }
    }
    pub fn scope(&self) -> Value {
        self.project.as_ref().map_or(
            json!({"kind":"Workspace"}),
            |id| json!({"kind":"Project","projectId":id}),
        )
    }
    pub fn nodes(&self) -> Read {
        let mut query = json!({"repositoryId":self.repository});
        if !self.node_kind.trim().is_empty() {
            query["nodeKind"] = json!(self.node_kind.trim());
        }
        if !self.module.trim().is_empty() {
            query["internalModule"] = json!(self.module.trim());
        }
        Read::new("repositories.nodes", "Nodes", query)
    }
    fn target(&self) -> Option<Value> {
        if !self.target_name.trim().is_empty() {
            Some(
                json!({"kind":"Name","name":self.target_name.trim(),"repositoryId":self.repository}),
            )
        } else {
            self.selected
                .as_ref()
                .map(|n| json!({"kind":"Node","nodeId":n.node_id}))
        }
    }
    fn source(&self, whole: bool) -> Option<Read> {
        self.selected.as_ref().filter(|node|node.source.is_some()).map(|node|
            Read::new("code.source",if whole{"Whole file"}else{"Source"},json!({"kind":"Source","selection":{"kind":"Node","nodeId":node.node_id,"mode":if whole{"WholeFile"}else{"NodeWindow"}}})))
    }
    pub fn ui(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<Action>) {
        let title = self.name.clone();
        widgets::pane(
            ui,
            widgets::Header {
                title: &title,
                summary: "Explore the indexed repository and its relationships.",
                tabs: &["Overview", "Nodes", "Graph", "Health", "Reviews"],
                tab: self.tab,
            },
            &mut (self, actions),
            |ui, (state, actions)| {
                if widgets::secondary_button(ui, "Back to repositories").clicked() {
                    actions.push(Action::Close);
                }
                if !state.history.is_empty()
                    && widgets::secondary_button(ui, "Back to previous graph view").clicked()
                {
                    actions.push(Action::Back);
                }
            },
            |ui, (state, actions), tab| {
                if tab != state.tab {
                    state.tab = tab;
                    match tab {
                        0=>actions.push(Action::Read(Read::new("code.architecture","Overview",json!({"kind":"Overview","repositoryId":state.repository})))),
                        1=>actions.push(Action::Read(state.nodes())),
                        2=>actions.push(Action::Read(Read::new("code.repository_dependencies","Dependencies",json!({"kind":"RepositoryDependencies","repositoryId":state.repository})))),
                        _=>{}
                    }
                }
                state.controls(ui, online, actions);
                if tab >= 3 {
                    widgets::chip(ui, "Unavailable", Tone::Attention);
                    ui.label(if tab==3{"Health measurements are not available from the current repository services."}else{"Review history is not available from the current repository services."});
                    return;
                }
                if let Some((tone, text)) = &state.notice {
                    widgets::chip(ui, text, *tone);
                }
                if let Some(reference) = &state.trust_reference {
                    widgets::caption(ui, format!("Trust request reference: {reference}"));
                }
                if let Some(error) = &state.error {
                    widgets::chip(ui, error, Tone::Attention);
                }
                if state.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading the selected graph view…");
                    });
                }
                if let Some(read) = &state.read {
                    ui.horizontal_wrapped(|ui| {
                        ui.heading(&read.title);
                        if ui
                            .add_enabled(
                                online && !state.loading && read.cursors.len() > 1,
                                egui::Button::new("Previous graph page"),
                            )
                            .clicked()
                        {
                            actions.push(Action::Page(false));
                        }
                        if ui
                            .add_enabled(
                                online
                                    && !state.loading
                                    && read.page.as_ref().is_some_and(|p| p.next_cursor.is_some()),
                                egui::Button::new("Next graph page"),
                            )
                            .clicked()
                        {
                            actions.push(Action::Page(true));
                        }
                    });
                    if let Some(page) = &read.page {
                        coverage(ui, page);
                        egui::ScrollArea::vertical()
                            .id_salt(("repository-graph", read.operation, &read.title))
                            .show(ui, |ui| {
                                render(
                                    ui,
                                    page,
                                    &state.repository,
                                    online && !state.loading,
                                    state.trust_busy,
                                    actions,
                                );
                            });
                    } else if !state.loading && state.error.is_none() {
                        ui.label("No graph view has been read. Choose a view and refresh.");
                    }
                }
            },
        );
    }
    fn controls(&mut self, ui: &mut Ui, online: bool, actions: &mut Vec<Action>) {
        ui.horizontal_wrapped(|ui| {
            widgets::caption(
                ui,
                format!(
                    "Repository {} · {}",
                    self.repository,
                    self.project
                        .as_ref()
                        .map_or("Workspace".into(), |p| format!("Project {p}"))
                ),
            );
            if ui.small_button("Repository registration").clicked() {
                actions.push(Action::Registration);
            }
            if ui.small_button("Indexing logs").clicked() {
                actions.push(Action::Indexing);
            }
        });
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("graph-view-mode")
                .selected_text(
                    [
                        "Current scope",
                        "Published repository",
                        "Exact indexed view",
                    ][self.view_mode],
                )
                .show_ui(ui, |ui| {
                    for (i, label) in [
                        "Current scope",
                        "Published repository",
                        "Exact indexed view",
                    ]
                    .iter()
                    .enumerate()
                    {
                        ui.selectable_value(&mut self.view_mode, i, *label);
                    }
                });
            if self.view_mode == 2 {
                egui::ComboBox::from_id_salt("graph-exact-view")
                    .selected_text(if self.exact_view.is_empty() {
                        "Choose an indexed view"
                    } else {
                        &self.exact_view
                    })
                    .show_ui(ui, |ui| {
                        for (id, label) in &self.exact_choices {
                            ui.selectable_value(&mut self.exact_view, id.clone(), label);
                        }
                    });
            }
            if ui
                .add_enabled(
                    online && !self.loading && (self.view_mode != 2 || !self.exact_view.is_empty()),
                    egui::Button::new("Refresh selected graph view"),
                )
                .clicked()
            {
                actions.push(Action::RefreshView);
            }
        });
        if let Some(pin) = &self.pin {
            widgets::caption(
                ui,
                "Browsing one captured view. View choices take effect only when refreshed.",
            );
            egui::CollapsingHeader::new("Captured revisions").show(ui, |ui| {
                for view in &pin.views {
                    selectable(
                        ui,
                        format!(
                            "{} · base {} · head {} · view {}",
                            view.snapshot.repository_id.as_str(),
                            view.snapshot.base.hex,
                            view.snapshot.head.hex,
                            view.view_id.as_str()
                        ),
                    );
                    optional(
                        ui,
                        "Worktree",
                        view.snapshot.worktree_id.as_ref().map(|id| id.as_str()),
                    );
                    widgets::caption(ui, format!("Coverage: {:?}", view.coverage.state));
                }
            });
        }
        ui.horizontal_wrapped(|ui|{
            field(ui,"Find code",&mut self.filter,"Name, symbol or repository");
            field(ui,"Node kind",&mut self.node_kind,"All kinds");
            field(ui,"Internal module",&mut self.module,"All modules");
            if ui.add_enabled(online&&!self.loading,egui::Button::new("Search graph")).clicked(){
                let mut selection=json!({"mode":"RankedNodes","text":if self.filter.trim().is_empty(){"*"}else{self.filter.trim()},"repositoryId":self.repository});
                if !self.node_kind.trim().is_empty(){selection["nodeKind"]=json!(self.node_kind.trim());}
                if !self.module.trim().is_empty(){selection["internalModule"]=json!(self.module.trim());}
                actions.push(Action::Read(Read::new("code.search","Search results",json!({"kind":"Search","selection":selection}))));
            }
            if ui.add_enabled(online&&!self.loading,egui::Button::new("Browse nodes")).clicked(){actions.push(Action::Read(self.nodes()));}
        });
        ui.horizontal_wrapped(|ui| {
            for (label, op, query) in [
                (
                    "Repository overview",
                    "code.overview",
                    json!({"kind":"Overview","repositoryId":self.repository}),
                ),
                (
                    "Dependencies",
                    "code.repository_dependencies",
                    json!({"kind":"RepositoryDependencies","repositoryId":self.repository}),
                ),
                (
                    "Graph schema",
                    "code.graph_schema",
                    json!({"kind":"Schema"}),
                ),
                (
                    "Index coverage",
                    "code.graph_scope_status",
                    json!({"kind":"ScopeStatus"}),
                ),
            ] {
                if ui
                    .add_enabled(online && !self.loading, egui::Button::new(label))
                    .clicked()
                {
                    actions.push(Action::Read(Read::new(op, label, query)));
                }
            }
        });
        if let Some(node) = &self.selected {
            widgets::caption(
                ui,
                format!("Selected: {} · {}", node.qualified_name.0, node.node_kind.0),
            );
            ui.horizontal_wrapped(|ui| {
                for (label, whole) in [("Read node source", false), ("Read whole file", true)] {
                    if ui
                        .add_enabled(
                            online && !self.loading && node.source.is_some(),
                            egui::Button::new(label),
                        )
                        .on_disabled_hover_text("This node has no retained source location.")
                        .clicked()
                        && let Some(read) = self.source(whole)
                    {
                        actions.push(Action::Read(read));
                    }
                }
                if ui
                    .add_enabled(
                        online && !self.loading && node.source.is_some(),
                        egui::Button::new("Code snippet"),
                    )
                    .clicked()
                {
                    if let Some(mut read) = self.source(false) {
                        read.operation = "code.snippet";
                        read.title = "Code snippet".into();
                        actions.push(Action::Read(read));
                    }
                }
            });
        }
        egui::CollapsingHeader::new("Explore calls, impact and data flow").show(ui,|ui|{
            ui.horizontal_wrapped(|ui|{
                field(ui,"Target name",&mut self.target_name,"Use selected node when empty");
                ui.add(egui::DragValue::new(&mut self.depth).range(1..=16).prefix("Depth "));
                egui::ComboBox::from_id_salt("graph-trace-direction").selected_text(["Callers","Callees","Both directions"][self.direction]).show_ui(ui,|ui|{
                    for(i,label)in ["Callers","Callees","Both directions"].iter().enumerate(){ui.selectable_value(&mut self.direction,i,*label);}
                });
                field(ui,"Relationship kind",&mut self.edge_kind,"All relationships");
            });
            let target=self.target();
            ui.horizontal_wrapped(|ui|{
                for (label,op,query) in [
                    ("Calls","code.call_path",json!({"kind":"Path","selection":{"target":target,"direction":(["Inbound","Outbound","Both"][self.direction]),"depth":self.depth}})),
                    ("Impact","code.impact",json!({"kind":"NodeImpact","selection":{"target":target,"depth":self.depth}})),
                    ("Data lineage","code.data_lineage",json!({"kind":"DataLineage","selection":target})),
                    ("Consumers","code.consumers",json!({"kind":"Consumers","selection":target})),
                    ("Publishers","code.publishers",json!({"kind":"Publishers","selection":target})),
                ]{if ui.add_enabled(online&&!self.loading&&target.is_some(),egui::Button::new(label)).clicked(){actions.push(Action::Read(Read::new(op,label,query)));}}
                for(label,op)in [("Relationships","code.traverse"),("Node dependencies","code.dependency")] {
                    if ui.add_enabled(online&&!self.loading&&self.selected.is_some(),egui::Button::new(label)).clicked(){
                        let node=self.selected.as_ref().expect("enabled selected node");
                        let kinds:Vec<&str>=self.edge_kind.split(',').map(str::trim).filter(|s|!s.is_empty()).collect();
                        actions.push(Action::Read(Read::new(op,label,json!({"kind":"Traverse","startIds":[node.node_id],"direction":(["Reverse","Forward","Both"][self.direction]),"edgeKinds":kinds,"maxDepth":self.depth}))));
                    }
                }
            });
        });
        egui::CollapsingHeader::new("Explore a source file").show(ui,|ui|{
            ui.horizontal_wrapped(|ui|{
                field(ui,"Source file",&mut self.file,"Repository-relative path");
                field(ui,"First source line",&mut self.line_start,"1");
                field(ui,"Last source line",&mut self.line_end,"200");
                let start=self.line_start.parse::<u64>().ok();let end=self.line_end.parse::<u64>().ok();
                let valid=matches!((start,end),(Some(a),Some(b))if a>0&&b>=a);
                if !valid {widgets::caption(ui,"Enter a positive first line and a last line at or after it.");}
                let(start,end)=(start.unwrap_or(0),end.unwrap_or(0));
                for(label,op,query)in [
                    ("Read line window","code.source",json!({"kind":"Source","selection":{"kind":"File","repositoryId":self.repository,"path":self.file,"startLine":start.to_string(),"endLine":end.to_string()}})),
                    ("Nodes in file","code.nodes_by_file",json!({"kind":"NodesByFile","selection":{"repositoryId":self.repository,"path":self.file,"line":start.to_string()}})),
                    ("File impact","code.file_impact",json!({"kind":"FileImpact","selection":{"repositoryId":self.repository,"path":self.file,"depth":self.depth}})),
                ]{if ui.add_enabled(online&&!self.loading&&!self.file.trim().is_empty()&&start>0&&end>=start,egui::Button::new(label)).clicked(){actions.push(Action::Read(Read::new(op,label,query)));}}
            });
        });
        ui.separator();
    }
}

pub fn coverage(ui: &mut Ui, page: &GraphPage) {
    if !matches!(
        page.completion,
        devworks_core::protocol::data::QueryCompletion::Complete
    ) {
        widgets::chip(
            ui,
            format!("Partial result: {:?}", page.completion),
            Tone::Attention,
        );
    }
    if let Some(pin) = &page.view {
        for view in &pin.views {
            if !matches!(
                view.coverage.state,
                devworks_core::protocol::data::CoverageState::Complete
            ) {
                widgets::caption(
                    ui,
                    format!(
                        "{} coverage: {:?}; {} reported gaps{}",
                        view.snapshot.repository_id.as_str(),
                        view.coverage.state,
                        view.coverage.gaps.len(),
                        if view.coverage.more_gaps {
                            " and more"
                        } else {
                            ""
                        }
                    ),
                );
            }
        }
    } else {
        widgets::caption(ui, "No indexed snapshot is available for this result.");
    }
}

fn node_link(ui: &mut Ui, node: &CodeNode, actions: &mut Vec<Action>) {
    ui.scope_builder(
        egui::UiBuilder::new().id_salt((
            node.repository_id.as_str(),
            node.unit_key.0.as_str(),
            node.node_id.as_str(),
        )),
        |ui| {
            if ui
                .button(&node.name.0)
                .on_hover_text(&node.qualified_name.0)
                .clicked()
            {
                actions.push(Action::Node(node.clone()));
            }
            widgets::caption(
                ui,
                format!(
                    "{} · {}{}",
                    node.repository_id.as_str(),
                    node.node_kind.0,
                    if node.do_not_trust {
                        " · Untrusted"
                    } else {
                        ""
                    }
                ),
            );
            optional(
                ui,
                "Confidence",
                node.analysis_confidence
                    .as_ref()
                    .or(node.confidence.as_ref())
                    .map(|v| v.0.as_str()),
            );
        },
    );
}
fn nodes(ui: &mut Ui, nodes: &[CodeNode], actions: &mut Vec<Action>) {
    let mut kinds: BTreeMap<&str, Vec<&CodeNode>> = BTreeMap::new();
    for node in nodes {
        kinds.entry(&node.node_kind.0).or_default().push(node);
    }
    for (kind, group) in kinds {
        egui::CollapsingHeader::new(format!("{kind} · {} shown", group.len()))
            .default_open(true)
            .show(ui, |ui| {
                for node in &group {
                    if node
                        .parent_id
                        .as_ref()
                        .is_some_and(|parent| nodes.iter().any(|n| n.node_id == *parent))
                    {
                        continue;
                    }
                    let members: Vec<_> = nodes
                        .iter()
                        .filter(|member| member.parent_id.as_ref() == Some(&node.node_id))
                        .collect();
                    if members.is_empty() {
                        node_link(ui, node, actions);
                    } else {
                        egui::CollapsingHeader::new(&node.name.0)
                            .id_salt(node.node_id.as_str())
                            .default_open(false)
                            .show(ui, |ui| {
                                node_link(ui, node, actions);
                                for member in members {
                                    node_link(ui, member, actions);
                                }
                            });
                    }
                }
            });
    }
}
fn properties(ui: &mut Ui, items: &[CodeProperty], complete: bool) {
    egui::CollapsingHeader::new("Properties").show(ui,|ui|{
        for property in items {ui.strong(&property.name.0);selectable(ui,&property.json_text.0);
            if property.next_offset.is_some(){widgets::caption(ui,format!("Property passage at byte {} of {}; more properties are available on the next graph page.",property.offset.as_str(),property.total_bytes.as_str()));}
        }
        if !complete {widgets::caption(ui,"Property coverage is partial; empty values do not establish absence.");}
    });
}
fn edges(
    ui: &mut Ui,
    title: &str,
    edges: &[CodeEdge],
    neighbors: &[CodeNode],
    actions: &mut Vec<Action>,
) {
    let mut groups: BTreeMap<&str, Vec<&CodeEdge>> = BTreeMap::new();
    for edge in edges {
        groups.entry(&edge.edge_kind.0).or_default().push(edge);
    }
    ui.heading(title);
    if edges.is_empty() {
        ui.label("No relationships in this page.");
    }
    for (kind, items) in groups {
        egui::CollapsingHeader::new(format!("{kind} · {} shown", items.len()))
            .default_open(true)
            .show(ui, |ui| {
                for edge in items {
                    ui.scope_builder(
                        egui::UiBuilder::new().id_salt(edge.edge_id.as_str()),
                        |ui| {
                            for (id, repo) in [
                                (&edge.source_id, &edge.source_repository_id),
                                (&edge.target_id, &edge.target_repository_id),
                            ] {
                                if let Some(node) = neighbors
                                    .iter()
                                    .find(|node| node.node_id == *id && node.repository_id == *repo)
                                {
                                    node_link(ui, node, actions);
                                } else if ui
                                    .button(format!("{} · {}", repo.as_str(), id.as_str()))
                                    .clicked()
                                {
                                    actions.push(Action::Endpoint {
                                        id: id.as_str().into(),
                                        repository: repo.as_str().into(),
                                    });
                                }
                            }
                            optional(
                                ui,
                                "Resolution",
                                edge.resolution.as_ref().map(|v| v.0.as_str()),
                            );
                            optional(
                                ui,
                                "Confidence",
                                edge.confidence_band.as_ref().map(|v| v.0.as_str()),
                            );
                            properties(ui, &edge.properties.0, edge.properties_complete);
                        },
                    );
                }
            });
    }
}

fn links(ui: &mut Ui, items: &[CodeRepositoryLink], actions: &mut Vec<Action>) {
    for link in items {
        selectable(
            ui,
            format!(
                "{} → {} · {} · {} recorded links",
                link.source_repository_id.as_str(),
                link.target_repository_id.as_str(),
                link.edge_kind.0,
                link.count.as_str()
            ),
        );
        if ui
            .small_button(format!(
                "Open related repository {}",
                link.target_repository_id.as_str()
            ))
            .clicked()
        {
            actions.push(Action::Repository(
                link.target_repository_id.as_str().into(),
            ));
        }
    }
}
fn entries(ui: &mut Ui, items: &[CodeTraversalEntry], actions: &mut Vec<Action>) {
    for entry in items {
        widgets::caption(ui, format!("Depth {}", entry.depth.as_str()));
        node_link(ui, &entry.node, actions);
        optional(
            ui,
            "Relationship",
            entry.edge_kind.as_ref().map(|v| v.0.as_str()),
        );
    }
}

fn render(
    ui: &mut Ui,
    page: &GraphPage,
    repository: &str,
    enabled: bool,
    trust_busy: bool,
    actions: &mut Vec<Action>,
) {
    match &page.result {
        GraphResult::CodeSearch { value } => {
            widgets::caption(
                ui,
                format!(
                    "Total: {} · {} low-confidence results",
                    value.total.as_ref().map_or("unknown", |v| v.as_str()),
                    value.low_confidence_count.as_str()
                ),
            );
            for item in &value.items.0 {
                match item {
                    CodeSearchHit::Node { node } => node_link(ui, node, actions),
                    CodeSearchHit::Repository { repository } => {
                        ui.heading(&repository.name.0);
                        optional(
                            ui,
                            "Description",
                            repository.description.as_ref().map(|v| v.0.as_str()),
                        );
                    }
                }
            }
        }
        GraphResult::CodeNodes { value } => {
            widgets::caption(
                ui,
                format!(
                    "Total: {} · {} low-confidence nodes",
                    value.total.as_ref().map_or("unknown", |v| v.as_str()),
                    value.low_confidence_count.as_str()
                ),
            );
            nodes(ui, &value.nodes.0, actions);
        }
        GraphResult::NodeDetail { value } => {
            let node = &value.node;
            ui.heading(&node.name.0);
            selectable(ui, &node.qualified_name.0);
            widgets::chip(
                ui,
                if node.do_not_trust {
                    "Untrusted"
                } else {
                    "Not marked untrusted"
                },
                if node.do_not_trust {
                    Tone::Attention
                } else {
                    Tone::Neutral
                },
            );
            optional(
                ui,
                "Description",
                node.description.as_ref().map(|v| v.0.as_str()),
            );
            widgets::caption(ui, "Trust affects ranking; it does not grant permissions.");
            optional(
                ui,
                "Internal module",
                node.internal_module.as_ref().map(|v| v.0.as_str()),
            );
            optional(
                ui,
                "Analysis confidence",
                node.analysis_confidence.as_ref().map(|v| v.0.as_str()),
            );
            optional(
                ui,
                "Confidence",
                node.confidence.as_ref().map(|v| v.0.as_str()),
            );
            optional(
                ui,
                "Extraction",
                node.extraction_source.as_ref().map(|v| v.0.as_str()),
            );
            optional(
                ui,
                "Fallback",
                node.fallback_reason.as_ref().map(|v| v.0.as_str()),
            );
            if let Some(score) = &node.trust_score {
                widgets::caption(ui, format!("Trust score {:.2}", score.get()));
            }
            if let Some(source) = &node.source {
                selectable(
                    ui,
                    format!(
                        "{} · lines {}–{}",
                        source.path.0,
                        source.start_line.as_str(),
                        source.end_line.as_str()
                    ),
                );
            } else {
                ui.label("No source location is retained for this node.");
            }
            if ui
                .add_enabled(
                    enabled && !trust_busy,
                    egui::Button::new(if node.do_not_trust {
                        "Restore trust"
                    } else {
                        "Mark untrusted"
                    }),
                )
                .clicked()
            {
                actions.push(Action::Trust(node.clone(), node.do_not_trust));
            }
            properties(ui, &node.properties.0, node.properties_complete);
            egui::CollapsingHeader::new("Indexed identity").show(ui, |ui| {
                selectable(ui, format!("Unit identity: {}", node.content_identity.0));
                selectable(ui, format!("View: {}", node.view_id.as_str()));
            });
            if !value.relationships_complete {
                widgets::chip(ui, "Relationship coverage is partial", Tone::Attention);
            }
            edges(
                ui,
                "Incoming",
                &value.incoming.0,
                &value.neighbors.0,
                actions,
            );
            edges(
                ui,
                "Outgoing",
                &value.outgoing.0,
                &value.neighbors.0,
                actions,
            );
            edges(
                ui,
                "Across repositories",
                &value.cross_repository.0,
                &value.neighbors.0,
                actions,
            );
        }
        GraphResult::SourceText { value } => {
            ui.heading(&value.source.path.0);
            widgets::caption(
                ui,
                format!(
                    "Lines {}–{} · bytes {}–{} of {}. A long line may continue on the next page.",
                    value.first_line.as_str(),
                    value.last_line.as_str(),
                    value.start_byte.as_str(),
                    value.end_byte.as_str(),
                    value.total_bytes.as_str()
                ),
            );
            egui::CollapsingHeader::new("Captured source identity").show(ui, |ui| {
                selectable(
                    ui,
                    format!("SHA-256: {}", value.source.source_digest.as_str()),
                )
            });
            for (i, line) in value.text.0.split('\n').enumerate() {
                ui.horizontal_top(|ui| {
                    ui.label(
                        RichText::new((value.first_line.get() + i as i64).to_string())
                            .monospace()
                            .color(theme::DIM),
                    );
                    ui.add(
                        egui::Label::new(RichText::new(line).monospace())
                            .selectable(true)
                            .wrap(),
                    );
                });
            }
        }
        GraphResult::RepositoryOverview { value } => {
            for repo in &value.repositories.0 {
                ui.heading(&repo.name.0);
                optional(
                    ui,
                    "Source group",
                    repo.source_group.as_ref().map(|v| v.0.as_str()),
                );
                optional(ui, "Language", repo.language.as_ref().map(|v| v.0.as_str()));
                optional(
                    ui,
                    "Framework",
                    repo.framework.as_ref().map(|v| v.0.as_str()),
                );
            }
            links(ui, &value.dependencies.0, actions);
        }
        GraphResult::Architecture { value } => {
            if let Some(summary) = &value.summary {
                selectable(ui, &summary.0);
            } else {
                ui.label("No repository analysis summary has been recorded.");
            }
            optional(
                ui,
                "Analysis confidence",
                value.confidence.as_ref().map(|v| v.0.as_str()),
            );
            ui.heading("Indexed node kinds");
            for count in &value.node_counts.0 {
                selectable(ui, format!("{} · {}", count.name.0, count.count.as_str()));
            }
            ui.heading("Hotspots");
            for item in &value.hotspots.0 {
                selectable(
                    ui,
                    format!(
                        "{} · {} incoming · {}",
                        item.qualified_name.0,
                        item.fan_in.as_str(),
                        item.reason.0
                    ),
                );
            }
            ui.heading("Inbound dependencies");
            links(ui, &value.inbound.0, actions);
            ui.heading("Outbound dependencies");
            links(ui, &value.outbound.0, actions);
            ui.heading("Packages");
            for package in &value.packages.0 {
                selectable(
                    ui,
                    format!(
                        "{} · {} · version {} · {} dependents",
                        package.name.0,
                        package.ecosystem.0,
                        package.version.as_ref().map_or("unknown", |v| v.0.as_str()),
                        package.dependent_count.as_str()
                    ),
                );
            }
            ui.heading("Supertypes");
            for item in &value.supertypes.0 {
                selectable(
                    ui,
                    format!(
                        "{} · {} · {} subtypes{}",
                        item.qualified_name.0,
                        item.node_kind.0,
                        item.subtype_count.as_str(),
                        if item.external { " · external" } else { "" }
                    ),
                );
            }
        }
        GraphResult::CodeGraph { value } => {
            nodes(ui, &value.nodes.0, actions);
            edges(ui, "Relationships", &value.edges.0, &value.nodes.0, actions);
        }
        GraphResult::CallTrace { value } => {
            if let Some(node) = &value.start {
                node_link(ui, node, actions);
            }
            entries(ui, &value.entries.0, actions);
        }
        GraphResult::Impact { value } => {
            ui.heading(format!("Impact: {:?}", value.blast_radius));
            if !value.found {
                ui.label("No matching indexed target was found in this view.");
            }
            widgets::caption(
                ui,
                format!(
                    "{} affected · {} across repositories · {} low confidence",
                    value.summary.total_affected.as_str(),
                    value.summary.cross_repository_count.as_str(),
                    value.summary.low_confidence_count.as_str()
                ),
            );
            for item in value.changed.0.iter().chain(&value.affected.0) {
                node_link(ui, &item.node, actions);
                widgets::caption(
                    ui,
                    format!(
                        "{:?} risk · depth {} · {}",
                        item.risk,
                        item.depth.as_str(),
                        item.edge_kind.0
                    ),
                );
                for reason in &item.risk_factors.0 {
                    ui.label(&reason.0);
                }
            }
            links(ui, &value.cross_repository.0, actions);
        }
        GraphResult::Lineage { value } => {
            ui.heading(&value.model_name.0);
            if let Some(node) = &value.model {
                node_link(ui, node, actions);
            }
            ui.heading("Producers");
            entries(ui, &value.producers.0, actions);
            ui.heading("Consumers");
            entries(ui, &value.consumers.0, actions);
            edges(
                ui,
                "Across repositories",
                &value.cross_repository.0,
                &value.related_nodes.0,
                actions,
            );
        }
        GraphResult::Related { value } => {
            for item in &value.nodes.0 {
                node_link(ui, &item.node, actions);
                widgets::caption(ui, &item.edge_kind.0);
            }
        }
        GraphResult::Dependencies { value } => {
            ui.heading("Inbound");
            links(ui, &value.inbound.0, actions);
            ui.heading("Outbound");
            links(ui, &value.outbound.0, actions);
            ui.heading("Rust semantic coverage");
            widgets::caption(ui, &value.rust_semantic.status.0);
            for group in &value.rust_semantic.tree_sitter_groups.0 {
                selectable(
                    ui,
                    format!("Group {} · {}", group.index.as_str(), group.outcome.0),
                );
                optional(
                    ui,
                    "Failure",
                    group.failure_code.as_ref().map(|v| v.0.as_str()),
                );
                optional(
                    ui,
                    "Detail",
                    group.detail_code.as_ref().map(|v| v.0.as_str()),
                );
            }
        }
        GraphResult::SchemaDetails { value } => {
            ui.heading("Node kinds");
            for kind in &value.node_kinds.0 {
                ui.label(&kind.0);
            }
            ui.heading("Relationship kinds");
            for kind in &value.edge_kinds.0 {
                ui.label(&kind.0);
            }
            for property in &value.properties.0 {
                selectable(
                    ui,
                    format!(
                        "{} · {} — {}",
                        property.owner_kind.0, property.name.0, property.description.0
                    ),
                );
            }
            for note in &value.notes.0 {
                ui.label(&note.0);
            }
        }
        GraphResult::ScopeDetails { value } => {
            widgets::caption(
                ui,
                format!(
                    "Coverage: {:?} · {}reported current",
                    value.coverage.state,
                    if value.current { "" } else { "not " }
                ),
            );
            optional(
                ui,
                "Unavailable",
                value.unavailable_reason.as_ref().map(|v| v.0.as_str()),
            );
            for repo in &value.repositories.0 {
                ui.heading(repo.repository_id.as_str());
                widgets::caption(ui, format!("Freshness: {:?}", repo.freshness.state));
                for language in &repo.languages.0 {
                    widgets::caption(
                        ui,
                        format!(
                            "{} · {:?} · {} files",
                            language.name.0,
                            language.mode,
                            language.files.as_str()
                        ),
                    );
                }
            }
        }
        _ => {
            ui.label(format!("A current typed graph result is unavailable for repository {repository}. Refresh the view or check compatibility."));
        }
    }
}
