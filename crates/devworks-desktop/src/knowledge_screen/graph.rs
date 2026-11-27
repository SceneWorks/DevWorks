//! The Knowledge graph view: a claim or entity bundle, an entity's edges, a subgraph around what
//! is shown (or a page of the whole graph), a graph query with its rendered text, one-step
//! frontier expansion and a published summary. Each is the same `knowledge.*` read an agent
//! tool makes. A view stays on screen until the reply that replaces it arrives, and a reply of
//! an older request is ignored.
use crate::frame;
use crate::ops::{self, Failure};
use crate::theme;
use crate::widgets::{self, Tone, chip};
use devworks_core::gateway::QueryOutcome;
use devworks_core::operations::{OperationInput as In, knowledge};
use devworks_core::protocol::Id;
use devworks_core::protocol::data::DataScope;
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{KnowledgeBundle, KnowledgeClaim, KnowledgeQuery};
use devworks_core::protocol::results::OperationResult;
use devworks_core::protocol::support::{BoundedList, Depth, Limit};
use devworks_core::protocol::wire_name;
use eframe::egui::{self, RichText, Ui};

/// Rows one page of the whole graph asks for.
pub const GRAPH_PAGE: u32 = 100;
/// The most IDs one graph request names (the contract's list bound).
const MAX_SEEDS: usize = 128;
/// The most rows of one kind a merged view keeps (the contract's row bound).
const MAX_ROWS: usize = 500;

/// What a graph view reads.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphRead {
    /// `knowledge.claim_bundle`: one claim with its entities, evidence, conflicts and relations.
    Claim(Id),
    /// `knowledge.entity_bundle`: one entity with its claims and neighbors.
    Entity(Id),
    /// `knowledge.entity_graph`: the claims that link one entity to other entities.
    EntityEdges(Id),
    /// `knowledge.subgraph` two hops around these entities and claims.
    Around { entities: Vec<Id>, claims: Vec<Id> },
    /// `knowledge.subgraph` without seeds: one page of the whole visible graph.
    Whole { cursor: Option<String> },
    /// `knowledge.query`: the graph a question reaches, with its rendered text.
    Query(String),
}

impl GraphRead {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Claim(_) => "knowledge.claim_bundle",
            Self::Entity(_) => "knowledge.entity_bundle",
            Self::EntityEdges(_) => "knowledge.entity_graph",
            Self::Around { .. } | Self::Whole { .. } => "knowledge.subgraph",
            Self::Query(_) => "knowledge.query",
        }
    }
    /// The typed request of this read in `scope`.
    pub fn input(&self, scope: DataScope) -> Option<In> {
        let one = |id: &Id| std::slice::from_ref(id).to_vec();
        let input = match self {
            Self::Claim(id) => knowledge::Input::ClaimBundle(knowledge::ClaimBundleInput {
                scope,
                query: graph_query(&[], &one(id), None, 1, None)?,
            }),
            Self::Entity(id) => knowledge::Input::EntityBundle(knowledge::EntityBundleInput {
                scope,
                query: graph_query(&one(id), &[], None, 1, None)?,
            }),
            Self::EntityEdges(id) => knowledge::Input::EntityGraph(knowledge::EntityGraphInput {
                scope,
                query: graph_query(&one(id), &[], None, 1, None)?,
            }),
            Self::Around { entities, claims } => {
                knowledge::Input::Subgraph(knowledge::SubgraphInput {
                    scope,
                    query: graph_query(entities, claims, None, 2, None)?,
                })
            }
            Self::Whole { cursor } => knowledge::Input::Subgraph(knowledge::SubgraphInput {
                scope,
                query: graph_query(&[], &[], None, 1, cursor.as_deref())?,
            }),
            Self::Query(text) => knowledge::Input::Query(knowledge::QueryInput {
                scope,
                query: graph_query(&[], &[], Some(text.trim()), 2, None)?,
            }),
        };
        Some(In::Knowledge(input))
    }
}

