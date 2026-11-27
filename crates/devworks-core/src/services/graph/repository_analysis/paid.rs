//! Attributed per-project inference, with an immutable request identity for every subject.
use super::super::budget::Budget;
use super::{
    links::LinkReport,
    modeling::{
        self, AnalysisModel, AnalysisModelInput, ProjectResult, ProjectsReport, ProviderAnalysis,
        Subject, SynthesisReport, SynthesisResult,
    },
    phase_store::{self, PhaseEvidence, PhaseStore, SynthesisEvent},
    selection,
    work::Claim,
};
use crate::fingerprint::sha256_hex;
use crate::ports::{IdKind, storage::*};
use crate::protocol::{Id, OperationState, data::QueryLimits};
use crate::services::kernel::Failure;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc};

pub(super) struct Paid<'a> {
    pub phases: &'a PhaseStore,
    pub model: &'a dyn AnalysisModel,
    pub cancellation: &'a Cancellation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(super) enum ModelBinding {
    Project {
        subject: Subject,
    },
    Synthesis {
        repository: Id,
        view: Id,
        project_checkpoints: Vec<Id>,
    },
}
impl ModelBinding {
    pub(super) fn recover(
        self,
        provider: ProviderAnalysis,
        checkpoint: Id,
    ) -> Option<serde_json::Value> {
        match self {
            Self::Project { subject } => {
                serde_json::to_value(project_result(&subject, provider, checkpoint)?).ok()
            }
            Self::Synthesis {
                repository,
                view,
                project_checkpoints,
            } => serde_json::to_value(SynthesisResult {
                repository,
                view,
                project_checkpoints,
                result: modeling::decode_synthesis(&provider.text)?,
                request: provider.request,
                attempt: provider.attempt,
                model: provider.model,
            })
            .ok(),
        }
    }
}

struct ModelSubject {
    key: Id,
    repository: Id,
    view: Id,
    prompt: String,
    binding: ModelBinding,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Document {
    Projects(ProjectsReport),
    Synthesis(SynthesisReport),
}

impl Paid<'_> {
    pub(super) fn phase(
        &self,
        claim: &Arc<Claim>,
        phase: AnalysisPhase,
    ) -> Result<StoredAnalysisRun, Failure> {
        let mut run = self.phases.begin(claim, phase, None)?;
        if run.phases.iter().any(|checkpoint| {
            checkpoint.phase == phase && checkpoint.state == OperationState::ObservedComplete
        }) {
            return Ok(run);
        }
        let (document, events) = match phase {
            AnalysisPhase::Projects => {
                let report = self.projects(claim, &mut run)?;
                (Document::Projects(report), Vec::new())
            }
            AnalysisPhase::Synthesis => {
                let (report, events) = self.synthesis(claim, &mut run)?;
                (Document::Synthesis(report), events)
            }
            _ => {
                return Err(
                    StorageError::InvalidRecord("invalid paid analysis phase".into()).into(),
                );
            }
        };
        let checkpoint = run
            .phases
            .iter()
            .find(|checkpoint| checkpoint.phase == phase)
            .ok_or_else(selection::unavailable)?;
        let has_unknown = checkpoint.subjects.iter().any(|subject| {
            matches!(
                subject.state,
                OperationState::Dispatching | OperationState::OutcomeUnknown
            )
        });
        let has_result = match &document {
            Document::Projects(report) => !report.completed.is_empty(),
            Document::Synthesis(report) => !report.completed.is_empty(),
        };
        let state = if has_unknown {
            OperationState::OutcomeUnknown
        } else if has_result {
            OperationState::ObservedComplete
        } else {
            OperationState::Failed
        };
        let staged = (state == OperationState::ObservedComplete)
            .then(|| self.phases.stage(&run, phase, &document))
            .transpose()?;
        let diagnostic = match state {
            OperationState::Failed => Some("NoValidModelResult"),
            OperationState::OutcomeUnknown => Some("ModelOutcomeUnknown"),
            _ => None,
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
            if state == OperationState::ObservedComplete {
                events
            } else {
                Vec::new()
            },
        )
    }

