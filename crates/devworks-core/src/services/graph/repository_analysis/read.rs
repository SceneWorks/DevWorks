//! Resolve pending and saved results under the reader's present graph and artifact authority.
use super::{
    observation,
    selection::{self, Selection},
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{AnalysisPhase, ReadSnapshot, StoredAnalysisPhase, StoredAnalysisRun};
use crate::protocol::data::{DataScope, ViewSelection};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::AnalysisViewBinding;
use crate::protocol::{Id, OperationState};
use crate::services::kernel::Failure;

pub(super) struct Report<'a> {
    pub run: &'a StoredAnalysisRun,
    pub checkpoint: &'a StoredAnalysisPhase,
}

impl Report<'_> {
    pub(super) fn reference(&self) -> Option<&ArtifactRef> {
        self.checkpoint.report.as_ref()
    }
}

pub(super) struct ReadPlan {
    pub selected: Selection,
    pub explicit: Option<StoredAnalysisRun>,
    pub exact: Vec<StoredAnalysisRun>,
    pub history: Vec<StoredAnalysisRun>,
    pub repository_runs: Vec<StoredAnalysisRun>,
    pub repository_search_clipped: bool,
}

impl ReadPlan {
    pub(super) fn load(
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        scope: &DataScope,
        views: &ViewSelection,
        run_id: Option<&Id>,
        want_history: bool,
        repository_projection: bool,
    ) -> Result<Self, Failure> {
        let now = read.stamp().now_ms;
        let selected = Selection::resolve(read, ctx, scope, views, now)?;
        if let Some(id) = run_id {
            let run = read
                .analysis()
                .analysis_run(id)?
                .ok_or_else(selection::unavailable)?;
            Selection::check_saved(read, ctx, scope, &run, now)?;
            if run.intent.views.len() != selected.views.len()
                || run
                    .intent
                    .views
                    .iter()
                    .zip(&selected.views)
                    .any(|(old, current)| old.repository_id != current.repository_id)
                || (run.binding_key != selected.binding_key
                    && run.lineage_key != selected.lineage_key)
            {
                return Err(selection::unavailable());
            }
            return Ok(Self {
                selected,
                explicit: Some(current_state(read, run)?),
                exact: Vec::new(),
                history: Vec::new(),
                repository_runs: Vec::new(),
                repository_search_clipped: false,
            });
        }
        let exact = read
            .analysis()
            .analysis_runs(&selected.binding_key, false, None, 64)?
            .into_iter()
            .map(|row| current_state(read, row))
            .collect::<Result<_, _>>()?;
        let history = if want_history {
            let oldest = now.saturating_sub(90 * 86_400_000);
            read.analysis()
                .analysis_runs(&selected.lineage_key, true, None, 64)?
                .into_iter()
                .filter(|run| run.updated_at_ms >= oldest)
                .filter_map(
                    |run| match Selection::check_saved(read, ctx, scope, &run, now) {
                        Ok(()) => Some(Ok(run)),
                        Err(Failure::Refused(_)) => None,
                        Err(error) => Some(Err(error)),
                    },
                )
                .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        let mut repository_runs = Vec::new();
        let mut repository_search_clipped = false;
        if repository_projection && !selected.views.is_empty() {
            let scope_key = match scope {
                DataScope::Workspace if ctx.principal_kind() == PrincipalKind::Human => None,
                _ => Some(super::commands::scope_key(scope)),
            };
            let repositories = selected
                .views
                .iter()
                .map(|view| view.repository_id.clone())
                .collect::<Vec<_>>();
            let rows = read.analysis().analysis_repository_runs(
                scope_key,
                &repositories,
                now.saturating_sub(90 * 86_400_000),
                128,
            )?;
            repository_search_clipped = rows.len() == 128;
            for run in rows {
                let source_scope = &run.intent.scope;
                match Selection::check_saved(read, ctx, source_scope, &run, now) {
                    Ok(()) => {}
                    Err(Failure::Refused(_)) => continue,
                    Err(error) => return Err(error),
                }
                if run.intent.configuration_revision
                    != selection::configuration_revision(read.indexing(), source_scope)?
                {
                    continue;
                }
                repository_runs.push(run);
            }
        }
        Ok(Self {
            selected,
            explicit: None,
            exact,
            history,
            repository_runs,
            repository_search_clipped,
        })
    }

    pub(super) fn status(&self) -> Option<&StoredAnalysisRun> {
        self.explicit.as_ref().or_else(|| self.exact.first())
    }

    pub(super) fn phase(&self, phase: AnalysisPhase, stale: bool) -> Option<Report<'_>> {
        if let Some(explicit) = &self.explicit {
            return find(std::slice::from_ref(explicit), phase, false);
        }
        find(&self.exact, phase, true)
            .or_else(|| stale.then(|| find(&self.history, phase, true)).flatten())
            .or_else(|| find(&self.exact, phase, false))
    }