/// A graph query naming these seeds (at most [`MAX_SEEDS`] of each), with evidence, conflicts
/// and supersession chains included.
fn graph_query(
    entities: &[Id],
    claims: &[Id],
    text: Option<&str>,
    depth: u32,
    cursor: Option<&str>,
) -> Option<KnowledgeQuery> {
    bounded_query(entities, claims, text, depth, cursor, (50, 100))
}
/// [`graph_query`] with its entity and claim limits.
fn bounded_query(
    entities: &[Id],
    claims: &[Id],
    text: Option<&str>,
    depth: u32,
    cursor: Option<&str>,
    (entity_limit, claim_limit): (u32, u32),
) -> Option<KnowledgeQuery> {
    let ids = |list: &[Id]| BoundedList(list.iter().take(MAX_SEEDS).cloned().collect());
    Some(KnowledgeQuery {
        entity_ids: ids(entities),
        claim_ids: ids(claims),
        predicates: BoundedList(Vec::new()),
        tags: BoundedList(Vec::new()),
        states: BoundedList(Vec::new()),
        text: frame::lift(text.map(frame::text))?,
        max_depth: Depth::new(depth)?,
        page: frame::page(GRAPH_PAGE, cursor)?,
        entity_limit: Limit::new(entity_limit)?,
        claim_limit: Limit::new(claim_limit)?,
        evidence_limit: Limit::new(100)?,
        neighbor_limit: Limit::new(25)?,
        include_superseded: false,
        include_conflicts: true,
        include_supersession_chain: true,
        include_evidence: true,
        confidence_min: None,
    })
}

/// The bundle a graph read answered with.
fn bundle_of(result: OperationResult) -> Result<KnowledgeBundle, String> {
    match result {
        OperationResult::KnowledgeClaimBundle { value }
        | OperationResult::KnowledgeEntityBundle { value }
        | OperationResult::KnowledgeEntityGraph { value }
        | OperationResult::KnowledgeSubgraph { value }
        | OperationResult::KnowledgeQuery { value }
        | OperationResult::KnowledgeExpandFrontier { value } => Ok(value),
        other => Err(format!("Unexpected reply {}", other.operation())),
    }
}

/// A shown graph: what was read, the bundle (with the expansions merged into it) and its summary.
#[derive(Clone, Debug)]
pub struct GraphView {
    pub read: GraphRead,
    pub bundle: KnowledgeBundle,
    /// Frontier steps merged since the read.
    pub steps: u32,
    /// The published summary's text, once read.
    pub summary: Option<String>,
}

impl GraphView {
    /// The view's heading.
    pub fn title(&self) -> String {
        match &self.read {
            GraphRead::Claim(id) => format!("Claim bundle: {}", claim_text(&self.bundle, id)),
            GraphRead::Entity(id) => format!("Entity bundle: {}", entity_label(&self.bundle, id)),
            GraphRead::EntityEdges(id) => format!("Edges of {}", entity_label(&self.bundle, id)),
            GraphRead::Around { .. } => "Subgraph".into(),
            GraphRead::Whole { .. } => "Whole graph".into(),
            GraphRead::Query(text) => format!("Graph query: {text}"),
        }
    }
    /// The seeds one frontier step expands: the frontier the read returned, or (when it named
    /// none) every entity shown, with the claims shown so that they are not added again.
    fn frontier_seeds(&self) -> (Vec<Id>, Vec<Id>) {
        let claims: Vec<Id> = self.bundle.claims.0.iter().map(|c| c.id.clone()).collect();
        if !self.bundle.frontier.0.is_empty() {
            return (self.bundle.frontier.0.clone(), claims);
        }
        let entities = self
            .bundle
            .entities
            .0
            .iter()
            .map(|e| e.id.clone())
            .collect();
        (entities, claims)
    }
    /// Adds what a frontier step reached; rows already shown are kept as shown.
    fn merge(&mut self, more: KnowledgeBundle) {
        fn add<T>(into: &mut Vec<T>, rows: Vec<T>, same: impl Fn(&T, &T) -> bool) -> bool {
            let mut cut = false;
            for row in rows {
                if into.iter().any(|x| same(x, &row)) {
                    continue;
                }
                if into.len() >= MAX_ROWS {
                    cut = true;
                    continue;
                }
                into.push(row);
            }
            cut
        }
        let b = &mut self.bundle;
        let mut cut = add(&mut b.entities.0, more.entities.0, |a, b| a.id == b.id);
        cut |= add(&mut b.claims.0, more.claims.0, |a, b| a.id == b.id);
        cut |= add(&mut b.evidence.0, more.evidence.0, |a, b| a.id == b.id);
        cut |= add(&mut b.conflicts.0, more.conflicts.0, |a, b| a.id == b.id);
        if let Some(rows) = more.relations {
            let into = b.relations.get_or_insert_with(|| BoundedList(Vec::new()));
            cut |= add(&mut into.0, rows.0, |a, b| a.id == b.id);
        }
        b.frontier = more.frontier;
        b.truncated |= more.truncated || cut;
        self.steps += 1;
        self.summary = None;
    }
    /// The entities and claims shown (at most [`MAX_SEEDS`] of each): what a summary covers.
    fn shown(&self) -> (Vec<Id>, Vec<Id>) {
        (
            self.bundle
                .entities
                .0
                .iter()
                .map(|e| e.id.clone())
                .collect(),
            self.bundle.claims.0.iter().map(|c| c.id.clone()).collect(),
        )
    }
}