    fn projects(
        &self,
        claim: &Arc<Claim>,
        run: &mut StoredAnalysisRun,
    ) -> Result<ProjectsReport, Failure> {
        let mut budget = Budget::new(&QueryLimits {
            max_results: 128,
            max_visited: 100_000,
            deadline_ms: 30_000,
        });
        budget.start_at(std::time::Instant::now(), Some(self.cancellation.clone()));
        let prompts = self.check(claim, |read, selected| {
            modeling::project_prompts(
                read,
                &selected,
                self.phases.artifacts.as_ref(),
                &run.intent,
                &mut budget,
            )
        })?;
        let mut report = ProjectsReport {
            computed_at: run.updated_at_ms,
            completed: Vec::new(),
            gaps: Vec::new(),
        };
        for prompt in prompts {
            if prompt.partial {
                report.gaps.push("ProjectPromptCoverageLimited".into());
            }
            let subject = prompt.subject;
            let selection = ModelSubject {
                key: subject.id.clone(),
                repository: subject.repository.clone(),
                view: subject.view.clone(),
                prompt: prompt.text,
                binding: ModelBinding::Project {
                    subject: subject.clone(),
                },
            };
            let output = self.execute(
                claim,
                run,
                AnalysisPhase::Projects,
                selection,
                |provider, checkpoint| project_result(&subject, provider, checkpoint),
            )?;
            match output {
                Some(result) => report.completed.push(result),
                None => report.gaps.push("ProjectAnalysisUnavailable".into()),
            }
        }
        report.computed_at = run.updated_at_ms;
        report.gaps.sort();
        report.gaps.dedup();
        Ok(report)
    }

    fn synthesis(
        &self,
        claim: &Arc<Claim>,
        run: &mut StoredAnalysisRun,
    ) -> Result<(SynthesisReport, Vec<SynthesisEvent>), Failure> {
        let source = if let Some(id) = &run.intent.prerequisite_run {
            let old = self.check(claim, |read, _| {
                let old = read
                    .analysis()
                    .analysis_run(id)?
                    .ok_or_else(selection::unavailable)?;
                selection::Selection::check_saved(
                    read,
                    &claim.context,
                    &run.intent.scope,
                    &old,
                    read.stamp().now_ms,
                )?;
                if old.binding_key != run.binding_key || old.owner.namespace != run.owner.namespace
                {
                    return Err(selection::unavailable());
                }
                Ok(old)
            })?;
            Some(old)
        } else {
            None
        };
        let input = source.as_ref().unwrap_or(run);
        let projects =
            output::<ProjectsReport>(self.phases, claim, input, AnalysisPhase::Projects)?
                .ok_or_else(selection::unavailable)?;
        let links = output::<LinkReport>(self.phases, claim, input, AnalysisPhase::Links)?;
        let mut report = SynthesisReport {
            computed_at: run.updated_at_ms,
            completed: Vec::new(),
            gaps: projects.gaps,
        };
        let mut events = Vec::new();
        let mut seen = BTreeSet::new();
        for project in &projects.completed {
            let repository = &project.subject.repository;
            if !seen.insert(repository.as_str()) {
                continue;
            }
            if seen.len() > usize::from(run.intent.batch_limit).min(32) {
                report.gaps.push("SynthesisBatchLimit".into());
                break;
            }
            let view = &project.subject.view;
            let (prompt, inputs, partial) =
                modeling::synthesis_prompt(repository, &projects.completed, links.as_ref())?;
            let digest = selection::digest(
                &json!({"repository": repository, "view": view, "inputs": inputs}),
            );
            let key =
                Id::new(format!("synthesis_{digest}")).map_err(|_| selection::unavailable())?;
            let selection = ModelSubject {
                key,
                repository: repository.clone(),
                view: view.clone(),
                prompt,
                binding: ModelBinding::Synthesis {
                    repository: repository.clone(),
                    view: view.clone(),
                    project_checkpoints: inputs.clone(),
                },
            };
            let result = self.execute(
                claim,
                run,
                AnalysisPhase::Synthesis,
                selection,
                |provider, _| {
                    Some(SynthesisResult {
                        repository: repository.clone(),
                        view: view.clone(),
                        project_checkpoints: inputs.clone(),
                        result: modeling::decode_synthesis(&provider.text)?,
                        request: provider.request,
                        attempt: provider.attempt,
                        model: provider.model,
                    })
                },
            )?;
            if let Some(result) = result {
                events.push(SynthesisEvent {
                    repository: repository.clone(),
                    project_checkpoints: inputs,
                    partial: partial || !report.gaps.is_empty(),
                });
                report.completed.push(result);
                if partial {
                    report.gaps.push("SynthesisInputCoverageLimited".into());
                }
            } else {
                report.gaps.push("SynthesisUnavailable".into());
            }
        }
        report.computed_at = run.updated_at_ms;
        report.gaps.sort();
        report.gaps.dedup();
        Ok((report, events))
    }

