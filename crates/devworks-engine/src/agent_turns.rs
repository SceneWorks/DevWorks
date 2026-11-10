//! The first provider input of a materialized agent turn comes from its durable assignment
//! context. The agent cannot replace its role text, project or work brief through tool arguments.
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::fingerprint::sha256_hex;
use devworks_core::ports::storage::{
    ActivationOutcome, ApplicationStore, AssignmentState, Cancellation, ReadOptions, StorageError,
    read_snapshot,
};
use devworks_core::ports::usage::{PriceQuery, PriceResolver};
use devworks_core::protocol::provider::{
    ArtifactRef, EndpointProfile, NativeBinding, TranscriptBody,
};
use devworks_core::protocol::provider::{
    Attribution, EffortOption, InferenceInput, InferenceOptions, InferencePurpose,
    InferenceRequest, ModelSelection,
};
use devworks_core::protocol::records::{
    AgentRecord, EffortName, RouteChoice, RouteIntent, TierName,
};
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::{AttemptId, CommandReply, Counter, Id, Protocol, Revision};
use devworks_core::providers::context::{
    ContentBlock, GenerationParameters, Message, MessageRole, ProviderContext, TextBlock,
};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use tokio::sync::watch;

use devworks_core::services::prices::CatalogPriceResolver;
use devworks_core::services::router::accounting::{AttemptSelection, PinFence, ReportCategory};
use devworks_core::services::router::{Purpose, RequestAdmission};
use devworks_core::services::usage::accounting::AttemptRequest;
use devworks_core::services::usage::auxiliary;
use devworks_core::services::usage::meter::InputSemantics;
use devworks_core::services::usage::pricing::PricingContext;
use devworks_core::settings::catalog::AgentRole;

use crate::Runtime;
use crate::agent_supervisor::AgentTurnExecutor;
use crate::providers::driver::NativeInvocation;
use crate::runs::{LaunchEnd, LaunchFuture};
use crate::store;
use devworks_core::services::runs::substrate::ActivationFailure;
use devworks_core::services::usage::accounting::AttemptEnd;

/// The desktop-owned turn driver holds only a weak runtime reference so the supervisor does not
/// keep its own runtime alive after shutdown.
pub struct NativeAgentTurnExecutor {
    runtime: Weak<Runtime>,
}

impl NativeAgentTurnExecutor {
    pub fn new(runtime: &Arc<Runtime>) -> Self {
        Self {
            runtime: Arc::downgrade(runtime),
        }
    }
}

impl AgentTurnExecutor for NativeAgentTurnExecutor {
    fn run(
        &self,
        context: InvocationContext,
        coordinator: AgentRecord,
        stop: watch::Receiver<bool>,
    ) -> LaunchFuture {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            let Some(runtime) = runtime.upgrade() else {
                return LaunchEnd::Finished {
                    outcome: ActivationOutcome::Interrupted,
                    detail: Some("runtime stopped before agent turn".into()),
                };
            };
            drive_agent_turn(runtime, context, coordinator, stop).await
        })
    }
}

fn turn_failure(error: StorageError) -> LaunchEnd {
    let detail = format!("native agent turn: {error:?}");
    let failure = match error {
        StorageError::ScopeDenied
        | StorageError::InvalidRecord(_)
        | StorageError::NotFound { .. } => ActivationFailure::Deterministic(detail),
        _ => ActivationFailure::Transient(detail),
    };
    LaunchEnd::Failed(failure)
}

fn cancelled() -> LaunchEnd {
    LaunchEnd::Finished {
        outcome: ActivationOutcome::Cancelled,
        detail: Some("agent turn stopped".into()),
    }
}