/// The text of a claim in `bundle` (its ID when not there).
pub fn claim_text(bundle: &KnowledgeBundle, id: &Id) -> String {
    bundle
        .claims
        .0
        .iter()
        .find(|c| c.id == *id)
        .map_or_else(|| id.as_str().to_owned(), text_of)
}
/// What a claim says: its value, or its predicate.
pub fn text_of(c: &KnowledgeClaim) -> String {
    c.value
        .as_ref()
        .map_or_else(|| c.predicate.0.clone(), |v| v.0.clone())
}
fn entity_label(bundle: &KnowledgeBundle, id: &Id) -> String {
    bundle
        .entities
        .0
        .iter()
        .find(|e| e.id == *id)
        .map_or_else(|| id.as_str().to_owned(), |e| e.label.0.clone())
}
/// A node named for a person: a claim's text (from `bundle` or the entries `known` besides)
/// or an entity's label.
fn node_name(bundle: &KnowledgeBundle, known: &[KnowledgeClaim], id: &Id) -> String {
    match bundle.claims.0.iter().chain(known).find(|c| c.id == *id) {
        Some(c) => text_of(c),
        None => entity_label(bundle, id),
    }
}

/// A rendered summary's progress.
#[derive(Clone, Debug, Default)]
pub enum Summary {
    #[default]
    Idle,
    /// `knowledge.render_summary` is publishing the summary.
    Rendering(u64),
    /// The published text is being read, page by page: the bytes so far and the byte cursor
    /// of the page requested last (`None`: the first page).
    Reading {
        generation: u64,
        artifact: ArtifactRef,
        bytes: Vec<u8>,
        cursor: Option<String>,
    },
    Failed(String),
}

/// What the graph read in flight will do with its reply.
#[derive(Clone, Debug)]
enum Pending {
    /// Show this read.
    Open(GraphRead),
    /// Merge one frontier step into the view.
    Expand,
    /// Replace an expanded view's rows, keeping what it was opened as and its steps.
    Refresh { read: GraphRead, steps: u32 },
}

/// The graph view and its requests.
#[derive(Default)]
pub struct Graph {
    pub view: Option<GraphView>,
    /// The read in flight: its generation and what its reply does.
    pending: Option<(u64, Pending)>,
    generation: u64,
    pub error: Option<String>,
    /// The question typed for `knowledge.query`.
    pub query: String,
    pub summary: Summary,
    summary_generation: u64,
}

