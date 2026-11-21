//! Native workspace chat consumes one saved message at a time, shared by Home and its sidebar.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    RetryPolicy,
};
use crate::turn_engine::TurnEngine;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind, WorkspaceReply};
use devworks_core::ports::storage::{
    OperationRecord, ReadOptions, ReadSnapshot, StorageError, StoredSession, WriteOptions,
    WriteTransaction, read_snapshot,
};
use devworks_core::protocol::provider::{
    ArtifactRef, EndpointProfile, NativeBinding, TranscriptBody, UserContent,
};
use devworks_core::protocol::records::RouteChoice;
use devworks_core::protocol::{Counter, Id, OperationId, OperationState, Revision};
use devworks_core::providers::context::{GenerationParameters, ProviderContext, TextBlock};
use devworks_core::services::sessions::workspace_reply::{CAPABILITY, CONSUMER, ReplyIntent};
use devworks_core::services::usage::{accounting::AttemptEnd, host::DriveOutcome};
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

#[derive(Default)]
pub(crate) struct WorkspaceReplies {
    engine: OnceLock<TurnEngine>,
}

impl WorkspaceReplies {
    pub(crate) fn bind(&self, engine: TurnEngine) {
        let _ = self.engine.set(engine);
    }

    pub(crate) fn consumer(self: &Arc<Self>) -> Consumer {
        Consumer {
            name: CONSUMER.into(),
            handler: self.clone(),
            effect: Some(Arc::new(ReplyEffects(self.clone()))),
            async_handler: None,
            eligibility: None,
            stop_grace_ms: 5_000,
            retry: RetryPolicy::default(),
            max_in_flight: Some(2),
        }
    }
}

impl DeliveryHandler for WorkspaceReplies {
    fn apply(
        &self,
        _: &mut dyn WriteTransaction,
        delivery: &Delivery,
    ) -> Result<Outcome, StorageError> {
        if delivery.entry.operation_id.is_none() {
            return Err(StorageError::ScopeDenied);
        }
        Ok(Outcome::Completed)
    }
}

struct ReplyEffects(Arc<WorkspaceReplies>);

impl EffectExecutor for ReplyEffects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        let worker = self.0.clone();
        Box::pin(async move {
            match worker.engine.get() {
                Some(engine) => run(engine, operation).await,
                None => EffectResult::NotApplied("Workspace reply engine is opening.".into()),
            }
        })
    }

    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let worker = self.0.clone();
        Box::pin(async move {
            let Some(attempt) = operation.latest_attempt() else {
                return EffectResult::Unknown("Workspace reply has no recorded attempt.".into());
            };
            if let Some(saved) = attempt
                .observations
                .iter()
                .rev()
                .find_map(|value| value.get("replySettlement").cloned())
            {
                return EffectResult::Completed(saved);
            }
            let Some(request) = attempt
                .observations
                .iter()
                .rev()
                .find_map(|value| value.get("requestId"))
                .cloned()
            else {
                return EffectResult::NotApplied("No provider request was prepared.".into());
            };
            let Some(engine) = worker.engine.get() else {
                return EffectResult::Unknown("Workspace reply engine is opening.".into());
            };
            match settled_request(engine, &operation, &attempt.boot_id, request).await {
                Ok(Some(outcome)) => outcome,
                Ok(None) => EffectResult::Unknown(
                    "An earlier workspace reply still needs reconciliation.".into(),
                ),
                Err(error) => EffectResult::Unknown(error.to_string()),
            }
        })
    }
}

