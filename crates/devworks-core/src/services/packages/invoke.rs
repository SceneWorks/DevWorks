//! `addons.invoke` and `addons.invocation`: a person runs a package tool.
//!
//! `addons.invoke` records one durable [`CAPABILITY`] operation for the calling person and
//! queues it for [`CONSUMER`], the same consumer that runs an agent's package tool calls. It
//! pins the active manifest digest and the access policy current when the person asked, and
//! returns the operation with its state; the invocation runs afterwards, once. Its brokered
//! application and inference calls run as that person, under their current authority and
//! narrowed by the version's approved manifest grants. A result that cannot be established is
//! held for reconciliation and never run again.
//!
//! Before it records anything, [`PackageService::verify_invocation`] runs outside the unit of
//! work what an agent's package tool call is verified with: the tool is a verified contribution
//! of the active version (its descriptor matches its approved digest and its input schema is a
//! shipped, hash-checked document), the person can read the input artifact, and the input
//! satisfies that schema. A failure refuses the command and records nothing.
//!
//! `addons.invocation` reads one such operation back: its state and, once it completed, the
//! artifacts its package returned or, when it failed, the recorded reason. It answers only the
//! person who started the invocation.
use super::{PackageService, Prep, conflict, invalid, not_found, policy};
use crate::authority::policy::resolve_policy;
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::operations::addons as ops;
use crate::ports::packages::InspectedFile;
use crate::ports::storage::{
    NewEvent, NewOperation, NewQueueEntry, OperationRecord, ReadSnapshot, StorageError,
    WriteTransaction,
};
use crate::ports::{AuthorityFence, IdKind, Preparation};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::PackageInvocation;
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{DomainErrorCode, EventId, OperationId, OperationState};
use crate::services::artifacts::lifecycle::ExactReadRefusal;
use crate::services::kernel::{Failure, Refusal};
use serde_json::json;

/// The journal capability of one package tool invocation.
pub const CAPABILITY: &str = "package.invoke";
/// The consumer that runs queued package tool invocations.
pub const CONSUMER: &str = "package-tools";