    pub(super) fn primary(
        &self,
        phase: AnalysisPhase,
        allow_history: bool,
        repository_projection: bool,
    ) -> Option<Report<'_>> {
        if self.explicit.is_some() || !repository_projection {
            return self.phase(phase, allow_history);
        }
        let exact = find(&self.exact, phase, true);
        if let [selected] = self.selected.views.as_slice() {
            return exact
                .or_else(|| {
                    self.repository_phases(phase, selected, false)
                        .next()
                        .map(|(_, report)| report)
                })
                .or_else(|| {
                    allow_history
                        .then(|| {
                            self.repository_phases(phase, selected, true)
                                .next()
                                .map(|(_, report)| report)
                        })
                        .flatten()
                })
                .or_else(|| find(&self.exact, phase, false));
        }
        exact.or_else(|| self.phase(phase, allow_history))
    }

    pub(super) fn phase_for_binding(&self, phase: AnalysisPhase, key: &str) -> Option<Report<'_>> {
        if let Some(explicit) = self.explicit.as_ref().filter(|run| run.binding_key == key)
            && let Some(report) = find(std::slice::from_ref(explicit), phase, true)
        {
            return Some(report);
        }
        self.exact
            .iter()
            .chain(&self.history)
            .filter(|run| run.binding_key == key)
            .find_map(|run| find(std::slice::from_ref(run), phase, true))
    }

    pub(super) fn repository_phases<'a>(
        &'a self,
        phase: AnalysisPhase,
        selected: &'a AnalysisViewBinding,
        allow_history: bool,
    ) -> impl Iterator<Item = (&'a AnalysisViewBinding, Report<'a>)> + 'a {
        let lineage = self
            .selected
            .views
            .iter()
            .position(|view| view.repository_id == selected.repository_id)
            .and_then(|position| self.selected.repository_lineages.get(position));
        self.repository_runs.iter().filter_map(move |run| {
            let (position, view) = run
                .intent
                .views
                .iter()
                .enumerate()
                .find(|(_, view)| view.repository_id == selected.repository_id)?;
            if !(view.view_id == selected.view_id
                && view.view_digest.as_str() == selected.view_digest.as_str()
                || allow_history && run.intent.repository_lineages.get(position) == lineage)
            {
                return None;
            }
            let checkpoint = find(std::slice::from_ref(run), phase, true)?.checkpoint;
            Some((view, Report { run, checkpoint }))
        })
    }

    /// A query holds an exact reference only while its report and its scope stay readable.
    pub(super) fn readable(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        report: &Report<'_>,
    ) -> Result<bool, Failure> {
        Selection::check_saved(
            read,
            ctx,
            &report.run.intent.scope,
            report.run,
            read.stamp().now_ms,
        )?;
        if let Some(reference) = report.reference() {
            return observation::readable(read, ctx, reference);
        }
        Ok(true)
    }

    pub(super) fn fence(&self) -> String {
        // A cursor is anchored to the report snapshots it actually projects. A
        // newer run or a settled phase invalidates the old page position.
        let rows = self
            .explicit
            .iter()
            .chain(&self.exact)
            .chain(&self.history)
            .chain(&self.repository_runs)
            .map(|run| (&run.id, run.revision, run.state))
            .collect::<Vec<_>>();
        selection::digest(&serde_json::json!({"binding": self.selected.binding_key, "rows": rows}))
    }
}

fn find(
    runs: &[StoredAnalysisRun],
    requested: AnalysisPhase,
    complete: bool,
) -> Option<Report<'_>> {
    runs.iter()
        .filter(|run| {
            !complete
                || !matches!(requested, AnalysisPhase::Links | AnalysisPhase::Communities)
                || run.intent.node_ids.is_empty()
        })
        .find_map(|run| {
            run.phases
                .iter()
                .find(|phase| {
                    phase.phase == requested
                        && (!complete
                            || phase.state == OperationState::ObservedComplete
                                && phase.report.is_some())
                })
                .map(|checkpoint| Report { run, checkpoint })
        })
}

fn current_state(
    read: &dyn ReadSnapshot,
    mut run: StoredAnalysisRun,
) -> Result<StoredAnalysisRun, Failure> {
    if !run.terminal()
        && let Some(operation) = read.operations().operation(&run.active_operation)?
    {
        let previous = run.state;
        run.state = operation.state;
        if matches!(
            run.state,
            OperationState::Failed | OperationState::CancelledBeforeDispatch
        ) && run.state != previous
        {
            for phase in run
                .phases
                .iter_mut()
                .filter(|phase| phase.state != OperationState::ObservedComplete)
            {
                if phase.state != OperationState::OutcomeUnknown {
                    phase.state = run.state;
                }
            }
        }
    }
    Ok(run)
}
