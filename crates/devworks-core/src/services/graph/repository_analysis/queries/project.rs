//! Operation-specific projections of one checked report snapshot.
use super::*;
use crate::services::graph::repository_analysis::vitals::FileMetric;
use std::collections::BTreeMap;

pub(super) struct Input<'a> {
    pub ctx: &'a InvocationContext,
    pub read: &'a dyn ReadSnapshot,
    pub plan: &'a ReadPlan,
    pub request: &'a Request<'a>,
    pub filters: &'a Filters,
    pub cursor: &'a CursorBinding,
    pub report: Option<&'a Report<'a>>,
    pub documents: &'a Documents,
    pub parser: Option<&'a dyn crate::ports::indexing::StructuralParser>,
}

pub(super) fn answer(i: Input<'_>) -> Result<OperationResult, Failure> {
    match i.request.kind {
        Kind::Status => status(&i),
        Kind::Health { top } => health(&i, top),
        Kind::Links => links(&i),
        Kind::Clusters { graph } => clusters(&i, graph),
        Kind::Cluster { id, ordinal } => detail(&i, id, ordinal),
        Kind::Security => security(&i),
        Kind::Metrics { project } => file_metrics(&i, project, None, "metrics"),
        Kind::Hotspots => file_metrics(&i, None, None, "hotspots"),
        Kind::File { path } => file_metrics(&i, None, Some(&path.0), "file_hotspot"),
        Kind::Fleet { maximum } => fleet(&i, maximum),
        Kind::Archival => archival(&i),
    }
}

impl Input<'_> {
    fn observe<'a>(&'a self, gaps: &'a [String]) -> Observation<'a> {
        Observation {
            selected: &self.plan.selected,
            run: self.report.map(|report| report.run),
            phase: self.report.map(|report| report.checkpoint),
            gaps,
        }
    }

    fn views(&self) -> BTreeMap<&str, &Id> {
        self.report
            .map_or(&self.plan.selected.views, |report| &report.run.intent.views)
            .iter()
            .map(|view| (view.repository_id.as_str(), &view.view_id))
            .collect()
    }

    fn metric_page(
        &self,
        rows: Vec<AnalysisMetricItem>,
        observation: AnalysisObservation,
        fleet: Option<AnalysisFleetTotals>,
        found: Option<bool>,
    ) -> Result<AnalysisMetricPageResult, Failure> {
        let page = cursor::slice(self.cursor, rows, self.request.page)?;
        Ok(AnalysisMetricPageResult::Current(AnalysisMetricPage {
            items: page.items,
            next_cursor: page.next_cursor,
            has_more: page.has_more,
            observation,
            fleet,
            found,
            language: None,
            language_indexed: None,
        }))
    }
}

fn status(i: &Input<'_>) -> Result<OperationResult, Failure> {
    if i.request.page.cursor.is_some() {
        return Err(StorageError::CursorExpired.into());
    }
    let report = i.plan.status();
    report
        .map(|run| {
            selection::Selection::check_saved(
                i.read,
                i.ctx,
                &i.plan.selected.scope,
                run,
                i.read.stamp().now_ms,
            )
        })
        .transpose()?;
    let observation = Observation {
        selected: &i.plan.selected,
        run: report,
        phase: None,
        gaps: &[],
    };
    let mut reference = None;
    if let Some(run) = report {
        for candidate in run
            .phases
            .iter()
            .rev()
            .filter_map(|phase| phase.report.as_ref())
        {
            if observation::readable(i.read, i.ctx, candidate)? {
                reference = Some(candidate.clone());
                break;
            }
        }
    }
    Ok(OperationResult::AnalysisStatus {
        value: observation.record(
            report.map_or("status", |r| r.intent.command.key()),
            reference,
            observation.wire(),
        )?,
    })
}

