//! Brokered inference: a package granted `inference` asks for one text completion with
//! `broker.call` of kind `Inference`, and gets the answer as an artifact of its invocation.
//!
//! * **Who pays.** The request runs as one text-only inference in the invoking agent's session,
//!   through the same router, pin, budget admission and authority checks as the agent's own turns
//!   (the routed sampling of [`crate::mcp_sampling`]). It needs the dispatching `package.invoke`
//!   tool call an agent made: an invocation without one (a person's, an event handler's) has no
//!   session to route or account against, and is refused.
//! * **What is recorded.** The attempt's usage and cost are priced and recorded by the usage
//!   ledger against that session and the invocation's tool call, under the `Other` report
//!   category, and count against the budget that admits the agent's own inference. A retried
//!   call under the same action key returns the recorded answer and is never sent again.
//! * **The request** is an artifact of the invocation in the host's request schema
//!   ([`request_schema`]): `{"messages": [{"role": "user"|"assistant", "text"}], "systemPrompt"?,
//!   "maxTokens"}`. The answer is published as `{"model", "text", "truncated"}`, owned by the
//!   invoking agent, and becomes one of the invocation's artifacts.
use super::session::{Slot, rpc_error};
use super::{PackageHost, SlotContext};
use crate::mcp_sampling::McpSampling;
use crate::providers::driver::NativeProviders;
use devworks_core::authority::authorization_holds;
use devworks_core::context::{HostIngress, PrincipalKind};
use devworks_core::fingerprint::canonical_digest;
use devworks_core::ports::mcp::{SamplingCall, SamplingMessage};
use devworks_core::ports::storage::{
    ApplicationStore, ArtifactScope, ReadOptions, WriteOptions, read_snapshot, run_write,
};
use devworks_core::ports::{AuthorityFence, Authorization};
use devworks_core::protocol::provider::{ArtifactRef as HostArtifact, ToolCallKey};
use devworks_core::protocol::{
    ActorId, AssignmentId, Counter, Id, OperationId, OperationState, ProjectId, Revision, RootId,
};
use devworks_core::services::artifacts::{
    ArtifactService, host::HostArtifactPublication, version_ref,
};
use devworks_core::services::mcp::SamplingGrant;
use devworks_core::services::router::InferenceRouter;
use devworks_extension_protocol::package::{
    BrokerCall, BrokerInvoke, RpcError, RpcErrorClass, RpcResult,
};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use std::sync::{Arc, Weak};

/// The schema ID of a package inference request.
pub const REQUEST_SCHEMA_ID: &str = "devworks/package.inference.request";
/// Largest request artifact.
const MAX_REQUEST: u64 = 256 * 1024;
/// Most output tokens one request may ask for.
const MAX_TOKENS: u32 = 4096;

/// What brokered inference needs, attached once by the runtime when providers are composed.
pub struct InferencePorts {
    pub providers: Weak<NativeProviders>,
    pub router: Arc<InferenceRouter>,
    pub runtime: tokio::runtime::Handle,
}

/// The host's request schema document (its digest is the `sha256` a call must name).
pub fn request_schema() -> Value {
    json!({
        "$id": REQUEST_SCHEMA_ID, "type": "object", "additionalProperties": false,
        "required": ["messages", "maxTokens"],
        "properties": {
            "messages": {"type": "array", "minItems": 1, "maxItems": 64, "items": {
                "type": "object", "additionalProperties": false, "required": ["role", "text"],
                "properties": {"role": {"enum": ["user", "assistant"]},
                    "text": {"type": "string", "maxLength": 65536}}}},
            "systemPrompt": {"type": "string", "maxLength": 16384},
            "maxTokens": {"type": "integer", "minimum": 1, "maximum": MAX_TOKENS}
        }
    })
}

