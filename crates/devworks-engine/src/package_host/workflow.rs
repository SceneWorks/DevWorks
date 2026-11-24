//! Executes one workflow operation recorded by the shared package service. The selected gate list
//! and exact source artifact are part of the command receipt before the first helper is invoked.
use super::{
    InvocationKind, InvokeOutcome, InvokeRefusal, InvokeRequest, PackageHost,
    contributions::scope_of,
};
use crate::dispatch::EffectResult;
use devworks_core::context::InvocationContext;
use devworks_core::operations::addons::RunWorkflowInput;
use devworks_core::ports::packages::Inspection;
use devworks_core::ports::storage::{
    OperationRecord, ReadOptions, StorageError, WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::{Id, OperationId, OperationState};
use devworks_extension_protocol::package::{
    Contribution, ContributionEvent, ContributionMeta, InvocationOutcome, WorkflowRole,
};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const MAX_DOCUMENT: u64 = 8 * 1024 * 1024;
const BUDGET: Duration = Duration::from_secs(10 * 60);

struct Target<'a> {
    package: &'a str,
    contribution: &'a str,
    digest: &'a str,
}

struct ResultSet {
    result: ArtifactRef,
    document: Value,
    evidence: Vec<ArtifactRef>,
}

enum WorkflowFailure {
    Failed(String),
    Unknown(String),
    Unsent(String),
}

impl From<String> for WorkflowFailure {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

impl From<&str> for WorkflowFailure {
    fn from(error: &str) -> Self {
        Self::Failed(error.into())
    }
}

impl PackageHost {
    pub(crate) fn execute_workflow(
        &self,
        principal: &InvocationContext,
        record: &OperationRecord,
        delivery: &Id,
        plan: &RunWorkflowInput,
    ) -> EffectResult {
        match self.execute_bound_workflow(principal, record, delivery, plan) {
            Ok(evidence) => EffectResult::Completed(evidence),
            Err(WorkflowFailure::Failed(cause)) => EffectResult::Failed(cause),
            Err(WorkflowFailure::Unknown(cause)) => EffectResult::Unknown(cause),
            Err(WorkflowFailure::Unsent(cause)) => EffectResult::NotApplied(cause),
        }
    }

