//! Agents' package tool calls: the production caller of [`PackageHost::invoke`].
//!
//! A recorded provider call of a selected package `Tool` contribution is queued ([`queue`]) as a
//! durable `package.invoke` operation, once per call: it pins the active manifest digest, the
//! invoking principal, its session and lineage, and the authority fences (principal binding and
//! access policy) current when the call was recorded. The dispatcher then runs it on
//! [`CONSUMER`]: the effect re-checks the fences and the session, invokes the contribution with
//! the call's arguments as input and the agent as the invoking principal (so its brokered
//! application calls run as that agent), and records what the package returned. Only artifacts
//! of the invocation itself (its input and its broker results) are accepted as output.
//!
//! A refused admission (no ready helper, shutting down) did not reach the package and is retried
//! under the consumer's policy. An invocation whose outcome cannot be established (the helper
//! died, the deadline passed and it had to be terminated) is `OutcomeUnknown`: the package's
//! effects cannot be observed afterwards, so it is held for a person and never re-run.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use crate::package_host::{
    InvocationKind, InvokeOutcome, InvokeRefusal, InvokeRequest, PackageHost,
};
use crate::providers::schemas::RegisteredSchemas;
use devworks_core::authority::authorization_holds;
use devworks_core::authority::policy::resolve_policy;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{
    ApplicationStore, NewEvent, NewOperation, NewQueueEntry, OperationRecord, ReadOptions,
    StorageError, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use devworks_core::ports::{AuthorityFence, Authorization, IdKind};
use devworks_core::protocol::provider::{CompleteToolCall, ToolCallKey, ToolSchema};
use devworks_core::protocol::{
    ActorId, AssignmentId, Counter, EventId, Id, OperationId, OperationState, ProjectId, Revision,
    RootId,
};
use devworks_core::providers::materialize::SchemaDocuments;
use devworks_extension_protocol::package::{InvocationOutcome, InvocationScope};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

/// The consumer that runs package tool invocations.
pub const CONSUMER: &str = "package-tools";
/// The journal capability of one package tool invocation.
pub const CAPABILITY: &str = "package.invoke";
/// How long one package tool invocation may run before the host cancels it.
pub const DEADLINE: Duration = Duration::from_secs(10 * 60);

/// What a queued invocation pinned when the call was recorded.
#[derive(Clone, Debug)]
struct Request {
    package_id: String,
    contribution_id: String,
    digest: String,
    input: wire::ArtifactRef,
    principal: ActorId,
    principal_kind: String,
    session_id: Id,
    project_id: Option<ProjectId>,
    root_id: Option<RootId>,
    assignment_id: Option<AssignmentId>,
    fences: Vec<AuthorityFence>,
    call: ToolCallKey,
}
impl Request {
    fn to_json(&self) -> Value {
        json!({
            "packageId": self.package_id, "contributionId": self.contribution_id,
            "digest": self.digest, "input": self.input, "principal": self.principal,
            "principalKind": self.principal_kind, "sessionId": self.session_id,
            "projectId": self.project_id, "rootId": self.root_id,
            "assignmentId": self.assignment_id, "fences": self.fences, "call": self.call,
        })
    }
    fn from_json(v: &Value) -> Option<Self> {
        macro_rules! field {
            ($name:literal) => {
                serde_json::from_value(v.get($name)?.clone()).ok()?
            };
        }
        Some(Self {
            package_id: field!("packageId"),
            contribution_id: field!("contributionId"),
            digest: field!("digest"),
            input: field!("input"),
            principal: field!("principal"),
            principal_kind: field!("principalKind"),
            session_id: field!("sessionId"),
            project_id: field!("projectId"),
            root_id: field!("rootId"),
            assignment_id: field!("assignmentId"),
            fences: field!("fences"),
            call: field!("call"),
        })
    }
}

/// Whether `capability` names a contribution of an installed package (not a built-in or MCP
/// tool).
pub(crate) fn is_package_tool(store: &dyn ApplicationStore, capability: &str) -> bool {
    if capability.starts_with("mcp/")
        || crate::providers::schemas::builtin_operation(capability).is_some()
    {
        return false;
    }
    let Some(package) = capability.split('/').next().map(str::to_owned) else {
        return false;
    };
    read_snapshot(store, &ReadOptions::default(), move |s| {
        s.packages().package(&package)
    })
    .is_ok_and(|p| p.is_some())
}

/// Queues one recorded package tool call as a durable `package.invoke` operation (module
/// documentation). Idempotent per call: the same call returns the same operation.
pub fn queue(
    store: &dyn ApplicationStore,
    schemas: &RegisteredSchemas,
    ctx: &InvocationContext,
    session_id: &Id,
    call: &CompleteToolCall,
) -> Result<OperationId, StorageError> {
    if ctx.session_id() != Some(session_id)
        || !matches!(
            ctx.principal_kind(),
            PrincipalKind::Agent | PrincipalKind::Host
        )
    {
        return Err(StorageError::ScopeDenied);
    }
    let key = &call.call;
    let identity = sha256_hex(
        format!(
            "{}\0{}\0{}\0{}",
            session_id.as_str(),
            key.attempt_id.0.as_str(),
            key.native_call_id,
            key.ordinal.as_str()
        )
        .as_bytes(),
    );
    let operation = OperationId(Id::new(format!("pkginv-{identity}")).expect("bounded id"));
    let capability = call.capability_id.as_str().to_owned();
    let digest = sha256_hex(
        format!(
            "{capability}\0{}\0{}\0{}",
            call.input_schema.sha256.as_str(),
            call.arguments.artifact_id.as_str(),
            call.arguments.version_id.as_str()
        )
        .as_bytes(),
    );
    let existing = read_snapshot(store, &ReadOptions::default(), |s| {
        s.operations().operation(&operation)
    })?;
    if let Some(existing) = existing {
        return if existing.capability == CAPABILITY && existing.args_digest == digest {
            Ok(operation)
        } else {
            Err(StorageError::ScopeDenied)
        };
    }
    // The tool must still be a selected, verified contribution of the active version.
    schemas
        .tool(
            ctx,
            &ToolSchema {
                capability_id: call.capability_id.clone(),
                wire_name: "package_tool".into(),
                input_schema: call.input_schema.clone(),
            },
        )
        .map_err(|_| StorageError::ScopeDenied)?;
    let package_id = capability
        .split('/')
        .next()
        .ok_or(StorageError::ScopeDenied)?
        .to_owned();
    let (active, fences) = read_snapshot(store, &ReadOptions::default(), |s| {
        let active = s
            .packages()
            .package(&package_id)?
            .and_then(|p| p.active_digest)
            .ok_or(StorageError::ScopeDenied)?;
        let policy = resolve_policy(s.configuration(), ctx.project_id())?;
        let mut fences = vec![AuthorityFence::Policy {
            project_id: ctx.project_id().cloned(),
            digest: policy.digest(),
        }];
        if ctx.principal_kind() == PrincipalKind::Agent {
            let binding = s
                .authority()
                .principal(ctx.principal())?
                .ok_or(StorageError::ScopeDenied)?;
            fences.push(AuthorityFence::Principal {
                actor: ctx.principal().clone(),
                revision: binding.revision,
            });
        }
        Ok((active, fences))
    })?;
    let request = Request {
        package_id,
        contribution_id: capability,
        digest: active,
        input: serde_json::to_value(&call.arguments)
            .and_then(serde_json::from_value)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        principal: ctx.principal().clone(),
        principal_kind: format!("{:?}", ctx.principal_kind()),
        session_id: session_id.clone(),
        project_id: ctx.project_id().cloned(),
        root_id: ctx.root_id().cloned(),
        assignment_id: ctx.assignment_id().cloned(),
        fences,
        call: key.clone(),
    };
    let ctx = ctx.clone();
    run_write(store, WriteOptions::default(), move |tx| {
        record(tx, &ctx, &operation, &digest, &request)
    })
}

fn record(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &OperationId,
    digest: &str,
    request: &Request,
) -> Result<OperationId, StorageError> {
    if let Some(existing) = tx.operations().operation(operation)? {
        return if existing.capability == CAPABILITY && existing.args_digest == digest {
            Ok(operation.clone())
        } else {
            Err(StorageError::ScopeDenied)
        };
    }
    if tx.store().store_id != *ctx.store_id()
        || tx.store().restore_epoch != ctx.restore_epoch().get()
        || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
        || !devworks_core::authority::fences_hold(tx, &request.fences)?
    {
        return Err(StorageError::ScopeDenied);
    }
    let session = tx
        .sessions()
        .session(&request.session_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if session.closed() || session.agent_id != *ctx.principal() {
        return Err(StorageError::ScopeDenied);
    }
    tx.operations().record_intent(NewOperation {
        id: operation.clone(),
        root_id: request.root_id.clone(),
        assignment_id: request.assignment_id.clone(),
        capability: CAPABILITY.into(),
        schema_version: 1,
        args_digest: digest.to_owned(),
        payload: request.to_json(),
    })?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event.clone(),
        actor: ctx.principal().clone(),
        project_id: request.project_id.clone(),
        root_id: request.root_id.clone(),
        operation_id: Some(operation.clone()),
        aggregate_kind: "package".into(),
        aggregate_id: request.package_id.clone(),
        aggregate_revision: 1,
        kind: "PackageToolRequested".into(),
        payload: json!({"contributionId": request.contribution_id, "digest": request.digest,
            "call": request.call}),
    })?;
    let entry = tx.new_id(IdKind::QueueEntry);
    let ready_at_ms = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id: entry,
        consumer: CONSUMER.into(),
        event_id: event,
        root_id: request.root_id.clone(),
        operation_id: Some(operation.clone()),
        partition: None,
        priority: 0,
        ready_at_ms,
        payload: json!({"packageId": request.package_id, "call": request.call}),
    })?;
    Ok(operation.clone())
}