fn health(i: &Input<'_>, top: usize) -> Result<OperationResult, Failure> {
    let security = i.documents.security.as_ref();
    let mut gaps = i
        .documents
        .vitals
        .as_ref()
        .map_or(Vec::new(), |report| report.gaps.clone());
    gaps.extend(
        security
            .into_iter()
            .flat_map(|security| security.gaps.clone()),
    );
    if let Some(vitals) = &i.documents.vitals
        && vitals
            .repositories
            .iter()
            .filter(|repo| i.filters.repository(&repo.repository))
            .any(|repo| {
                security
                    .and_then(|report| report.complete_score(&repo.repository, &repo.view))
                    .is_none()
            })
    {
        gaps.push("SecurityUnavailableForView".into());
    }
    gaps.extend(i.documents.measurement_gaps.iter().cloned());
    let observation = i.observe(&gaps);
    let mut value = observation.wire();
    value.trends = Some(BoundedList(i.documents.trends.clone()));
    value.narratives = Some(BoundedList(
        i.documents
            .narratives
            .iter()
            .filter(|report| i.filters.repository(&report.repository_id))
            .cloned()
            .collect(),
    ));
    value.related_work_ids = Some(BoundedList(related_work(i, None)?));
    if let Some(report) = &i.documents.vitals {
        let mut summaries = Vec::new();
        for row in report
            .repositories
            .iter()
            .filter(|repo| i.filters.repository(&repo.repository))
        {
            let security_score =
                security.and_then(|report| report.complete_score(&row.repository, &row.view));
            summaries.push(row.summary(None, security_score)?);
            for (project, score) in &row.projects {
                summaries.push(row.summary(Some((project, score)), security_score)?);
            }
        }
        if summaries
            .iter()
            .filter(|summary| summary.dotnet_project.is_none())
            .count()
            == 1
            && let Some(summary) = summaries
                .iter()
                .find(|summary| summary.dotnet_project.is_none())
        {
            value.base_score = Some(summary.base_score.clone());
            value.adjusted_score = summary.adjusted_score.clone();
        }
        let spots = ranked(report, i.filters, None, None);
        value.top_hotspots = Some(BoundedList(
            spots
                .into_iter()
                .take(top)
                .map(|file| file.wire("Concern", file.score.concern, "score", report.computed_at))
                .collect::<Result<_, _>>()?,
        ));
        let page = cursor::slice(i.cursor, summaries, i.request.page)?;
        value.health = Some(page.items);
        value.next_cursor = page.next_cursor;
        value.has_more = Some(page.has_more);
    }
    if let Some(security) = security {
        value.security = Some(BoundedList(
            security
                .repositories
                .iter()
                .filter(|repo| i.filters.repository(&repo.repository))
                .map(|repo| repo.summary())
                .collect::<Result<_, _>>()?,
        ));
    }
    let reference = i.report.and_then(Report::reference).cloned();
    Ok(OperationResult::AnalysisHealth {
        value: observation.record("health", reference, value)?,
    })
}

fn related_work(i: &Input<'_>, path: Option<&str>) -> Result<Vec<Id>, Failure> {
    let bindings = i.report.map_or(i.plan.selected.views.as_slice(), |report| {
        &report.run.intent.views
    });
    let repositories = bindings
        .iter()
        .filter(|view| i.filters.repository(&view.repository_id))
        .map(|view| view.repository_id.clone())
        .collect::<Vec<_>>();
    if repositories.is_empty() {
        return Ok(Vec::new());
    }
    let mut projects = Vec::new();
    match &i.plan.selected.scope {
        DataScope::Project { project_id } => projects.push(project_id.clone()),
        DataScope::Workspace => {
            for repository in &repositories {
                for project in i.read.projects().repository_projects(repository)? {
                    if !projects.contains(&project) && projects.len() < 16 {
                        projects.push(project);
                    }
                }
            }
        }
    }
    let mut ids = Vec::new();
    for project in projects {
        let work = i.read.work();
        let filter = WorkFilter {
            project,
            selection: WorkSelection::Tasks,
            status_ids: Vec::new(),
            repository_ids: repositories.clone(),
            text: None,
        };
        for candidate in work.works(&filter, None, 16)? {
            match crate::services::work::visible_work(work, i.ctx, &candidate.id) {
                Ok(card) if !ids.contains(&card.id) => {
                    let brief = crate::kernel::work_brief::WorkBrief::of(&card)?;
                    let touches = brief
                        .touches
                        .iter()
                        .filter(|touch| repositories.contains(&touch.repository_id));
                    let has_scoped_touches = touches.clone().next().is_some();
                    let mut relevant = false;
                    for touch in touches {
                        let Some(binding) = bindings.iter().find(|binding| {
                            binding.repository_id == touch.repository_id
                                && touch
                                    .view_id
                                    .as_ref()
                                    .is_none_or(|view| view == &binding.view_id)
                        }) else {
                            continue;
                        };
                        if let Some(path) = path {
                            if touch.path.as_ref().is_some_and(|value| value.0 != path) {
                                continue;
                            }
                            if let Some(node) = &touch.node_id {
                                let graph = GraphScope {
                                    views: vec![GraphViewBinding {
                                        repository_id: binding.repository_id.clone(),
                                        view_id: binding.view_id.clone(),
                                        view_digest: binding.view_digest.as_str().to_owned(),
                                    }],
                                };
                                let node = GraphNodeIdentity {
                                    repository_id: touch.repository_id.clone(),
                                    unit_key: path.to_owned(),
                                    node_id: node.as_str().to_owned(),
                                };
                                match i.read.graph().graph_node(&graph, &node) {
                                    Ok(Some(_)) => {}
                                    Ok(None) | Err(StorageError::NotFound { .. }) => continue,
                                    Err(error) => return Err(error.into()),
                                }
                            } else if touch.path.is_none() {
                                continue;
                            }
                        }
                        relevant = true;
                        break;
                    }
                    if relevant || (!has_scoped_touches && path.is_none()) {
                        ids.push(card.id);
                    }
                }
                Ok(_) | Err(Failure::Refused(_)) => {}
                Err(error) => return Err(error),
            }
            if ids.len() == 16 {
                return Ok(ids);
            }
        }
    }
    Ok(ids)
}