impl Graph {
    /// Whether a graph read is in flight.
    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }
    /// Whether a summary is being rendered or read.
    pub fn summarizing(&self) -> bool {
        matches!(
            self.summary,
            Summary::Rendering(_) | Summary::Reading { .. }
        )
    }
    /// Closes the view; replies still in flight are ignored.
    pub fn close(&mut self) {
        self.view = None;
        self.pending = None;
        self.error = None;
        self.summary = Summary::Idle;
        self.generation += 1;
        self.summary_generation += 1;
    }
    /// Starts reading `read` in `scope`: its generation, operation and request. The view shown
    /// stays until the reply replaces it.
    pub fn begin(
        &mut self,
        scope: Option<DataScope>,
        read: GraphRead,
    ) -> (u64, &'static str, Option<In>) {
        self.generation += 1;
        self.error = None;
        self.pending = Some((self.generation, Pending::Open(read.clone())));
        (
            self.generation,
            read.operation(),
            scope.and_then(|s| read.input(s)),
        )
    }
    /// Reads the graph again after a write changed Knowledge; `None` with nothing to read.
    /// A read the person is still waiting on is sent again. A view with frontier steps is
    /// re-read as a depth-1 subgraph over what it shows, keeping its title and steps, so rows a
    /// step reached stay. A later page of the whole graph re-reads from the first page: the
    /// write moved the revision its cursor was bound to.
    pub fn begin_refresh(
        &mut self,
        scope: Option<DataScope>,
    ) -> Option<(u64, &'static str, Option<In>)> {
        if let Some((_, Pending::Open(read))) = &self.pending {
            let read = read.clone();
            return Some(self.begin(scope, read));
        }
        let view = self.view.as_ref()?;
        if view.steps == 0 {
            let read = match &view.read {
                GraphRead::Whole { cursor: Some(_) } => GraphRead::Whole { cursor: None },
                read => read.clone(),
            };
            return Some(self.begin(scope, read));
        }
        let (entities, claims) = view.shown();
        let pending = Pending::Refresh {
            read: view.read.clone(),
            steps: view.steps,
        };
        self.generation += 1;
        self.error = None;
        self.pending = Some((self.generation, pending));
        let input = (|| {
            Some(In::Knowledge(knowledge::Input::Subgraph(
                knowledge::SubgraphInput {
                    scope: scope?,
                    query: bounded_query(&entities, &claims, None, 1, None, (100, 200))?,
                },
            )))
        })();
        Some((self.generation, "knowledge.subgraph", input))
    }
    /// Starts one frontier step of the view shown (`knowledge.expand_frontier`).
    pub fn begin_expand(
        &mut self,
        scope: Option<DataScope>,
    ) -> Result<(u64, &'static str, Option<In>), String> {
        let view = self.view.as_ref().ok_or("Open a bundle first.")?;
        if self.pending.is_some() {
            return Err("The graph is still being read.".into());
        }
        let (entities, claims) = view.frontier_seeds();
        if entities.is_empty() && claims.is_empty() {
            return Err("Nothing is shown to expand from.".into());
        }
        self.generation += 1;
        self.error = None;
        self.pending = Some((self.generation, Pending::Expand));
        let input = (|| {
            Some(In::Knowledge(knowledge::Input::ExpandFrontier(
                knowledge::ExpandFrontierInput {
                    scope: scope?,
                    query: graph_query(&entities, &claims, None, 1, None)?,
                },
            )))
        })();
        Ok((self.generation, "knowledge.expand_frontier", input))
    }
    /// A read that could not be submitted is not left in flight.
    pub fn abandon(&mut self, generation: u64, why: &str) {
        if self.pending.as_ref().is_some_and(|(g, _)| *g == generation) {
            self.pending = None;
            self.error = Some(why.to_owned());
        }
    }
    /// Applies a graph read's reply (ignored unless it answers the read in flight).
    pub fn reply(&mut self, generation: u64, outcome: QueryOutcome) {
        let Some((g, pending)) = self.pending.clone() else {
            return;
        };
        if g != generation {
            return;
        }
        self.pending = None;
        let bundle = match ops::query_result(outcome) {
            Ok((result, _)) => bundle_of(result),
            Err(e) => Err(plain(&e)),
        };
        let bundle = match bundle {
            Ok(bundle) => bundle,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        match pending {
            Pending::Expand => match self.view.as_mut() {
                Some(view) => view.merge(bundle),
                None => return,
            },
            Pending::Open(read) => self.show(read, bundle, 0),
            Pending::Refresh { read, steps } => self.show(read, bundle, steps),
        }
        // What the summary covered changed: one in progress no longer applies.
        self.summary_generation += 1;
        self.summary = Summary::Idle;
    }
    fn show(&mut self, read: GraphRead, bundle: KnowledgeBundle, steps: u32) {
        self.view = Some(GraphView {
            read,
            bundle,
            steps,
            summary: None,
        });
    }
    /// Starts publishing a summary of what is shown (`knowledge.render_summary`). Not while a
    /// graph read is in flight: its reply would replace what the summary covers.
    pub fn begin_summary(&mut self, scope: Option<DataScope>) -> Result<(u64, Option<In>), String> {
        let view = self.view.as_ref().ok_or("Open a bundle first.")?;
        if self.summarizing() {
            return Err("A summary is already being rendered.".into());
        }
        if self.loading() {
            return Err("The graph is still being read.".into());
        }
        let (entities, claims) = view.shown();
        if entities.is_empty() && claims.is_empty() {
            return Err("Nothing is shown to summarize.".into());
        }
        self.summary_generation += 1;
        self.summary = Summary::Rendering(self.summary_generation);
        let input = (|| {
            Some(In::Knowledge(knowledge::Input::RenderSummary(
                knowledge::RenderSummaryInput {
                    scope: scope?,
                    query: graph_query(&entities, &claims, None, 1, None)?,
                },
            )))
        })();
        Ok((self.summary_generation, input))
    }
    /// A summary that could not be submitted is not left in progress.
    pub fn abandon_summary(&mut self, generation: u64, why: &str) {
        let current = match &self.summary {
            Summary::Rendering(g) => *g,
            Summary::Reading { generation, .. } => *generation,
            _ => return,
        };
        if current == generation {
            self.summary = Summary::Failed(why.to_owned());
        }
    }
    /// Applies the published summary's reference: the artifact to read next.
    pub fn summary_published(
        &mut self,
        generation: u64,
        outcome: QueryOutcome,
    ) -> Option<ArtifactRef> {
        if !matches!(self.summary, Summary::Rendering(g) if g == generation) {
            return None;
        }
        match ops::query_result(outcome) {
            Ok((OperationResult::KnowledgeRenderSummary { value }, _)) => {
                self.summary = Summary::Reading {
                    generation,
                    artifact: value.clone(),
                    bytes: Vec::new(),
                    cursor: None,
                };
                Some(value)
            }
            Ok((other, _)) => {
                self.summary = Summary::Failed(format!("Unexpected reply {}", other.operation()));
                None
            }
            Err(e) => {
                self.summary = Summary::Failed(plain(&e));
                None
            }
        }
    }
    /// Applies one page of the summary's text: the artifact and byte cursor of the next page,
    /// or `None` once the whole text is shown (or the read failed).
    pub fn summary_text(
        &mut self,
        generation: u64,
        outcome: QueryOutcome,
    ) -> Option<(ArtifactRef, String)> {
        let Summary::Reading {
            generation: g,
            artifact,
            bytes,
            cursor,
        } = &mut self.summary
        else {
            return None;
        };
        if *g != generation {
            return None;
        }
        // The page step the shared artifact reader uses: length bound, progress guard.
        let step = ops::read_content(outcome)
            .map_err(|e| e.describe())
            .and_then(|page| ops::append_text_page(bytes, artifact, cursor.as_deref(), page));
        let done = match step {
            Ok(ops::TextPage::More(next)) => {
                *cursor = Some(next.clone());
                return Some((artifact.clone(), next));
            }
            Ok(ops::TextPage::Done) => ops::verify_artifact_bytes(bytes, artifact).and_then(|()| {
                String::from_utf8(std::mem::take(bytes))
                    .map_err(|_| "The summary is not UTF-8 text.".to_owned())
            }),
            Err(e) => Err(e),
        };
        match done {
            Ok(text) => {
                self.summary = Summary::Idle;
                if let Some(view) = self.view.as_mut() {
                    view.summary = Some(text);
                }
            }
            Err(e) => self.summary = Summary::Failed(e),
        }
        None
    }

    /// The graph panel: the view, its controls and its summary.
    /// `known`: entries listed besides, which name relation ends the view does not hold.
    pub fn panel(
        &mut self,
        ui: &mut Ui,
        online: bool,
        known: &[KnowledgeClaim],
        actions: &mut Vec<GraphAction>,
    ) {
        if self.view.is_none() && self.pending.is_none() && self.error.is_none() {
            return;
        }
        egui::Frame::new()
            .stroke(theme::soft_line())
            .corner_radius(theme::radius())
            .inner_margin(theme::margin(12, 10))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let title = self
                        .view
                        .as_ref()
                        .map_or_else(|| "Knowledge graph".to_owned(), GraphView::title);
                    ui.label(RichText::new(title).strong());
                    if self.loading() {
                        chip(ui, "Reading the graph…", Tone::Neutral);
                    }
                    if let Some(v) = &self.view {
                        if v.steps > 0 {
                            let unit = if v.steps == 1 { "step" } else { "steps" };
                            chip(ui, format!("Expanded {} {unit}", v.steps), Tone::Accent);
                        }
                        if v.bundle.truncated {
                            chip(ui, "More exists than is shown", Tone::Attention);
                        }
                    }
                });
                let idle = online && !self.loading();
                ui.horizontal_wrapped(|ui| {
                    if self.view.is_some() {
                        if ui
                            .add_enabled(idle, egui::Button::new("Expand one step"))
                            .clicked()
                        {
                            actions.push(GraphAction::Expand);
                        }
                        if ui
                            .add_enabled(idle, egui::Button::new("Subgraph around this"))
                            .clicked()
                            && let Some(v) = &self.view
                        {
                            let (entities, claims) = v.shown();
                            actions.push(GraphAction::Open(GraphRead::Around { entities, claims }));
                        }
                        if ui
                            .add_enabled(
                                idle && !self.summarizing(),
                                egui::Button::new("Render summary"),
                            )
                            .clicked()
                        {
                            actions.push(GraphAction::Summary);
                        }
                        if let Some(v) = &self.view
                            && matches!(v.read, GraphRead::Whole { .. })
                            && let Some(next) = &v.bundle.next_cursor
                            && ui
                                .add_enabled(idle, egui::Button::new("Next graph page"))
                                .clicked()
                        {
                            actions.push(GraphAction::Open(GraphRead::Whole {
                                cursor: Some(next.0.clone()),
                            }));
                        }
                    }
                    if widgets::secondary_button(ui, "Close graph").clicked() {
                        actions.push(GraphAction::Close);
                    }
                });
                if let Some(e) = &self.error {
                    chip(ui, e.clone(), Tone::Bad);
                }
                match &self.summary {
                    Summary::Rendering(_) | Summary::Reading { .. } => {
                        widgets::caption(ui, "Rendering the summary…");
                    }
                    Summary::Failed(e) => {
                        chip(ui, format!("Summary not rendered. {e}"), Tone::Bad);
                    }
                    Summary::Idle => {}
                }
                let Some(view) = &self.view else {
                    return;
                };
                egui::ScrollArea::vertical()
                    .id_salt("knowledge-graph")
                    .max_height(340.0)
                    .show(ui, |ui| view_body(ui, view, known, actions));
            });
        ui.add_space(6.0);
    }
}