struct Effects {
    store: Arc<dyn ApplicationStore>,
    host: Arc<OnceLock<Weak<PackageHost>>>,
}

/// Runs one dispatched `package.invoke` operation (blocking).
fn execute(
    store: &Arc<dyn ApplicationStore>,
    host: &Arc<OnceLock<Weak<PackageHost>>>,
    op: &OperationRecord,
) -> EffectResult {
    if op.state != OperationState::Dispatching || op.capability != CAPABILITY {
        return EffectResult::Failed("not a dispatched package tool invocation".into());
    }
    let Some(request) = Request::from_json(&op.payload) else {
        return EffectResult::Failed("invalid package tool invocation".into());
    };
    let kind = match request.principal_kind.as_str() {
        "Agent" => PrincipalKind::Agent,
        "Host" => PrincipalKind::Host,
        _ => return EffectResult::Failed("invalid package tool principal".into()),
    };
    let Some(host) = host.get().and_then(Weak::upgrade) else {
        return EffectResult::NotApplied("the package host is not running".into());
    };
    let info = store.info();
    let (Ok(epoch), Ok(policy)) = (Counter::new(info.restore_epoch.max(0)), Revision::new(1))
    else {
        return EffectResult::Failed("invalid store epoch".into());
    };
    let ctx = HostIngress::new(&**store)
        .context(
            request.principal.clone(),
            kind,
            op.id.0.clone(),
            Id::new("package-tools").expect("static ingress"),
            epoch,
            policy,
        )
        .with_lineage(
            request.project_id.clone(),
            request.root_id.clone(),
            request.assignment_id.clone(),
        )
        .with_session(request.session_id.clone());
    // The authority the call was recorded under must still hold, and the session must still be
    // the principal's open session.
    let current = read_snapshot(&**store, &ReadOptions::default(), |s| {
        if s.operations().operation(&op.id)?.as_ref() != Some(op)
            || !authorization_holds(
                s,
                &Authorization::direct().fenced_by(request.fences.clone()),
            )?
        {
            return Ok(false);
        }
        let session = s.sessions().session(&request.session_id)?;
        Ok(session.is_some_and(|session| {
            !session.closed()
                && session.agent_id == request.principal
                && session.project_id == request.project_id
        }))
    });
    match current {
        Ok(true) => {}
        Ok(false) => {
            return EffectResult::Failed("the call's authority or session changed".into());
        }
        Err(e) => return EffectResult::NotApplied(format!("{e:?}")),
    }
    let scope = match &request.project_id {
        Some(project) => match wire::Id::try_from(project.0.as_str().to_owned()) {
            Ok(project_id) => InvocationScope::Project {
                project_id: wire::ProjectId(project_id),
            },
            Err(_) => return EffectResult::Failed("invalid project".into()),
        },
        None => InvocationScope::Workspace,
    };
    let admitted = host.invoke(InvokeRequest {
        package_id: request.package_id.clone(),
        contribution_id: request.contribution_id.clone(),
        scope,
        input: request.input.clone(),
        operation_id: op.id.0.clone(),
        deadline: DEADLINE,
        allowed_operations: Vec::new(),
        kind: InvocationKind::Ordinary,
        principal: Some(ctx),
        expected_digest: Some(request.digest.clone()),
    });
    let handle = match admitted {
        Ok(handle) => handle,
        // Nothing reached the package: safe to try again under the retry policy.
        Err(
            refusal @ (InvokeRefusal::NotReady(_)
            | InvokeRefusal::ShuttingDown
            | InvokeRefusal::Storage(_)),
        ) => return EffectResult::NotApplied(format!("{refusal:?}")),
        Err(refusal) => return EffectResult::Failed(format!("{refusal:?}")),
    };
    // The host enforces the deadline; this wait only bounds a lost watcher.
    let outcome = handle
        .wait(DEADLINE + Duration::from_secs(60))
        .unwrap_or_else(|| handle.cancel());
    match outcome {
        InvokeOutcome::Finished(InvocationOutcome::Completed { artifacts }) => {
            if !artifacts.iter().all(|a| handle.in_scope(a)) {
                return EffectResult::Failed(
                    "the package returned artifacts that are not its invocation's".into(),
                );
            }
            EffectResult::Completed(json!({"outcome": "Completed", "artifacts": artifacts}))
        }
        InvokeOutcome::Finished(InvocationOutcome::Failed { code }) => {
            EffectResult::Failed(format!("the package failed: {}", code.as_str()))
        }
        InvokeOutcome::Finished(InvocationOutcome::CancelledBeforeEffect) => {
            EffectResult::Failed("the invocation was cancelled before any effect".into())
        }
        InvokeOutcome::Finished(InvocationOutcome::OutcomeUnknown { .. }) => {
            EffectResult::Unknown("the package reports its outcome unknown".into())
        }
        InvokeOutcome::Failed(why) => EffectResult::Failed(why),
        InvokeOutcome::Unknown(why) => EffectResult::Unknown(why),
    }
}