async fn drive_agent_turn(
    runtime: Arc<Runtime>,
    ctx: InvocationContext,
    coordinator: AgentRecord,
    mut stop: watch::Receiver<bool>,
) -> LaunchEnd {
    if *stop.borrow() {
        return cancelled();
    }
    let Some(session_id) = ctx.session_id().cloned() else {
        return turn_failure(StorageError::ScopeDenied);
    };
    let Some(assignment_id) = ctx.assignment_id().cloned() else {
        return turn_failure(StorageError::ScopeDenied);
    };
    let Some((activation_id, epoch)) = ctx.activation() else {
        return turn_failure(StorageError::ScopeDenied);
    };
    let activation_id = activation_id.clone();
    let control = runtime.runs().service().clone();
    let root_id = coordinator.root_id.clone();
    let coordinator_assignment = assignment_id.clone();
    let execution = activation_id.clone();
    let ack = store::write(runtime.store(), move |tx| {
        let assignment = tx
            .runs()
            .assignment(&coordinator_assignment)?
            .ok_or(StorageError::ScopeDenied)?;
        if assignment.parent_id.is_some() {
            return Ok(Ok(()));
        }
        let revision = tx
            .runs()
            .root_control(&root_id)?
            .ok_or(StorageError::ScopeDenied)?
            .instruction_revision;
        control
            .acknowledge_instructions(tx, &execution, epoch, revision)
            .map(|result| result.map(|_| ()))
    })
    .await;
    match ack {
        Ok(Ok(())) => {}
        Ok(Err(fenced)) => {
            return LaunchEnd::Failed(ActivationFailure::Transient(format!(
                "agent instruction acknowledgement fenced: {fenced:?}"
            )));
        }
        Err(error) => return turn_failure(error),
    }
    let control = runtime.runs().service().clone();
    let session = session_id.clone();
    let turn = match store::write(runtime.store(), move |tx| {
        control.begin_turn(tx, &session, &assignment_id, &activation_id, epoch)
    })
    .await
    {
        Ok(Ok(turn)) => turn,
        Ok(Err(fenced)) => {
            return LaunchEnd::Failed(ActivationFailure::Transient(format!(
                "agent turn fenced: {fenced:?}"
            )));
        }
        Err(error) => return turn_failure(error),
    };
    let cancellation = Cancellation::new();
    let signal = cancellation.clone();
    let bridge = tokio::spawn(async move {
        while !*stop.borrow() && stop.changed().await.is_ok() {}
        signal.cancel();
    });
    let result = run_native_loop(&runtime, &ctx, &coordinator, &cancellation).await;
    bridge.abort();
    let control = runtime.runs().service().clone();
    let turn_id = turn.turn_id;
    let end = store::write(runtime.store(), move |tx| {
        control.end_turn(tx, &session_id, &turn_id)
    })
    .await;
    match end {
        Ok(Ok(())) => {}
        Ok(Err(fenced)) => {
            return LaunchEnd::Failed(ActivationFailure::Transient(format!(
                "agent turn end fenced: {fenced:?}"
            )));
        }
        Err(error) => return turn_failure(error),
    }
    match result {
        Ok(()) if cancellation.is_cancelled() => cancelled(),
        Ok(()) => LaunchEnd::Finished {
            outcome: ActivationOutcome::Completed,
            detail: None,
        },
        Err(StorageError::Cancelled) if cancellation.is_cancelled() => cancelled(),
        Err(error) => turn_failure(error),
    }
}