/// What verifying an invocation outside the unit of work found: the manifest digest it verified
/// the tool and input against, or the refusal.
pub(super) type Verified = Result<String, (DomainErrorCode, &'static str)>;

/// The largest input artifact or schema document of an invocation that is read.
const MAX_DOCUMENT: u64 = 8 * 1024 * 1024;

impl PackageService {
    /// Verifies the tool and input of `i` for `ctx`'s person (see the module documentation).
    pub(super) fn verify_invocation(
        &self,
        ctx: &InvocationContext,
        i: &ops::InvokeInput,
        env: &Preparation<'_>,
    ) -> Result<Verified, StorageError> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Ok(Err((
                DomainErrorCode::PermissionDenied,
                "package_invocation",
            )));
        }
        let unavailable = (DomainErrorCode::NotFound, "package_tool");
        let found = env.read(|s| {
            let Some(active) = s
                .packages()
                .package(i.package_id.as_str())?
                .filter(|p| p.installed && p.enabled)
                .and_then(|p| p.active_digest)
            else {
                return Ok(None);
            };
            Ok(s.packages()
                .package_version(i.package_id.as_str(), &active)?
                .map(|version| (active, version)))
        })?;
        let Some((digest, version)) = found else {
            return Ok(Err((DomainErrorCode::NotFound, "package_version")));
        };
        let Some(mut inspection) = policy::inspection(&version) else {
            return Ok(Err(unavailable));
        };
        // A version stored with an older compact inspection has no descriptors or schema
        // assets: recover them from the bundle it retains (digest re-verified), or refuse.
        if super::recovery::compact(&inspection) {
            match super::recovery::recover(&*self.store, &self.artifacts, self, ctx, &version) {
                Ok(full) => inspection = full,
                Err(_) => return Ok(Err(unavailable)),
            }
        }
        // The tool is a contribution whose verified descriptor is the approved one.
        let Some((contribution, descriptor)) = inspection
            .contributions
            .iter()
            .find(|c| c.id == i.contribution_id.0.as_str() && c.kind == "Tool")
            .and_then(|c| Some((c, c.descriptor.as_ref()?)))
        else {
            return Ok(Err(unavailable));
        };
        if crate::fingerprint::canonical_digest(descriptor)
            .ok()
            .as_deref()
            != Some(contribution.contract_digest.as_str())
        {
            return Ok(Err(unavailable));
        }
        // Its input schema is a document the package ships, whose bytes are the installed ones.
        let schema_file = inspection
            .schema_assets
            .iter()
            .find(|a| a.reference == descriptor["meta"]["inputSchema"])
            .and_then(|a| inspection.files.iter().find(|f| f.path == a.path));
        let Some(schema) =
            schema_file.and_then(|file| self.read_schema(&inspection.package_id, &digest, file))
        else {
            return Ok(Err(unavailable));
        };
        // The person can read the input, and it satisfies the schema.
        let bytes = match self
            .artifacts
            .read_exact(&*self.store, ctx, &i.input, MAX_DOCUMENT)
        {
            Ok((bytes, _)) => bytes,
            Err(ExactReadRefusal::Storage(e)) => return Err(e),
            Err(_) => return Ok(Err((DomainErrorCode::InvalidArgument, "input"))),
        };
        let Some(value) = crate::framing::parse(&bytes)
            .ok()
            .and_then(|object| serde_json::to_value(object).ok())
        else {
            return Ok(Err((DomainErrorCode::InvalidArgument, "input")));
        };
        if self.validator.validate(&schema, &value).is_err() {
            return Ok(Err((DomainErrorCode::InvalidArgument, "input")));
        }
        Ok(Ok(digest))
    }

    fn read_schema(
        &self,
        package: &str,
        digest: &str,
        file: &InspectedFile,
    ) -> Option<serde_json::Value> {
        let bytes = self
            .files
            .read_verified(package, digest, file, MAX_DOCUMENT)
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Records the calling person's invocation of a tool contribution of the active version.
    pub(super) fn invoke(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &ops::InvokeInput,
        prepared: Option<&Prep>,
    ) -> Result<DomainDecision, Failure> {
        if ctx.principal_kind() != PrincipalKind::Human {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "package_invocation",
                None,
            )
            .into());
        }
        let package = tx
            .packages()
            .package(i.package_id.as_str())?
            .filter(|p| p.installed)
            .ok_or_else(|| not_found("package", i.package_id.as_str()))?;
        let active = package
            .active_digest
            .clone()
            .filter(|_| package.enabled)
            .ok_or_else(|| conflict("package_version", i.package_id.as_str()))?;
        if let Some(expected) = &i.expected_digest
            && String::from(expected.clone()) != active
        {
            return Err(conflict("package_version", i.package_id.as_str()).into());
        }
        let version = tx
            .packages()
            .package_version(i.package_id.as_str(), &active)?
            .ok_or_else(|| not_found("package_version", i.package_id.as_str()))?;
        let is_tool = policy::inspection(&version).is_some_and(|inspection| {
            inspection
                .contributions
                .iter()
                .any(|c| c.id == i.contribution_id.0.as_str() && c.kind == "Tool")
        });
        if !is_tool {
            return Err(not_found("package_tool", i.contribution_id.0.as_str()).into());
        }
        let input_is_live = tx
            .artifacts()
            .version(&i.input.version_id)?
            .is_some_and(|v| {
                v.live()
                    && v.artifact_id == i.input.artifact_id
                    && v.blob_digest == String::from(i.input.sha256.clone())
                    && v.byte_count == i.input.byte_length.get()
            });
        if !input_is_live {
            return Err(invalid("input").into());
        }
        // What was verified outside this unit of work is what is recorded: the same version.
        match prepared {
            Some(Prep::Invoke(Ok(verified))) if *verified == active => {}
            Some(Prep::Invoke(Err((code, field)))) => {
                return Err(Refusal::new(*code, field, None).into());
            }
            _ => return Err(conflict("package_version", i.package_id.as_str()).into()),
        }
        let policy_digest = resolve_policy(&*tx.configuration(), ctx.project_id())?.digest();
        let fences = vec![AuthorityFence::Policy {
            project_id: ctx.project_id().cloned(),
            digest: policy_digest,
        }];
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let payload = json!({
            "packageId": i.package_id, "contributionId": i.contribution_id,
            "digest": active, "input": i.input, "principal": ctx.principal(),
            "principalKind": "Human", "sessionId": null, "projectId": ctx.project_id(),
            "rootId": null, "assignmentId": null, "fences": fences, "call": null,
            "activation": null,
        });
        let args_digest = sha256_hex(
            format!(
                "{}\0{}\0{}\0{}\0{}",
                i.package_id.as_str(),
                i.contribution_id.0.as_str(),
                active,
                i.input.artifact_id.as_str(),
                i.input.version_id.as_str()
            )
            .as_bytes(),
        );
        tx.operations().record_intent(NewOperation {
            id: operation.clone(),
            root_id: None,
            assignment_id: None,
            capability: CAPABILITY.into(),
            schema_version: 1,
            args_digest,
            payload,
        })?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: None,
            operation_id: Some(operation.clone()),
            aggregate_kind: super::AGGREGATE.into(),
            aggregate_id: i.package_id.as_str().to_owned(),
            aggregate_revision: 1,
            kind: "PackageToolRequested".into(),
            payload: json!({"contributionId": i.contribution_id, "digest": active}),
        })?;
        let entry = tx.new_id(IdKind::QueueEntry);
        let ready_at_ms = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: entry,
            consumer: CONSUMER.into(),
            event_id: event,
            root_id: None,
            operation_id: Some(operation.clone()),
            partition: None,
            priority: 0,
            ready_at_ms,
            payload: json!({"packageId": i.package_id, "operationId": operation}),
        })?;
        let recorded = tx.operations().operation(&operation)?.ok_or_else(|| {
            StorageError::InvalidRecord("the recorded invocation is missing".into())
        })?;
        Ok(DomainDecision::Applied {
            result: OperationResult::AddonsInvoke {
                value: invocation(&recorded),
            },
        })
    }

    /// The state and result of an invocation `ctx`'s person started.
    pub(super) fn invocation_view(
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        i: &ops::InvocationInput,
    ) -> Result<OperationResult, Failure> {
        let found = snapshot
            .operations()
            .operation(&i.operation_id)?
            .filter(|op| {
                op.capability == CAPABILITY
                    && op.payload["principalKind"] == "Human"
                    && op.payload["principal"].as_str() == Some(ctx.principal().0.as_str())
            })
            .ok_or_else(|| not_found("package_invocation", i.operation_id.0.as_str()))?;
        Ok(OperationResult::AddonsInvocation {
            value: invocation(&found),
        })
    }
}

/// What `operation` shows a person: its state and, from the observation that settled it, the
/// artifacts its package returned or why it failed.
fn invocation(operation: &OperationRecord) -> PackageInvocation {
    let settled = operation
        .attempts
        .iter()
        .rev()
        .flat_map(|a| a.observations.iter().rev())
        .find_map(|o| o.get("effect").filter(|e| e["outcome"] != "unknown"));
    let artifacts: Vec<ArtifactRef> = match (operation.state, settled) {
        (OperationState::ObservedComplete, Some(effect)) => effect["evidence"]["artifacts"]
            .as_array()
            .map(|all| {
                all.iter()
                    .filter_map(|a| serde_json::from_value(a.clone()).ok())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    let failure = match (operation.state, settled) {
        (OperationState::Failed, Some(effect)) => effect["cause"].as_str().map(|cause| {
            BoundedText(
                cause
                    .chars()
                    .filter(|c| *c != '\0')
                    .take(65_536)
                    .collect::<String>(),
            )
        }),
        _ => None,
    };
    PackageInvocation {
        operation_id: operation.id.clone(),
        state: operation.state,
        artifacts: BoundedList(artifacts),
        failure,
    }
}
