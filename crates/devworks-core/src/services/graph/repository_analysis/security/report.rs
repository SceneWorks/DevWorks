//! Source findings, exact installed advisory versions, and explicit security coverage.
use super::super::super::{budget::Budget, model, scan};
use super::super::{capture::CapturedBytes, selection::Selection};
use super::{
    Finding, Severity,
    advisories::{self, Feed, Package},
    scanner, score,
};
use crate::ports::storage::*;
use crate::protocol::{Id, records::AnalysisSecuritySummary};
use crate::services::artifacts::ArtifactService;
use crate::services::indexing::worker;
use crate::services::kernel::Failure;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

const EXCLUDED: &[&str] = &[
    "bin",
    "obj",
    "node_modules",
    ".git",
    "packages",
    ".vs",
    "dist",
    "wwwroot",
];
const EXTENSIONS: &[&str] = &[
    "cs",
    "ts",
    "tsx",
    "js",
    "jsx",
    "json",
    "xml",
    "config",
    "yaml",
    "yml",
    "env",
    "cfg",
    "ini",
    "properties",
    "csproj",
    "sln",
    "slnx",
    "tf",
    "cfm",
    "cfc",
];
const TEST_PACKAGES: &[&str] = &[
    "microsoft.net.test.sdk",
    "coverlet.collector",
    "xunit",
    "xunit.runner.visualstudio",
    "nunit",
    "nunit3testadapter",
    "mstest.testadapter",
    "mstest.testframework",
    "fluentassertions",
    "shouldly",
    "moq",
    "nsubstitute",
    "fakeiteasy",
];
static TEST_PROJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<IsTestProject>\s*true\s*</IsTestProject>").expect("test project selector")
});

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(in crate::services::graph::repository_analysis) struct RepositorySecurity {
    pub repository: Id,
    pub view: Id,
    pub score: f64,
    pub findings: Vec<Finding>,
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub scanned_files: usize,
    pub advisories: Vec<AdvisoryObservation>,
    pub gaps: Vec<String>,
    #[serde(skip)]
    packages: BTreeSet<Package>,
}