/// The rows of a shown graph.
fn view_body(
    ui: &mut Ui,
    view: &GraphView,
    known: &[KnowledgeClaim],
    actions: &mut Vec<GraphAction>,
) {
    let b = &view.bundle;
    if let Some(text) = &view.summary {
        ui.label(RichText::new("Summary").strong());
        ui.label(text);
        widgets::divider(ui);
    } else if let (GraphRead::Query(_), Some(text)) = (&view.read, &b.summary) {
        ui.label(RichText::new("Answer").strong());
        ui.label(&text.0);
        widgets::divider(ui);
    }
    ui.label(RichText::new(format!("Entities ({})", b.entities.0.len())).strong());
    if b.entities.0.is_empty() {
        widgets::caption(ui, "No entities.");
    }
    for e in &b.entities.0 {
        ui.horizontal_wrapped(|ui| {
            let name = e.label.0.clone();
            let r = ui.link(format!("{name} · {}", e.entity_type.0));
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Link, true, format!("Open entity {name}"))
            });
            if r.clicked() {
                actions.push(GraphAction::Open(GraphRead::Entity(e.id.clone())));
            }
            let r = ui.small_button("Edges");
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Entity edges {name}"))
            });
            if r.clicked() {
                actions.push(GraphAction::Open(GraphRead::EntityEdges(e.id.clone())));
            }
        });
    }
    widgets::divider(ui);
    ui.label(RichText::new(format!("Entries ({})", b.claims.0.len())).strong());
    if b.claims.0.is_empty() {
        widgets::caption(ui, "No entries.");
    }
    for c in &b.claims.0 {
        let text = text_of(c);
        let kind = c
            .entry_kind
            .as_ref()
            .map_or_else(|| wire_name(&c.kind), wire_name);
        let about = node_name(b, known, &c.subject_id);
        let between = match &c.object_id {
            Some(o) => format!("{about} and {}", node_name(b, known, o)),
            None => about,
        };
        let r = ui.link(format!("{kind} · {text} ({between})"));
        r.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Link, true, format!("Open claim {text}"))
        });
        if r.clicked() {
            actions.push(GraphAction::Select(c.id.clone()));
            actions.push(GraphAction::Open(GraphRead::Claim(c.id.clone())));
        }
    }
    let relations = b.relations.as_ref().map_or(&[][..], |r| r.0.as_slice());
    if !relations.is_empty() {
        widgets::divider(ui);
        ui.label(RichText::new(format!("Relations ({})", relations.len())).strong());
        for r in relations {
            widgets::caption(
                ui,
                format!(
                    "\"{}\" {} \"{}\"",
                    node_name(b, known, &r.subject_id),
                    r.predicate.0.replace('_', " "),
                    node_name(b, known, &r.object_id)
                ),
            );
        }
    }
    if !b.conflicts.0.is_empty() {
        widgets::divider(ui);
        for k in &b.conflicts.0 {
            chip(
                ui,
                format!("Conflict: {}", k.explanation.0),
                Tone::Attention,
            );
        }
    }
    if !b.evidence.0.is_empty() {
        widgets::divider(ui);
        ui.label(RichText::new(format!("Evidence ({})", b.evidence.0.len())).strong());
        for e in &b.evidence.0 {
            widgets::caption(
                ui,
                format!(
                    "{} {}: {}",
                    wire_name(&e.source.kind),
                    e.source.source_ref.0,
                    e.snippet.0
                ),
            );
        }
    }
    if !b.frontier.0.is_empty() {
        widgets::caption(
            ui,
            format!(
                "{} entities are at the frontier: Expand one step reaches their neighbors.",
                b.frontier.0.len()
            ),
        );
    }
}