/// Reads durable evidence only. A known completed continuation can settle a lost acknowledgment,
/// and a known tool continuation can safely resume from its published context.
async fn settled_request(
    engine: &TurnEngine,
    operation: &OperationRecord,
    attempt_boot: &Id,
    request: Value,
) -> Result<Option<EffectResult>, StorageError> {
    let request: Id = serde_json::from_value(request)
        .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
    let intent: ReplyIntent = serde_json::from_value(operation.payload.clone())
        .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
    let operation_id = operation.id.clone();
    let previous_boot = engine
        .store()
        .info()
        .boot_id
        .as_ref()
        .is_some_and(|boot| boot != attempt_boot);
    crate::store::read(engine.store(), move |read| {
        let session = session_for_reply(read, &intent)?;
        let Some(routed) = read.router().route_request(&request)? else {
            return Ok(previous_boot.then(|| {
                EffectResult::NotApplied(
                    "The earlier boot did not admit the prepared provider request.".into(),
                )
            }));
        };
        if routed.session_id.as_ref() != Some(&session.id) {
            return Err(StorageError::ScopeDenied);
        }
        let Some(head) = read.sessions().provider_context_head(&session.id)? else {
            return Ok(None);
        };
        let Some(attempt_id) = head
            .attempt_id
            .as_ref()
            .filter(|_| head.kind == "ProviderTurnContinuation")
        else {
            return Ok(None);
        };
        let source: ArtifactRef = serde_json::from_value(head.artifact)
            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
        let Some(version) = read.artifacts().version(&source.version_id)? else {
            return Ok(None);
        };
        if version.source["derived"]["workspaceReply"] != json!(operation_id.0) {
            return Ok(None);
        }
        let Some(attempt) = read.usage().inference_attempt(attempt_id)? else {
            return Ok(None);
        };
        if attempt.request_id != request
            || attempt.actor_id != intent.agent_id
            || attempt.session_id.as_ref() != Some(&intent.session_id)
        {
            return Ok(None);
        }
        let Some(balance) = read.usage().attempt_balance(attempt_id)? else {
            return Ok(None);
        };
        let summary = devworks_core::services::usage::accounting::summary(&balance)?;
        Ok(match summary.outcome.as_deref() {
            Some("Completed") => Some(EffectResult::Completed(json!({
                "kind":"WorkspaceReplyCompleted", "sessionId": intent.session_id,
                "entryId":intent.user_entry_id, "attemptId": attempt_id,
            }))),
            Some("ToolCallsReady") => Some(EffectResult::NotApplied(
                "Resume from the recorded workspace tool continuation.".into(),
            )),
            _ => None,
        })
    })
    .await
}

async fn run(engine: &TurnEngine, operation: OperationRecord) -> EffectResult {
    let (intent, attempt_no) = match (&operation.capability[..], operation.latest_attempt()) {
        (CAPABILITY, Some(attempt)) if operation.state == OperationState::Dispatching => {
            match serde_json::from_value::<ReplyIntent>(operation.payload.clone()) {
                Ok(intent) if intent.user_sequence > 0 => (intent, attempt.attempt_no),
                Ok(_) => {
                    return EffectResult::Failed("Workspace reply has an invalid sequence.".into());
                }
                Err(error) => return EffectResult::Failed(error.to_string()),
            }
        }
        _ => return EffectResult::Failed("Invalid workspace reply operation.".into()),
    };
    let ctx = match invocation(engine.store().as_ref(), &intent, &operation.id) {
        Ok(ctx) => ctx,
        Err(error) => return EffectResult::Failed(error.to_string()),
    };
    let (route, binding, mut head) = match prepare(engine, &ctx, &intent).await {
        Ok(value) => value,
        Err(error) => return settled(engine, &operation.id, attempt_no, unavailable(error)).await,
    };
    for _ in 0..64 {
        let request = match crate::agent_turns::plan_generation_window(
            engine, &ctx, &route, &binding, &head, 0,
        ) {
            Ok(request) => request,
            Err(error) => {
                return settled(engine, &operation.id, attempt_no, unavailable(error)).await;
            }
        };
        if let Err(error) = observe(
            engine,
            &operation.id,
            attempt_no,
            json!({
                "requestId":request.logical.request_id,
                "head":head,
            }),
        )
        .await
        {
            return EffectResult::Unknown(error.to_string());
        }
        let progress = match engine.produce_and_dispatch_native_turn(request, None).await {
            Ok(progress) => progress,
            Err(error) => return EffectResult::Unknown(error.to_string()),
        };
        match progress.provider.result.outcome {
            Some(AttemptEnd::Completed) if progress.continuation.is_some() => {
                return settled(engine, &operation.id, attempt_no, json!({
                    "kind":"WorkspaceReplyCompleted","sessionId":intent.session_id,
                    "entryId":intent.user_entry_id,"attemptId":progress.provider.result.attempt_id,
                })).await;
            }
            Some(AttemptEnd::ToolCallsReady) => {
                let Some(next) = progress.continuation else {
                    return EffectResult::Unknown(
                        "A workspace tool call is still unsettled.".into(),
                    );
                };
                head = next;
            }
            Some(AttemptEnd::UnknownTail) | Some(AttemptEnd::Completed) => {
                return EffectResult::Unknown("Workspace provider outcome is unresolved.".into());
            }
            None if matches!(
                progress.provider.accounting,
                DriveOutcome::AlreadyDispatched
            ) =>
            {
                return EffectResult::Unknown(
                    "Workspace provider request is already dispatched.".into(),
                );
            }
            _ => {
                return settled(
                    engine,
                    &operation.id,
                    attempt_no,
                    unavailable(progress.provider.result.faults.first().map_or(
                        "The selected route did not complete a reply.",
                        String::as_str,
                    )),
                )
                .await;
            }
        }
    }
    settled(
        engine,
        &operation.id,
        attempt_no,
        unavailable("Workspace reply exceeded its tool-turn limit."),
    )
    .await
}

