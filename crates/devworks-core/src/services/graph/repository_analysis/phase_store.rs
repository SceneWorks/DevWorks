//! Report artifacts and their owning phase advance in one fenced transaction.
use super::{commands::AGGREGATE, selection, work::Claim};
use crate::context::InvocationContext;
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::protocol::data::DataScope;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{Counter, EventId, Id, OperationId, OperationState};
use crate::services::artifacts::{
    ArtifactService, StagedFile, host::HostArtifactPublication, lifecycle::ExactReadRefusal,
};
use crate::services::kernel::Failure;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
const REPORT_MEDIA: &str = "application/vnd.devworks.repository-analysis+json";
const PIN_TTL_MS: i64 = 300_000;

pub(super) struct PhaseStore {
    pub store: Arc<dyn ApplicationStore>,
    pub artifacts: Arc<ArtifactService>,
}

#[derive(Clone)]
pub(super) struct PhaseEvidence {
    pub state: OperationState,
    pub attempt: Option<OperationId>,
    pub diagnostic: Option<&'static str>,
}

impl PhaseStore {
    pub(super) fn begin(
        &self,
        claim: &Arc<Claim>,
        phase: AnalysisPhase,
        attempt: Option<OperationId>,
    ) -> Result<StoredAnalysisRun, Failure> {
        let (run, selection) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                Ok(claim.run(read))
            })??;
        let claim = claim.clone();
        run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            claim
                .validate_in(tx, &run, &selection)
                .map_err(Failure::into_storage)?;
            let mut next = run.clone();
            let checkpoint = phase_mut(&mut next, phase)?;
            if checkpoint.state == OperationState::ObservedComplete {
                return Ok(next);
            }
            if checkpoint.state == OperationState::OutcomeUnknown
                || (checkpoint.operation.is_some()
                    && checkpoint.state == OperationState::Dispatching)
            {
                return Err(StorageError::Busy);
            }
            checkpoint.state = OperationState::Dispatching;
            checkpoint.operation = attempt;
            checkpoint.diagnostic = None;
            checkpoint.finished_at_ms = None;
            next.state = OperationState::Dispatching;
            advance(tx, &run, &mut next)?;
            renew_pin(tx, &next)?;
            Ok(next)
        })
        .map_err(Failure::from)
    }

    pub(super) fn stage(
        &self,
        run: &StoredAnalysisRun,
        phase: AnalysisPhase,
        report: &impl Serialize,
    ) -> Result<StagedFile, Failure> {
        let bytes = serde_json::to_vec(report)
            .map_err(|_| StorageError::InvalidRecord("analysis report encoding failed".into()))?;
        if bytes.len() > MAX_REPORT_BYTES {
            return Err(StorageError::InvalidRecord(
                "analysis report exceeded its byte bound".into(),
            )
            .into());
        }
        self.artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                &bytes,
                json!({"purpose": "repository_analysis", "runId": run.id, "phase": phase.key()}),
            )?
            .map_err(|_| {
                StorageError::InvalidRecord("analysis report staging failed".into()).into()
            })
    }

    pub(super) fn settle(
        &self,
        claim: &Arc<Claim>,
        run: &StoredAnalysisRun,
        phase: AnalysisPhase,
        evidence: PhaseEvidence,
        staged: Option<&StagedFile>,
        synthesis: Vec<SynthesisEvent>,
    ) -> Result<StoredAnalysisRun, Failure> {
        let result = self.publish(claim, run, phase, evidence, staged, synthesis);
        if let Some(staged) = staged {
            match &result {
                Ok(_) => self
                    .artifacts
                    .settle_host_bytes(self.store.as_ref(), staged, true),
                // The staging journal must resolve its own state if the writer
                // could have committed a publication before losing its reply.
                Err(Failure::Storage(StorageError::CommitUnknown)) => {}
                Err(_) => self
                    .artifacts
                    .settle_host_bytes(self.store.as_ref(), staged, false),
            }
        }
        result
    }

    fn publish(
        &self,
        claim: &Arc<Claim>,
        run: &StoredAnalysisRun,
        phase: AnalysisPhase,
        evidence: PhaseEvidence,
        staged: Option<&StagedFile>,
        synthesis: Vec<SynthesisEvent>,
    ) -> Result<StoredAnalysisRun, Failure> {
        let (current, selection) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                Ok(claim.run(read))
            })??;
        if current.revision != run.revision {
            return Err(StorageError::Busy.into());
        }
        let claim = claim.clone();
        let artifacts = self.artifacts.clone();
        let staged = staged.cloned();
        run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            claim
                .validate_in(tx, &current, &selection)
                .map_err(Failure::into_storage)?;
            if !matches!(
                evidence.state,
                OperationState::ObservedComplete
                    | OperationState::Failed
                    | OperationState::OutcomeUnknown
            ) || (staged.is_some() && evidence.state != OperationState::ObservedComplete)
                || (!synthesis.is_empty()
                    && (phase != AnalysisPhase::Synthesis || staged.is_none()))
            {
                return Err(StorageError::InvalidRecord(
                    "invalid phase settlement".into(),
                ));
            }
            let mut next = current.clone();
            let checkpoint = phase_mut(&mut next, phase)?;
            if checkpoint.state != OperationState::Dispatching
                || checkpoint.operation != evidence.attempt
            {
                return Err(StorageError::Busy);
            }
            let publication = staged
                .as_ref()
                .map(|staged| publish_artifact(&artifacts, tx, &current, phase, staged))
                .transpose()?;
            checkpoint.state = evidence.state;
            checkpoint.report = publication.clone();
            checkpoint.diagnostic = evidence.diagnostic.map(str::to_owned);
            checkpoint.finished_at_ms =
                (evidence.state != OperationState::OutcomeUnknown).then(|| tx.now_ms());
            next.state = run_state(&next);
            advance(tx, &current, &mut next)?;
            if let Some(reference) = publication {
                for event in synthesis {
                    event.append_in(tx, &next, reference.clone())?;
                }
            }
            if next.terminal() {
                retire_pin(tx, &next)?;
            }
            Ok(next)
        })
        .map_err(Failure::from)
    }

    pub(super) fn read<T: DeserializeOwned>(
        &self,
        ctx: &InvocationContext,
        reference: &ArtifactRef,
    ) -> Result<T, Failure> {
        if reference.media_type != REPORT_MEDIA {
            return Err(selection::unavailable());
        }
        let (bytes, _) = self
            .artifacts
            .read_exact(self.store.as_ref(), ctx, reference, MAX_REPORT_BYTES as u64)
            .map_err(|failure| match failure {
                ExactReadRefusal::Storage(error) => error.into(),
                _ => selection::unavailable(),
            })?;
        serde_json::from_slice(&bytes).map_err(|_| selection::unavailable())
    }
}