    fn execute<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        claim: &Arc<Claim>,
        run: &mut StoredAnalysisRun,
        phase: AnalysisPhase,
        selected: ModelSubject,
        decode: impl FnOnce(ProviderAnalysis, Id) -> Option<T>,
    ) -> Result<Option<T>, Failure> {
        if self.cancellation.is_cancelled() {
            return Err(StorageError::Cancelled.into());
        }
        self.check(claim, |read, _| {
            claim.model_owner(read, &self.phases.store.info(), run)?;
            Ok(())
        })?;
        let checkpoint = subject_intent(self.phases, claim, run, phase, &selected)?;
        if let Some(existing) = checkpoint.output {
            return Ok(serde_json::from_value(existing).ok());
        }
        if checkpoint.state == OperationState::Failed
            || checkpoint.state == OperationState::OutcomeUnknown
        {
            return Ok(None);
        }
        let response = self.model.generate(AnalysisModelInput {
            context: claim.context.clone(),
            fences: claim.work.authority.fences.clone(),
            analysis_operation: run.active_operation.clone(),
            scope: run.intent.scope.clone(),
            request: checkpoint.request.clone(),
            system: modeling::SYSTEM,
            prompt: selected.prompt,
            cancellation: self.cancellation.clone(),
        });
        let (state, output, diagnostic) = score_response(response, &checkpoint, decode);
        settle_subject(
            self.phases,
            claim,
            run,
            phase,
            &checkpoint.id,
            state,
            output.as_ref(),
            diagnostic,
        )?;
        Ok(output)
    }

    fn check<T>(
        &self,
        claim: &Claim,
        f: impl FnOnce(&dyn ReadSnapshot, selection::Selection) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        read_snapshot(
            self.phases.store.as_ref(),
            &ReadOptions::default(),
            |read| {
                Ok(claim
                    .run(read)
                    .and_then(|(_, selection)| f(read, selection)))
            },
        )?
    }
}

fn score_response<T>(
    response: modeling::Observation,
    checkpoint: &StoredAnalysisSubject,
    decode: impl FnOnce(ProviderAnalysis, Id) -> Option<T>,
) -> (OperationState, Option<T>, Option<&'static str>) {
    match response {
        modeling::Observation::Completed(result) if result.request == checkpoint.request => {
            match decode(result, checkpoint.id.clone()) {
                Some(result) => (OperationState::ObservedComplete, Some(result), None),
                None => (OperationState::Failed, None, Some("InvalidModelResult")),
            }
        }
        modeling::Observation::NotApplied => (
            OperationState::Failed,
            None,
            Some("ModelRequestNotAdmitted"),
        ),
        modeling::Observation::Failed => (OperationState::Failed, None, Some("ModelRequestFailed")),
        modeling::Observation::Refused(reason) => (OperationState::Failed, None, Some(reason)),
        _ => (
            OperationState::OutcomeUnknown,
            None,
            Some("ModelOutcomeUnknown"),
        ),
    }
}

fn project_result(
    subject: &Subject,
    provider: ProviderAnalysis,
    checkpoint: Id,
) -> Option<ProjectResult> {
    Some(ProjectResult {
        checkpoint,
        subject: subject.clone(),
        result: modeling::decode_project(&provider.text, subject)?,
        request: provider.request,
        attempt: provider.attempt,
        model: provider.model,
    })
}

