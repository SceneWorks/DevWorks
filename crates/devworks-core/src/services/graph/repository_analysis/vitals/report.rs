//! Project and file vitals from their authorized composed units.
use super::super::super::{budget::Budget, model, modules::Modules};
use super::super::{
    capture::CapturedBytes,
    selection::{self, Selection},
    work::Claim,
};
use super::{
    complexity,
    history::{self, HistoryMetrics, Vitality},
    scores,
};
use crate::ports::storage::*;
use crate::protocol::Id;
use crate::protocol::records::{
    AnalysisFileMetrics, AnalysisHealthSummary, AnalysisMetricItem, MetricRecord,
};
use crate::protocol::support::BoundedText;
use crate::services::artifacts::ArtifactService;
use crate::services::git::{
    client::{GitFault, GitHistory},
    helper::HelperFault,
};
use crate::services::indexing::{
    IndexEnv,
    views::{self, ViewLayers},
    worker,
};
use crate::services::kernel::{Failure, counter};
use jiff::{Timestamp, ToSpan, tz::TimeZone};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_GIT_COMMITS: usize = 10_000;
const PAGE: u16 = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
enum HistoryCoverage {
    Exact,
    BaseOnly,
    NoHistory,
    Unavailable,
}
impl HistoryCoverage {
    fn key(self) -> &'static str {
        match self {
            Self::Exact => "Exact",
            Self::BaseOnly => "BaseOnly",
            Self::NoHistory => "NoHistory",
            Self::Unavailable => "Unavailable",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::services::graph::repository_analysis) struct FileMetric {
    pub repository_id: Id,
    pub view_id: Id,
    pub node_id: Id,
    pub path: String,
    pub dotnet_project: Option<String>,
    pub language: String,
    pub language_indexed: bool,
    pub namespace: Option<String>,
    pub score: scores::Scores,
    pub role: String,
    complexity: scores::Complexity,
    complexity_score: u32,
    history_coverage: HistoryCoverage,
    current_path_history_source: Option<String>,
    churn: scores::Churn,
    coupling: scores::Coupling,
    knowledge: scores::Knowledge,
    lint_errors: Option<usize>,
    lint_warnings: Option<usize>,
}

impl FileMetric {
    pub(in crate::services::graph::repository_analysis) fn wire(
        &self,
        name: &str,
        value: f64,
        unit: &str,
        computed_at: i64,
    ) -> Result<AnalysisMetricItem, Failure> {
        Ok(AnalysisMetricItem {
            metric: MetricRecord {
                name: model::text(name)?,
                value: model::wire(serde_json::json!(value))?,
                unit: model::text(unit)?,
                source_id: self.node_id.clone(),
                inferred: false,
            },
            repository_id: self.repository_id.clone(),
            view_id: self.view_id.clone(),
            path: Some(model::text(&self.path)?),
            computed_at_ms: Some(counter(computed_at)),
            namespace: self.namespace.as_deref().map(model::text).transpose()?,
            language: Some(model::text(&self.language)?),
            language_indexed: Some(self.language_indexed),
            history_coverage: Some(BoundedText(self.history_coverage.key().into())),
            link: None,
            file_metrics: Some(self.details()?),
        })
    }
    fn details(&self) -> Result<AnalysisFileMetrics, Failure> {
        let number = |value: f64| model::wire(serde_json::json!(value));
        Ok(AnalysisFileMetrics {
            health: number(self.score.health)?,
            risk: number(self.score.risk)?,
            concern: number(self.score.concern)?,
            trust: number(self.score.trust)?,
            complexity: counter(i64::from(self.complexity_score)),
            max_nesting: model::count(self.complexity.nesting as u64)?,
            deep_lines: model::count(self.complexity.deep_lines as u64)?,
            function_count: model::count(self.complexity.functions as u64)?,
            longest_function: model::count(self.complexity.longest_function as u64)?,
            branches: model::count(self.complexity.branches as u64)?,
            lines_added_90d: model::count(self.churn.lines_added)?,
            lines_removed_90d: model::count(self.churn.lines_removed)?,
            changes_90d: model::count(self.churn.changes as u64)?,
            churn_30d: number(self.churn.weighted_30)?,
            churn_90d: number(self.churn.weighted_90)?,
            churn_365d: number(self.churn.weighted_365)?,
            last_change_at_ms: self.churn.last_change_ms.map(counter),
            bug_fix_ratio: number(self.score.bug_fix_ratio)?,
            recurring_churn: number(self.score.recurring_churn)?,
            coupling_strength: number(self.coupling.strength)?,
            coupling_partners: model::count(self.coupling.partners as u64)?,
            truck_factor: model::count(self.knowledge.truck_factor as u64)?,
            lint_errors: self.lint_errors.map(|errors| counter(errors as i64)),
            lint_warnings: self.lint_warnings.map(|warnings| counter(warnings as i64)),
            role: model::text(&self.role)?,
            dotnet_project: self
                .dotnet_project
                .as_deref()
                .map(model::text)
                .transpose()?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::services::graph::repository_analysis) struct RepositoryVitals {
    pub repository: Id,
    pub view: Id,
    pub health: scores::Health,
    pub projects: BTreeMap<String, scores::Health>,
    pub files: Vec<FileMetric>,
    pub vitality: Option<Vitality>,
    pub gaps: Vec<String>,
}

impl RepositoryVitals {
    pub(in crate::services::graph::repository_analysis) fn summary(
        &self,
        project: Option<(&str, &scores::Health)>,
        security: Option<f64>,
    ) -> Result<AnalysisHealthSummary, Failure> {
        let health = project.map_or(&self.health, |(_, health)| health);
        let adjusted = security
            .filter(|_| {
                self.gaps
                    .iter()
                    .all(|gap| gap == "LintMeasurementsUnavailable")
            })
            .map(|security| scores::round(health.base * 0.8 + security * 0.2, 10.0));
        Ok(AnalysisHealthSummary {
            repository_id: self.repository.clone(),
            view_id: self.view.clone(),
            dotnet_project: project.map(|(name, _)| model::text(name)).transpose()?,
            base_score: model::wire(serde_json::json!(health.base))?,
            adjusted_score: adjusted
                .map(|value| model::wire(serde_json::json!(value)))
                .transpose()?,
            files: model::count(health.files as u64)?,
            hotspots: model::count(health.hotspots as u64)?,
            alerts: model::count(health.alerts as u64)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::services::graph::repository_analysis) struct VitalsReport {
    pub computed_at: i64,
    pub repositories: Vec<RepositoryVitals>,
    pub gaps: Vec<String>,
}

pub(in crate::services::graph::repository_analysis) struct HistoryInput {
    location: String,
    oid: String,
}
pub(in crate::services::graph::repository_analysis) fn history_inputs(
    read: &dyn ReadSnapshot,
    selected: &Selection,
) -> Result<BTreeMap<String, HistoryInput>, Failure> {
    let mut inputs = BTreeMap::new();
    for binding in &selected.views {
        let repository = &binding.repository_id;
        let repo = read
            .projects()
            .repository(repository)?
            .ok_or_else(selection::unavailable)?;
        let location = read
            .indexing()
            .repository_source(repository)?
            .map_or(repo.location, |source| source.location);
        inputs.insert(
            repository.as_str().to_owned(),
            HistoryInput {
                location,
                oid: binding.snapshot.head.hex.clone(),
            },
        );
    }
    Ok(inputs)
}

#[derive(Default)]
pub(in crate::services::graph::repository_analysis) struct HistoryGate(Mutex<BTreeSet<String>>);

struct HistoryLease<'a> {
    gate: &'a HistoryGate,
    repository: &'a str,
}

impl Drop for HistoryLease<'_> {
    fn drop(&mut self) {
        self.gate
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(self.repository);
    }
}

impl HistoryGate {
    fn wait<'a>(
        &'a self,
        repository: &'a str,
        stopped: &dyn Fn() -> bool,
    ) -> Option<HistoryLease<'a>> {
        while !stopped() {
            if self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(repository.to_owned())
            {
                return Some(HistoryLease {
                    gate: self,
                    repository,
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

pub(in crate::services::graph::repository_analysis) fn history_reads(
    claim: &Claim,
    index: &IndexEnv,
    inputs: BTreeMap<String, HistoryInput>,
    gate: &HistoryGate,
    now_ms: i64,
    stopped: &(dyn Fn() -> bool + Sync),
) -> Result<BTreeMap<String, Result<GitHistory, &'static str>>, Failure> {
    let now = Timestamp::from_millisecond(now_ms).map_err(|_| selection::unavailable())?;
    let since = now
        .to_zoned(TimeZone::UTC)
        .checked_sub(2.years())
        .map_err(|_| selection::unavailable())?
        .timestamp()
        .as_second();
    let started = Instant::now();
    let stop = || stopped() || started.elapsed() >= Duration::from_secs(30);
    let mut git = worker::git_for(index, &claim.context, &stop);
    git.deadline = git.deadline.min(Duration::from_secs(30));
    let mut observed = BTreeMap::new();
    for (repository, input) in inputs {
        let history = if let Some(_lease) = gate.wait(&repository, &stop) {
            git.history(
                Path::new(&input.location),
                &input.oid,
                since,
                MAX_GIT_COMMITS,
            )
        } else {
            Err(GitFault::Helper(HelperFault::Cancelled))
        };
        let result = match history {
            Ok(history) => Ok(history),
            Err(GitFault::Helper(HelperFault::Cancelled)) if stopped() => {
                return Err(StorageError::Cancelled.into());
            }
            Err(GitFault::Helper(HelperFault::TimedOut | HelperFault::Cancelled)) => {
                Err("GitHistoryDeadline")
            }
            Err(GitFault::Helper(HelperFault::OutputTooLarge)) => Err("GitHistoryByteLimit"),
            Err(GitFault::Helper(HelperFault::Refused(_))) => Err("GitHistoryDenied"),
            Err(GitFault::Helper(HelperFault::Failed { .. })) => Err("GitHistoryObjectUnavailable"),
            Err(_) => Err("GitHistoryUnavailable"),
        };
        observed.insert(repository, result);
    }
    Ok(observed)
}

pub(in crate::services::graph::repository_analysis) fn build(
    read: &dyn ReadSnapshot,
    artifacts: &ArtifactService,
    selection: &Selection,
    histories: &BTreeMap<String, Result<GitHistory, &'static str>>,
    now_ms: i64,
    budget: &mut Budget,
) -> Result<VitalsReport, Failure> {
    let now = Timestamp::from_millisecond(now_ms).map_err(|_| selection::unavailable())?;
    let scope = selection.graph_scope();
    let mut repositories = Vec::new();
    let mut gaps: Vec<String> = selection
        .unavailable
        .iter()
        .map(|row| row.reason.clone())
        .collect();
    for (layers, binding) in selection.layers.iter().zip(&scope.views) {
        let history = histories.get(binding.repository_id.as_str());
        let mut known = match history {
            Some(Ok(history)) => HistoryMetrics::build(history, now, budget),
            Some(Err(cause)) => HistoryMetrics {
                gaps: vec![(*cause).to_owned()],
                ..Default::default()
            },
            None => HistoryMetrics {
                gaps: vec!["GitHistoryUnavailable".into()],
                ..Default::default()
            },
        };
        let local = GraphScope {
            views: vec![binding.clone()],
        };
        let modules = Modules::load(read, &local, budget)?;
        let files = files(
            read,
            artifacts,
            layers,
            &local,
            &modules,
            &mut known,
            history.is_some_and(Result::is_ok),
            now_ms,
            budget,
        )?;
        let health = scores::aggregate(files.iter().map(|file| file.score.health));
        let mut project_files = BTreeMap::<String, Vec<f64>>::new();
        for file in &files {
            if let Some(project) = &file.dotnet_project {
                project_files
                    .entry(project.clone())
                    .or_default()
                    .push(file.score.health);
            }
        }
        let projects = project_files
            .into_iter()
            .map(|(project, scores)| (project, scores::aggregate(scores)))
            .collect();
        if files
            .iter()
            .any(|file| file.lint_errors.is_none() && file.lint_warnings.is_none())
        {
            known.gaps.push("LintMeasurementsUnavailable".into());
        }
        for generation in &layers.layers {
            let stored = read
                .indexing()
                .generation(generation)?
                .ok_or_else(selection::unavailable)?;
            let (_, gaps) = crate::services::indexing::status::manifest_coverage(&stored.manifest);
            known.gaps.extend(gaps);
        }
        if let Some(exhausted) = budget.exhausted {
            known.gaps.push(exhausted.into());
        }
        known.gaps.sort();
        known.gaps.dedup();
        gaps.extend(known.gaps.iter().cloned());
        repositories.push(RepositoryVitals {
            repository: binding.repository_id.clone(),
            view: binding.view_id.clone(),
            health,
            projects,
            files,
            vitality: known.vitality,
            gaps: known.gaps,
        });
        if budget.exhausted.is_some() {
            break;
        }
    }
    gaps.sort();
    gaps.dedup();
    Ok(VitalsReport {
        computed_at: now_ms,
        repositories,
        gaps,
    })
}

#[allow(clippy::too_many_arguments)]
fn files(
    read: &dyn ReadSnapshot,
    artifacts: &ArtifactService,
    layers: &ViewLayers,
    scope: &GraphScope,
    modules: &Modules,
    history: &mut HistoryMetrics,
    history_observed: bool,
    now_ms: i64,
    budget: &mut Budget,
) -> Result<Vec<FileMetric>, Failure> {
    let mut after = None;
    let mut metrics = Vec::new();
    let mut source_bytes = CapturedBytes::default();
    let binding = &scope.views[0];
    let committed_layers = ViewLayers {
        view: layers.view.clone(),
        layers: layers
            .layers
            .iter()
            .filter(|layer| Some(*layer) != layers.view.worktree_id.as_ref())
            .cloned()
            .collect(),
    };
    loop {
        let page = read.graph().graph_units(scope, after.as_ref(), PAGE)?;
        for row in page.items {
            if !budget.visit() {
                return Ok(metrics);
            }
            let path = row.unit.unit_key;
            after = Some(GraphPosition {
                repository_id: row.repository_id,
                unit_key: path.clone(),
                item_id: String::new(),
            });
            let Some(extension) = history::extension(&path) else {
                continue;
            };
            if matches!(
                row.unit.payload["mode"].as_str(),
                Some(worker::MODE_LINK | worker::MODE_SUBMODULE | worker::MODE_DIRECTORY)
            ) {
                continue;
            }
            let (unit, source) =
                match source_bytes.source(read, artifacts, layers, &path, budget)? {
                    Ok(source) => source,
                    Err(cause) => {
                        history.gaps.push(cause.into());
                        if cause == "CapturedSourceByteLimit" {
                            return Ok(metrics);
                        }
                        continue;
                    }
                };
            let complexity = complexity::compute(&extension, &source);
            let worktree_change = layers.view.worktree_id.as_ref() == Some(&unit.generation_id);
            let previous = unit.payload["previousPath"].as_str();
            let original = previous.unwrap_or(&path);
            let (history_coverage, key) = if !history_observed {
                (HistoryCoverage::Unavailable, None)
            } else if worktree_change {
                if views::resolve_unit(read.indexing(), &committed_layers, original)?.is_some() {
                    (HistoryCoverage::BaseOnly, Some(original.to_lowercase()))
                } else {
                    (HistoryCoverage::NoHistory, None)
                }
            } else {
                (HistoryCoverage::Exact, Some(path.to_lowercase()))
            };
            let key = key.as_deref().unwrap_or("");
            let churn = history.churn.get(key).cloned().unwrap_or_default();
            let coupling = history.coupling.get(key).cloned().unwrap_or_default();
            let knowledge = history.knowledge.get(key).cloned().unwrap_or_default();
            let identity = GraphNodeIdentity {
                repository_id: binding.repository_id.clone(),
                unit_key: path.clone(),
                node_id: worker::file_node_id(binding.repository_id.as_str(), &path),
            };
            let file = read.graph().graph_node(scope, &identity)?;
            let untrusted = read
                .graph()
                .graph_trust(scope, &identity, &unit.content_digest)?
                .is_some_and(|trust| !trust.trusted);
            let (dotnet, ambiguous) = file
                .as_ref()
                .map(|node| modules.owner(node))
                .unwrap_or((None, false));
            if ambiguous {
                history.gaps.push("AmbiguousDotnetProject".into());
            }
            let properties = file.as_ref().map(|file| &file.node.payload["properties"]);
            let lint = properties.and_then(|props| {
                let errors = props["lintErrors"].as_u64();
                let warnings = props["lintWarnings"].as_u64();
                (errors.is_some() || warnings.is_some()).then(|| scores::Lint {
                    errors: errors.unwrap_or_default() as usize,
                    warnings: warnings.unwrap_or_default() as usize,
                })
            });
            let namespace = properties
                .and_then(|props| props["namespace"].as_str())
                .map(str::to_owned);
            let test = scores::is_test(&path);
            let score = scores::scores(
                &complexity,
                &churn,
                &coupling,
                &knowledge,
                lint.as_ref(),
                scores::FileClassification { untrusted, test },
                now_ms,
            );
            if !budget.retain(
                640 + path.len()
                    + knowledge
                        .top_authors
                        .iter()
                        .map(|a| a.0.len())
                        .sum::<usize>(),
            ) {
                return Ok(metrics);
            }
            metrics.push(FileMetric {
                repository_id: binding.repository_id.clone(),
                view_id: binding.view_id.clone(),
                node_id: model::text(&identity.node_id)?,
                path,
                dotnet_project: dotnet.map(str::to_owned),
                language: unit.payload["language"]
                    .as_str()
                    .unwrap_or(&extension)
                    .to_owned(),
                language_indexed: unit.payload["mode"].as_str() == Some(worker::MODE_STRUCTURAL),
                namespace,
                score,
                role: if test { "test" } else { "core" }.into(),
                complexity_score: complexity.score(),
                complexity,
                history_coverage,
                current_path_history_source: previous.map(str::to_owned),
                churn,
                coupling,
                knowledge,
                lint_errors: lint.as_ref().map(|lint| lint.errors),
                lint_warnings: lint.as_ref().map(|lint| lint.warnings),
            });
        }
        if !page.has_more {
            return Ok(metrics);
        }
    }
}
