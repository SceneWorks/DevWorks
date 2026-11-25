//! Exact-view measurements across authorized saved compositions.
use super::*;
use crate::services::graph::repository_analysis::commands::scope_key;
use std::collections::BTreeMap;

const MAX_READ_BYTES: i64 = 64 * 1024 * 1024;
const HISTORY_POINTS: usize = 12;

pub(super) struct FleetReading {
    pub binding: AnalysisViewBinding,
    pub result: Option<(AnalysisHealthSummary, i64)>,
}

struct Reports<'a> {
    queries: &'a Queries,
    ctx: &'a InvocationContext,
    remaining: i64,
    clipped: bool,
    refs: &'a mut Vec<ArtifactRef>,
    vitals: BTreeMap<String, VitalsReport>,
    security: BTreeMap<String, SecurityReport>,
}

impl<'a> Reports<'a> {
    fn new(
        queries: &'a Queries,
        ctx: &'a InvocationContext,
        refs: &'a mut Vec<ArtifactRef>,
    ) -> Self {
        let remaining = refs.iter().fold(MAX_READ_BYTES, |left, r| {
            left.saturating_sub(r.byte_length.get())
        });
        Self {
            queries,
            ctx,
            remaining,
            clipped: false,
            refs,
            vitals: BTreeMap::new(),
            security: BTreeMap::new(),
        }
    }

    fn load<T: serde::de::DeserializeOwned>(
        queries: &Queries,
        ctx: &InvocationContext,
        cache: &mut BTreeMap<String, T>,
        reference: &ArtifactRef,
        remaining: &mut i64,
        refs: &mut Vec<ArtifactRef>,
    ) -> Result<bool, Failure> {
        let key = reference.version_id.as_str();
        if cache.contains_key(key) {
            return Ok(true);
        }
        let len = reference.byte_length.get();
        if len > *remaining {
            return Ok(false);
        }
        *remaining -= len;
        cache.insert(key.to_owned(), queries.reports.read(ctx, reference)?);
        if !refs
            .iter()
            .any(|other| other.version_id == reference.version_id)
        {
            refs.push(reference.clone());
        }
        Ok(true)
    }

    fn vitals(&mut self, report: &Report<'_>) -> Result<Option<&VitalsReport>, Failure> {
        let Some(reference) = report.reference() else {
            return Ok(None);
        };
        if !Self::load(
            self.queries,
            self.ctx,
            &mut self.vitals,
            reference,
            &mut self.remaining,
            self.refs,
        )? {
            self.clipped = true;
            return Ok(None);
        }
        Ok(self.vitals.get(reference.version_id.as_str()))
    }

    fn security(
        &mut self,
        plan: &ReadPlan,
        selected: &AnalysisViewBinding,
        target: &AnalysisViewBinding,
        measurement: &Report<'_>,
    ) -> Result<Option<f64>, Failure> {
        let source = scope_key(&measurement.run.intent.scope);
        let same_binding =
            plan.phase_for_binding(AnalysisPhase::Security, &measurement.run.binding_key);
        let companion = same_binding.or_else(|| {
            plan.repository_phases(AnalysisPhase::Security, selected, true)
                .find(|(view, report)| {
                    view.view_id == target.view_id
                        && view.view_digest.as_str() == target.view_digest.as_str()
                        && scope_key(&report.run.intent.scope) == source
                })
                .map(|(_, report)| report)
        });
        let Some(reference) = companion.as_ref().and_then(Report::reference) else {
            return Ok(None);
        };
        if !Self::load(
            self.queries,
            self.ctx,
            &mut self.security,
            reference,
            &mut self.remaining,
            self.refs,
        )? {
            self.clipped = true;
            return Ok(None);
        }
        Ok(self
            .security
            .get(reference.version_id.as_str())
            .and_then(|report| report.complete_score(&target.repository_id, &target.view_id)))
    }
}

pub(super) fn fleet(
    queries: &Queries,
    ctx: &InvocationContext,
    plan: &ReadPlan,
) -> Result<Documents, Failure> {
    let mut docs = Documents::default();
    let mut reports = Reports::new(queries, ctx, &mut docs.references);
    for binding in &plan.selected.views {
        let mut result = None;
        let direct = plan
            .phase(AnalysisPhase::Vitals, false)
            .filter(|report| report.run.binding_key == plan.selected.binding_key);
        let composed = plan
            .repository_phases(AnalysisPhase::Vitals, binding, false)
            .next()
            .map(|(_, report)| report);
        if let Some(report) = direct
            .filter(|report| report.reference().is_some())
            .or(composed)
        {
            let security = reports.security(plan, binding, binding, &report)?;
            if let Some(vitals) = reports.vitals(&report)?
                && let Some(repo) = vitals.repositories.iter().find(|repo| {
                    repo.repository == binding.repository_id && repo.view == binding.view_id
                })
            {
                result = Some((repo.summary(None, security)?, vitals.computed_at));
                docs.measurement_gaps.extend(repo.gaps.iter().cloned());
            }
        }
        docs.fleet.push(FleetReading {
            binding: binding.clone(),
            result,
        });
    }
    if reports.clipped {
        docs.measurement_gaps.push("ReportReadBudget".into());
    }
    if plan.repository_search_clipped {
        docs.measurement_gaps.push("BoundedReportSearch".into());
    }
    Ok(docs)
}

