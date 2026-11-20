//! Deterministic phases share their exact graph input and independent durable reports.
use super::super::budget::Budget;
use super::{
    communities,
    links::{self, LinkReport},
    phase_store::{PhaseEvidence, PhaseStore},
    security::{self, Feed, RequestBudget},
    selection, vitals,
    work::Claim,
};
use crate::ports::storage::*;
use crate::protocol::{
    OperationState,
    data::{DataScope, QueryLimits},
};
use crate::services::{indexing::IndexEnv, kernel::Failure};
use serde::Serialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const PHASE_LIMITS: QueryLimits = QueryLimits {
    max_results: 500,
    max_visited: 100_000,
    deadline_ms: 30_000,
};

#[derive(Serialize)]
#[serde(untagged)]
enum ResultDocument {
    Links(links::LinkReport),
    Communities(communities::CommunityReport),
    Vitals(vitals::VitalsReport),
    Security(security::SecurityReport),
}

pub(super) struct Deterministic<'a> {
    pub phases: &'a PhaseStore,
    pub index: &'a IndexEnv,
    pub advisory: Option<&'a Feed>,
    pub history_gate: &'a vitals::HistoryGate,
    pub cancellation: &'a Cancellation,
}

impl Deterministic<'_> {
    pub(super) fn phase(
        &self,
        claim: &Arc<Claim>,
        phase: AnalysisPhase,
    ) -> Result<StoredAnalysisRun, Failure> {
        let run = self.phases.begin(claim, phase, None)?;
        if run
            .phases
            .iter()
            .any(|record| record.phase == phase && record.state == OperationState::ObservedComplete)
        {
            return Ok(run);
        }
        let stage = self.compute(claim, &run, phase).and_then(|result| {
            if self.cancellation.is_cancelled() {
                return Err(StorageError::Cancelled.into());
            }
            self.phases.stage(&run, phase, &result)
        });
        let (state, staged, diagnostic) = match stage {
            Ok(staged) => (OperationState::ObservedComplete, Some(staged), None),
            Err(
                error @ Failure::Storage(
                    StorageError::Cancelled
                    | StorageError::CommitUnknown
                    | StorageError::LeaseLost { .. }
                    | StorageError::Busy
                    | StorageError::ScopeDenied,
                ),
            ) => {
                return Err(error);
            }
            Err(_) => (
                OperationState::Failed,
                None,
                Some("AnalysisPhaseUnavailable"),
            ),
        };
        self.phases.settle(
            claim,
            &run,
            phase,
            PhaseEvidence {
                state,
                attempt: None,
                diagnostic,
            },
            staged.as_ref(),
            Vec::new(),
        )
    }

    fn compute(
        &self,
        claim: &Arc<Claim>,
        run: &StoredAnalysisRun,
        phase: AnalysisPhase,
    ) -> Result<ResultDocument, Failure> {
        let started = Instant::now();
        let mut budget = Budget::new(&PHASE_LIMITS);
        budget.start_at(started, Some(self.cancellation.clone()));
        let snapshots = ReadOptions {
            cancellation: Some(self.cancellation.clone()),
            ..Default::default()
        };
        let store = self.phases.store.as_ref();
        let artifacts = self.phases.artifacts.as_ref();
        let computed_at = self.index.now();
        match phase {
            AnalysisPhase::Links => {
                if (run.intent.command == AnalysisCommand::Community
                    || claim.context.principal_kind() == crate::context::PrincipalKind::Schedule)
                    && let Some(existing) = self.exact_links(claim, run)?
                {
                    return Ok(ResultDocument::Links(existing));
                }
                let result = read_snapshot(store, &snapshots, |read| {
                    Ok((|| {
                        let (_, selected) = claim.run(read)?;
                        links::build(read, &selected, &mut budget)
                    })())
                })??;
                let mut result = result;
                if !run.intent.node_ids.is_empty() {
                    let include =
                        |node: &str| run.intent.node_ids.iter().any(|id| id.as_str() == node);
                    result.links.retain(|link| {
                        include(&link.source.node_id)
                            || link
                                .target
                                .as_ref()
                                .is_some_and(|target| include(&target.node_id))
                    });
                    result.gaps.push("SelectedNodesOnly".into());
                }
                Ok(ResultDocument::Links(result))
            }
            AnalysisPhase::Communities => {
                let reference = run
                    .phases
                    .iter()
                    .find(|phase| phase.phase == AnalysisPhase::Links)
                    .filter(|phase| phase.state == OperationState::ObservedComplete)
                    .and_then(|phase| phase.report.as_ref())
                    .ok_or_else(selection::unavailable)?;
                let links = self.phases.read(&claim.context, reference)?;
                let result = read_snapshot(store, &snapshots, |read| {
                    Ok((|| {
                        let (_, selected) = claim.run(read)?;
                        communities::build(read, &selected, links, computed_at, &mut budget)
                    })())
                })??;
                Ok(ResultDocument::Communities(result))
            }
            AnalysisPhase::Vitals => {
                let inputs = read_snapshot(store, &snapshots, |read| {
                    Ok((|| {
                        let (_, selected) = claim.run(read)?;
                        vitals::history_inputs(read, &selected)
                    })())
                })??;
                let stopped = || {
                    self.cancellation.is_cancelled() || started.elapsed() >= Duration::from_secs(30)
                };
                let history = vitals::history_reads(
                    claim,
                    self.index,
                    inputs,
                    self.history_gate,
                    computed_at,
                    &stopped,
                )?;
                let result = read_snapshot(store, &snapshots, |read| {
                    Ok((|| {
                        let (_, selected) = claim.run(read)?;
                        vitals::build(
                            read,
                            artifacts,
                            &selected,
                            &history,
                            computed_at,
                            &mut budget,
                        )
                    })())
                })??;
                Ok(ResultDocument::Vitals(result))
            }
            AnalysisPhase::Security => {
                let mut report = read_snapshot(store, &snapshots, |read| {
                    Ok((|| {
                        let (_, selected) = claim.run(read)?;
                        security::local(read, artifacts, &selected, computed_at, &mut budget)
                    })())
                })??;
                let mut requests = RequestBudget::new(
                    self.cancellation.clone(),
                    started + Duration::from_secs(30),
                );
                let mut allowed =
                    || {
                        read_snapshot(store, &snapshots, |read| {
                            if claim.run(read).is_err() {
                                return Ok(false);
                            }
                            let project = match &run.intent.scope {
                                DataScope::Project { project_id } => Some(project_id),
                                DataScope::Workspace => None,
                            };
                            Ok(crate::authority::policy::resolve_policy(
                                read.configuration(),
                                project,
                            )?
                            .web_fetch)
                        })
                        .unwrap_or(false)
                    };
                report.settle_advisories(self.advisory, &mut allowed, &mut requests);
                Ok(ResultDocument::Security(report))
            }
            _ => Err(
                StorageError::InvalidRecord("analysis phase is not deterministic".into()).into(),
            ),
        }
    }

    fn exact_links(
        &self,
        claim: &Claim,
        run: &StoredAnalysisRun,
    ) -> Result<Option<LinkReport>, Failure> {
        let found = read_snapshot(
            self.phases.store.as_ref(),
            &ReadOptions::default(),
            |read| {
                let outcome = (|| -> Result<_, Failure> {
                    let runs = read
                        .analysis()
                        .analysis_runs(&run.binding_key, false, None, 32)?;
                    for candidate in runs {
                        if candidate.id == run.id
                            || candidate.intent.node_ids != run.intent.node_ids
                        {
                            continue;
                        }
                        selection::Selection::check_saved(
                            read,
                            &claim.context,
                            &run.intent.scope,
                            &candidate,
                            read.stamp().now_ms,
                        )?;
                        for report in candidate
                            .phases
                            .iter()
                            .filter(|phase| {
                                phase.phase == AnalysisPhase::Links
                                    && phase.state == OperationState::ObservedComplete
                            })
                            .filter_map(|phase| phase.report.as_ref())
                        {
                            if super::observation::readable(read, &claim.context, report)? {
                                return Ok(Some(report.clone()));
                            }
                        }
                    }
                    Ok(None)
                })();
                Ok(outcome)
            },
        )??;
        found
            .as_ref()
            .map(|reference| self.phases.read(&claim.context, reference))
            .transpose()
    }
}
