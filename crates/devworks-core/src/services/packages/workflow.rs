//! Durable admission for approved package workflows. A workflow operation freezes its source,
//! step, and required gates before the dispatcher can produce any results.
use super::{PackageService, conflict, invalid, invoke, not_found, policy};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::addons::RunWorkflowInput;
use crate::ports::Preparation;
use crate::ports::storage::{
    EventFilter, EventRecord, MAX_PAGE, PackageRegistryRead, QueueState, StorageError,
    StoredPackageVersion, WriteTransaction,
};
use crate::protocol::results::DomainDecision;
use crate::protocol::{DomainErrorCode, Id, ProjectId};
use crate::services::artifacts::exact_reference_readable_in;
use crate::services::kernel::{Failure, Refusal};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

pub const OPERATION: &str = "addons.run_workflow";
pub const MAX_GATES: usize = 16;

struct Target<'a> {
    package: &'a Id,
    contribution: &'a str,
    digest: &'a str,
    role: &'static str,
}

fn targets(i: &RunWorkflowInput) -> impl Iterator<Item = Target<'_>> {
    std::iter::once(Target {
        package: &i.package_id,
        contribution: i.contribution_id.0.as_str(),
        digest: &i.manifest_digest.0,
        role: "Step",
    })
    .chain(i.required_gates.0.iter().map(|gate| Target {
        package: &gate.package_id,
        contribution: gate.contribution_id.0.as_str(),
        digest: &gate.manifest_digest.0,
        role: "VerificationGate",
    }))
}

fn eligible(
    registry: &dyn PackageRegistryRead,
    target: &Target<'_>,
    project: Option<&ProjectId>,
) -> Result<Option<StoredPackageVersion>, StorageError> {
    let id = target.package.as_str();
    let Some(record) = registry.package(id)? else {
        return Ok(None);
    };
    if !record.installed
        || !record.enabled
        || record.active_digest.as_deref() != Some(target.digest)
        || project.is_some_and(|project| policy::project_disabled(&record, project.0.as_str()))
    {
        return Ok(None);
    }
    let version = registry.package_version(id, target.digest)?;
    Ok(version.filter(|version| {
        version.approval.as_ref().is_some_and(|approval| {
            matches!(
                approval["kind"].as_str(),
                Some("Person" | "TrustedPublisher")
            )
        })
    }))
}

impl PackageService {
    pub(super) fn prepare_workflow(
        &self,
        ctx: &InvocationContext,
        i: &RunWorkflowInput,
        env: &Preparation<'_>,
    ) -> Result<invoke::Verified, StorageError> {
        let bad_input = (DomainErrorCode::InvalidArgument, "workflow");
        if i.required_gates.0.len() > MAX_GATES
            || targets(i)
                .map(|target| target.contribution)
                .collect::<BTreeSet<_>>()
                .len()
                != i.required_gates.0.len() + 1
        {
            return Ok(Err(bad_input));
        }
        let input = match self
            .artifacts
            .read_exact(&*self.store, ctx, &i.input, 8 * 1024 * 1024)
        {
            Ok((bytes, _)) => bytes,
            Err(crate::services::artifacts::lifecycle::ExactReadRefusal::Storage(error)) => {
                return Err(error);
            }
            Err(_) => return Ok(Err(bad_input)),
        };
        let Ok(input) = crate::framing::parse(&input) else {
            return Ok(Err(bad_input));
        };
        let unavailable = (DomainErrorCode::NotFound, "workflow_contribution");
        for target in targets(i) {
            let Some(version) = env.read(|s| eligible(s.packages(), &target, ctx.project_id()))?
            else {
                return Ok(Err(unavailable));
            };
            let Some(mut inspection) = policy::inspection(&version) else {
                return Ok(Err(unavailable));
            };
            if super::recovery::compact(&inspection) {
                let Ok(recovered) =
                    super::recovery::recover(&*self.store, &self.artifacts, self, ctx, &version)
                else {
                    return Ok(Err(unavailable));
                };
                inspection = recovered;
            }
            let Some(descriptor) = inspection
                .contributions
                .iter()
                .find(|entry| entry.id == target.contribution && entry.kind == "Workflow")
                .and_then(|entry| entry.verified_descriptor())
                .filter(|descriptor| descriptor["role"] == target.role)
            else {
                return Ok(Err(unavailable));
            };
            let Some(document) = inspection.schema_file(&descriptor["meta"]["inputSchema"]) else {
                return Ok(Err(unavailable));
            };
            if inspection
                .schema_file(&descriptor["meta"]["outputSchema"])
                .is_none()
                || inspection
                    .schema_file(&descriptor["evidenceSchema"])
                    .is_none()
            {
                return Ok(Err(unavailable));
            }
            if target.role == "Step" {
                let Some(schema) =
                    self.read_schema(target.package.as_str(), target.digest, document)
                else {
                    return Ok(Err(unavailable));
                };
                if self.validator.validate(&schema, &input).is_err() {
                    return Ok(Err(bad_input));
                }
            }
        }
        Ok(Ok(i.manifest_digest.0.clone()))
    }

