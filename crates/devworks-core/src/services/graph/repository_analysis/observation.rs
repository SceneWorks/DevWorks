//! State and coverage of an immutable analysis report.
use super::super::model;
use super::selection::{self, Selection};
use crate::context::InvocationContext;
use crate::ports::storage::{ArtifactState, ReadSnapshot, StoredAnalysisPhase, StoredAnalysisRun};
use crate::protocol::data::{Coverage, CoverageState, ViewSelection};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::{
    AnalysisAvailability, AnalysisObservation, AnalysisPhaseStatus, AnalysisRecord,
    AnalysisViewBinding,
};
use crate::protocol::support::BoundedList;
use crate::protocol::{Id, OperationState};
use crate::services::artifacts::{Viewer, visible};
use crate::services::kernel::{Failure, counter, label, rev};

pub(super) fn readable(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    reference: &ArtifactRef,
) -> Result<bool, Failure> {
    let viewer = Viewer::resolve(read.authority(), ctx)?;
    let storage = read.artifacts();
    let Some(artifact) = storage.artifact(&reference.artifact_id)? else {
        return Ok(false);
    };
    if artifact.state != ArtifactState::Live || !visible(storage, &viewer, &artifact)? {
        return Ok(false);
    }
    Ok(storage
        .version(&reference.version_id)?
        .is_some_and(|version| {
            version.live()
                && version.artifact_id == artifact.id
                && version.blob_digest == reference.sha256.as_str()
                && version.byte_count == reference.byte_length.get()
                && version.media_type == reference.media_type
        }))
}

pub(super) struct Observation<'a> {
    pub selected: &'a Selection,
    pub run: Option<&'a StoredAnalysisRun>,
    pub phase: Option<&'a StoredAnalysisPhase>,
    pub gaps: &'a [String],
}

impl Observation<'_> {
    pub(super) fn bindings(&self) -> &[AnalysisViewBinding] {
        self.run
            .map_or(&self.selected.views, |run| &run.intent.views)
    }

    pub(super) fn state(&self) -> OperationState {
        self.phase
            .map(|phase| phase.state)
            .or_else(|| self.run.map(|run| run.state))
            .unwrap_or(OperationState::ObservedComplete)
    }

    pub(super) fn wire(&self) -> AnalysisObservation {
        let state = self.state();
        let mut diagnostics: Vec<String> = self.gaps.to_vec();
        diagnostics.extend(
            self.selected
                .unavailable
                .iter()
                .map(|entry| entry.reason.clone()),
        );
        if let Some(phase) = self.phase
            && let Some(diagnostic) = &phase.diagnostic
        {
            diagnostics.push(diagnostic.clone());
        }
        if state == OperationState::OutcomeUnknown {
            diagnostics.push("OutcomeUnknown".into());
        }
        diagnostics.retain(|diagnostic| !diagnostic.is_empty());
        diagnostics.sort();
        diagnostics.dedup();
        let stale = self.run.is_some_and(|run| {
            run.binding_key != self.selected.binding_key
                && !self.phase.is_some_and(|phase| {
                    matches!(
                        phase.phase,
                        crate::ports::storage::AnalysisPhase::Vitals
                            | crate::ports::storage::AnalysisPhase::Security
                    ) && !self.selected.views.is_empty()
                        && self.selected.views.iter().all(|selected| {
                            run.intent.views.iter().any(|view| {
                                view.repository_id == selected.repository_id
                                    && view.view_id == selected.view_id
                                    && view.view_digest.as_str() == selected.view_digest.as_str()
                            })
                        })
                })
        });
        let availability = match (self.run.is_some(), state) {
            (false, _) => AnalysisAvailability::NoData,
            (_, OperationState::Failed) => AnalysisAvailability::Failed,
            (_, OperationState::CancelledBeforeDispatch) => AnalysisAvailability::Unavailable,
            (_, OperationState::ObservedComplete) if stale => AnalysisAvailability::Stale,
            (_, OperationState::ObservedComplete) if diagnostics.is_empty() => {
                AnalysisAvailability::Available
            }
            (_, OperationState::ObservedComplete) => AnalysisAvailability::Partial,
            _ => AnalysisAvailability::Pending,
        };
        let coverage_state = match availability {
            AnalysisAvailability::Available => CoverageState::Complete,
            AnalysisAvailability::Stale | AnalysisAvailability::Partial => CoverageState::Partial,
            _ => CoverageState::Unavailable,
        };
        let phase_statuses = self.run.map(|run| {
            BoundedList(
                run.phases
                    .iter()
                    .map(|phase| AnalysisPhaseStatus {
                        phase: label(phase.phase.key(), "analysis"),
                        state: phase.state,
                        diagnostic: phase
                            .diagnostic
                            .as_deref()
                            .map(|s| label(s, "AnalysisUnavailable")),
                        completed_at_ms: phase.finished_at_ms.map(counter),
                    })
                    .collect(),
            )
        });
        let completed = self
            .phase
            .and_then(|phase| phase.finished_at_ms)
            .or_else(|| {
                self.run
                    .filter(|run| run.terminal())
                    .map(|run| run.updated_at_ms)
            });
        AnalysisObservation {
            availability,
            bindings: BoundedList(self.bindings().to_vec()),
            computed_at_ms: completed.map(counter),
            coverage: Coverage {
                state: coverage_state,
                gaps: diagnostics
                    .iter()
                    .take(128)
                    .map(|diagnostic| {
                        Id::new(diagnostic).unwrap_or_else(|_| {
                            Id::new(format!(
                                "gap_{}",
                                crate::fingerprint::sha256_hex(diagnostic.as_bytes())
                            ))
                            .expect("valid analysis gap key")
                        })
                    })
                    .collect(),
                more_gaps: diagnostics.len() > 128,
            },
            diagnostics: BoundedList(
                diagnostics
                    .iter()
                    .take(128)
                    .map(|diagnostic| label(diagnostic, "AnalysisUnavailable"))
                    .collect(),
            ),
            phases: phase_statuses,
            base_score: None,
            adjusted_score: None,
            trends: None,
            related_work_ids: None,
            top_hotspots: None,
            health: None,
            security: None,
            next_cursor: None,
            has_more: None,
            missing_repositories: None,
            indexed_bindings: stale.then(|| BoundedList(self.selected.views.clone())),
            narratives: None,
        }
    }

    pub(super) fn record(
        &self,
        kind: &str,
        report: Option<ArtifactRef>,
        observation: AnalysisObservation,
    ) -> Result<AnalysisRecord, Failure> {
        let fallback = || {
            Id::new(format!("analysis_{}", self.selected.binding_key))
                .map_err(|_| selection::unavailable())
        };
        let (id, revision) = match self.run {
            Some(run) => (run.id.clone(), run.revision),
            None => (fallback()?, self.selected.configuration_revision),
        };
        let views = if self.bindings().is_empty() {
            ViewSelection::Published {
                repository_ids: self
                    .selected
                    .unavailable
                    .iter()
                    .map(|entry| entry.repository_id.clone())
                    .collect(),
            }
        } else {
            ViewSelection::Exact {
                view_ids: self
                    .bindings()
                    .iter()
                    .map(|binding| binding.view_id.clone())
                    .collect(),
            }
        };
        Ok(AnalysisRecord {
            id,
            revision: rev(revision),
            views,
            kind: model::text(kind)?,
            state: self.state(),
            report,
            coverage: observation.coverage.clone(),
            observation: Some(observation),
        })
    }
}