fn security(i: &Input<'_>) -> Result<OperationResult, Failure> {
    let gaps = i
        .documents
        .security
        .as_ref()
        .map_or(&[][..], |report| &report.gaps);
    let observation = i.observe(gaps);
    let mut value = observation.wire();
    if let Some(report) = &i.documents.security {
        let rows = report
            .repositories
            .iter()
            .filter(|repo| i.filters.repository(&repo.repository))
            .map(|repo| repo.summary())
            .collect::<Result<_, _>>()?;
        let page = cursor::slice(i.cursor, rows, i.request.page)?;
        value.security = Some(page.items);
        value.next_cursor = page.next_cursor;
        value.has_more = Some(page.has_more);
    } else {
        cursor::slice::<()>(i.cursor, Vec::new(), i.request.page)?;
    }
    let reference = i.report.and_then(Report::reference).cloned();
    Ok(OperationResult::AnalysisSecurity {
        value: observation.record("security", reference, value)?,
    })
}

fn links(i: &Input<'_>) -> Result<OperationResult, Failure> {
    let selected = i.views();
    let mut rows = Vec::new();
    let mut gaps = &[][..];
    if let Some(report) = &i.documents.links {
        gaps = &report.gaps;
        for link in &report.links {
            let member = i.filters.repository(&link.source.repository_id)
                || link
                    .target
                    .as_ref()
                    .is_some_and(|target| i.filters.repository(&target.repository_id));
            if member && i.filters.relationship(&link.kind) {
                rows.push(link.metric(&selected)?);
            }
        }
    }
    let observation = i.observe(gaps).wire();
    Ok(OperationResult::AnalysisCrossRepoLinks {
        value: i.metric_page(rows, observation, None, None)?,
    })
}

fn ranked<'a>(
    report: &'a VitalsReport,
    filters: &Filters,
    project: Option<&str>,
    path: Option<&str>,
) -> Vec<&'a FileMetric> {
    let mut rows = report
        .repositories
        .iter()
        .filter(|repo| filters.repository(&repo.repository))
        .flat_map(|repo| &repo.files)
        .filter(|row| {
            filters.role(&row.role)
                && path.is_none_or(|path| row.path == path)
                && project.is_none_or(|project| row.dotnet_project.as_deref() == Some(project))
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.score
            .concern
            .total_cmp(&a.score.concern)
            .then_with(|| b.score.risk.total_cmp(&a.score.risk))
            .then_with(|| a.repository_id.as_str().cmp(b.repository_id.as_str()))
            .then_with(|| a.path.cmp(&b.path))
    });
    rows
}

