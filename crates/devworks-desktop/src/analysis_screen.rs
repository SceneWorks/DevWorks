//! Analysis reads share the repository graph's authority and captured view.
use crate::{
    backend::Backend,
    frame::{self, Negotiate},
    widgets::{self, Tone},
};
use devworks_core::{
    operations::{OperationInput, analysis as a},
    protocol::{
        Id, Revision,
        data::{DataScope, ViewSelection},
        provider::ArtifactRef,
        records::{
            AnalysisAvailability, AnalysisClusterPageResult, AnalysisClusterProjection,
            AnalysisLinkMode, AnalysisMetricItem, AnalysisMetricPage, AnalysisMetricPageResult,
            AnalysisObservation, AnalysisRefreshMode,
        },
        results::OperationResult,
        support::{BoundedList, Limit, RelativePath},
    },
};
use eframe::egui::{self, Ui};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Health,
    Security,
    Hotspots,
    File(String),
    Links,
    Clusters(bool),
    Cluster(Id),
    Status,
    Fleet,
    Archival,
}

impl Query {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Health => "analysis.health",
            Self::Security => "analysis.security",
            Self::Hotspots => "analysis.hotspots",
            Self::File(_) => "analysis.file_hotspot",
            Self::Links => "analysis.cross_repo_links",
            Self::Clusters(_) => "analysis.clusters",
            Self::Cluster(_) => "analysis.cluster_detail",
            Self::Status => "analysis.status",
            Self::Fleet => "analysis.fleet_health",
            Self::Archival => "analysis.archival_candidates",
        }
    }

    pub fn frame(
        self,
        backend: &dyn Backend,
        scope: Option<DataScope>,
        views: Option<ViewSelection>,
        repository: Option<&str>,
        cursor: Option<&str>,
    ) -> Vec<u8> {
        frame::with(backend, Negotiate::Latest, || {
            let (scope, views) = (scope?, views?);
            let page = frame::page(25, cursor)?;
            let filters = if let Some(repository) = repository {
                BoundedList(vec![frame::text(&format!("repository:{repository}"))?])
            } else {
                BoundedList(Vec::new())
            };
            macro_rules! input {
                ($kind:ident, $type:ident $(, $key:ident: $value:expr)*) => {
                    a::Input::$kind(a::$type { scope, views, page, filters $(, $key: $value)* })
                };
            }
            Some(OperationInput::Analysis(match self {
                Self::Health => input!(Health, HealthInput, top_hotspots: None, run_id: None),
                Self::Security => input!(Security, SecurityInput, run_id: None),
                Self::Hotspots => input!(Hotspots, HotspotsInput),
                Self::File(path) => {
                    input!(FileHotspot, FileHotspotInput, path: frame::text(&path)?)
                }
                Self::Links => input!(CrossRepoLinks, CrossRepoLinksInput),
                Self::Clusters(graph) => input!(Clusters, ClustersInput, projection: Some(
                    if graph { AnalysisClusterProjection::Graph } else { AnalysisClusterProjection::Overview }
                )),
                Self::Cluster(cluster_id) => input!(ClusterDetail, ClusterDetailInput,
                    cluster_id: cluster_id, legacy_ordinal: None),
                Self::Status => input!(Status, StatusInput, run_id: None),
                Self::Fleet => input!(FleetHealth, FleetHealthInput, max_health: None),
                Self::Archival => input!(ArchivalCandidates, ArchivalCandidatesInput),
            }))
        })
    }

    pub fn repository_filter(&self) -> bool {
        matches!(
            self,
            Self::Health | Self::Security | Self::Hotspots | Self::File(_)
        )
    }
}

#[derive(Clone, Copy)]
pub enum Command {
    Refresh,
    AnalyzeProjects,
    Synthesize,
    Link,
    Communities,
    ProcessBatch,
}