async fn run_native_loop(
    runtime: &Runtime,
    ctx: &InvocationContext,
    agent: &AgentRecord,
    cancellation: &Cancellation,
) -> Result<(), StorageError> {
    let session_id = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    let mut recovered = runtime.recover_native_context_head(ctx, session_id)?;
    let mut window = 0;
    let mut completed_windows = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let (mut head, mut binding, observed_sequence) = match recovered.take() {
            Some(value) => value,
            None => {
                let tools_required = !runtime
                    .providers()
                    .ok_or(StorageError::ScopeDenied)?
                    .schemas()
                    .selected_agent_tools(ctx)?
                    .is_empty();
                let route = route_context_head(runtime, ctx, session_id, 0, None, tools_required)?;
                let (head, binding) = publish_first_context(runtime, ctx, agent, &route)?;
                (head, binding, 0)
            }
        };
        if let Some(unpublished) =
            unpublished_turn(runtime.store().as_ref(), ctx, session_id, observed_sequence)?
        {
            let (attempt_id, has_tools) = match unpublished {
                UnpublishedTurn::Settled {
                    attempt_id,
                    has_tools,
                } => (attempt_id, has_tools),
                UnpublishedTurn::Interrupted {
                    attempt_id,
                    sequence,
                } => {
                    let bytes = runtime
                        .artifacts()
                        .read_exact(runtime.store().as_ref(), ctx, &head, 8 * 1024 * 1024)
                        .map_err(|_| StorageError::ScopeDenied)?
                        .0;
                    let mut context: ProviderContext =
                        serde_json::from_slice(&bytes).map_err(|_| StorageError::ScopeDenied)?;
                    context.messages.push(Message {
                        role: MessageRole::User,
                        content: vec![ContentBlock::Text {
                            text: "Host notice: The previous assistant response was interrupted before completion. Continue from the last completed step; do not assume the partial response was applied.".into(),
                            cache: None,
                        }],
                    });
                    crate::tool_turn::publish_continuation(
                        runtime.store().as_ref(),
                        runtime.artifacts(),
                        ctx,
                        session_id,
                        &attempt_id,
                        &binding,
                        crate::tool_turn::ContinuationMaterial {
                            context,
                            observed_sequence: sequence,
                        },
                    )?;
                    recovered = runtime.recover_native_context_head(ctx, session_id)?;
                    continue;
                }
            };
            if has_tools {
                runtime.dispatch_pending_tool_calls_cancellable(
                    ctx,
                    session_id,
                    Some(cancellation),
                )?;
                if !crate::tool_turn::pending_calls(runtime.store().as_ref(), ctx, session_id)?
                    .is_empty()
                {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    recovered = Some((head, binding, observed_sequence));
                    continue;
                }
            }
            let material = crate::tool_turn::continuation_context(
                runtime.store().as_ref(),
                runtime.artifacts(),
                ctx,
                crate::tool_turn::TurnSource {
                    session_id,
                    attempt_id: &attempt_id,
                    after_sequence: observed_sequence,
                    has_tools,
                },
                &binding,
                &head,
            )?;
            crate::tool_turn::publish_continuation(
                runtime.store().as_ref(),
                runtime.artifacts(),
                ctx,
                session_id,
                &attempt_id,
                &binding,
                material,
            )?;
            recovered = runtime.recover_native_context_head(ctx, session_id)?;
            continue;
        }
        head = runtime.refresh_native_tool_context(ctx, session_id, &head)?;
        let selected_count = runtime
            .providers()
            .ok_or(StorageError::ScopeDenied)?
            .schemas()
            .selected_agent_tools(ctx)?
            .len();
        let tools_required = selected_count != 0;
        let windows = tool_window_count(selected_count);
        window %= windows;
        let route = route_context_head(
            runtime,
            ctx,
            session_id,
            binding.context_epoch.get(),
            Some(head.sha256.as_str()),
            tools_required,
        )?;
        let next = binding_for_route(runtime, &route, binding.context_epoch.get())?;
        if next.connection_id != binding.connection_id
            || next.model_id != binding.model_id
            || next.account_revision != binding.account_revision
            || next.endpoint_profile != binding.endpoint_profile
        {
            let epoch = binding
                .context_epoch
                .get()
                .checked_add(1)
                .ok_or(StorageError::ScopeDenied)?;
            let next = NativeBinding {
                context_epoch: Counter::new(epoch).map_err(|_| StorageError::ScopeDenied)?,
                ..next
            };
            head = runtime.change_provider_epoch(ctx, session_id, &head, &binding, &next)?;
            binding = next;
        }
        head = runtime.adopt_instruction_epoch(ctx, session_id, &head, &binding, &[])?;
        binding = runtime
            .recover_native_context_head(ctx, session_id)?
            .ok_or(StorageError::ScopeDenied)?
            .1;
        let invocation = plan_generation_window(runtime, ctx, &route, &binding, &head, window)?;
        let progress = runtime
            .produce_and_dispatch_native_turn(invocation, Some(cancellation.clone()))
            .await?;
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        match progress.provider.result.outcome {
            Some(AttemptEnd::Completed) if progress.continuation.is_some() => {
                completed_windows += 1;
                if completed_windows >= windows {
                    return Ok(());
                }
                window = (window + 1) % windows;
                recovered = runtime.recover_native_context_head(ctx, session_id)?;
            }
            Some(AttemptEnd::ToolCallsReady) if progress.continuation.is_some() => {
                completed_windows = 0;
                window = (window + 1) % windows;
                recovered = runtime.recover_native_context_head(ctx, session_id)?;
            }
            Some(AttemptEnd::ToolCallsReady) => {
                // An approval or a still-running operation can delay the initial result. The
                // native request is durable: replaying its key resumes the pending dispatches
                // without sending a second provider request.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                recovered = runtime
                    .recover_native_context_head(ctx, session_id)?
                    .or(Some((head, binding, observed_sequence)));
            }
            Some(AttemptEnd::Cancelled) => return Err(StorageError::Cancelled),
            outcome => {
                return Err(StorageError::InvalidRecord(format!(
                    "agent inference did not yield a settled continuation: {outcome:?}; faults: {:?}",
                    progress.provider.result.faults
                )));
            }
        }
    }
}

/// A provider exchange can be durable before its next context head is published. Recover that
/// exchange from the transcript cursor first; a refresh only accepts operation updates.
enum UnpublishedTurn {
    Settled {
        attempt_id: AttemptId,
        has_tools: bool,
    },
    Interrupted {
        attempt_id: AttemptId,
        sequence: i64,
    },
}