fn file_metrics(
    i: &Input<'_>,
    project: Option<&str>,
    path: Option<&str>,
    mode: &str,
) -> Result<OperationResult, Failure> {
    if path.is_some() {
        let selection = &i.plan.selected;
        let repositories = selection
            .views
            .iter()
            .map(|view| &view.repository_id)
            .chain(
                selection
                    .unavailable
                    .iter()
                    .map(|entry| &entry.repository_id),
            );
        if repositories
            .filter(|repository| i.filters.repository(repository))
            .count()
            != 1
        {
            return Err(invalid("file_hotspot_repository"));
        }
    }
    let gaps = i
        .documents
        .vitals
        .as_ref()
        .map_or(&[][..], |report| &report.gaps);
    let mut observation = i.observe(gaps).wire();
    if path.is_some() {
        observation.related_work_ids = Some(BoundedList(related_work(i, path)?));
    }
    let mut rows = Vec::new();
    if let Some(report) = &i.documents.vitals {
        for file in ranked(report, i.filters, project, path) {
            rows.push(file.wire("Concern", file.score.concern, "score", report.computed_at)?);
        }
    }
    let found = path.map(|_| !rows.is_empty());
    let language = path.and_then(|path| i.parser?.language_of(path));
    let measured_language = rows.first().and_then(|item| item.language.clone());
    let measured_indexed = rows.first().and_then(|item| item.language_indexed);
    let mut value = i.metric_page(rows, observation, None, found)?;
    if let AnalysisMetricPageResult::Current(page) = &mut value
        && path.is_some()
    {
        page.language =
            measured_language.or(language.map(|info| model::text(info.id)).transpose()?);
        page.language_indexed = measured_indexed.or_else(|| {
            i.parser.map(|_| {
                language.is_some_and(|info| {
                    info.mode == crate::ports::indexing::LanguageMode::Structural
                })
            })
        });
    }
    Ok(match mode {
        "metrics" => OperationResult::AnalysisMetrics { value },
        "hotspots" => OperationResult::AnalysisHotspots { value },
        _ => OperationResult::AnalysisFileHotspot { value },
    })
}

fn effective_health(summary: &AnalysisHealthSummary) -> f64 {
    summary
        .adjusted_score
        .as_ref()
        .unwrap_or(&summary.base_score)
        .get()
}