pub(super) fn trends(
    queries: &Queries,
    ctx: &InvocationContext,
    request: &Request<'_>,
    plan: &ReadPlan,
    docs: &mut Documents,
) -> Result<(), Failure> {
    if request.run_id.is_some() {
        return Ok(());
    }
    let mut reports = Reports::new(queries, ctx, &mut docs.references);
    for binding in plan
        .selected
        .views
        .iter()
        .filter(|binding| {
            plan.selected.views.len() == 1
                || request.filters.iter().any(|filter| {
                    filter.0.strip_prefix("repository:") == Some(binding.repository_id.as_str())
                })
        })
        .take(1)
    {
        let mut observed = BTreeSet::new();
        for (source, report) in plan.repository_phases(AnalysisPhase::Vitals, binding, true) {
            if !observed.insert(source.view_digest.as_str().to_owned()) {
                continue;
            }
            let security = reports.security(plan, binding, source, &report)?;
            let Some(vitals) = reports.vitals(&report)? else {
                continue;
            };
            let Some(repo) = vitals.repositories.iter().find(|repo| {
                repo.repository == source.repository_id && repo.view == source.view_id
            }) else {
                continue;
            };
            let summary = repo.summary(None, security)?;
            docs.trends.push(AnalysisTrendPoint {
                view: source.clone(),
                computed_at_ms: counter(vitals.computed_at),
                base_score: summary.base_score,
                adjusted_score: summary.adjusted_score,
            });
            if docs.trends.len() == HISTORY_POINTS {
                break;
            }
        }
    }
    docs.trends.sort_by_key(|point| point.computed_at_ms.get());
    if reports.clipped {
        docs.measurement_gaps.push("TrendReadBudget".into());
    }
    if plan.repository_search_clipped && docs.trends.len() < HISTORY_POINTS {
        docs.measurement_gaps.push("BoundedTrendSearch".into());
    }
    Ok(())
}

pub(super) fn narratives(
    queries: &Queries,
    ctx: &InvocationContext,
    plan: &ReadPlan,
    measured: &Report<'_>,
    docs: &mut Documents,
) -> Result<(), Failure> {
    use crate::services::graph::repository_analysis::modeling::{ProjectsReport, SynthesisReport};
    use crate::services::kernel::label;
    let selected = |repository: &Id, view: &Id| {
        measured
            .run
            .intent
            .views
            .iter()
            .any(|binding| &binding.repository_id == repository && &binding.view_id == view)
            && plan
                .selected
                .views
                .iter()
                .any(|binding| &binding.repository_id == repository)
    };
    let binding = &measured.run.binding_key;
    let projects = plan.phase_for_binding(AnalysisPhase::Projects, binding);
    if let Some(reference) = projects.as_ref().and_then(Report::reference)
        && let Some(report) =
            queries.document::<ProjectsReport>(ctx, projects.as_ref(), &mut docs.references)?
    {
        for result in report
            .completed
            .into_iter()
            .filter(|result| selected(&result.subject.repository, &result.subject.view))
        {
            docs.narratives.push(AnalysisNarrative {
                repository_id: result.subject.repository,
                view_id: result.subject.view,
                project: Some(label(&result.subject.name, "Project")),
                summary: model::text(&result.result.summary)?,
                confidence: model::wire(json!(result.result.confidence))?,
                model: label(&result.model, "Analysis model"),
                computed_at_ms: counter(report.computed_at),
                evidence: reference.clone(),
            });
        }
    }
    let synthesis = plan.phase_for_binding(AnalysisPhase::Synthesis, binding);
    if let Some(reference) = synthesis.as_ref().and_then(Report::reference)
        && let Some(report) =
            queries.document::<SynthesisReport>(ctx, synthesis.as_ref(), &mut docs.references)?
    {
        for result in report
            .completed
            .into_iter()
            .filter(|result| selected(&result.repository, &result.view))
        {
            docs.narratives.push(AnalysisNarrative {
                repository_id: result.repository,
                view_id: result.view,
                project: None,
                summary: model::text(&result.result.summary)?,
                confidence: model::wire(json!(result.result.confidence))?,
                model: label(&result.model, "Analysis model"),
                computed_at_ms: counter(report.computed_at),
                evidence: reference.clone(),
            });
        }
    }
    Ok(())
}