fn unpublished_turn(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
    after_sequence: i64,
) -> Result<Option<UnpublishedTurn>, StorageError> {
    read_snapshot(store, &ReadOptions::default(), |read| {
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.agent_id != *ctx.principal() || session.closed() {
            return Err(StorageError::ScopeDenied);
        }
        let mut position = after_sequence;
        let mut attempt = None;
        let mut has_tools = false;
        let mut interrupted = false;
        while position < session.last_durable_sequence() {
            if position - after_sequence >= 10_000 {
                return Err(StorageError::InvalidRecord(
                    "unpublished turn exceeds scan bound".into(),
                ));
            }
            let page = read.sessions().entries(session_id, position, 500)?;
            if page.is_empty() {
                return Err(StorageError::InvalidRecord(
                    "unpublished turn transcript gap".into(),
                ));
            }
            for entry in page {
                if entry.sequence != position + 1 {
                    return Err(StorageError::InvalidRecord(
                        "unpublished turn transcript gap".into(),
                    ));
                }
                position = entry.sequence;
                match serde_json::from_value::<TranscriptBody>(entry.body)
                    .map_err(|_| StorageError::ScopeDenied)?
                {
                    TranscriptBody::AssistantAttempt {
                        attempt_id,
                        interrupted: ended,
                        ..
                    } => {
                        if attempt.as_ref().is_some_and(|known| known != &attempt_id) {
                            return Err(StorageError::InvalidRecord(
                                "multiple unpublished turns".into(),
                            ));
                        }
                        attempt = Some(attempt_id);
                        interrupted = ended;
                    }
                    TranscriptBody::ToolCallRecorded { tool_call } => {
                        let call_attempt = tool_call.call.attempt_id;
                        if attempt.as_ref().is_some_and(|known| known != &call_attempt) {
                            return Err(StorageError::InvalidRecord(
                                "multiple unpublished turns".into(),
                            ));
                        }
                        attempt = Some(call_attempt);
                        has_tools = true;
                    }
                    _ => {}
                }
            }
        }
        if has_tools && attempt.is_none() {
            return Err(StorageError::InvalidRecord(
                "tool call lacks provider attempt".into(),
            ));
        }
        if interrupted && has_tools {
            return Err(StorageError::InvalidRecord(
                "interrupted turn contains tool calls".into(),
            ));
        }
        Ok(attempt.map(|attempt_id| {
            if interrupted {
                UnpublishedTurn::Interrupted {
                    attempt_id,
                    sequence: position,
                }
            } else {
                UnpublishedTurn::Settled {
                    attempt_id,
                    has_tools,
                }
            }
        }))
    })
}