fn unavailable(detail: impl ToString) -> Value {
    json!({"kind":"WorkspaceReplyUnavailable","detail":detail.to_string()})
}

async fn settled(
    engine: &TurnEngine,
    operation: &OperationId,
    attempt: i64,
    value: Value,
) -> EffectResult {
    match observe(engine, operation, attempt, json!({"replySettlement":value})).await {
        Ok(()) => EffectResult::Completed(value),
        Err(error) => EffectResult::Unknown(error.to_string()),
    }
}

async fn observe(
    engine: &TurnEngine,
    operation: &OperationId,
    attempt: i64,
    value: Value,
) -> Result<(), StorageError> {
    let operation = operation.clone();
    crate::store::write(engine.store(), WriteOptions::observed(), move |tx| {
        let current = tx
            .operations()
            .operation(&operation)?
            .ok_or(StorageError::ScopeDenied)?;
        if current.state != OperationState::Dispatching
            || current
                .latest_attempt()
                .is_none_or(|entry| entry.attempt_no != attempt)
        {
            return Err(StorageError::LeaseLost {
                kind: "workspace_reply",
                id: operation.0.as_str().to_owned(),
            });
        }
        tx.operations().observe(&operation, attempt, value)
    })
    .await
}

async fn prepare(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    intent: &ReplyIntent,
) -> Result<(RouteChoice, NativeBinding, ArtifactRef), String> {
    let (engine, ctx, intent) = (engine.clone(), ctx.clone(), intent.clone());
    tokio::task::spawn_blocking(move || {
        let session = &intent.session_id;
        let head = engine.recover_native_context_head(&ctx, session).map_err(|error| error.to_string())?;
        let (inputs, images, role) = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |snapshot| {
            let after = if head.is_none() { 0 } else { intent.user_sequence - 1 };
            let inputs = input_prefix(snapshot, &intent, after)?;
            let images = inputs.iter().any(|body| {
                matches!(body, TranscriptBody::UserMessage { content } if content.iter().any(|item|
                    matches!(item, UserContent::Attachment { artifact } if artifact.media_type.starts_with("image/"))))
            });
            let role = devworks_core::settings::service::role_instructions(
                snapshot.configuration(), AgentRole::WorkspaceCoordinator, None,
            ).map_err(|_| StorageError::ScopeDenied)?;
            Ok((inputs, images, role))
        }).map_err(|error| error.to_string())?;
        let tools = !engine.providers().ok_or("The native provider is unavailable.")?
            .schemas().selected_agent_tools(&ctx).map_err(|error| error.to_string())?.is_empty();
        let route = crate::agent_turns::route_context_head(
            &engine, &ctx, session, intent.context_epoch,
            head.as_ref().map(|(artifact, _, _)| artifact.sha256.as_str()),
            tools, images,
        ).map_err(|error| format!("{error:?}"))?;
        let (artifact, binding) = match head {
            Some((artifact, binding, _)) => (artifact, binding),
            None => {
                let (binding, headroom) = crate::agent_turns::initial_route_binding(&engine, session, &route)
                    .map_err(|error| error.to_string())?;
                let mut context = ProviderContext {
                    version:1,
                    instructions: vec![TextBlock { text: role.text, cache:None }],
                    instruction_stamps: Vec::new(), skill_stamp:None, messages: Vec::new(),
                    parameters: GenerationParameters {
                        max_output_tokens: (binding.endpoint_profile != EndpointProfile::CodexSubscriptionResponses)
                            .then_some(headroom as u32),
                        ..GenerationParameters::default()
                    },
                };
                for body in inputs {
                    crate::tool_turn::append_context_fact(
                        &mut context, engine.store().as_ref(), engine.artifacts(), &ctx, session, body,
                    ).map_err(|error| error.to_string())?;
                }
                let context = engine.bind_provider_instructions(&ctx, context, &[])
                    .map_err(|error| error.to_string())?;
                let artifact = engine.publish_initial_native_context(&ctx, session, &binding, &context)
                    .map_err(|error| error.to_string())?;
                (artifact, binding)
            }
        };
        let head = crate::tool_turn::append_operation_updates(
            engine.store().as_ref(), engine.artifacts(), &ctx, session, &artifact,
        ).map_err(|error| error.to_string())?;
        Ok((route, binding, head))
    }).await.map_err(|error| error.to_string())?
}