impl EffectExecutor for Effects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let (store, host) = (self.store.clone(), self.host.clone());
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || execute(&store, &host, &operation)).await {
                Ok(result) => result,
                Err(_) => EffectResult::Unknown("the package tool invocation panicked".into()),
            }
        })
    }

    fn observe(&self, _operation: OperationRecord) -> EffectFuture {
        // A package's effects cannot be read back after the fact: a person reconciles it.
        Box::pin(async {
            EffectResult::Unknown("a package invocation's outcome cannot be observed".into())
        })
    }
}

struct Settle;
impl DeliveryHandler for Settle {
    fn apply(
        &self,
        _tx: &mut dyn WriteTransaction,
        _delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        Ok(Outcome::Completed)
    }
}

/// The consumer that runs queued package tool invocations on `host` (set once it exists).
pub fn consumer(
    store: Arc<dyn ApplicationStore>,
    host: Arc<OnceLock<Weak<PackageHost>>>,
) -> Consumer {
    Consumer {
        name: CONSUMER.into(),
        handler: Arc::new(Settle),
        effect: Some(Arc::new(Effects { store, host })),
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry: RetryPolicy {
            base_delay_ms: 500,
            max_delay_ms: 30_000,
            max_attempts: 4,
        },
        max_in_flight: None,
    }
}