    pub(super) fn run_workflow(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        i: &RunWorkflowInput,
        prepared: Option<&super::Prep>,
    ) -> Result<DomainDecision, Failure> {
        if !matches!(
            ctx.principal_kind(),
            PrincipalKind::Agent | PrincipalKind::Host | PrincipalKind::Human
        ) {
            return Err(Refusal::new(DomainErrorCode::PermissionDenied, "workflow", None).into());
        }
        match prepared {
            Some(super::Prep::Workflow(Ok(digest))) if *digest == i.manifest_digest.0 => {}
            Some(super::Prep::Workflow(Err((code, field)))) => {
                return Err(Refusal::new(*code, field, None).into());
            }
            _ => return Err(conflict("package_version", i.package_id.as_str()).into()),
        }
        let mut versions = BTreeMap::new();
        for target in targets(i) {
            let version = eligible(tx.packages(), &target, ctx.project_id())?
                .ok_or_else(|| not_found("workflow_contribution", target.contribution))?;
            versions.insert(target.package.as_str(), version.version);
        }
        if !exact_reference_readable_in(tx, ctx, &i.input)? {
            return Err(invalid("input").into());
        }
        let workflow_key = crate::fingerprint::canonical_digest(&json!({
            "project":ctx.project_id(), "root":ctx.root_id(), "workflow":i.workflow_id,
        }))
        .map_err(|error| StorageError::InvalidRecord(format!("workflow identity: {error:?}")))?;
        let mut prior_step = None;
        if let Some(previous) = latest_run(tx, &workflow_key)? {
            let prior: RunWorkflowInput =
                serde_json::from_value(previous.payload["workflow"].clone()).map_err(|_| {
                    StorageError::InvalidRecord("invalid recorded workflow binding".into())
                })?;
            let unchanged_or_newer = |package: &Id, old_digest: &str, new_digest: &str| {
                if old_digest == new_digest {
                    return true;
                }
                let before = previous.payload["workflowVersions"][package.as_str()]
                    .as_str()
                    .and_then(|version| semver::Version::parse(version).ok());
                let after = versions
                    .get(package.as_str())
                    .and_then(|version| semver::Version::parse(version).ok());
                before
                    .zip(after)
                    .is_some_and(|(before, after)| after.cmp_precedence(&before).is_gt())
            };
            let keeps_required_gates = prior.required_gates.0.iter().all(|old| {
                i.required_gates.0.iter().any(|current| {
                    old.package_id == current.package_id
                        && old.contribution_id.0 == current.contribution_id.0
                        && unchanged_or_newer(
                            &old.package_id,
                            &old.manifest_digest.0,
                            &current.manifest_digest.0,
                        )
                })
            });
            if prior.package_id != i.package_id
                || prior.contribution_id.0 != i.contribution_id.0
                || !unchanged_or_newer(
                    &i.package_id,
                    &prior.manifest_digest.0,
                    &i.manifest_digest.0,
                )
                || !keeps_required_gates
            {
                return Err(conflict("workflow_binding", i.workflow_id.as_str()).into());
            }
            let prior_operation = recorded_operation(tx, &previous)?;
            if unsettled(tx, &prior_operation)? {
                return Err(conflict("workflow_in_flight", i.workflow_id.as_str()).into());
            }
            if prior_operation.state == crate::protocol::OperationState::Failed
                && prior.input.artifact_id == i.input.artifact_id
                && prior.input.version_id == i.input.version_id
                && prior.input.sha256 == i.input.sha256
                && prior.manifest_digest == i.manifest_digest
            {
                prior_step = prior_operation
                    .attempts
                    .iter()
                    .rev()
                    .flat_map(|attempt| attempt.observations.iter().rev())
                    .find_map(|observation| observation.get("workflowStep").cloned());
            }
        }
        let mut fences = command.authorization().fences().to_vec();
        let digest =
            crate::authority::policy::resolve_policy(tx.configuration(), ctx.project_id())?
                .digest();
        fences.push(crate::ports::AuthorityFence::Policy {
            project_id: ctx.project_id().cloned(),
            digest,
        });
        let payload = json!({
            "packageId": i.package_id, "contributionId": i.contribution_id,
            "digest": i.manifest_digest, "input": i.input, "principal": ctx.principal(),
            "principalKind": format!("{:?}", ctx.principal_kind()), "sessionId": ctx.session_id(),
            "projectId": ctx.project_id(), "rootId": ctx.root_id(),
            "assignmentId": ctx.assignment_id(), "fences": fences, "call": null,
            "activation": ctx.activation(), "workflow": i, "workflowKey": workflow_key,
            "workflowVersions": versions, "priorStep": prior_step,
            "restoreEpoch": ctx.restore_epoch(),
        });
        let request_digest = crate::fingerprint::canonical_digest(&payload)
            .map_err(|error| StorageError::InvalidRecord(format!("workflow digest: {error:?}")))?;
        let operation = invoke::queue_invocation(tx, ctx, payload, request_digest)?;
        Ok(DomainDecision::Accepted {
            operation_id: operation.id,
        })
    }
}