/// What the graph panel asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum GraphAction {
    Open(GraphRead),
    /// One frontier step of the view shown.
    Expand,
    /// Renders a summary of what is shown.
    Summary,
    /// Selects a claim (its detail and relation form).
    Select(Id),
    Close,
}

/// A graph read's failure in plain words.
fn plain(e: &Failure) -> String {
    use devworks_core::protocol::DomainErrorCode as Code;
    match e {
        Failure::Refused(f) => match (&f.code, f.target_kind.as_ref().map(|k| k.0.as_str())) {
            (Code::NotFound, _) => {
                "An entry or entity shown is no longer in this scope; search again.".into()
            }
            (Code::CursorExpired, _) => {
                "Knowledge changed while paging the graph; open it again.".into()
            }
            (Code::UnsupportedCapability, Some("knowledge_summary")) => {
                "Summaries are not available in this build.".into()
            }
            _ => e.describe(),
        },
        _ => e.describe(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// A query reply carrying `result` (as the gateway answers).
    fn answer(result: Value) -> QueryOutcome {
        Ok(
            serde_json::from_value(json!({"kind": "Result", "protocol": "1.0-draft.1",
            "queryId": "q", "stamp": {"storeId": "store", "restoreEpoch": "1", "sequence": "1",
                "policyRevision": "1", "scopeRevision": "1"}, "result": result}))
            .expect("a query reply"),
        )
    }
    fn bundle(entities: &[&str], claims: &[&str]) -> Value {
        json!({"entities": entities.iter().map(|id| json!({"id": id, "revision": "1",
                "label": id, "entityType": "topic", "aliases": [], "scope": {"kind": "Workspace"}}))
                .collect::<Vec<_>>(),
            "claims": claims.iter().map(|id| json!({"id": id, "revision": "1", "kind": "Fact",
                "subjectId": entities[0], "predicate": "is", "value": id, "confidence": 0.5,
                "state": "Active", "evidenceIds": [], "tags": [], "scope": {"kind": "Workspace"}}))
                .collect::<Vec<_>>(),
            "evidence": [], "frontier": [], "truncated": false, "conflicts": [],
            "supersessionIds": []})
    }
    fn body(input: Option<In>) -> Value {
        input.expect("a typed input").body().expect("a body")
    }
    fn id(s: &str) -> Id {
        Id::new(s).unwrap()
    }
    const WORKSPACE: Option<DataScope> = Some(DataScope::Workspace);

    /// A graph showing Storage's entity bundle, expanded one step to SQLite's claim.
    fn expanded() -> Graph {
        let mut g = Graph::default();
        let (opened, _, _) = g.begin(WORKSPACE, GraphRead::Entity(id("storage")));
        g.reply(
            opened,
            answer(json!({"kind": "knowledge.entity_bundle",
                "value": bundle(&["storage", "sqlite"], &["uses"])})),
        );
        let (step, _, _) = g.begin_expand(WORKSPACE).unwrap();
        g.reply(
            step,
            answer(json!({"kind": "knowledge.expand_frontier",
                "value": bundle(&["sqlite", "fts5"], &["provides"])})),
        );
        assert_eq!(g.view.as_ref().unwrap().steps, 1);
        g
    }

    #[test]
    fn an_expanded_view_is_refreshed_over_what_it_shows_and_keeps_its_steps() {
        let mut g = expanded();
        let (again, op, input) = g.begin_refresh(WORKSPACE).expect("a refresh");
        assert_eq!(op, "knowledge.subgraph");
        let q = &body(input)["query"];
        assert_eq!(q["entityIds"], json!(["storage", "sqlite", "fts5"]));
        assert_eq!(q["claimIds"], json!(["uses", "provides"]));
        assert_eq!(q["maxDepth"], json!(1));
        g.reply(
            again,
            answer(json!({"kind": "knowledge.subgraph",
                "value": bundle(&["storage", "sqlite", "fts5"], &["uses", "provides"])})),
        );
        let v = g.view.as_ref().unwrap();
        assert_eq!(v.steps, 1, "the steps stay");
        assert_eq!(v.title(), "Entity bundle: storage", "so does the title");
        assert!(
            v.bundle
                .claims
                .0
                .iter()
                .any(|c| c.id.as_str() == "provides")
        );
    }

    #[test]
    fn a_later_page_of_the_whole_graph_is_refreshed_from_the_first_page() {
        let mut g = Graph::default();
        let (page, _, _) = g.begin(
            WORKSPACE,
            GraphRead::Whole {
                cursor: Some("page-2".into()),
            },
        );
        g.reply(
            page,
            answer(json!({"kind": "knowledge.subgraph", "value": bundle(&["a"], &[])})),
        );
        let (_, op, input) = g.begin_refresh(WORKSPACE).unwrap();
        assert_eq!(op, "knowledge.subgraph");
        assert!(body(input)["query"]["page"].get("cursor").is_none());
    }

    fn reference(text: &str) -> Value {
        use sha2::{Digest, Sha256};
        let digest: String = Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        json!({"artifactId": "summary", "versionId": "v1", "sha256": digest,
            "byteLength": text.len().to_string(), "mediaType": "text/markdown; charset=utf-8"})
    }
    fn page(
        reference: &Value,
        first: usize,
        content: &str,
        next: usize,
        more: bool,
    ) -> QueryOutcome {
        answer(json!({"kind": "artifacts.read", "value": {"kind": "Text",
            "artifact": reference, "firstByte": first.to_string(), "nextByte": next.to_string(),
            "content": content, "truncated": more}}))
    }
    /// A summary of the view published as `reference`: its generation.
    fn summarizing(g: &mut Graph, reference: &Value) -> u64 {
        let (generation, _) = g.begin_summary(WORKSPACE).unwrap();
        let published = g.summary_published(
            generation,
            answer(json!({"kind": "knowledge.render_summary", "value": reference})),
        );
        assert!(published.is_some());
        generation
    }

    #[test]
    fn a_summary_is_read_page_by_page_and_checked_against_its_reference() {
        let text = "## Relevant Memory\n\nstorage";
        let r = reference(text);
        let mut g = expanded();
        let generation = summarizing(&mut g, &r);
        let next = g.summary_text(generation, page(&r, 0, &text[..10], 10, true));
        assert_eq!(next.map(|(_, cursor)| cursor).as_deref(), Some("10"));
        let next = g.summary_text(generation, page(&r, 10, &text[10..], text.len(), false));
        assert!(next.is_none());
        assert_eq!(g.view.as_ref().unwrap().summary.as_deref(), Some(text));

        // A text that does not match the reference's digest is refused.
        let wrong = reference("other text of 27 characters");
        let generation = summarizing(&mut g, &wrong);
        g.summary_text(
            generation,
            page(&wrong, 0, "other text of 27 characterz", 27, false),
        );
        assert!(
            matches!(&g.summary, Summary::Failed(e) if e.contains("digest")),
            "{:?}",
            g.summary
        );
    }

    #[test]
    fn a_summary_page_that_does_not_advance_fails_instead_of_looping() {
        let r = reference("some summary text");
        let mut g = expanded();
        let generation = summarizing(&mut g, &r);
        assert!(
            g.summary_text(generation, page(&r, 0, "some ", 5, true))
                .is_some()
        );
        let next = g.summary_text(generation, page(&r, 5, "", 5, true));
        assert!(next.is_none(), "no further page is requested");
        assert!(
            matches!(&g.summary, Summary::Failed(e) if e.contains("did not advance")),
            "{:?}",
            g.summary
        );
    }

    #[test]
    fn a_summary_reply_from_before_the_view_changed_is_ignored() {
        let r = reference("some summary text");
        let mut g = expanded();
        let generation = summarizing(&mut g, &r);
        // A frontier step changes what the summary covered.
        let (step, _, _) = g.begin_expand(WORKSPACE).unwrap();
        g.reply(
            step,
            answer(json!({"kind": "knowledge.expand_frontier", "value": bundle(&["x"], &[])})),
        );
        assert!(matches!(g.summary, Summary::Idle));
        assert!(
            g.summary_text(generation, page(&r, 0, "some summary text", 17, false))
                .is_none()
        );
        assert!(
            g.view.as_ref().unwrap().summary.is_none(),
            "the old text is not shown"
        );
        // No summary starts while the graph is read: the reply would replace what it covers.
        let (reading, _, _) = g.begin(WORKSPACE, GraphRead::Claim(id("uses")));
        assert_eq!(
            g.begin_summary(WORKSPACE).unwrap_err(),
            "The graph is still being read."
        );
        g.reply(
            reading,
            answer(json!({"kind": "knowledge.claim_bundle",
                "value": bundle(&["storage"], &["uses"])})),
        );
        // A summary reply from before a new read is ignored too.
        let generation = summarizing(&mut g, &r);
        let (opened, _, _) = g.begin(WORKSPACE, GraphRead::Claim(id("uses")));
        g.reply(
            opened,
            answer(json!({"kind": "knowledge.claim_bundle",
                "value": bundle(&["storage"], &["uses"])})),
        );
        g.summary_text(generation, page(&r, 0, "some summary text", 17, false));
        assert!(g.view.as_ref().unwrap().summary.is_none());
    }
}