pub(super) fn phase_mut(
    run: &mut StoredAnalysisRun,
    phase: AnalysisPhase,
) -> Result<&mut StoredAnalysisPhase, StorageError> {
    run.phases
        .iter_mut()
        .find(|record| record.phase == phase)
        .ok_or_else(|| StorageError::InvalidRecord("analysis phase missing from intent".into()))
}

fn run_state(run: &StoredAnalysisRun) -> OperationState {
    use OperationState as S;
    if run
        .phases
        .iter()
        .any(|phase| phase.state == S::OutcomeUnknown)
    {
        S::OutcomeUnknown
    } else if run
        .phases
        .iter()
        .any(|phase| !matches!(phase.state, S::Failed | S::ObservedComplete))
    {
        S::Dispatching
    } else if run.phases.iter().any(|phase| phase.state == S::Failed) {
        S::Failed
    } else {
        S::ObservedComplete
    }
}

pub(super) fn advance(
    tx: &mut dyn WriteTransaction,
    previous: &StoredAnalysisRun,
    next: &mut StoredAnalysisRun,
) -> Result<(), StorageError> {
    next.revision = previous
        .revision
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("analysis revision exhausted".into()))?;
    next.updated_at_ms = tx.now_ms();
    tx.analysis().update_analysis_run(next, previous.revision)
}