impl Command {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Refresh | Self::AnalyzeProjects => "analysis.reanalyze",
            Self::Synthesize => "analysis.synthesize",
            Self::Link => "analysis.link_and_detect",
            Self::Communities => "analysis.community_analysis",
            Self::ProcessBatch => "analysis.process_batch",
        }
    }

    pub fn frame(
        self,
        backend: &dyn Backend,
        scope: Option<DataScope>,
        views: Option<ViewSelection>,
        revision: i64,
    ) -> Vec<u8> {
        frame::with(backend, Negotiate::Latest, || {
            let (scope, views) = (scope?, views?);
            let expected_source_revision = Revision::try_from(revision.to_string()).ok()?;
            let node_ids = BoundedList(Vec::new());
            let batch_limit = Limit::new(32)?;
            let resume_run_id = None;
            macro_rules! input {
                ($kind:ident, $type:ident $(, $key:ident: $value:expr)*) => {
                    a::Input::$kind(a::$type {
                        scope, views, expected_source_revision, node_ids, batch_limit,
                        resume_run_id $(, $key: $value)*
                    })
                };
            }
            Some(OperationInput::Analysis(match self {
                Self::Refresh | Self::AnalyzeProjects => input!(Reanalyze, ReanalyzeInput,
                    refresh_mode: Some(if matches!(self, Self::Refresh) {
                        AnalysisRefreshMode::VitalsOnly
                    } else {
                        AnalysisRefreshMode::WithProjectAnalysis
                    }),
                    include_all_source: None),
                Self::Synthesize => input!(Synthesize, SynthesizeInput),
                Self::Link => input!(LinkAndDetect, LinkAndDetectInput,
                    link_mode: Some(AnalysisLinkMode::LinksAndCommunities)),
                Self::Communities => input!(CommunityAnalysis, CommunityAnalysisInput),
                Self::ProcessBatch => input!(ProcessBatch, ProcessBatchInput),
            }))
        })
    }
}

#[derive(Clone)]
pub enum Action {
    Read(Query, Option<String>),
    Command(Command),
    OpenFile {
        repository: Id,
        view: Id,
        path: RelativePath,
    },
    OpenRepository(Id),
    OpenEndpoint {
        repository: Id,
        view: Id,
        node: Id,
    },
    OpenWork(Id),
    Evidence(ArtifactRef),
}

#[derive(Default)]
pub struct AnalysisScreen {
    pub generation: u64,
    pub query: Option<Query>,
    pub result: Option<OperationResult>,
    pub revision: Option<i64>,
    pub loading: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub evidence: Option<String>,
}