/// A package inference request as the package sent it.
#[derive(Clone, Debug)]
pub struct PackageInferenceRequest {
    pub messages: Vec<RequestMessage>,
    pub system_prompt: Option<String>,
    pub max_tokens: u32,
}
#[derive(Clone, Debug)]
pub struct RequestMessage {
    pub role: String,
    pub text: String,
}
impl PackageInferenceRequest {
    /// Decodes a request in the host's schema strictly (no other member anywhere).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let v: Value = devworks_core::framing::parse(bytes).ok()?;
        let o = v.as_object()?;
        if o.keys()
            .any(|k| !matches!(k.as_str(), "messages" | "systemPrompt" | "maxTokens"))
        {
            return None;
        }
        let messages = o
            .get("messages")?
            .as_array()?
            .iter()
            .map(|m| {
                let m = m.as_object().filter(|m| m.len() == 2)?;
                Some(RequestMessage {
                    role: m.get("role")?.as_str()?.to_owned(),
                    text: m.get("text")?.as_str()?.to_owned(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let system_prompt = match o.get("systemPrompt") {
            None => None,
            Some(s) => Some(s.as_str()?.to_owned()),
        };
        let max_tokens = u32::try_from(o.get("maxTokens")?.as_u64()?).ok()?;
        Some(Self {
            messages,
            system_prompt,
            max_tokens,
        })
    }
}
impl PackageInferenceRequest {
    /// The routed sampling call of this request, when it is within its bounds.
    pub fn sampling_call(&self) -> Option<SamplingCall> {
        let valid = !self.messages.is_empty()
            && self.messages.len() <= 64
            && (1..=MAX_TOKENS).contains(&self.max_tokens)
            && self.system_prompt.as_ref().is_none_or(|s| s.len() <= 16384)
            && self
                .messages
                .iter()
                .all(|m| matches!(m.role.as_str(), "user" | "assistant") && m.text.len() <= 65536);
        valid.then(|| SamplingCall {
            ordinal: 1,
            messages: self
                .messages
                .iter()
                .map(|m| SamplingMessage {
                    assistant: m.role == "assistant",
                    text: m.text.clone(),
                })
                .collect(),
            system_prompt: self.system_prompt.clone(),
            max_tokens: self.max_tokens,
            temperature: None,
            stop_sequences: vec![],
        })
    }
}

/// The grant of the dispatching `package.invoke` tool call `operation`: the invoking agent, its
/// session and the authority fences its call was recorded under, all still current.
pub fn grant_for(
    store: &dyn ApplicationStore,
    operation: &OperationId,
) -> Result<SamplingGrant, &'static str> {
    let op = read_snapshot(store, &ReadOptions::default(), |s| {
        s.operations().operation(operation)
    })
    .map_err(|_| "the invocation's tool call cannot be read")?
    .ok_or("the invocation has no agent tool call to account inference against")?;
    let p = &op.payload;
    if op.capability != "package.invoke"
        || op.state != OperationState::Dispatching
        || p["principalKind"].as_str() != Some("Agent")
    {
        return Err("the invocation has no dispatching agent tool call");
    }
    let field = |name: &str| p.get(name).cloned().unwrap_or(Value::Null);
    let parsed = (
        serde_json::from_value::<ActorId>(field("principal")),
        serde_json::from_value::<Id>(field("sessionId")),
        serde_json::from_value::<Option<ProjectId>>(field("projectId")),
        serde_json::from_value::<Option<RootId>>(field("rootId")),
        serde_json::from_value::<Option<AssignmentId>>(field("assignmentId")),
        serde_json::from_value::<Vec<AuthorityFence>>(field("fences")),
        serde_json::from_value::<ToolCallKey>(field("call")),
        serde_json::from_value::<Option<(Id, i64)>>(field("activation")),
    );
    let (
        Ok(principal),
        Ok(session_id),
        Ok(project_id),
        Ok(root_id),
        Ok(assignment_id),
        Ok(fences),
        Ok(call),
        Ok(activation),
    ) = parsed
    else {
        return Err("the invocation's tool call is invalid");
    };
    let attempt_id = op
        .latest_attempt()
        .map(|a| a.id.0.clone())
        .ok_or("the invocation's tool call has no attempt")?;
    let current = read_snapshot(store, &ReadOptions::default(), |s| {
        if !authorization_holds(s, &Authorization::direct().fenced_by(fences.clone()))? {
            return Ok(false);
        }
        Ok(s.sessions().session(&session_id)?.is_some_and(|session| {
            !session.closed() && session.agent_id == principal && session.project_id == project_id
        }))
    })
    .map_err(|_| "the invocation's authority cannot be read")?;
    if !current {
        return Err("the invoking agent's authority or session changed");
    }
    let info = store.info();
    let (Ok(epoch), Ok(policy)) = (Counter::new(info.restore_epoch.max(0)), Revision::new(1))
    else {
        return Err("invalid store epoch");
    };
    let context = HostIngress::new(store)
        .context(
            principal,
            PrincipalKind::Agent,
            op.id.0.clone(),
            Id::new("package-inference").expect("static ingress"),
            epoch,
            policy,
        )
        .with_lineage(project_id, root_id, assignment_id)
        .with_session(session_id);
    let context = match activation {
        Some((activation, epoch)) => context.with_activation(activation, epoch),
        None => context,
    };
    Ok(SamplingGrant {
        context,
        operation_id: op.id.clone(),
        attempt_id,
        call,
        fences,
    })
}

/// Runs `request` for the package call `action_key` of the invocation whose tool call is
/// `operation`, through the router (module documentation); the answer as `{"model", "text",
/// "truncated"}`.
#[allow(clippy::too_many_arguments)]
pub async fn infer(
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    providers: Arc<NativeProviders>,
    router: Arc<InferenceRouter>,
    grant: &SamplingGrant,
    action_key: &str,
    request: &PackageInferenceRequest,
    cancel: Option<devworks_core::ports::storage::Cancellation>,
) -> Result<Value, &'static str> {
    let call = request
        .sampling_call()
        .ok_or("the inference request is outside its bounds")?;
    let sampling = McpSampling::for_package(store, artifacts, providers, router, action_key.into());
    let sampled = sampling
        .sample(grant, &call, cancel)
        .await
        .map_err(|e| match e {
            devworks_core::ports::mcp::SamplingRefusal::Unauthorized(why)
            | devworks_core::ports::mcp::SamplingRefusal::InvalidRequest(why)
            | devworks_core::ports::mcp::SamplingRefusal::Failed(why) => why,
        })?;
    Ok(json!({"model": sampled.model, "text": sampled.text, "truncated": sampled.truncated}))
}

fn denied(message: &str) -> RpcError {
    rpc_error(RpcErrorClass::PermissionDenied, message)
}

/// Answers one `broker.call` of kind `Inference` whose package the approved manifest grants.
pub(super) fn call(
    host: &PackageHost,
    ctx: &SlotContext,
    slot: &Arc<Slot>,
    params: &BrokerInvoke,
) -> Result<RpcResult, RpcError> {
    let BrokerCall::Inference {
        input_schema,
        request,
    } = &params.call
    else {
        return Err(rpc_error(
            RpcErrorClass::ProtocolViolation,
            "not an inference call",
        ));
    };
    let schema_digest = canonical_digest(&request_schema()).expect("static schema");
    if input_schema.schema_id.as_str() != REQUEST_SCHEMA_ID
        || String::from(input_schema.revision.clone()) != "1"
        || String::from(input_schema.sha256.clone()) != schema_digest
    {
        return Err(rpc_error(
            RpcErrorClass::ProtocolViolation,
            "the input schema is not the host's inference request schema",
        ));
    }
    if !ctx.in_scope(request) {
        return Err(denied("the request is not an artifact of this invocation"));
    }
    let (Some(ports), Some(broker)) = (host.inference.get(), host.broker.get()) else {
        return Err(rpc_error(
            RpcErrorClass::Unavailable,
            "brokered inference is not composed in this host",
        ));
    };
    let Some(providers) = ports.providers.upgrade() else {
        return Err(rpc_error(
            RpcErrorClass::Unavailable,
            "the application stopped",
        ));
    };
    let grant = grant_for(
        &*host.store,
        &OperationId(Id::new(ctx.operation_id.clone()).map_err(|_| denied("invalid invocation"))?),
    )
    .map_err(denied)?;
    let reference: HostArtifact = serde_json::to_value(request)
        .and_then(serde_json::from_value)
        .map_err(|_| rpc_error(RpcErrorClass::ProtocolViolation, "invalid request artifact"))?;
    let bytes = broker
        .artifacts
        .read_exact(&*host.store, &grant.context, &reference, MAX_REQUEST)
        .map_err(|_| denied("the request artifact is not readable by the invoking agent"))?
        .0;
    let parsed = PackageInferenceRequest::parse(&bytes).ok_or_else(|| {
        rpc_error(
            RpcErrorClass::ProtocolViolation,
            "the request does not match the inference request schema",
        )
    })?;
    let answer = ports
        .runtime
        .block_on(infer(
            host.store.clone(),
            broker.artifacts.clone(),
            providers,
            ports.router.clone(),
            &grant,
            params.action_key.as_str(),
            &parsed,
            Some(slot.cancellation.clone()),
        ))
        .map_err(|why| rpc_error(RpcErrorClass::Unavailable, why))?;
    // The answer is the invocation's: owned by the invoking agent, in its scope.
    let staged = broker
        .artifacts
        .stage_host_bytes(
            &*host.store,
            answer.to_string().as_bytes(),
            json!({"kind": "PackageInferenceResult", "invocationOperationId": ctx.operation_id}),
        )
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| rpc_error(RpcErrorClass::OutcomeUnknown, "the answer was not saved"))?;
    let (service, pending, owner) = (
        broker.artifacts.clone(),
        staged.clone(),
        grant.context.principal().clone(),
    );
    let scope = grant
        .context
        .project_id()
        .cloned()
        .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
    let (operation, live) = (ctx.operation_id.clone(), slot.clone());
    let committed = run_write(&*host.store, WriteOptions::default(), move |tx| {
        // Serialized with revocation: a cancelled or expired invocation publishes nothing.
        let _guard = live.state.lock().unwrap_or_else(|p| p.into_inner());
        if live.revoked.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(devworks_core::ports::storage::StorageError::ScopeDenied);
        }
        let version = service.commit_host_bytes_in_scope(
            tx,
            &pending,
            HostArtifactPublication {
                title: "Package inference answer",
                media_type: "application/json",
                provenance: json!({"kind": "PackageInferenceResult",
                    "invocationOperationId": operation}),
                scope,
                owner,
            },
        )?;
        Ok(version_ref(&version))
    });
    broker
        .artifacts
        .settle_host_bytes(&*host.store, &staged, committed.is_ok());
    let produced = committed
        .map_err(|_| rpc_error(RpcErrorClass::OutcomeUnknown, "the answer was not saved"))?;
    let produced: wire::ArtifactRef = serde_json::to_value(&produced)
        .and_then(serde_json::from_value)
        .map_err(|_| rpc_error(RpcErrorClass::OutcomeUnknown, "invalid result artifact"))?;
    ctx.admit_artifact(&produced);
    Ok(RpcResult::BrokerCompleted { artifact: produced })
}