fn binding_for_route(
    runtime: &Runtime,
    route: &RouteChoice,
    epoch: i64,
) -> Result<NativeBinding, StorageError> {
    read_snapshot(runtime.store().as_ref(), &ReadOptions::default(), |read| {
        let connection = read
            .router()
            .connection(&route.connection_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let profile = serde_json::from_value::<EndpointProfile>(Value::String(connection.profile))
            .map_err(|_| StorageError::InvalidRecord("route endpoint profile".into()))?;
        Ok(NativeBinding {
            connection_id: route.connection_id.clone(),
            account_revision: Revision::new(connection.account_revision)
                .map_err(|_| StorageError::ScopeDenied)?,
            endpoint_profile: profile,
            model_id: route.model_id.clone(),
            context_epoch: Counter::new(epoch).map_err(|_| StorageError::ScopeDenied)?,
        })
    })
}

const DEFAULT_OUTPUT_HEADROOM: i64 = 8_192;
const MAX_ALIASES_PER_TURN: usize = 256;

fn tool_window<T>(all: &[T], index: usize) -> Result<&[T], StorageError> {
    let start = index
        .checked_mul(MAX_ALIASES_PER_TURN)
        .ok_or(StorageError::ScopeDenied)?;
    if start >= all.len() && !(all.is_empty() && index == 0) {
        return Err(StorageError::ScopeDenied);
    }
    Ok(&all[start..all.len().min(start.saturating_add(MAX_ALIASES_PER_TURN))])
}

fn tool_window_count(total: usize) -> usize {
    total.div_ceil(MAX_ALIASES_PER_TURN).max(1)
}

fn output_headroom(context_tokens: Option<i64>) -> i64 {
    context_tokens
        .map(|window| (window / 4).max(1))
        .unwrap_or(DEFAULT_OUTPUT_HEADROOM)
        .min(DEFAULT_OUTPUT_HEADROOM)
}

/// Route one immutable context head through the ordinary gateway. The request key is stable
/// while the head and observed pin revision are unchanged. A new head or pin permits fresh
/// availability assessment.
pub fn route_context_head(
    runtime: &Runtime,
    ctx: &InvocationContext,
    session_id: &Id,
    context_epoch: i64,
    head_digest: Option<&str>,
    tools_required: bool,
) -> Result<RouteChoice, StorageError> {
    let (intent, effort, mut required, pin_revision, reviewer) =
        read_snapshot(runtime.store().as_ref(), &ReadOptions::default(), |read| {
            let session = read
                .sessions()
                .session(session_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if ctx.principal_kind() != PrincipalKind::Agent
                || ctx.session_id() != Some(session_id)
                || session.agent_id != *ctx.principal()
                || session.closed()
                || session.context_epoch != context_epoch
            {
                return Err(StorageError::ScopeDenied);
            }
            let reviewer = read
                .authority()
                .principal(ctx.principal())?
                .is_some_and(|binding| {
                    binding.active
                        && binding.kind == PrincipalKind::Agent
                        && binding.role == Some(AgentRole::CodeReviewer)
                });
            match read.router().session_pin(session_id)? {
                Some(pin) => Ok((
                    serde_json::from_str::<RouteIntent>(&pin.intent)
                        .map_err(|_| StorageError::InvalidRecord("stored route intent".into()))?,
                    serde_json::from_value::<EffortName>(Value::String(pin.requested_effort))
                        .map_err(|_| StorageError::InvalidRecord("stored route effort".into()))?,
                    pin.required_capabilities,
                    pin.revision,
                    reviewer,
                )),
                None => Ok((
                    RouteIntent::Tier {
                        tier: TierName::High,
                    },
                    EffortName::Default,
                    Vec::new(),
                    0,
                    reviewer,
                )),
            }
        })?;
    if tools_required && !required.iter().any(|capability| capability == "tools") {
        required.push("tools".into());
        required.sort();
    }
    let key = sha256_hex(
        format!(
            "agent-route:{}:{context_epoch}:{pin_revision}:{}:{}",
            session_id.as_str(),
            head_digest.unwrap_or("initial"),
            required.join(",")
        )
        .as_bytes(),
    );
    let frame = serde_json::to_vec(&json!({
        "mode": "Command",
        "protocol": devworks_core::protocol::CONTRACT,
        "requestKey": format!("agent-route-{key}"),
        "body": {
            "kind": "router.route",
            "sessionId": session_id,
            "expectedContextEpoch": context_epoch.to_string(),
            "intent": intent,
            "effort": effort,
            "requiredCapabilities": required,
        }
    }))
    .map_err(|error| StorageError::InvalidRecord(format!("route frame: {error}")))?;
    // Reviewers cannot issue Commands. Routing is host bookkeeping for their own validated
    // session; their later selected tool inventory still uses the read-only reviewer principal.
    let route_ctx = if reviewer {
        let info = runtime.store().info();
        let mut host = HostIngress::new(runtime.store().as_ref())
            .context(
                info.host_actor,
                PrincipalKind::Host,
                ctx.invocation_id().clone(),
                Id::new("agent-route-host").expect("static ingress"),
                ctx.restore_epoch().clone(),
                ctx.policy_revision().clone(),
            )
            .with_session(session_id.clone())
            .with_lineage(
                ctx.project_id().cloned(),
                ctx.root_id().cloned(),
                ctx.assignment_id().cloned(),
            );
        if let Some((activation, epoch)) = ctx.activation() {
            host = host.with_activation(activation.clone(), epoch);
        }
        host
    } else {
        ctx.clone()
    };
    let reply = runtime
        .command(&route_ctx, "router.route", &frame)
        .map_err(|error| StorageError::InvalidRecord(format!("agent route gateway: {error}")))?;
    let choice = match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied {
                result: OperationResult::RouterRoute { value },
            }
            | DomainDecision::NoChange {
                result: OperationResult::RouterRoute { value },
            } => Ok(value),
            DomainDecision::Refused { fault } => Err(StorageError::InvalidRecord(format!(
                "agent route refused: {fault:?}"
            ))),
            _ => Err(StorageError::InvalidRecord("agent route reply".into())),
        },
        CommandReply::Rejected { .. } => Err(StorageError::ScopeDenied),
        CommandReply::CommandStatusUnknown { .. } | CommandReply::ReceiptPurged { .. } => Err(
            StorageError::InvalidRecord("agent route status unconfirmed".into()),
        ),
    }?;
    read_snapshot(runtime.store().as_ref(), &ReadOptions::default(), |read| {
        let pin = read
            .router()
            .session_pin(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if pin.pin_id != choice.pin_id
            || pin.revision != choice.pin_revision.get()
            || pin.model_id != choice.model_id
            || pin.connection_id != choice.connection_id
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(choice)
    })
}

pub fn initial_context(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    agent: &AgentRecord,
) -> Result<ProviderContext, StorageError> {
    let (role, brief, tasks, unlinked) =
        read_snapshot(store, &ReadOptions::default(), |snapshot| {
            let assignment = snapshot
                .runs()
                .assignment(&agent.assignment_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let session = snapshot
                .sessions()
                .session(&assignment.session_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let source = snapshot
                .runs()
                .agent_assignment_context(&assignment.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if ctx.principal_kind() != PrincipalKind::Agent
                || snapshot.stamp().store_id != *ctx.store_id()
                || snapshot.stamp().restore_epoch != ctx.restore_epoch().get()
                || snapshot.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Agent)
                || ctx.session_id() != Some(&session.id)
                || ctx.assignment_id() != Some(&assignment.id)
                || ctx.root_id() != Some(&assignment.root_id)
                || ctx.project_id() != Some(&source.project_id)
                || session.agent_id != *ctx.principal()
                || session.closed()
                || session.context_epoch != 0
                || assignment.state != AssignmentState::Active
                || agent.root_id != assignment.root_id
                || agent.role_id != source.role_id
                || source.assignment_id != assignment.id
            {
                return Err(StorageError::ScopeDenied);
            }
            let tasks: Vec<Id> = serde_json::from_value(source.task_ids)
                .map_err(|_| StorageError::InvalidRecord("stored task references".into()))?;
            let mut unlinked = Vec::new();
            let mut after = 0;
            loop {
                let page = snapshot
                    .runs()
                    .instructions(&assignment.root_id, after, 500)?;
                let more = page.len() == 500;
                for instruction in page {
                    after = instruction.seq;
                    if instruction.session_id.as_ref() != Some(&session.id) {
                        unlinked.push(instruction);
                    }
                }
                if !more {
                    break;
                }
            }
            Ok((source.role_instructions, source.brief, tasks, unlinked))
        })?;
    let brief = if tasks.is_empty() {
        brief
    } else {
        format!(
            "{brief}\n\nAssigned task IDs: {}",
            tasks.iter().map(Id::as_str).collect::<Vec<_>>().join(", ")
        )
    };
    let mut context = ProviderContext {
        version: 1,
        instructions: if role.is_empty() {
            Vec::new()
        } else {
            vec![TextBlock {
                text: role,
                cache: None,
            }]
        },
        instruction_stamps: Vec::new(),
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: brief,
                cache: None,
            }],
        }],
        parameters: GenerationParameters::default(),
    };
    for instruction in unlinked {
        let mut content = Vec::new();
        if !instruction.message.is_empty() {
            content.push(ContentBlock::Text {
                text: instruction.message,
                cache: None,
            });
        }
        let attachments: Vec<ArtifactRef> = serde_json::from_value(instruction.attachments)
            .map_err(|_| StorageError::InvalidRecord("stored redirect attachments".into()))?;
        for artifact in attachments {
            if artifact.media_type.starts_with("image/") {
                content.push(ContentBlock::Image {
                    artifact,
                    detail: None,
                    cache: None,
                });
            } else {
                content.push(ContentBlock::Text {
                    text: format!(
                        "User attachment: {} ({})",
                        artifact.artifact_id.as_str(),
                        artifact.media_type
                    ),
                    cache: None,
                });
            }
        }
        context.messages.push(Message {
            role: MessageRole::User,
            content,
        });
    }
    context
        .validate()
        .map_err(|e| StorageError::InvalidRecord(format!("agent provider context: {e:?}")))?;
    Ok(context)
}