fn renew_pin(tx: &mut dyn WriteTransaction, run: &StoredAnalysisRun) -> Result<(), StorageError> {
    let pin = tx
        .graph()
        .graph_pin(&run.pin_id)?
        .ok_or(StorageError::Cancelled)?;
    let expires = pin
        .view_witnesses
        .iter()
        .filter_map(|witness| witness.expires_at_ms)
        .fold(tx.now_ms().saturating_add(PIN_TTL_MS), i64::min);
    if expires > pin.expires_at_ms {
        tx.graph()
            .renew_graph_pin(&pin.id, &pin.owner_namespace, pin.revision, expires)?;
    }
    Ok(())
}

fn retire_pin(tx: &mut dyn WriteTransaction, run: &StoredAnalysisRun) -> Result<(), StorageError> {
    if let Some(pin) = tx.graph().graph_pin(&run.pin_id)? {
        tx.graph()
            .retire_graph_pin(&pin.id, &pin.owner_namespace, pin.revision)?;
    }
    Ok(())
}

fn publish_artifact(
    artifacts: &ArtifactService,
    tx: &mut dyn WriteTransaction,
    run: &StoredAnalysisRun,
    phase: AnalysisPhase,
    staged: &StagedFile,
) -> Result<ArtifactRef, StorageError> {
    let scope = match &run.intent.scope {
        DataScope::Workspace => ArtifactScope::Workspace,
        DataScope::Project { project_id } => ArtifactScope::Project(project_id.clone()),
    };
    let title = format!("Repository analysis {}", phase.key());
    let version = artifacts.commit_host_bytes_in_scope(
        tx,
        staged,
        HostArtifactPublication {
            title: &title,
            media_type: REPORT_MEDIA,
            provenance: json!({
                "purpose": "repository_analysis",
                "runId": run.id,
                "binding": run.binding_key,
                "phase": phase.key(),
            }),
            scope,
            owner: run.owner.actor.clone(),
        },
    )?;
    let invalid = || StorageError::InvalidRecord("analysis artifact version is invalid".into());
    Ok(ArtifactRef {
        artifact_id: version.artifact_id,
        version_id: version.id,
        sha256: version.blob_digest.try_into().map_err(|_| invalid())?,
        byte_length: Counter::new(version.byte_count).map_err(|_| invalid())?,
        media_type: version.media_type,
    })
}

/// Version 1 of the observed synthesis result handed to DW-53's documentation producer.
pub(super) struct SynthesisEvent {
    pub repository: Id,
    pub project_checkpoints: Vec<Id>,
    pub partial: bool,
}

impl SynthesisEvent {
    fn append_in(
        self,
        tx: &mut dyn WriteTransaction,
        run: &StoredAnalysisRun,
        result: ArtifactRef,
    ) -> Result<(), StorageError> {
        if self.project_checkpoints.is_empty()
            || !run
                .intent
                .views
                .iter()
                .any(|view| view.repository_id == self.repository)
        {
            return Err(StorageError::InvalidRecord(
                "synthesis evidence is incomplete".into(),
            ));
        }
        let bindings: Vec<Value> = run
            .intent
            .views
            .iter()
            .map(|view| {
                json!({
                    "repositoryId": view.repository_id, "viewId": view.view_id,
                    "viewDigest": view.view_digest, "commit": view.snapshot.head,
                })
            })
            .collect();
        let project_id = match &run.intent.scope {
            DataScope::Workspace => None,
            DataScope::Project { project_id } => Some(project_id.clone()),
        };
        let id = EventId(tx.new_id(IdKind::Event));
        let now = tx.now_ms();
        tx.history().append(NewEvent {
            id,
            actor: run.owner.actor.clone(),
            project_id,
            root_id: run.owner.root.clone(),
            operation_id: Some(run.active_operation.clone()),
            aggregate_kind: AGGREGATE.into(),
            aggregate_id: run.id.as_str().to_owned(),
            aggregate_revision: run.revision,
            kind: "RepositoryAnalysisSynthesized".into(),
            payload: json!({
                "schemaVersion": 1, "runId": run.id, "repositoryId": self.repository,
                "viewBindings": bindings, "result": result,
                "computedAtMs": now, "projectCheckpointIds": self.project_checkpoints,
                "coverage": if self.partial { "Partial" } else { "Complete" },
            }),
        })?;
        Ok(())
    }
}