fn fleet(i: &Input<'_>, maximum: Option<&Finite>) -> Result<OperationResult, Failure> {
    let mut missing = i
        .plan
        .selected
        .unavailable
        .iter()
        .filter(|entry| i.filters.repository(&entry.repository_id))
        .map(|entry| entry.repository_id.clone())
        .collect::<Vec<_>>();
    let mut measured = Vec::new();
    let mut newest = None;
    for reading in &i.documents.fleet {
        if !i.filters.repository(&reading.binding.repository_id) {
            continue;
        }
        if let Some((summary, time)) = &reading.result {
            newest = Some(newest.map_or(*time, |at: i64| at.max(*time)));
            if maximum.is_none_or(|max| effective_health(summary) <= max.get()) {
                measured.push((summary, *time));
            }
        } else {
            missing.push(reading.binding.repository_id.clone());
        }
    }
    measured.sort_by(|(a, _), (b, _)| {
        effective_health(a)
            .total_cmp(&effective_health(b))
            .then_with(|| a.repository_id.as_str().cmp(b.repository_id.as_str()))
    });
    let mut gaps = i.documents.measurement_gaps.clone();
    if !missing.is_empty() {
        gaps.push("MissingRepositoryMetrics".into());
    }
    let mut observation = i.observe(&gaps).wire();
    observation.availability = if newest.is_none() {
        AnalysisAvailability::NoData
    } else if gaps.is_empty() {
        AnalysisAvailability::Available
    } else {
        AnalysisAvailability::Partial
    };
    observation.coverage.state = match observation.availability {
        AnalysisAvailability::NoData => crate::protocol::data::CoverageState::Unavailable,
        AnalysisAvailability::Available => crate::protocol::data::CoverageState::Complete,
        _ => crate::protocol::data::CoverageState::Partial,
    };
    observation.computed_at_ms = newest.map(counter);
    observation.missing_repositories = Some(BoundedList(missing));
    let total = measured
        .iter()
        .map(|(summary, _)| effective_health(summary))
        .sum::<f64>();
    let scored = measured.len() as i64;
    let average = (scored > 0)
        .then(|| model::wire(json!(total / scored as f64)))
        .transpose()?;
    let totals = AnalysisFleetTotals {
        repository_count: counter(scored),
        average_health: average,
        hotspot_count: counter(
            measured
                .iter()
                .map(|(summary, _)| summary.hotspots.get())
                .sum(),
        ),
        alert_count: counter(
            measured
                .iter()
                .map(|(summary, _)| summary.alerts.get())
                .sum(),
        ),
        below_hotspot: counter(
            measured
                .iter()
                .filter(|(summary, _)| effective_health(summary) < 4.0)
                .count() as i64,
        ),
        below_alert: counter(
            measured
                .iter()
                .filter(|(summary, _)| effective_health(summary) < 2.5)
                .count() as i64,
        ),
    };
    let rows = measured
        .iter()
        .map(|(summary, time)| {
            repository_metric(
                &summary.repository_id,
                &summary.view_id,
                "Health",
                "score",
                effective_health(summary),
                Some(*time),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let value = i.metric_page(rows, observation, Some(totals), None)?;
    Ok(OperationResult::AnalysisFleetHealth { value })
}

fn archival(i: &Input<'_>) -> Result<OperationResult, Failure> {
    let mut gaps = Vec::new();
    let mut rows = Vec::new();
    if !i.filters.complete_workspace {
        gaps.push("WorkspaceCoverageRequired".into());
    }
    if let Some(report) = &i.documents.links {
        gaps.extend(report.gaps.clone());
        if report.links.iter().any(|link| link.target.is_none()) {
            gaps.push("UnresolvedCrossRepositoryLinks".into());
        }
        if gaps.is_empty() {
            let mut connected = BTreeSet::new();
            for link in &report.links {
                connected.insert(link.source.repository_id.as_str());
                if let Some(target) = &link.target {
                    connected.insert(target.repository_id.as_str());
                }
            }
            let mut candidates = Vec::new();
            for view in &i.plan.selected.views {
                let repository = &view.repository_id;
                if i.filters.repository(repository) && !connected.contains(repository.as_str()) {
                    let row = i
                        .read
                        .projects()
                        .repository(repository)?
                        .ok_or_else(selection::unavailable)?;
                    if row.lifecycle == "Active" {
                        candidates.push((row.name.to_lowercase(), view));
                    }
                }
            }
            candidates.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then_with(|| a.1.repository_id.as_str().cmp(b.1.repository_id.as_str()))
            });
            for (_, view) in candidates {
                rows.push(repository_metric(
                    &view.repository_id,
                    &view.view_id,
                    "Cross-repository dependencies",
                    "count",
                    0.0,
                    None,
                )?);
            }
        }
    }
    let mut observation = i.observe(&gaps).wire();
    if i.filters.workspace_snapshots_stale && i.documents.links.is_some() {
        observation.availability = AnalysisAvailability::Stale;
        observation.coverage.state = crate::protocol::data::CoverageState::Partial;
    }
    Ok(OperationResult::AnalysisArchivalCandidates {
        value: i.metric_page(rows, observation, None, None)?,
    })
}

fn repository_metric(
    repository: &Id,
    view: &Id,
    name: &str,
    unit: &str,
    score: f64,
    computed_at: Option<i64>,
) -> Result<AnalysisMetricItem, Failure> {
    Ok(AnalysisMetricItem {
        metric: MetricRecord {
            name: model::text(name)?,
            value: model::wire(json!(score))?,
            unit: model::text(unit)?,
            source_id: repository.clone(),
            inferred: false,
        },
        repository_id: repository.clone(),
        view_id: view.clone(),
        path: None,
        computed_at_ms: computed_at.map(counter),
        namespace: None,
        language: None,
        language_indexed: None,
        history_coverage: None,
        link: None,
        file_metrics: None,
    })
}

fn clusters(i: &Input<'_>, graph: bool) -> Result<OperationResult, Failure> {
    enum Row {
        Summary(AnalysisClusterItem),
        Repository(AnalysisClusterMember),
        Link(AnalysisLinkEvidence),
        Pair(AnalysisClusterGraphEdge),
    }
    let mut rows = Vec::new();
    let mut gaps = &[][..];
    let (mut modularity, mut repositories, mut clustered) = (None, None, None);
    if let Some(report) = &i.documents.communities {
        modularity = report
            .modularity
            .map(|score| model::wire(json!(score)))
            .transpose()?;
        repositories = Some(counter(report.repositories.len() as i64));
        clustered = Some(counter(
            report
                .repositories
                .iter()
                .filter(|repo| repo.assigned.is_some())
                .count() as i64,
        ));
        gaps = &report.gaps;
        for group in &report.communities {
            if group
                .members
                .iter()
                .any(|&index| i.filters.repository(&report.repositories[index].repository))
            {
                rows.push(Row::Summary(AnalysisClusterItem {
                    cluster: report.cluster(group)?,
                }));
            }
        }
        if graph {
            for member in report
                .repositories
                .iter()
                .filter(|member| i.filters.repository(&member.repository))
            {
                let cluster_id = member
                    .assigned
                    .and_then(|id| report.communities.get(id))
                    .map(|community| community.id.clone());
                rows.push(Row::Repository(member.wire(cluster_id)?));
            }
            let views = i.views();
            let mut pairs = BTreeMap::<(&str, &str), BTreeMap<&str, i64>>::new();
            let members: BTreeMap<_, _> = report
                .repositories
                .iter()
                .map(|member| (member.repository.as_str(), member))
                .collect();
            for link in &report.links {
                if i.filters.relationship(&link.kind)
                    && (i.filters.repository(&link.source.repository_id)
                        || link
                            .target
                            .as_ref()
                            .is_some_and(|target| i.filters.repository(&target.repository_id)))
                {
                    if let Some(target) = &link.target {
                        let count = pairs
                            .entry((
                                link.source.repository_id.as_str(),
                                target.repository_id.as_str(),
                            ))
                            .or_default()
                            .entry(&link.kind)
                            .or_default();
                        *count += 1;
                    } else {
                        rows.push(Row::Link(link.wire(&views)?));
                    }
                }
            }
            for ((source, target), counts) in pairs {
                let from = members.get(source).ok_or_else(selection::unavailable)?;
                let to = members.get(target).ok_or_else(selection::unavailable)?;
                rows.push(Row::Pair(AnalysisClusterGraphEdge {
                    source_repository_id: from.repository.clone(),
                    source_view_id: from.view.clone(),
                    target_repository_id: to.repository.clone(),
                    target_view_id: to.view.clone(),
                    edge_count: counter(counts.values().sum()),
                    relationship_counts: BoundedList(
                        counts
                            .into_iter()
                            .map(|(kind, count)| AnalysisClusterRelationshipCount {
                                relationship: crate::services::kernel::label(kind, "relationship"),
                                count: counter(count),
                            })
                            .collect(),
                    ),
                    cross_cluster: from.assigned.is_none() || from.assigned != to.assigned,
                }));
            }
        }
    }
    let page = cursor::slice(i.cursor, rows, i.request.page)?;
    let (mut items, mut members, mut links, mut graph_edges) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for row in page.items.0 {
        match row {
            Row::Summary(summary) => items.push(summary),
            Row::Repository(repository) => members.push(repository),
            Row::Link(link) => links.push(link),
            Row::Pair(edge) => graph_edges.push(edge),
        }
    }
    Ok(OperationResult::AnalysisClusters {
        value: AnalysisClusterPageResult::Current(AnalysisClusterPage {
            items: BoundedList(items),
            next_cursor: page.next_cursor,
            has_more: page.has_more,
            observation: i.observe(gaps).wire(),
            modularity,
            total_repositories: repositories,
            clustered_repositories: clustered,
            repositories: graph.then_some(BoundedList(members)),
            links: graph.then_some(BoundedList(links)),
            graph_edges: graph.then_some(BoundedList(graph_edges)),
        }),
    })
}

fn detail(i: &Input<'_>, id: &Id, ordinal: Option<i64>) -> Result<OperationResult, Failure> {
    let report = i
        .documents
        .communities
        .as_ref()
        .ok_or_else(selection::unavailable)?;
    if let Some((binding, _)) = id
        .as_str()
        .strip_prefix("community_")
        .and_then(|key| key.split_once('_'))
        && binding != &i.plan.selected.binding_key[..16]
    {
        return Err(selection::unavailable());
    }
    let group = report
        .communities
        .iter()
        .find(|group| &group.id == id)
        .filter(|group| {
            group
                .members
                .iter()
                .any(|&index| i.filters.repository(&report.repositories[index].repository))
        })
        .ok_or_else(|| {
            Refusal::new(
                DomainErrorCode::NotFound,
                "analysis_cluster",
                Some(id.clone()),
            )
        })?;
    if ordinal.is_some_and(|ordinal| group.ordinal as i64 != ordinal) {
        return Err(selection::unavailable());
    }
    let mut record = report.cluster(group)?;
    if let Some(details) = &mut record.details {
        let rows = std::mem::take(&mut details.members.0);
        let page = cursor::slice(i.cursor, rows, i.request.page)?;
        details.members = page.items;
        details.next_cursor = page.next_cursor;
        details.has_more = Some(page.has_more);
    }
    Ok(OperationResult::AnalysisClusterDetail { value: record })
}