fn output<T: serde::de::DeserializeOwned>(
    store: &PhaseStore,
    claim: &Claim,
    run: &StoredAnalysisRun,
    phase: AnalysisPhase,
) -> Result<Option<T>, Failure> {
    let reference = run
        .phases
        .iter()
        .find(|checkpoint| checkpoint.phase == phase)
        .filter(|checkpoint| checkpoint.state == OperationState::ObservedComplete)
        .and_then(|checkpoint| checkpoint.report.as_ref());
    reference
        .map(|reference| store.read(&claim.context, reference))
        .transpose()
}

fn subject_intent(
    store: &PhaseStore,
    claim: &Arc<Claim>,
    run: &mut StoredAnalysisRun,
    phase: AnalysisPhase,
    subject: &ModelSubject,
) -> Result<StoredAnalysisSubject, Failure> {
    let digest = sha256_hex(subject.prompt.as_bytes());
    if let Some(existing) = run
        .phases
        .iter()
        .find(|checkpoint| checkpoint.phase == phase)
        .and_then(|checkpoint| {
            checkpoint
                .subjects
                .iter()
                .find(|record| record.key == subject.key)
        })
    {
        if existing.prompt_digest != digest {
            return Err(selection::unavailable());
        }
        // A prior dispatch without an observation must be reconciled first.
        let mut existing = existing.clone();
        if existing.state == OperationState::Dispatching {
            existing.state = OperationState::OutcomeUnknown;
        }
        return Ok(existing);
    }
    let binding = serde_json::to_value(&subject.binding).map_err(|_| {
        StorageError::InvalidRecord("model subject binding cannot be encoded".into())
    })?;
    let key = subject.key.clone();
    let repository = subject.repository.clone();
    let view = subject.view.clone();
    subject_unit(store, claim, run, move |tx, run| {
        let intent = StoredAnalysisSubject {
            id: tx.new_id(IdKind::Operation),
            key,
            repository,
            view,
            request: tx.new_id(IdKind::Operation),
            prompt_digest: digest,
            binding,
            state: OperationState::Dispatching,
            output: None,
            diagnostic: None,
        };
        phase_store::phase_mut(run, phase)?
            .subjects
            .push(intent.clone());
        Ok(intent)
    })
}

#[allow(clippy::too_many_arguments)]
fn settle_subject<T: Serialize>(
    store: &PhaseStore,
    claim: &Arc<Claim>,
    run: &mut StoredAnalysisRun,
    phase: AnalysisPhase,
    id: &Id,
    state: OperationState,
    output: Option<&T>,
    diagnostic: Option<&'static str>,
) -> Result<(), Failure> {
    let id = id.clone();
    let output = output
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| StorageError::InvalidRecord("model result cannot be saved".into()))?;
    subject_unit(store, claim, run, move |_, run| {
        let subject = phase_store::phase_mut(run, phase)?
            .subjects
            .iter_mut()
            .find(|subject| subject.id == id && subject.state == OperationState::Dispatching)
            .ok_or(StorageError::Busy)?;
        subject.state = state;
        subject.output = output;
        subject.diagnostic = diagnostic.map(str::to_owned);
        Ok(())
    })
}

fn subject_unit<T: Send + 'static>(
    store: &PhaseStore,
    claim: &Arc<Claim>,
    run: &mut StoredAnalysisRun,
    edit: impl FnOnce(&mut dyn WriteTransaction, &mut StoredAnalysisRun) -> Result<T, StorageError>
    + Send
    + 'static,
) -> Result<T, Failure> {
    let (previous, selection) =
        read_snapshot(store.store.as_ref(), &ReadOptions::default(), |read| {
            Ok(claim.run(read))
        })??;
    if previous.revision != run.revision {
        return Err(StorageError::Busy.into());
    }
    let claim = claim.clone();
    let (next, result) = run_write(
        store.store.as_ref(),
        WriteOptions::settlement(),
        move |tx| {
            claim
                .validate_in(tx, &previous, &selection)
                .map_err(Failure::into_storage)?;
            let mut next = previous.clone();
            let result = edit(tx, &mut next)?;
            phase_store::advance(tx, &previous, &mut next)?;
            Ok((next, result))
        },
    )?;
    *run = next;
    Ok(result)
}