impl RepositorySecurity {
    pub(in crate::services::graph::repository_analysis) fn summary(
        &self,
    ) -> Result<AnalysisSecuritySummary, Failure> {
        Ok(AnalysisSecuritySummary {
            repository_id: self.repository.clone(),
            view_id: self.view.clone(),
            score: model::wire(serde_json::json!(self.score))?,
            complete: self.gaps.is_empty(),
            critical: model::count(self.critical as u64)?,
            high: model::count(self.high as u64)?,
            medium: model::count(self.medium as u64)?,
            low: model::count(self.low as u64)?,
            scanned_files: model::count(self.scanned_files as u64)?,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::services::graph::repository_analysis) struct AdvisoryObservation {
    pub name: String,
    pub version: String,
    pub state: AdvisoryState,
    pub checked_at_ms: Option<i64>,
    pub diagnostic: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(in crate::services::graph::repository_analysis) enum AdvisoryState {
    Current,
    Stale,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::services::graph::repository_analysis) struct SecurityReport {
    pub computed_at: i64,
    pub repositories: Vec<RepositorySecurity>,
    pub gaps: Vec<String>,
}

fn selected(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    !scanner::nonproduction(&path)
        && !path.split('/').any(|part| EXCLUDED.contains(&part))
        && path
            .rsplit_once('.')
            .is_some_and(|(_, ext)| EXTENSIONS.contains(&ext))
}

pub(in crate::services::graph::repository_analysis) fn local(
    read: &dyn ReadSnapshot,
    artifacts: &ArtifactService,
    selection: &Selection,
    now: i64,
    budget: &mut Budget,
) -> Result<SecurityReport, Failure> {
    let graph = selection.graph_scope();
    let mut repositories = Vec::new();
    let mut gaps: Vec<String> = selection
        .unavailable
        .iter()
        .map(|row| row.reason.clone())
        .collect();
    for (layer, binding) in selection.layers.iter().zip(graph.views) {
        let mut repo = RepositorySecurity {
            repository: binding.repository_id.clone(),
            view: binding.view_id.clone(),
            score: 10.0,
            findings: Vec::new(),
            critical: 0,
            high: 0,
            medium: 0,
            low: 0,
            scanned_files: 0,
            advisories: Vec::new(),
            gaps: Vec::new(),
            packages: BTreeSet::new(),
        };
        let scope = GraphScope {
            views: vec![binding],
        };
        let mut project_files = BTreeMap::<String, bool>::new();
        let mut source = CapturedBytes::default();
        let mut after = None;
        'pages: loop {
            let page = read.graph().graph_units(&scope, after.as_ref(), 32)?;
            for row in page.items {
                if !budget.visit() {
                    break 'pages;
                }
                let path = row.unit.unit_key;
                after = Some(GraphPosition {
                    repository_id: row.repository_id,
                    unit_key: path.clone(),
                    item_id: String::new(),
                });
                let project = path.to_ascii_lowercase().ends_with(".csproj");
                if project {
                    project_files.insert(path.clone(), scanner::nonproduction(&path));
                }
                if !selected(&path)
                    || matches!(
                        row.unit.payload["mode"].as_str(),
                        Some(worker::MODE_LINK | worker::MODE_SUBMODULE | worker::MODE_DIRECTORY)
                    )
                {
                    continue;
                }
                let (_, content) = match source.source(read, artifacts, layer, &path, budget)? {
                    Ok(bytes) => bytes,
                    Err(cause) => {
                        repo.gaps.push(cause.to_owned());
                        if project {
                            // A missing declaration cannot prove a project is production.
                            project_files.remove(&path);
                        }
                        if cause == "CapturedSourceByteLimit" {
                            break 'pages;
                        }
                        continue;
                    }
                };
                if project && TEST_PROJECT.is_match(&content) {
                    project_files.insert(path.clone(), true);
                }
                repo.scanned_files += 1;
                scanner::scan(&path, &content, budget, &mut repo.findings);
            }
            if !page.has_more {
                break;
            }
        }
        packages(read, &scope, &project_files, &mut repo, budget)?;
        for id in &layer.layers {
            let generation = read
                .indexing()
                .generation(id)?
                .ok_or_else(super::super::selection::unavailable)?;
            let (_, coverage) =
                crate::services::indexing::status::manifest_coverage(&generation.manifest);
            repo.gaps.extend(coverage);
        }
        if let Some(exhausted) = budget.exhausted {
            repo.gaps.push(exhausted.into());
        }
        repo.gaps.sort();
        repo.gaps.dedup();
        gaps.extend(repo.gaps.iter().cloned());
        repositories.push(repo);
        if budget.exhausted.is_some() {
            break;
        }
    }
    Ok(SecurityReport {
        computed_at: now,
        repositories,
        gaps,
    })
}

fn packages(
    read: &dyn ReadSnapshot,
    scope: &GraphScope,
    projects: &BTreeMap<String, bool>,
    repo: &mut RepositorySecurity,
    budget: &mut Budget,
) -> Result<(), Failure> {
    let mut packages = BTreeMap::<String, (BTreeSet<Package>, bool, bool)>::new();
    scan::nodes(
        read,
        scope,
        &GraphNodeFilter {
            kinds: vec!["NuGetPackage".into()],
            ..Default::default()
        },
        budget,
        |node, budget| {
            let unit = node.node.unit_key;
            let version = node.node.payload["properties"]["version"]
                .as_str()
                .unwrap_or("");
            if !budget.retain(unit.len() + node.node.name.len() + version.len() + 160) {
                return Ok(false);
            }
            let (facts, test, unknown) = packages.entry(unit).or_default();
            let name = node.node.name.to_ascii_lowercase();
            *test |= TEST_PACKAGES.contains(&name.as_str());
            if let Some(package) = Package::exact(&name, version) {
                facts.insert(package);
            } else {
                *unknown = true;
            }
            Ok(true)
        },
    )?;
    for (unit, (facts, test, unknown)) in packages {
        if test || projects.get(&unit) == Some(&true) || scanner::nonproduction(&unit) {
            continue;
        }
        if projects.get(&unit).is_none() && unit.to_ascii_lowercase().ends_with(".csproj") {
            repo.gaps.push("NugetProjectCoverageUnavailable".into());
            continue;
        }
        if unknown {
            repo.gaps.push("NugetExactVersionUnavailable".into());
        }
        repo.packages.extend(facts);
    }
    Ok(())
}

impl SecurityReport {
    pub(in crate::services::graph::repository_analysis) fn settle_advisories(
        &mut self,
        feed: Option<&Feed>,
        allowed: &mut impl FnMut() -> bool,
        budget: &mut advisories::RequestBudget,
    ) {
        for repository in &mut self.repositories {
            for package in std::mem::take(&mut repository.packages) {
                let observation = feed.map_or(
                    advisories::Coverage::Unknown {
                        cause: "AdvisoryFeedUnavailable",
                    },
                    |feed| feed.check(&package, self.computed_at, allowed, budget),
                );
                let (state, checked_at_ms, diagnostic) = match observation {
                    advisories::Coverage::Current {
                        checked_at,
                        findings,
                    } => {
                        repository.findings.extend(findings);
                        (AdvisoryState::Current, Some(checked_at), None)
                    }
                    advisories::Coverage::Stale {
                        checked_at,
                        findings,
                    } => {
                        repository.findings.extend(findings);
                        repository.gaps.push("AdvisoryStale".into());
                        (
                            AdvisoryState::Stale,
                            Some(checked_at),
                            Some("AdvisoryStale".into()),
                        )
                    }
                    advisories::Coverage::Unknown { cause } => {
                        repository.gaps.push(cause.into());
                        (AdvisoryState::Unknown, None, Some(cause.into()))
                    }
                };
                repository.advisories.push(AdvisoryObservation {
                    name: package.name,
                    version: package.version,
                    state,
                    checked_at_ms,
                    diagnostic,
                });
            }
            repository.findings.sort_by(|a, b| {
                a.category
                    .cmp(&b.category)
                    .then_with(|| a.path.cmp(&b.path))
                    .then_with(|| a.line.cmp(&b.line))
                    .then_with(|| a.package.cmp(&b.package))
                    .then_with(|| a.title.cmp(&b.title))
            });
            repository.score = score(&repository.findings);
            for finding in &repository.findings {
                match finding.severity {
                    Severity::Critical => repository.critical += 1,
                    Severity::High => repository.high += 1,
                    Severity::Medium => repository.medium += 1,
                    Severity::Low => repository.low += 1,
                }
            }
            repository.gaps.sort();
            repository.gaps.dedup();
            self.gaps.extend(repository.gaps.iter().cloned());
        }
        self.gaps.sort();
        self.gaps.dedup();
    }

    pub(in crate::services::graph::repository_analysis) fn complete_score(
        &self,
        repository: &Id,
        view: &Id,
    ) -> Option<f64> {
        self.repositories
            .iter()
            .find(|row| row.repository == *repository && row.view == *view && row.gaps.is_empty())
            .map(|row| row.score)
    }
}