/// Publish a materialized assignment's first bound prompt after the current route resolves.
/// The immutable role brief, repository instructions and native account binding share the
/// first durable context head.
pub fn publish_first_context(
    runtime: &Runtime,
    ctx: &InvocationContext,
    agent: &AgentRecord,
    route: &RouteChoice,
) -> Result<(ArtifactRef, NativeBinding), StorageError> {
    let session_id = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    if runtime
        .recover_native_context_head(ctx, session_id)?
        .is_some()
    {
        return Err(StorageError::ScopeDenied);
    }
    let (binding, headroom) =
        read_snapshot(runtime.store().as_ref(), &ReadOptions::default(), |read| {
            let pin = read
                .router()
                .session_pin(session_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let connection = read
                .router()
                .connection(&route.connection_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let session = read
                .sessions()
                .session(session_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let model = read
                .router()
                .route_model(&route.model_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if pin.pin_id != route.pin_id
                || pin.revision != route.pin_revision.get()
                || connection.id != pin.connection_id
                || session.context_epoch != 0
                || connection.account_revision < 1
            {
                return Err(StorageError::ScopeDenied);
            }
            let endpoint_profile =
                serde_json::from_value::<EndpointProfile>(Value::String(connection.profile))
                    .map_err(|_| StorageError::InvalidRecord("route endpoint profile".into()))?;
            Ok((
                NativeBinding {
                    connection_id: pin.connection_id,
                    account_revision: Revision::new(connection.account_revision)
                        .map_err(|_| StorageError::ScopeDenied)?,
                    endpoint_profile,
                    model_id: pin.model_id,
                    context_epoch: Counter::new(0).expect("zero epoch"),
                },
                output_headroom(model.context_tokens),
            ))
        })?;
    let mut prompt = initial_context(runtime.store().as_ref(), ctx, agent)?;
    let selected_count = runtime
        .providers()
        .ok_or(StorageError::ScopeDenied)?
        .schemas()
        .selected_agent_tools(ctx)?
        .len();
    if tool_window_count(selected_count) > 1 {
        prompt.instructions.push(TextBlock {
            text: "Selected tools are presented in successive windows. A completed response may be followed by another turn with the remaining tools before the assignment closes. If a needed tool is absent, state the intended operation and continue when the next window appears.".into(),
            cache: None,
        });
    }
    if binding.endpoint_profile != EndpointProfile::CodexSubscriptionResponses {
        prompt.parameters.max_output_tokens = Some(headroom as u32);
    }
    let bound = runtime.bind_provider_instructions(ctx, prompt, &[])?;
    let head = runtime.publish_initial_native_context(ctx, session_id, &binding, &bound)?;
    Ok((head, binding))
}

/// Build an attributed native request from the exact published head and routed pin. The driver
/// rechecks the pin, current owner, price and context before sending anything to a provider.
pub fn plan_generation(
    runtime: &Runtime,
    ctx: &InvocationContext,
    route: &RouteChoice,
    binding: &NativeBinding,
    head: &ArtifactRef,
) -> Result<NativeInvocation, StorageError> {
    plan_generation_window(runtime, ctx, route, binding, head, 0)
}

/// A bounded, deterministic window of the complete selected tool inventory. The caller rotates
/// across windows after settled turns so every selected binding can be offered without changing
/// the immutable alias map of an already dispatched request.
pub fn plan_generation_window(
    runtime: &Runtime,
    ctx: &InvocationContext,
    route: &RouteChoice,
    binding: &NativeBinding,
    head: &ArtifactRef,
    window: usize,
) -> Result<NativeInvocation, StorageError> {
    let session_id = ctx.session_id().ok_or(StorageError::ScopeDenied)?;
    let root_id = ctx.root_id().ok_or(StorageError::ScopeDenied)?;
    let project_id = ctx.project_id().ok_or(StorageError::ScopeDenied)?;
    if binding.model_id != route.model_id || binding.connection_id != route.connection_id {
        return Err(StorageError::ScopeDenied);
    }
    let providers = runtime.providers().ok_or(StorageError::ScopeDenied)?;
    let selected = providers.schemas().selected_agent_tools(ctx)?;
    let tools = tool_window(&selected, window)?.to_vec();
    let tool_bytes = serde_json::to_vec(&tools)
        .map_err(|error| StorageError::InvalidRecord(format!("tool schemas: {error}")))?
        .len();
    let (context_bytes, _) = runtime
        .artifacts()
        .read_exact(runtime.store().as_ref(), ctx, head, 32 * 1024 * 1024)
        .map_err(|error| StorageError::InvalidRecord(format!("agent context read: {error:?}")))?;
    let provider_context: ProviderContext = serde_json::from_slice(&context_bytes)
        .map_err(|error| StorageError::InvalidRecord(format!("agent context: {error}")))?;
    let instruction_targets = provider_context
        .instruction_stamps
        .iter()
        .map(|stamp| (stamp.repository_id.clone(), stamp.targets.clone()))
        .collect();
    let input_headroom = head
        .byte_length
        .get()
        .checked_add(i64::try_from(tool_bytes).map_err(|_| StorageError::ScopeDenied)?)
        .ok_or(StorageError::ScopeDenied)?;
    let request_id = Id::new(format!(
        "agent-request-{}",
        sha256_hex(
            format!(
                "{}:{}:{}:{}:{window}",
                session_id.as_str(),
                head.sha256.as_str(),
                route.pin_id.as_str(),
                route.pin_revision.get()
            )
            .as_bytes()
        )
    ))
    .map_err(|_| StorageError::ScopeDenied)?;
    let info = runtime.store().info();
    let (
        query,
        owner,
        quote,
        price_generation,
        override_revision,
        control,
        work,
        headroom,
        required,
    ) = read_snapshot(runtime.store().as_ref(), &ReadOptions::default(), |read| {
        let pin = read
            .router()
            .session_pin(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let connection = read
            .router()
            .connection(&route.connection_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let model = read
            .router()
            .route_model(&route.model_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let control = read
            .runs()
            .root_control(root_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let work = read
            .work()
            .work(&root_id.0)?
            .ok_or(StorageError::ScopeDenied)?;
        if pin.pin_id != route.pin_id
            || pin.revision != route.pin_revision.get()
            || pin.model_id != binding.model_id
            || pin.connection_id != binding.connection_id
            || connection.account_revision != binding.account_revision.get()
            || serde_json::from_value::<EndpointProfile>(Value::String(connection.profile.clone()))
                .ok()
                .as_ref()
                != Some(&binding.endpoint_profile)
            || session.context_epoch != binding.context_epoch.get()
            || (!tools.is_empty()
                && !pin
                    .required_capabilities
                    .iter()
                    .any(|capability| capability == "tools"))
            || work.project_id != *project_id
            || control.generation < 1
        {
            return Err(StorageError::ScopeDenied);
        }
        let query = PriceQuery {
            catalog_model_id: model.id,
            provider_model_id: pin.upstream_model_id,
            connection_id: connection.id,
            endpoint_profile: connection.profile.clone(),
            access_mode: if matches!(
                connection.profile.as_str(),
                "CodexSubscriptionResponses" | "ClaudeSubscriptionMessages"
            ) {
                "Subscription"
            } else {
                "Api"
            }
            .into(),
            service_tier: None,
            input_tokens: None,
            cache_write_ttl_ms: None,
            processing_region: None,
            required_modalities: vec!["Text".into()],
        };
        let owner =
            auxiliary::ownership(read, &info, ctx).map_err(|_| StorageError::ScopeDenied)?;
        let quote = CatalogPriceResolver.quote(read, &query)?;
        Ok((
            query,
            owner,
            quote,
            read.usage().price_revision()?,
            read.usage().price_override_revision(&route.model_id)?,
            control,
            work,
            output_headroom(model.context_tokens),
            pin.required_capabilities,
        ))
    })?;
    let revision = |value: i64| Revision::new(value).map_err(|_| StorageError::ScopeDenied);
    let effort_name =
        serde_json::to_value(&route.requested_effort).map_err(|_| StorageError::ScopeDenied)?;
    let effort = effort_name.as_str().ok_or(StorageError::ScopeDenied)?;
    let inference = InferenceRequest {
        options: InferenceOptions {
            selection: ModelSelection::Model {
                model_id: route.model_id.clone(),
            },
            effort: if effort == "Default" {
                None
            } else {
                Some(EffortOption {
                    value: Id::new(effort).map_err(|_| StorageError::ScopeDenied)?,
                    required: true,
                })
            },
            output_schema: None,
            deadline_ms: 120_000,
        },
        authority_revision: ctx.policy_revision().clone(),
        control_generation: revision(control.generation)?,
        instruction_revision: revision(control.instruction_revision.max(1))?,
        cancellation_revision: revision(
            control
                .stop_epoch
                .checked_add(1)
                .ok_or(StorageError::ScopeDenied)?,
        )?,
        protocol: Protocol::Draft1,
        request_id: request_id.clone(),
        purpose: if owner
            .dispatch
            .as_ref()
            .is_some_and(|dispatch| dispatch.implementation)
        {
            InferencePurpose::Implementation
        } else {
            InferencePurpose::Planning
        },
        attribution: if work.is_feature() {
            Attribution::Feature {
                project_id: project_id.clone(),
                root_id: root_id.clone(),
                assignment_id: ctx.assignment_id().cloned(),
                job_run_id: ctx.job_run_id().cloned(),
            }
        } else {
            Attribution::Issue {
                project_id: project_id.clone(),
                root_id: root_id.clone(),
                assignment_id: ctx.assignment_id().cloned(),
                job_run_id: ctx.job_run_id().cloned(),
            }
        },
        input: InferenceInput::Generation {
            session_id: session_id.clone(),
            context: head.clone(),
            context_epoch: binding.context_epoch.clone(),
            tools,
            response_headroom_tokens: revision(headroom)?,
            requires_images: false,
            requires_parallel_tools: false,
        },
    };
    let canonical = devworks_core::jcs::canonicalize(
        &serde_json::to_value(&inference)
            .map_err(|error| StorageError::InvalidRecord(format!("agent inference: {error}")))?,
    )
    .map_err(|error| StorageError::InvalidRecord(format!("agent inference JSON: {error}")))?;
    let logical = RequestAdmission {
        request_id: request_id.clone(),
        purpose: Purpose::Conversation,
        session_id: Some(session_id.clone()),
        project_id: Some(project_id.clone()),
        root_id: Some(root_id.clone()),
        model_id: route.model_id.clone(),
        connection_id: route.connection_id.clone(),
        pin_id: Some(route.pin_id.clone()),
        args_digest: sha256_hex(canonical.as_bytes()),
    };
    let attempt = AttemptRequest {
        admission_key: request_id.clone(),
        request_id,
        actor_id: ctx.principal().clone(),
        work_id: owner.work_id,
        job_run_id: owner.job_run_id,
        role: owner.role,
        query,
        quote,
        price_generation,
        override_revision,
        pricing: PricingContext {
            input_semantics: InputSemantics::Unknown,
            cache_read_possible: false,
            cache_write_possible: false,
            cache_write_ttl_ms: None,
        },
        selection: AttemptSelection::Conversation {
            intent: route.intent.clone(),
            effort: route.requested_effort.clone(),
            required_capabilities: required,
            pin: Some(PinFence {
                id: route.pin_id.clone(),
                revision: route.pin_revision.get(),
            }),
        },
        input_tokens: input_headroom,
        output_headroom: headroom,
        unit_headroom: vec![],
        expected_context_epoch: owner.expected_context_epoch,
        dispatch: owner.dispatch,
        cache_diagnostics: vec![],
    };
    Ok(NativeInvocation {
        source_fences: Vec::new(),
        instruction_targets: Some(instruction_targets),
        context: ctx.clone(),
        logical,
        category: ReportCategory::Assistant,
        attempt,
        request: inference,
    })
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[test]
    fn every_selected_alias_is_reachable_across_bounded_windows() {
        let selected: Vec<_> = (0..374).collect();
        assert_eq!(tool_window_count(selected.len()), 2);
        assert_eq!(tool_window(&selected, 0).unwrap().len(), 256);
        assert_eq!(tool_window(&selected, 1).unwrap().len(), 118);
        let projected: Vec<_> = (0..tool_window_count(selected.len()))
            .flat_map(|window| tool_window(&selected, window).unwrap().iter().copied())
            .collect();
        assert_eq!(projected, selected);
        assert!(tool_window(&selected, 2).is_err());
        assert!(tool_window::<usize>(&[], 0).unwrap().is_empty());
    }
}