fn latest_run(
    tx: &mut dyn WriteTransaction,
    workflow: &str,
) -> Result<Option<EventRecord>, StorageError> {
    let filter = EventFilter::Aggregate {
        kind: "package_workflow".into(),
        id: workflow.into(),
    };
    let mut cursor = 0;
    loop {
        let mut page = tx.history().page(&filter, cursor, MAX_PAGE)?;
        if !page.has_more {
            return Ok(page.events.pop());
        }
        cursor = page.next_after;
    }
}

fn recorded_operation(
    tx: &mut dyn WriteTransaction,
    run: &EventRecord,
) -> Result<crate::ports::storage::OperationRecord, StorageError> {
    let operation = run
        .operation_id
        .as_ref()
        .ok_or_else(|| StorageError::InvalidRecord("the workflow has no operation".into()))?;
    tx.operations()
        .operation(operation)?
        .ok_or_else(|| StorageError::InvalidRecord("the workflow operation is missing".into()))
}

fn unsettled(
    tx: &mut dyn WriteTransaction,
    record: &crate::ports::storage::OperationRecord,
) -> Result<bool, StorageError> {
    if crate::ports::storage::is_terminal(record.state) {
        return Ok(false);
    }
    let entry: Id = serde_json::from_value(record.payload["deliveryId"].clone())
        .map_err(|_| StorageError::InvalidRecord("invalid workflow delivery identity".into()))?;
    Ok(!tx.queue().entry(&entry)?.is_some_and(|entry| {
        matches!(entry.state, QueueState::Failed | QueueState::Dismissed)
            && record.state == crate::protocol::OperationState::RetryPending
    }))
}

/// Rechecks package approval, scope and all exact artifact references at settlement.
pub fn evidence_current(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    plan: &RunWorkflowInput,
    evidence: &serde_json::Value,
) -> Result<bool, StorageError> {
    for target in targets(plan) {
        if eligible(tx.packages(), &target, ctx.project_id())?.is_none() {
            return Ok(false);
        }
    }
    if !exact_reference_readable_in(tx, ctx, &plan.input)? {
        return Ok(false);
    }
    let Some(results) = evidence["artifacts"].as_array() else {
        return Ok(false);
    };
    for reference in results.iter().chain(
        evidence["requiredGates"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|gate| &gate["result"]),
    ) {
        let Ok(reference) = serde_json::from_value(reference.clone()) else {
            return Ok(false);
        };
        if !exact_reference_readable_in(tx, ctx, &reference)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Requests cancellation of the exact queue entry attached to a workflow operation.
pub(crate) fn request_cancel(
    tx: &mut dyn WriteTransaction,
    operation: &crate::protocol::OperationId,
) -> Result<(), StorageError> {
    let Some(record) = tx.operations().operation(operation)? else {
        return Ok(());
    };
    if crate::ports::storage::is_terminal(record.state)
        || record.capability != invoke::CAPABILITY
        || !record.payload["workflow"].is_object()
    {
        return Ok(());
    }
    let entry: Id = serde_json::from_value(record.payload["deliveryId"].clone())
        .map_err(|_| StorageError::InvalidRecord("invalid workflow delivery identity".into()))?;
    let Some(current) = tx.queue().entry(&entry)? else {
        return Ok(());
    };
    if current.consumer != invoke::CONSUMER || current.operation_id.as_ref() != Some(operation) {
        return Err(StorageError::ScopeDenied);
    }
    if !current.cancel_requested {
        tx.queue().cancel(&entry)?;
    }
    Ok(())
}