impl AnalysisScreen {
    pub fn invalidate(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ..Self::default()
        };
    }

    pub fn ui(&self, ui: &mut Ui, online: bool, commands: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let enabled = online && !self.loading;
        let fleet = matches!(self.query, Some(Query::Fleet | Query::Archival));
        let tabs = if fleet {
            vec![
                ("Health", Query::Fleet),
                ("Archival candidates", Query::Archival),
            ]
        } else {
            vec![
                ("Health", Query::Health),
                ("Security", Query::Security),
                ("Hotspots", Query::Hotspots),
                ("Links", Query::Links),
                ("Clusters", Query::Clusters(false)),
                ("Cluster graph", Query::Clusters(true)),
                ("Runs", Query::Status),
            ]
        };
        ui.horizontal_wrapped(|ui| {
            for (label, query) in tabs {
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::selectable(self.query.as_ref() == Some(&query), label),
                    )
                    .clicked()
                {
                    actions.push(Action::Read(query, None));
                }
            }
            if let Some(query) = &self.query
                && ui
                    .add_enabled(enabled, egui::Button::new("Reload report"))
                    .clicked()
            {
                actions.push(Action::Read(query.clone(), None));
            }
        });
        if commands && self.query == Some(Query::Status) {
            ui.horizontal_wrapped(|ui| {
                for (label, command) in [
                    ("Refresh measurements", Command::Refresh),
                    ("Resolve links and clusters", Command::Link),
                    ("Detect clusters", Command::Communities),
                    ("Check saved requests", Command::ProcessBatch),
                    ("Analyze projects with a model", Command::AnalyzeProjects),
                    ("Synthesize with a model", Command::Synthesize),
                ] {
                    if ui
                        .add_enabled(enabled && self.revision.is_some(), egui::Button::new(label))
                        .clicked()
                    {
                        actions.push(Action::Command(command));
                    }
                }
            });
        }
        if self.loading {
            ui.spinner();
        }
        if let Some(error) = &self.error {
            widgets::chip(ui, error, Tone::Attention);
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
        let next = match &self.result {
            Some(
                OperationResult::AnalysisHealth { value }
                | OperationResult::AnalysisSecurity { value }
                | OperationResult::AnalysisStatus { value },
            ) => {
                if let Some(report) = &value.report
                    && ui
                        .add_enabled(enabled, egui::Button::new("Read report evidence"))
                        .clicked()
                {
                    actions.push(Action::Evidence(report.clone()));
                }
                value.observation.as_ref().and_then(|data| {
                    observation(ui, data, &mut actions);
                    if self.query == Some(Query::Health) {
                        trend(ui, data);
                    }
                    data.next_cursor.as_ref()
                })
            }
            Some(
                OperationResult::AnalysisCrossRepoLinks { value }
                | OperationResult::AnalysisHotspots { value }
                | OperationResult::AnalysisFileHotspot { value }
                | OperationResult::AnalysisFleetHealth { value }
                | OperationResult::AnalysisArchivalCandidates { value },
            ) => {
                if let AnalysisMetricPageResult::Current(page) = value {
                    metrics(ui, page, &mut actions, fleet);
                    page.next_cursor.as_ref()
                } else {
                    None
                }
            }
            Some(OperationResult::AnalysisClusters {
                value: AnalysisClusterPageResult::Current(page),
            }) => {
                observation(ui, &page.observation, &mut actions);
                if let Some(modularity) = &page.modularity {
                    ui.label(format!("Modularity {:.3}", modularity.get()));
                }
                for item in &page.items.0 {
                    if ui.link(&item.cluster.name.0).clicked() {
                        actions.push(Action::Read(Query::Cluster(item.cluster.id.clone()), None));
                    }
                }
                if let Some(repos) = &page.repositories {
                    for member in &repos.0 {
                        if ui.link(&member.name.0).clicked() {
                            actions.push(Action::OpenRepository(member.repository_id.clone()));
                        }
                    }
                }
                if let Some(edges) = &page.graph_edges {
                    for edge in &edges.0 {
                        ui.horizontal_wrapped(|ui| {
                            for repository in
                                [&edge.source_repository_id, &edge.target_repository_id]
                            {
                                if ui.link(repository.as_str()).clicked() {
                                    actions.push(Action::OpenRepository(repository.clone()));
                                }
                            }
                            ui.label(format!("{} relationships", edge.edge_count.get()));
                            if edge.cross_cluster {
                                ui.label("Across clusters");
                            }
                            for count in &edge.relationship_counts.0 {
                                widgets::caption(
                                    ui,
                                    format!("{} {}", count.count.get(), count.relationship.0),
                                );
                            }
                        });
                    }
                }
                if let Some(links) = &page.links {
                    for link in &links.0 {
                        link_row(ui, link, &mut actions);
                    }
                }
                page.next_cursor.as_ref()
            }
            Some(OperationResult::AnalysisClusterDetail { value }) => {
                ui.heading(&value.name.0);
                value.details.as_ref().and_then(|detail| {
                    ui.label(format!(
                        "Density {:.3} · {} internal edges · {} external edges",
                        detail.density.get(),
                        detail.internal_edges.get(),
                        detail.external_edges.get()
                    ));
                    for member in &detail.members.0 {
                        if ui.link(&member.name.0).clicked() {
                            actions.push(Action::OpenRepository(member.repository_id.clone()));
                        }
                        ui.label(format!("Centrality {:.3}", member.centrality.get()));
                    }
                    for id in &detail.bridges.0 {
                        if ui.link(format!("Bridge {}", id.as_str())).clicked() {
                            actions.push(Action::OpenRepository(id.clone()));
                        }
                    }
                    for connection in &detail.connections.0 {
                        if ui
                            .link(format!(
                                "Connected cluster · {} edges",
                                connection.edge_count.get()
                            ))
                            .clicked()
                        {
                            actions.push(Action::Read(
                                Query::Cluster(connection.cluster_id.clone()),
                                None,
                            ));
                        }
                    }
                    detail.next_cursor.as_ref()
                })
            }
            _ => None,
        };
        if let (Some(query), Some(cursor)) = (&self.query, next)
            && ui
                .add_enabled(enabled, egui::Button::new("Next report page"))
                .clicked()
        {
            actions.push(Action::Read(query.clone(), Some(cursor.0.clone())));
        }
        if let Some(evidence) = &self.evidence {
            ui.collapsing("Report evidence", |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(evidence).monospace()).selectable(true),
                );
            });
        }
        actions
    }
}