    fn execute_bound_workflow(
        &self,
        principal: &InvocationContext,
        record: &OperationRecord,
        delivery: &Id,
        plan: &RunWorkflowInput,
    ) -> Result<Value, WorkflowFailure> {
        let operation = &record.id;
        let stopped = || {
            read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                Ok(read.queue().entry(delivery)?.is_none_or(|entry| {
                    entry.operation_id.as_ref() != Some(operation) || entry.cancel_requested
                }))
            })
            .unwrap_or(true)
        };
        if stopped() {
            return Err(WorkflowFailure::Unsent(
                "workflow cancelled before dispatch".into(),
            ));
        }
        let source = self.workflow_json(principal, &plan.input)?;
        let deadline = Instant::now() + BUDGET;
        let step = if let Some(prior) = record
            .payload
            .get("priorStep")
            .filter(|value| !value.is_null())
        {
            let result =
                serde_json::from_value(prior.clone()).map_err(|error| error.to_string())?;
            let document = self
                .workflow_json(principal, &result)
                .map_err(WorkflowFailure::Unknown)?;
            ResultSet {
                result,
                document,
                evidence: Vec::new(),
            }
        } else {
            self.run_workflow_target(
                principal,
                operation,
                Target {
                    package: plan.package_id.as_str(),
                    contribution: plan.contribution_id.0.as_str(),
                    digest: plan.manifest_digest.as_str(),
                },
                &plan.input,
                &source,
                InvocationKind::WorkflowStep,
                deadline,
                &stopped,
            )?
        };
        self.record_step(record, &step.result)?;
        let mut artifacts = vec![step.result.clone()];
        let mut gates = Vec::new();
        for gate in &plan.required_gates.0 {
            let package = gate.package_id.as_str();
            let digest = gate.manifest_digest.as_str();
            let input = json!({
                "workflowOperation": operation, "source": plan.input, "sourceDocument": source,
                "stepResult": step.result, "stepDocument": step.document,
            });
            let bytes = serde_json::to_vec(&input).map_err(|error| error.to_string())?;
            if bytes.len() as u64 > MAX_DOCUMENT {
                return Err("the gate's source and step data exceed the workflow bound".into());
            }
            let gate_input = self.stage_contribution_input(
                principal,
                (package, digest),
                &bytes,
                json!({"kind": "PackageWorkflowGate", "workflowOperation": operation, "source": plan.input,
                    "stepResult": step.result, "gate": gate}),
            )?;
            let result = self
                .run_workflow_target(
                    principal,
                    operation,
                    Target {
                        package,
                        digest,
                        contribution: &gate.contribution_id.0,
                    },
                    &gate_input,
                    &input,
                    InvocationKind::WorkflowGate,
                    deadline,
                    &stopped,
                )
                .map_err(|error| match error {
                    // A step may already have executed. Never retry the whole workflow after it.
                    WorkflowFailure::Unsent(cause) => WorkflowFailure::Failed(cause),
                    other => other,
                })?;
            artifacts.extend(result.evidence.iter().cloned());
            gates.push(
                json!({"binding": gate, "result": result.result, "evidence": result.evidence}),
            );
        }
        if stopped() {
            return Err("the workflow was cancelled before its evidence could settle".into());
        }
        let current = self.active_contributions(principal, "Workflow")?;
        if !current.iter().any(|entry| {
            entry.package_id == plan.package_id.as_str()
                && entry.manifest_digest == plan.manifest_digest.as_str()
        }) || plan.required_gates.0.iter().any(|gate| {
            !current.iter().any(|entry| {
                entry.package_id == gate.package_id.as_str()
                    && entry.manifest_digest == gate.manifest_digest.as_str()
            })
        }) {
            return Err("a bound workflow version changed during the run".into());
        }
        self.workflow_json(principal, &plan.input)?;
        Ok(
            json!({"outcome": "Completed", "artifacts": artifacts, "source": plan.input,
            "step": step.result, "requiredGates": gates}),
        )
    }

    fn record_step(
        &self,
        record: &OperationRecord,
        result: &ArtifactRef,
    ) -> Result<(), WorkflowFailure> {
        let id = record.id.clone();
        let fence = record.latest_attempt().map(|attempt| attempt.fence());
        let result = result.clone();
        run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            let current = tx
                .operations()
                .operation(&id)?
                .ok_or_else(|| StorageError::InvalidRecord("missing workflow operation".into()))?;
            let attempt = current
                .latest_attempt()
                .ok_or_else(|| StorageError::InvalidRecord("missing workflow attempt".into()))?;
            if current.state != OperationState::Dispatching || Some(attempt.fence()) != fence {
                return Err(StorageError::ScopeDenied);
            }
            tx.operations()
                .observe(&id, attempt.attempt_no, json!({"workflowStep":result}))
        })
        .map_err(|error| WorkflowFailure::Unknown(format!("workflow step receipt: {error}")))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_workflow_target(
        &self,
        principal: &InvocationContext,
        operation: &OperationId,
        target: Target<'_>,
        input: &ArtifactRef,
        document: &Value,
        kind: InvocationKind,
        deadline: Instant,
        stopped: &dyn Fn() -> bool,
    ) -> Result<ResultSet, WorkflowFailure> {
        if stopped() {
            return Err(WorkflowFailure::Unsent(
                "workflow target cancelled before dispatch".into(),
            ));
        }
        if Instant::now() >= deadline {
            return Err("the workflow reached its deadline".into());
        }
        let inspection = self
            .active_contributions(principal, "Workflow")?
            .into_iter()
            .find(|version| {
                version.package_id == target.package && version.manifest_digest == target.digest
            })
            .ok_or("the bound workflow version is unavailable")?;
        let (meta, evidence_schema) = workflow_metadata(&inspection, target.contribution, &kind)?;
        self.service
            .validator()
            .validate(
                &self.contribution_schema(&inspection, &meta.input_schema)?,
                document,
            )
            .map_err(|_| "workflow input does not match the selected schema")?;
        let output_schema = self.contribution_schema(&inspection, &meta.output_schema)?;
        let handle = self
            .invoke(InvokeRequest {
                package_id: target.package.into(),
                contribution_id: target.contribution.into(),
                scope: scope_of(principal)?,
                input: serde_json::to_value(input)
                    .and_then(serde_json::from_value)
                    .map_err(|error| error.to_string())?,
                operation_id: operation.0.clone(),
                deadline: deadline.saturating_duration_since(Instant::now()),
                allowed_operations: Vec::new(),
                kind: kind.clone(),
                principal: Some(principal.clone()),
                expected_digest: Some(target.digest.into()),
            })
            .map_err(|error| match error {
                InvokeRefusal::NotReady(_)
                | InvokeRefusal::ShuttingDown
                | InvokeRefusal::Storage(_) => {
                    WorkflowFailure::Unsent(format!("workflow helper was not invoked: {error:?}"))
                }
                other => WorkflowFailure::Failed(format!("workflow helper was refused: {other:?}")),
            })?;
        let outcome = loop {
            let time = deadline.saturating_duration_since(Instant::now());
            if time.is_zero() || stopped() {
                break handle.cancel();
            }
            if let Some(done) = handle.wait(time.min(Duration::from_millis(250))) {
                break done;
            }
        };
        let artifacts = match outcome {
            InvokeOutcome::Finished(InvocationOutcome::Completed { artifacts }) => artifacts,
            InvokeOutcome::Finished(InvocationOutcome::Failed { code }) => {
                return Err(format!("workflow helper failed: {}", code.as_str()).into());
            }
            InvokeOutcome::Finished(InvocationOutcome::OutcomeUnknown { .. })
            | InvokeOutcome::Unknown(_) => {
                return Err(WorkflowFailure::Unknown(
                    "the workflow helper outcome is unknown".into(),
                ));
            }
            _ => return Err("the workflow helper did not complete".into()),
        };
        let [result] = artifacts.as_slice() else {
            return Err("the workflow helper must return one result artifact".into());
        };
        let read = |reference: &wire::ArtifactRef, schema: &Value| -> Result<_, WorkflowFailure> {
            if !handle.produced(reference) {
                return Err("a workflow artifact was not produced by this invocation".into());
            }
            let reference = serde_json::to_value(reference)
                .and_then(serde_json::from_value)
                .map_err(|error| error.to_string())?;
            let document = self.workflow_json(principal, &reference)?;
            self.service
                .validator()
                .validate(schema, &document)
                .map_err(|_| "workflow output does not match its selected schema")?;
            Ok((reference, document))
        };
        let (result, document) = read(result, &output_schema)?;
        let mut collected = Vec::new();
        if matches!(kind, InvocationKind::WorkflowGate) {
            let (events, dropped) = handle.events();
            let [
                ContributionEvent::Verification {
                    passed: true,
                    evidence,
                },
            ] = events.as_slice()
            else {
                return Err(format!("required gate {} did not pass", target.contribution).into());
            };
            if dropped != 0 || evidence.is_empty() || evidence.len() > 4 {
                return Err("the gate must return one to four evidence artifacts".into());
            }
            let evidence_schema = self.contribution_schema(&inspection, &evidence_schema)?;
            for reference in evidence {
                collected.push(read(reference, &evidence_schema)?.0);
            }
        }
        Ok(ResultSet {
            result,
            document,
            evidence: collected,
        })
    }

    fn workflow_json(
        &self,
        principal: &InvocationContext,
        artifact: &ArtifactRef,
    ) -> Result<Value, String> {
        let ports = self
            .broker
            .get()
            .ok_or("package artifacts are not available")?;
        let (bytes, _) = ports
            .artifacts
            .read_exact(&*self.store, principal, artifact, MAX_DOCUMENT)
            .map_err(|_| "the workflow artifact is unavailable")?;
        devworks_core::framing::parse(&bytes).map_err(|error| error.to_string())
    }
}

fn workflow_metadata(
    inspection: &Inspection,
    id: &str,
    kind: &InvocationKind,
) -> Result<(ContributionMeta, wire::SchemaRef), String> {
    let descriptor = inspection
        .contributions
        .iter()
        .find(|entry| entry.id == id && entry.kind == "Workflow")
        .and_then(|entry| entry.verified_descriptor())
        .ok_or("the selected workflow contribution cannot be verified")?;
    let Contribution::Workflow {
        meta,
        role,
        evidence_schema,
    } = serde_json::from_value(descriptor.clone()).map_err(|error| error.to_string())?
    else {
        return Err("the selected descriptor is not a workflow contribution".into());
    };
    if !matches!(
        (kind, role),
        (InvocationKind::WorkflowStep, WorkflowRole::Step)
            | (InvocationKind::WorkflowGate, WorkflowRole::VerificationGate)
    ) {
        return Err("the workflow contribution does not have the selected role".into());
    }
    Ok((meta, evidence_schema))
}