fn session_for_reply(
    snapshot: &dyn ReadSnapshot,
    intent: &ReplyIntent,
) -> Result<StoredSession, StorageError> {
    let session = snapshot
        .sessions()
        .session(&intent.session_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if session.closed()
        || session.agent_id != intent.agent_id
        || session.created_by != intent.creator_id
        || session.root_id.is_some()
        || session.project_id.is_some()
        || session.context_epoch != intent.context_epoch
        || !snapshot
            .authority()
            .principal(&intent.agent_id)?
            .is_some_and(|binding| {
                binding.active
                    && binding.kind == PrincipalKind::Agent
                    && binding.role == Some(AgentRole::WorkspaceCoordinator)
                    && binding.project_id.is_none()
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(session)
}

fn invocation(
    store: &dyn devworks_core::ports::storage::ApplicationStore,
    intent: &ReplyIntent,
    operation_id: &OperationId,
) -> Result<InvocationContext, StorageError> {
    let info = store.info();
    Ok(HostIngress::new(store)
        .context(
            intent.agent_id.clone(),
            PrincipalKind::Agent,
            operation_id.0.clone(),
            Id::new("workspace-replies").expect("fixed ingress"),
            Counter::new(info.restore_epoch).map_err(|_| StorageError::ScopeDenied)?,
            Revision::new(1).expect("positive revision"),
        )
        .with_session(intent.session_id.clone())
        .with_workspace_reply(WorkspaceReply {
            user_sequence: intent.user_sequence,
            preference: intent
                .route
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| StorageError::InvalidRecord(error.to_string()))?,
        }))
}

/// Only the ordered human/host inputs through this message can enter its first prompt.
/// Later messages and notices remain available for their own turn.
fn input_prefix(
    snapshot: &dyn ReadSnapshot,
    intent: &ReplyIntent,
    after: i64,
) -> Result<Vec<TranscriptBody>, StorageError> {
    let session = session_for_reply(snapshot, intent)?;
    if after < 0
        || intent.user_sequence <= after
        || intent.user_sequence > session.last_durable_sequence()
    {
        return Err(StorageError::ScopeDenied);
    }
    let mut cursor = after;
    let mut inputs = Vec::new();
    while cursor < intent.user_sequence {
        let remaining = (intent.user_sequence - cursor).min(500) as u16;
        let page = snapshot
            .sessions()
            .entries(&intent.session_id, cursor, remaining)?;
        if page.is_empty() || cursor - after >= 10_000 {
            return Err(StorageError::InvalidRecord(
                "workspace reply transcript prefix".into(),
            ));
        }
        for entry in page {
            if entry.sequence != cursor + 1 {
                return Err(StorageError::InvalidRecord(
                    "workspace reply transcript gap".into(),
                ));
            }
            cursor = entry.sequence;
            let body: TranscriptBody = serde_json::from_value(entry.body)
                .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
            if cursor == intent.user_sequence
                && (entry.entry_id != intent.user_entry_id
                    || entry.actor != intent.creator_id
                    || entry.context_epoch != intent.context_epoch
                    || !matches!(&body, TranscriptBody::UserMessage { .. }))
            {
                return Err(StorageError::ScopeDenied);
            }
            if matches!(
                &body,
                TranscriptBody::UserMessage { .. } | TranscriptBody::HostNotice { .. }
            ) {
                inputs.push(body);
                if inputs.len() > 1_000 {
                    return Err(StorageError::InvalidRecord(
                        "workspace reply input exceeds its bound".into(),
                    ));
                }
            }
        }
    }
    Ok(inputs)
}