fn observation(ui: &mut Ui, data: &AnalysisObservation, actions: &mut Vec<Action>) {
    let (status, tone) = match data.availability {
        AnalysisAvailability::Available => ("Available", Tone::Good),
        AnalysisAvailability::NoData => ("No saved report", Tone::Neutral),
        AnalysisAvailability::Pending => ("Pending", Tone::Neutral),
        AnalysisAvailability::Stale => ("Stale report", Tone::Attention),
        AnalysisAvailability::Partial => ("Partial coverage", Tone::Attention),
        AnalysisAvailability::Unavailable => ("Unavailable", Tone::Attention),
        AnalysisAvailability::Failed => ("Failed", Tone::Attention),
    };
    widgets::chip(ui, status, tone);
    if let Some(computed) = &data.computed_at_ms {
        widgets::caption(
            ui,
            format!("Computed {}", widgets::local_time(computed.get())),
        );
    }
    if let [binding] = data.bindings.0.as_slice() {
        widgets::caption(ui, format!("Captured commit {}", binding.snapshot.head.hex));
    }
    for gap in &data.diagnostics.0 {
        ui.label(&gap.0);
    }
    if let Some(phases) = &data.phases {
        for phase in &phases.0 {
            ui.label(format!("{} · {:?}", phase.phase.0, phase.state));
        }
    }
    if let Some(health) = &data.health {
        for repo in &health.0 {
            let subject = repo
                .dotnet_project
                .as_ref()
                .map_or_else(|| repo.repository_id.as_str(), |project| &project.0);
            ui.label(format!(
                "{subject} · base health {:.2} · {} files · {} hotspots · {} alerts",
                repo.base_score.get(),
                repo.files.get(),
                repo.hotspots.get(),
                repo.alerts.get()
            ));
            if let Some(score) = &repo.adjusted_score {
                widgets::caption(ui, format!("Adjusted health {:.2}", score.get()));
            }
        }
    }
    if let Some(security) = &data.security {
        for repo in &security.0 {
            ui.label(format!(
                "{} · security {:.2} · {} files",
                repo.repository_id.as_str(),
                repo.score.get(),
                repo.scanned_files.get()
            ));
            widgets::caption(
                ui,
                format!(
                    "{} critical · {} high · {} medium · {} low",
                    repo.critical.get(),
                    repo.high.get(),
                    repo.medium.get(),
                    repo.low.get()
                ),
            );
            if !repo.complete {
                widgets::caption(ui, "Security coverage is partial.");
            }
        }
    }
    if let Some(hotspots) = &data.top_hotspots {
        for item in &hotspots.0 {
            metric_row(ui, item, actions, false);
        }
    }
    if let Some(reports) = &data.narratives {
        for report in &reports.0 {
            ui.collapsing(
                report
                    .project
                    .as_ref()
                    .map_or("Repository synthesis", |project| &project.0),
                |ui| {
                    widgets::caption(
                        ui,
                        format!(
                            "{} · {} confidence · {}",
                            report.model.0,
                            report.confidence.0,
                            widgets::local_time(report.computed_at_ms.get())
                        ),
                    );
                    ui.label(&report.summary.0);
                    if ui.button("Read model evidence").clicked() {
                        actions.push(Action::Evidence(report.evidence.clone()));
                    }
                },
            );
        }
    }
    if let Some(work) = &data.related_work_ids {
        for id in &work.0 {
            if ui.link(format!("Related work {}", id.as_str())).clicked() {
                actions.push(Action::OpenWork(id.clone()));
            }
        }
    }
}

fn trend(ui: &mut Ui, data: &AnalysisObservation) {
    ui.heading("Health history");
    let points = data.trends.as_ref().map_or(&[][..], |points| &points.0);
    if points.len() < 2 {
        widgets::caption(ui, "Insufficient history");
    }
    if let [first, .., last] = points {
        let first_time = first.computed_at_ms.get();
        let duration = (last.computed_at_ms.get() - first_time).max(1) as f64;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().min(480.0), 116.0),
            egui::Sense::hover(),
        );
        let plot = rect.shrink2(egui::vec2(20.0, 12.0));
        let painter = ui.painter();
        let axis = ui.visuals().weak_text_color();
        for score in [0.0, 5.0, 10.0] {
            let y = egui::lerp(plot.bottom()..=plot.top(), score / 10.0);
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                egui::Stroke::new(0.5, axis),
            );
            painter.text(
                egui::pos2(plot.left() - 4.0, y),
                egui::Align2::RIGHT_CENTER,
                format!("{score:.0}"),
                egui::FontId::proportional(11.0),
                axis,
            );
        }
        let position = |point: &devworks_core::protocol::records::AnalysisTrendPoint,
                        value: f64| {
            let x = ((point.computed_at_ms.get() - first_time) as f64 / duration) as f32;
            let y = (value.clamp(0.0, 10.0) / 10.0) as f32;
            egui::pos2(
                egui::lerp(plot.left()..=plot.right(), x),
                egui::lerp(plot.bottom()..=plot.top(), y),
            )
        };
        for pair in points.windows(2) {
            painter.line_segment(
                [
                    position(&pair[0], pair[0].base_score.get()),
                    position(&pair[1], pair[1].base_score.get()),
                ],
                egui::Stroke::new(2.0, crate::theme::ACCENT),
            );
            if let (Some(a), Some(b)) = (&pair[0].adjusted_score, &pair[1].adjusted_score) {
                painter.line_segment(
                    [position(&pair[0], a.get()), position(&pair[1], b.get())],
                    egui::Stroke::new(2.0, crate::theme::GREEN),
                );
            }
        }
        for point in points {
            painter.circle_filled(
                position(point, point.base_score.get()),
                2.5,
                crate::theme::ACCENT,
            );
            if let Some(adjusted) = &point.adjusted_score {
                painter.circle_filled(position(point, adjusted.get()), 2.5, crate::theme::GREEN);
            }
        }
        ui.horizontal(|ui| {
            ui.colored_label(crate::theme::ACCENT, "Base health");
            ui.colored_label(crate::theme::GREEN, "Adjusted health");
        });
    }
    for point in points {
        let adjusted = point.adjusted_score.as_ref().map_or_else(
            || "adjusted score unavailable".into(),
            |value| format!("adjusted {:.2}", value.get()),
        );
        ui.label(format!(
            "{} · {} · base {:.2} · {adjusted}",
            widgets::local_time(point.computed_at_ms.get()),
            point.view.snapshot.head.hex,
            point.base_score.get()
        ));
    }
}

