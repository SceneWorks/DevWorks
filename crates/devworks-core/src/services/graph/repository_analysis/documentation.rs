//! Resolve documentation inputs from the exact successful analysis rather than mixing phases
//! from separately selected runs. Rendering consumes only the checked, immutable reports.
use super::{
    links::LinkReport,
    modeling::{ProjectsReport, SynthesisReport, documentation},
    phase_store::PhaseStore,
    read::ReadPlan,
    selection::{self, Selection},
};
use crate::context::{BoundWorktree, InvocationContext, PrincipalKind};
use crate::operations::repository_docs::GenerateInput;
use crate::ports::storage::{
    AnalysisPhase, EventRecord, QueueEntry, ReadSnapshot, StorageError, StoredAnalysisRun,
    WriteTransaction,
};
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::AnalysisViewBinding;
use crate::protocol::support::{BoundedList, BoundedText, Check, RelativePath};
use crate::protocol::{DomainErrorCode, Id, OperationState, Revision};
use crate::services::kernel::{Failure, Refusal, rev};
use std::collections::{BTreeMap, BTreeSet};

pub const DOCS_CONSUMER: &str = "RepositoryAnalysisDocumentation";

pub(crate) fn synthesis_request(
    read: &dyn ReadSnapshot,
    entry: &QueueEntry,
    event: &EventRecord,
) -> Result<(InvocationContext, GenerateInput, Id), Failure> {
    if entry.consumer != DOCS_CONSUMER
        || entry.event_id != event.id
        || event.kind != "RepositoryAnalysisSynthesized"
        || event.payload["schemaVersion"] != 1
    {
        return Err(StorageError::ScopeDenied.into());
    }
    let run_id: Id = serde_json::from_value(event.payload["runId"].clone())
        .map_err(|_| selection::unavailable())?;
    let repository_id: Id = serde_json::from_value(event.payload["repositoryId"].clone())
        .map_err(|_| selection::unavailable())?;
    let result: ArtifactRef = serde_json::from_value(event.payload["result"].clone())
        .map_err(|_| selection::unavailable())?;
    let run = read
        .analysis()
        .analysis_run(&run_id)?
        .ok_or_else(selection::unavailable)?;
    if run.revision != event.aggregate_revision
        || run.owner.actor != event.actor
        || run.owner.root != event.root_id
        || event.operation_id.as_ref() != Some(&run.active_operation)
        || settled_report(&run, AnalysisPhase::Synthesis).as_ref() != Some(&result)
    {
        return Err(selection::unavailable());
    }
    let owner = crate::kernel::analysis::Work {
        run_id: run_id.clone(),
        delivery_id: entry.id.clone(),
        authority: run.owner,
    };
    let context = owner.context(&read.stamp().store_id)?;
    let view = run
        .intent
        .views
        .iter()
        .find(|view| view.repository_id == repository_id)
        .ok_or_else(selection::unavailable)?;
    let (context, tree) = if let Some(bound) = context.worktree() {
        let tree = read
            .git()
            .git_worktree(bound.id())?
            .filter(|tree| tree.repository_id == repository_id)
            .ok_or_else(selection::unavailable)?;
        (context, tree)
    } else {
        let DataScope::Project { project_id } = &run.intent.scope else {
            return Err(selection::unavailable());
        };
        if context.principal_kind() != PrincipalKind::Human
            || context
                .project_id()
                .is_some_and(|current| current != project_id)
            || context.assignment_id().is_some()
        {
            return Err(selection::unavailable());
        }
        let tree = crate::services::git::context::documentation_target(
            read,
            &context,
            project_id,
            &repository_id,
            &view.snapshot.head,
        )?
        .ok_or_else(selection::unavailable)?;
        if context.root_id().is_some_and(|root| root.0 != tree.work_id) {
            return Err(selection::unavailable());
        }
        let scoped = context
            .with_lineage(
                Some(project_id.clone()),
                Some(crate::protocol::RootId(tree.work_id.clone())),
                None,
            )
            .bind_worktree(BoundWorktree::resolved(
                tree.id.clone(),
                repository_id.clone(),
                tree.work_id.clone(),
                tree.ownership_revision,
            ));
        (scoped, tree)
    };
    let views = ViewSelection::Exact {
        view_ids: run
            .intent
            .views
            .into_iter()
            .map(|view| view.view_id)
            .collect(),
    };
    Ok((
        context,
        GenerateInput {
            repository_id,
            views,
            expected_revision: rev(run.revision),
            output_root_id: tree.registered_root_id,
            topics: BoundedList(vec![BoundedText("all".into())]),
            analysis_run_id: Some(run_id.clone()),
            delivery: entry
                .payload
                .get("delivery")
                .map(|value| {
                    serde_json::from_value(value.clone()).map_err(|_| {
                        StorageError::InvalidRecord(
                            "unknown repository documentation delivery mode".into(),
                        )
                    })
                })
                .transpose()?,
        },
        run_id,
    ))
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DocumentationEvidence {
    pub repository: Id,
    pub run: Id,
    pub revision: Revision,
    pub view: AnalysisViewBinding,
    reports: Reports,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Reports {
    synthesis: ArtifactRef,
    projects: ArtifactRef,
    links: Option<ArtifactRef>,
}

pub(super) struct MaterializedReports {
    pub synthesis: SynthesisReport,
    pub projects: ProjectsReport,
    pub links: Option<LinkReport>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DocumentationDraft {
    pub evidence: DocumentationEvidence,
    pub files: BTreeMap<String, String>,
    pub missing_projects: Vec<String>,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub delivery: Option<crate::protocol::records::RepositoryDocsDelivery>,
}

pub(crate) struct DocumentationSourceFacts {
    pub tracked: Vec<String>,
    pub languages: Vec<(String, String)>,
    pub languages_partial: bool,
}

impl DocumentationEvidence {
    pub(super) fn prepare(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        request: &GenerateInput,
        run_id: Option<&Id>,
    ) -> Result<Self, Failure> {
        let scope = ctx
            .project_id()
            .cloned()
            .map_or(DataScope::Workspace, |project_id| DataScope::Project {
                project_id,
            });
        // The synthesis event names the precise retained run, including missing repositories.
        // An Exact request only lists available views and cannot reproduce that selection key.
        if let Some(id) = run_id.or(request.analysis_run_id.as_ref()) {
            let run = read
                .analysis()
                .analysis_run(id)?
                .ok_or_else(selection::unavailable)?;
            let expected = ViewSelection::Exact {
                view_ids: run
                    .intent
                    .views
                    .iter()
                    .map(|view| view.view_id.clone())
                    .collect(),
            };
            if run.revision != request.expected_revision.get()
                || serde_json::to_value(expected).ok() != serde_json::to_value(&request.views).ok()
            {
                return Err(selection::unavailable());
            }
            Selection::check_saved(read, ctx, &scope, &run, read.stamp().now_ms)?;
            return Self::from_run(read, ctx, &run, &request.repository_id);
        }
        let plan = ReadPlan::load(read, ctx, &scope, &request.views, None, false, false)?;
        let mut matches = plan.exact.iter().filter(|run| {
            run.revision == request.expected_revision.get()
                && settled_report(run, AnalysisPhase::Synthesis).is_some()
        });
        let run = matches.next().ok_or_else(selection::unavailable)?;
        if matches.next().is_some() || plan.exact.len() == 64 {
            return Err(Refusal::new(
                DomainErrorCode::Conflict,
                "analysis_revision",
                Some(run.id.clone()),
            )
            .into());
        }
        Selection::check_saved(read, ctx, &scope, run, read.stamp().now_ms)?;
        Self::from_run(read, ctx, run, &request.repository_id)
    }

    pub(super) fn from_run(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        run: &StoredAnalysisRun,
        repository: &Id,
    ) -> Result<Self, Failure> {
        Selection::check_saved(read, ctx, &run.intent.scope, run, read.stamp().now_ms)?;
        let view = run
            .intent
            .views
            .iter()
            .find(|view| &view.repository_id == repository)
            .cloned()
            .ok_or_else(selection::unavailable)?;
        let synthesis =
            settled_report(run, AnalysisPhase::Synthesis).ok_or_else(selection::unavailable)?;
        let predecessor = match &run.intent.prerequisite_run {
            Some(id) => {
                let previous = read
                    .analysis()
                    .analysis_run(id)?
                    .ok_or_else(selection::unavailable)?;
                Selection::check_saved(
                    read,
                    ctx,
                    &run.intent.scope,
                    &previous,
                    read.stamp().now_ms,
                )?;
                if previous.binding_key != run.binding_key
                    || previous.owner.namespace != run.owner.namespace
                {
                    return Err(selection::unavailable());
                }
                Some(previous)
            }
            None => None,
        };
        let project_run = predecessor.as_ref().unwrap_or(run);
        let projects = settled_report(project_run, AnalysisPhase::Projects)
            .ok_or_else(selection::unavailable)?;
        Ok(Self {
            repository: repository.clone(),
            run: run.id.clone(),
            revision: rev(run.revision),
            view,
            reports: Reports {
                synthesis,
                projects,
                links: settled_report(project_run, AnalysisPhase::Links),
            },
        })
    }

    pub(super) fn read(
        &self,
        phases: &PhaseStore,
        ctx: &InvocationContext,
    ) -> Result<MaterializedReports, Failure> {
        let synthesis: SynthesisReport = phases.read(ctx, &self.reports.synthesis)?;
        let projects: ProjectsReport = phases.read(ctx, &self.reports.projects)?;
        let mut selected = synthesis
            .completed
            .iter()
            .filter(|row| row.repository == self.repository && row.view == self.view.view_id);
        let result = selected.next().ok_or_else(selection::unavailable)?;
        if selected.next().is_some() || result.project_checkpoints.is_empty() {
            return Err(selection::unavailable());
        }
        let checkpoint_matches = |checkpoint: &Id| {
            projects
                .completed
                .iter()
                .filter(|row| {
                    &row.checkpoint == checkpoint
                        && row.subject.repository == self.repository
                        && row.subject.view == self.view.view_id
                })
                .count()
                == 1
        };
        if !result.project_checkpoints.iter().all(checkpoint_matches) {
            return Err(selection::unavailable());
        }
        let links = self
            .reports
            .links
            .as_ref()
            .map(|reference| phases.read(ctx, reference))
            .transpose()?;
        Ok(MaterializedReports {
            synthesis,
            projects,
            links,
        })
    }

    pub(crate) fn validate_in(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
    ) -> Result<bool, Failure> {
        let Some(run) = tx.analysis().analysis_run(&self.run)? else {
            return Ok(false);
        };
        if run.revision != self.revision.get()
            || settled_report(&run, AnalysisPhase::Synthesis).as_ref()
                != Some(&self.reports.synthesis)
            || !run.intent.views.iter().any(|view| {
                view.repository_id == self.repository
                    && view.view_id == self.view.view_id
                    && view.view_digest == self.view.view_digest
                    && view.snapshot.head == self.view.snapshot.head
            })
        {
            return Ok(false);
        }
        Selection::validate_saved_authority_in(tx, ctx, &run)?;
        for reference in [&self.reports.synthesis, &self.reports.projects]
            .into_iter()
            .chain(self.reports.links.as_ref())
        {
            if !crate::services::artifacts::exact_reference_readable_in(tx, ctx, reference)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn render(
        self,
        materialized: MaterializedReports,
        repository_name: &str,
        facts: &DocumentationSourceFacts,
        topics: &[crate::protocol::support::Label],
        delivery: Option<crate::protocol::records::RepositoryDocsDelivery>,
    ) -> Result<DocumentationDraft, Failure> {
        let topics: BTreeSet<_> = topics.iter().map(|topic| topic.0.as_str()).collect();
        let all = topics.contains("all");
        if topics
            .iter()
            .any(|topic| !matches!(*topic, "all" | "repository" | "projects"))
        {
            return Err(Refusal::new(
                DomainErrorCode::InvalidArgument,
                "repository_docs_topics",
                None,
            )
            .into());
        }
        let synthesis = materialized
            .synthesis
            .completed
            .iter()
            .find(|row| row.repository == self.repository && row.view == self.view.view_id)
            .ok_or_else(selection::unavailable)?;
        let projects = materialized
            .projects
            .completed
            .iter()
            .filter(|project| {
                synthesis.project_checkpoints.contains(&project.checkpoint)
                    && project.subject.repository == self.repository
                    && project.subject.view == self.view.view_id
            })
            .collect::<Vec<_>>();
        let mut files = BTreeMap::new();
        let mut missing_projects = Vec::new();
        if all || topics.contains("projects") {
            for project in &projects {
                let Some(directory) = project_directory(&facts.tracked, &project.subject) else {
                    missing_projects.push(project.subject.name.clone());
                    continue;
                };
                // The repository document already includes the root project's full report.
                if directory.is_empty() && (all || topics.contains("repository")) {
                    continue;
                }
                let path = if directory.is_empty() {
                    "CODEGRAPH.md".into()
                } else {
                    format!("{directory}/CODEGRAPH.md")
                };
                let source = self.source(
                    materialized.projects.computed_at,
                    !materialized.projects.gaps.is_empty(),
                );
                if files
                    .insert(path, documentation::project(&source, project))
                    .is_some()
                {
                    return Err(Refusal::new(
                        DomainErrorCode::Conflict,
                        "repository_docs_path",
                        None,
                    )
                    .into());
                }
            }
        }
        if all || topics.contains("repository") {
            let links = materialized.links.as_ref();
            let mut dependencies = BTreeSet::new();
            let mut dependents = BTreeSet::new();
            if let Some(links) = links {
                for link in &links.links {
                    if let Some(target) = &link.target
                        && link.source.repository_id != target.repository_id
                    {
                        let description = format!(
                            "{} via {} ({}, confidence {:.0}%) between {} and {}",
                            link.kind,
                            link.matched_by,
                            if link.inferred {
                                "inferred"
                            } else {
                                "resolved"
                            },
                            link.confidence * 100.0,
                            link.source.unit_key,
                            target.unit_key
                        );
                        if link.source.repository_id == self.repository {
                            dependencies.insert((
                                target.repository_id.as_str().to_owned(),
                                description.clone(),
                            ));
                        }
                        if target.repository_id == self.repository {
                            dependents.insert((
                                link.source.repository_id.as_str().to_owned(),
                                description,
                            ));
                        }
                    }
                }
            }
            let source = self.source(
                materialized.synthesis.computed_at,
                !materialized.synthesis.gaps.is_empty()
                    || links.is_none_or(|report| !report.gaps.is_empty())
                    || !missing_projects.is_empty()
                    || facts.languages_partial,
            );
            files.insert(
                "CODEGRAPH.md".into(),
                documentation::repository(
                    repository_name,
                    &source,
                    synthesis,
                    &projects,
                    &facts.languages,
                    &dependencies.into_iter().collect::<Vec<_>>(),
                    &dependents.into_iter().collect::<Vec<_>>(),
                ),
            );
        }
        if files.is_empty() {
            return Err(selection::unavailable());
        }
        let complete = (all || (topics.contains("repository") && topics.contains("projects")))
            && missing_projects.is_empty()
            && !facts.languages_partial
            && materialized.synthesis.gaps.is_empty()
            && materialized.projects.gaps.is_empty()
            && materialized
                .links
                .as_ref()
                .is_some_and(|links| links.gaps.is_empty());
        Ok(DocumentationDraft {
            evidence: self,
            files,
            missing_projects,
            complete,
            delivery,
        })
    }

    fn source(&self, computed_at_ms: i64, partial: bool) -> documentation::DocumentationSource<'_> {
        documentation::DocumentationSource {
            run: &self.run,
            revision: &self.revision.0,
            view: &self.view.view_id,
            view_digest: &self.view.view_digest.0,
            commit: &self.view.snapshot.head.hex,
            computed_at_ms,
            partial,
        }
    }
}

fn project_directory(files: &[String], project: &super::modeling::Subject) -> Option<String> {
    let used = |directory: &str| {
        directory.is_empty()
            || (RelativePath(directory.to_owned()).check().is_ok()
                && project.evidence.iter().any(|node| {
                    node.unit_key
                        .strip_prefix(directory)
                        .is_some_and(|tail| tail.starts_with('/'))
                })
                && files.iter().any(|file| {
                    file.strip_prefix(directory)
                        .is_some_and(|tail| tail.starts_with('/'))
                }))
    };
    if let Some(directory) = &project.project_directory {
        return used(directory).then(|| directory.clone());
    }
    // Preexisting project reports did not store a manifest directory. Accept a name-based
    // candidate only when its own captured graph evidence actually belongs to that path.
    let direct = [project.name.clone(), format!("src/{}", project.name)];
    for directory in direct {
        if used(&directory) {
            return Some(directory);
        }
    }
    let project_file = format!("{}.csproj", project.name);
    let mut candidates = files.iter().filter_map(|file| {
        let (directory, name) = file.rsplit_once('/').unwrap_or(("", file.as_str()));
        (name == project_file && used(directory)).then_some(directory)
    });
    if let Some(selected) = candidates.next() {
        return candidates.next().is_none().then(|| selected.to_owned());
    }
    // When analysis grouped the repository itself, the root document carries that report.
    (project.name == project.repository_name).then(String::new)
}

fn settled_report(run: &StoredAnalysisRun, phase: AnalysisPhase) -> Option<ArtifactRef> {
    run.phases
        .iter()
        .find(|row| row.phase == phase && row.state == OperationState::ObservedComplete)?
        .report
        .clone()
}