fn metrics(ui: &mut Ui, page: &AnalysisMetricPage, actions: &mut Vec<Action>, fleet: bool) {
    observation(ui, &page.observation, actions);
    if let Some(totals) = &page.fleet {
        ui.label(format!(
            "{} measured repositories · {} hotspots · {} alerts",
            totals.repository_count.get(),
            totals.hotspot_count.get(),
            totals.alert_count.get()
        ));
    }
    if let Some(missing) = &page.observation.missing_repositories {
        for repository in &missing.0 {
            ui.horizontal_wrapped(|ui| {
                if ui.link(repository.as_str()).clicked() {
                    actions.push(Action::OpenRepository(repository.clone()));
                }
                ui.label("No measurements for this view");
            });
        }
    }
    if page.found == Some(false) {
        ui.label("No saved measurements for this file.");
    }
    for item in &page.items.0 {
        metric_row(ui, item, actions, fleet);
    }
}

fn metric_row(ui: &mut Ui, item: &AnalysisMetricItem, actions: &mut Vec<Action>, fleet: bool) {
    if let Some(link) = &item.link {
        return link_row(ui, link, actions);
    }
    ui.horizontal_wrapped(|ui| {
        if let Some(path) = &item.path {
            if ui.link(&path.0).clicked() {
                actions.push(Action::OpenFile {
                    repository: item.repository_id.clone(),
                    view: item.view_id.clone(),
                    path: path.clone(),
                });
            }
        } else if fleet && ui.link(item.repository_id.as_str()).clicked() {
            actions.push(Action::OpenRepository(item.repository_id.clone()));
        }
        ui.label(format!(
            "{} {:.2}",
            item.metric.name.0,
            item.metric.value.get()
        ));
        if let Some(file) = &item.file_metrics {
            widgets::caption(
                ui,
                format!(
                    "Health {:.2} · risk {:.2} · complexity {}",
                    file.health.get(),
                    file.risk.get(),
                    file.complexity.get()
                ),
            );
        }
    });
}

fn link_row(
    ui: &mut Ui,
    link: &devworks_core::protocol::records::AnalysisLinkEvidence,
    actions: &mut Vec<Action>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("{} · {}", link.relationship.0, link.resolution.0));
        if ui.link(link.source.repository_id.as_str()).clicked() {
            actions.push(Action::OpenEndpoint {
                repository: link.source.repository_id.clone(),
                view: link.source.view_id.clone(),
                node: link.source.node_id.clone(),
            });
        }
        if let Some(target) = &link.target {
            if ui.link(target.repository_id.as_str()).clicked() {
                actions.push(Action::OpenEndpoint {
                    repository: target.repository_id.clone(),
                    view: target.view_id.clone(),
                    node: target.node_id.clone(),
                });
            }
        } else if let Some(label) = &link.target_label {
            ui.label(&label.0);
        }
        widgets::caption(
            ui,
            format!(
                "{} · confidence {:.2}",
                link.matched_by.0,
                link.confidence.get()
            ),
        );
    });
}
