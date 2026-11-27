//! Native workspace chat consumes one saved message at a time, shared by Home and its sidebar.
use crate::agent_turns::ToolWindowSweep;
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectCapacity, EffectExecutor, EffectFuture,
    EffectResult, Outcome, RetryPolicy, ScopedEffectFuture,
};
use crate::providers::sink::ProviderResult;
use crate::turn_engine::TurnEngine;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind, WorkspaceReply};
use devworks_core::ports::storage::{
    AttemptRecord, Cancellation, EventFilter, OperationRecord, QueueState, ReadOptions,
    ReadSnapshot, StorageError, StoredSession, Wake, WatchEventState, WriteOptions,
    WriteTransaction, read_snapshot,
};
use devworks_core::protocol::provider::{
    ArtifactRef, EndpointProfile, InferenceInput, NativeBinding, TranscriptBody, UserContent,
};
use devworks_core::protocol::records::RouteChoice;
use devworks_core::protocol::{Counter, Id, OperationId, OperationState, Revision};
use devworks_core::providers::context::{GenerationParameters, ProviderContext, TextBlock};
use devworks_core::services::sessions::workspace_reply::{CAPABILITY, CONSUMER, ReplyIntent};
use devworks_core::services::usage::{accounting::AttemptEnd, host::DriveOutcome};
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock, Weak};

/// The durable reply at the head of one workspace conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceReplyStatus {
    Queued,
    Running,
    NeedsReconciliation,
    Unavailable,
    Cancelled,
}

pub(crate) fn status(
    store: &dyn devworks_core::ports::storage::ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
) -> Result<Option<WorkspaceReplyStatus>, StorageError> {
    let info = store.info();
    if ctx.store_id() != &info.store_id || ctx.restore_epoch().get() != info.restore_epoch {
        return Err(StorageError::ScopeDenied);
    }
    read_snapshot(store, &ReadOptions::default(), |read| {
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if ctx.principal_kind() != PrincipalKind::Human
            || ctx.project_id().is_some()
            || ctx.root_id().is_some()
            || ctx.session_id().is_some_and(|id| id != session_id)
            || ctx.principal() != &session.created_by
            || session.root_id.is_some()
            || session.project_id.is_some()
        {
            return Err(StorageError::ScopeDenied);
        }
        use WorkspaceReplyStatus::*;
        let partition = session_id.as_str();
        if let Some(entry) = read.queue().partition_head(CONSUMER, partition)? {
            return Ok(match entry.state {
                QueueState::Ready => Some(Queued),
                QueueState::Claimed => Some(Running),
                QueueState::Reconciling => Some(NeedsReconciliation),
                QueueState::Failed => Some(Unavailable),
                _ => None,
            });
        }
        let checkpoint = read.queue().checkpoint(CONSUMER, Some(partition))?;
        if checkpoint == 0 {
            return Ok(None);
        }
        let event = read
            .history()
            .page(
                &EventFilter::Aggregate {
                    kind: devworks_core::services::sessions::transcript::SESSION_AGGREGATE.into(),
                    id: partition.into(),
                },
                checkpoint - 1,
                1,
            )?
            .events
            .into_iter()
            .next();
        let Some(event) = event.filter(|event| {
            event.sequence == checkpoint && event.kind == "WorkspaceReplyRequested"
        }) else {
            return Ok(None);
        };
        let Some(entry) = read.queue().delivery(CONSUMER, &event.id)? else {
            return Ok(None);
        };
        if entry.state == QueueState::Cancelled {
            return Ok(Some(Cancelled));
        }
        let Some(operation) = entry.operation_id else {
            return Ok(None);
        };
        let ended = read
            .operations()
            .operation(&operation)?
            .filter(|op| op.capability == CAPABILITY)
            .and_then(|op| op.attempts.into_iter().last());
        Ok(ended
            .filter(|attempt| {
                attempt.observations.iter().any(|value| {
                    value["replySettlement"]["kind"] == "WorkspaceReplyUnavailable"
                        || value["reconciliation"]["evidence"]["kind"]
                            == "WorkspaceReplyUnavailable"
                })
            })
            .map(|_| Unavailable))
    })
}

#[derive(Default)]
pub(crate) struct WorkspaceReplies {
    engine: OnceLock<Weak<TurnEngine>>,
    stopping: Cancellation,
}

impl WorkspaceReplies {
    pub(crate) fn bind(&self, engine: &Arc<TurnEngine>) {
        // The turn engine's command ingress already holds this dispatcher.
        let _ = self.engine.set(Arc::downgrade(engine));
    }

    pub(crate) fn stop(&self) {
        self.stopping.cancel();
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

impl ReplyEffects {
    fn run<'a>(
        &self,
        operation: OperationRecord,
        capacity: Option<EffectCapacity<'a>>,
    ) -> ScopedEffectFuture<'a> {
        let worker = self.0.clone();
        Box::pin(async move {
            match worker.engine.get().and_then(Weak::upgrade) {
                Some(engine) => run(&engine, operation, &worker.stopping, capacity).await,
                None => EffectResult::NotApplied("Workspace reply engine is opening.".into()),
            }
        })
    }
}

impl EffectExecutor for ReplyEffects {
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        self.run(operation, None)
    }

    fn execute_with_capacity<'a>(
        &self,
        operation: OperationRecord,
        _: Cancellation,
        capacity: EffectCapacity<'a>,
    ) -> ScopedEffectFuture<'a> {
        self.run(operation, Some(capacity))
    }

    fn resumes_after_observation(&self, _operation: &OperationRecord) -> bool {
        true
    }

    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        let worker = self.0.clone();
        Box::pin(async move {
            let Some(engine) = worker.engine.get().and_then(Weak::upgrade) else {
                return EffectResult::Unknown("Workspace reply engine is opening.".into());
            };
            match settle_from_evidence(&engine, &operation).await {
                Ok(outcome) => outcome,
                Err(error) => EffectResult::Unknown(error.to_string()),
            }
        })
    }
}

#[derive(Clone)]
struct PreparedReply {
    request_id: Id,
    head: ArtifactRef,
    compaction_source: Option<ArtifactRef>,
    sweep: ToolWindowSweep,
}

fn prepared_reply(attempt: &AttemptRecord) -> Result<Option<PreparedReply>, StorageError> {
    attempt
        .observations
        .iter()
        .rev()
        .find(|value| value.get("requestId").is_some())
        .map(|value| {
            Ok(PreparedReply {
                request_id: serde_json::from_value(value["requestId"].clone()).map_err(invalid)?,
                head: serde_json::from_value(value["head"].clone()).map_err(invalid)?,
                compaction_source: serde_json::from_value(value["compactionSource"].clone())
                    .map_err(invalid)?,
                sweep: value
                    .get("toolWindowSweep")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(invalid)?
                    .map(ToolWindowSweep::restore)
                    .transpose()?
                    .unwrap_or_default(),
            })
        })
        .transpose()
}

fn invalid(error: impl ToString) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}

struct RecordedReply {
    provider: ProviderResult,
    published: Option<ArtifactRef>,
}

async fn recorded_reply(
    engine: &TurnEngine,
    operation: &OperationId,
    intent: &ReplyIntent,
    prepared: &PreparedReply,
) -> Result<Option<RecordedReply>, StorageError> {
    let (engine_for_read, operation, intent, prepared) = (
        engine.clone(),
        operation.clone(),
        intent.clone(),
        prepared.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let engine = engine_for_read;
        let admitted = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
            let session = session_for_reply(read, &intent)?;
            let route = read.router().route_request(&prepared.request_id)?;
            if route
                .as_ref()
                .is_some_and(|route| route.session_id.as_ref() != Some(&session.id))
            {
                return Err(StorageError::ScopeDenied);
            }
            let Some(route) = route else {
                return Ok(false);
            };
            let journal = read
                .operations()
                .operation(&route.operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            Ok(!journal.attempts.is_empty())
        })?;
        if !admitted {
            return Ok(None);
        }
        let provider = engine
            .providers()
            .ok_or(StorageError::ScopeDenied)?
            .recorded_result(&prepared.request_id, &prepared.request_id)?;
        let published = if let Some(attempt) = &provider.attempt_id {
            if provider.source_context.as_ref() != Some(&prepared.head) {
                return Err(StorageError::ScopeDenied);
            }
            read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
                let inference = read
                    .usage()
                    .inference_attempt(&attempt.0)?
                    .ok_or(StorageError::ScopeDenied)?;
                if inference.actor_id != intent.agent_id
                    || inference.session_id.as_ref() != Some(&intent.session_id)
                    || inference.request_id != prepared.request_id
                {
                    return Err(StorageError::ScopeDenied);
                }
                if let Some(source) = &prepared.compaction_source {
                    let binding = provider.binding.as_ref().ok_or(StorageError::ScopeDenied)?;
                    let next = binding
                        .context_epoch
                        .get()
                        .checked_add(1)
                        .ok_or(StorageError::ScopeDenied)?;
                    let Some(epoch) = read.sessions().epoch(&intent.session_id, next)? else {
                        return Ok(None);
                    };
                    let artifact: ArtifactRef =
                        serde_json::from_value(epoch.context).map_err(invalid)?;
                    let version = read
                        .artifacts()
                        .version(&artifact.version_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    let derived = &version.source["derived"];
                    if derived["workspaceReply"] != json!(operation.0)
                        || derived["sourceContext"] != json!(source)
                        || derived["reason"] != "Compaction"
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    return Ok(Some(artifact));
                }
                let Some(head) = read
                    .sessions()
                    .provider_context_head(&intent.session_id)?
                    .filter(|head| {
                        head.kind == "ProviderTurnContinuation"
                            && head.attempt_id.as_ref() == Some(&attempt.0)
                    })
                else {
                    return Ok(None);
                };
                let artifact: ArtifactRef =
                    serde_json::from_value(head.artifact).map_err(invalid)?;
                let version = read
                    .artifacts()
                    .version(&artifact.version_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if version.source["derived"]["workspaceReply"] != json!(operation.0) {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(Some(artifact))
            })?
        } else {
            None
        };
        Ok(Some(RecordedReply {
            provider,
            published,
        }))
    })
    .await
    .map_err(invalid)?
}

fn completed(intent: &ReplyIntent, result: &ProviderResult) -> Value {
    json!({
        "kind":"WorkspaceReplyCompleted", "sessionId":intent.session_id,
        "entryId":intent.user_entry_id, "attemptId":result.attempt_id,
    })
}

/// A workspace reply's durable operation is the turn identity used by its provider send fence.
pub(crate) fn watcher_turn(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    session: &Id,
) -> Result<Option<Id>, StorageError> {
    let id = OperationId(ctx.invocation_id().clone());
    let operation = tx
        .operations()
        .operation(&id)?
        .filter(|operation| {
            operation.capability == CAPABILITY && operation.state == OperationState::Dispatching
        })
        .ok_or(StorageError::ScopeDenied)?;
    let intent: ReplyIntent = serde_json::from_value(operation.payload).map_err(invalid)?;
    if intent.session_id != *session
        || intent.agent_id != *ctx.principal()
        || ctx
            .workspace_reply()
            .is_none_or(|reply| reply.user_sequence != intent.user_sequence)
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(Some(id.0))
}

fn release_carry(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    completed: bool,
) -> Result<(), StorageError> {
    let session = operation.payload["sessionId"]
        .as_str()
        .and_then(|id| Id::new(id).ok())
        .ok_or(StorageError::ScopeDenied)?;
    let now = tx.now_ms();
    tx.watchers()
        .release_watch_events(&session, &operation.id.0, completed, now)?;
    Ok(())
}

fn settlement_outcome(watcher: bool, value: Value) -> EffectResult {
    if watcher && value["kind"] == "WorkspaceReplyUnavailable" {
        return EffectResult::NotApplied(
            value["detail"]
                .as_str()
                .unwrap_or("The watcher reply did not finish")
                .to_owned(),
        );
    }
    EffectResult::Completed(value)
}

async fn recover_settlement(
    engine: &TurnEngine,
    id: &OperationId,
    attempt_no: i64,
    value: Value,
) -> Result<EffectResult, StorageError> {
    let id = id.clone();
    let completed = value["kind"] == "WorkspaceReplyCompleted";
    let recovered = value.clone();
    let watcher = crate::store::write(engine.store(), WriteOptions::observed(), move |tx| {
        let operation = tx
            .operations()
            .operation(&id)?
            .filter(|operation| {
                operation.capability == CAPABILITY
                    && operation
                        .latest_attempt()
                        .is_some_and(|attempt| attempt.attempt_no == attempt_no)
            })
            .ok_or(StorageError::ScopeDenied)?;
        release_carry(tx, &operation, completed)?;
        let watcher = operation.payload["watcherSource"].is_string();
        if watcher && !completed {
            tx.operations()
                .observe(&id, attempt_no, json!({"replySettlement":recovered}))?;
        }
        Ok(watcher)
    })
    .await?;
    Ok(settlement_outcome(watcher, value))
}

/// A durable provider result can finish local publication during observation. A tool dispatch
/// still belongs to the executor and can resume from the exact recorded request on the next pass.
async fn settle_from_evidence(
    engine: &TurnEngine,
    operation: &OperationRecord,
) -> Result<EffectResult, StorageError> {
    let attempt = operation
        .latest_attempt()
        .ok_or(StorageError::ScopeDenied)?;
    if let Some(saved) = attempt
        .observations
        .iter()
        .rev()
        .find_map(|value| value.get("replySettlement").cloned())
    {
        return Ok(settlement_outcome(
            operation.payload["watcherSource"].is_string(),
            saved,
        ));
    }
    let Some(prepared) = prepared_reply(attempt)? else {
        return Ok(EffectResult::NotApplied(
            "No provider request was prepared.".into(),
        ));
    };
    let intent: ReplyIntent = serde_json::from_value(operation.payload.clone()).map_err(invalid)?;
    let previous_boot = engine
        .store()
        .info()
        .boot_id
        .as_ref()
        .is_some_and(|boot| boot != &attempt.boot_id);
    let unknown =
        || EffectResult::Unknown("The workspace reply still needs reconciliation.".into());
    let Some(recorded) = recorded_reply(engine, &operation.id, &intent, &prepared).await? else {
        return Ok(if previous_boot {
            EffectResult::NotApplied("The earlier boot did not admit this provider request.".into())
        } else {
            unknown()
        });
    };
    match recorded.provider.outcome {
        Some(AttemptEnd::Completed) if prepared.compaction_source.is_some() => {
            Ok(if recorded.provider.result_artifact.is_some() {
                EffectResult::NotApplied("Resume after the recorded workspace compaction.".into())
            } else {
                unknown()
            })
        }
        Some(AttemptEnd::Completed) => {
            if recorded.published.is_none() {
                let ctx = invocation(engine.store().as_ref(), &intent, &operation.id)?;
                let (_, published) = engine
                    .continue_recorded_turn(
                        &ctx,
                        &intent.session_id,
                        &prepared.head,
                        &recorded.provider,
                        None,
                        false,
                    )
                    .await?;
                if published.is_none() {
                    return Ok(unknown());
                }
            }
            let mut sweep = prepared.sweep;
            if sweep.complete_window() {
                recover_settlement(
                    engine,
                    &operation.id,
                    attempt.attempt_no,
                    completed(&intent, &recorded.provider),
                )
                .await
            } else {
                Ok(EffectResult::NotApplied(
                    "Resume with the next workspace tool window.".into(),
                ))
            }
        }
        Some(AttemptEnd::ToolCallsReady) if previous_boot || recorded.published.is_some() => Ok(
            EffectResult::NotApplied("Resume the recorded workspace tool turn.".into()),
        ),
        Some(AttemptEnd::Cancelled | AttemptEnd::Failed | AttemptEnd::Incomplete) => {
            let ctx = invocation(engine.store().as_ref(), &intent, &operation.id)?;
            recover_interruption(engine, ctx, intent.session_id).await?;
            recover_settlement(
                engine,
                &operation.id,
                attempt.attempt_no,
                unavailable(provider_fault(&recorded.provider)),
            )
            .await
        }
        _ => Ok(unknown()),
    }
}

async fn run(
    engine: &TurnEngine,
    operation: OperationRecord,
    stopping: &Cancellation,
    mut capacity: Option<EffectCapacity<'_>>,
) -> EffectResult {
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
    if let Some(source) = &intent.watcher_source {
        let consumed = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
            Ok(read.watchers().watch_event(source)?.is_some_and(|event| {
                event.session_id == intent.session_id
                    && event.state == WatchEventState::Acknowledged
            }))
        });
        match consumed {
            Ok(true) => {
                return settled(
                    engine,
                    &operation.id,
                    attempt_no,
                    json!({
                        "kind":"WorkspaceReplyCompleted", "sessionId":intent.session_id,
                        "entryId":intent.user_entry_id,
                    }),
                )
                .await;
            }
            Ok(false) => {}
            Err(error) => return EffectResult::Unknown(error.to_string()),
        }
    }
    let mut sweep =
        match resume_recorded_turn(engine, &ctx, &intent, &operation, attempt_no, stopping).await {
            Ok(sweep) => sweep,
            Err(outcome) => return outcome,
        };
    let mut generation = match prepare(engine, &ctx, &intent, stopping, capacity.as_mut()).await {
        Ok(value) => value,
        Err(error) => {
            if stopping.is_cancelled() {
                return EffectResult::Unknown("Workspace reply paused for shutdown.".into());
            }
            return settled(engine, &operation.id, attempt_no, unavailable(error)).await;
        }
    };
    for _ in 0..64 {
        if stopping.is_cancelled() {
            return EffectResult::Unknown("Workspace reply paused for shutdown.".into());
        }
        match compact_workspace(
            engine,
            &ctx,
            &generation,
            (&operation.id, attempt_no),
            sweep,
            stopping,
        )
        .await
        {
            Ok(CompactionStep::Published) => {
                match prepare(engine, &ctx, &intent, stopping, capacity.as_mut()).await {
                    Ok(next) => generation = next,
                    Err(_) if stopping.is_cancelled() => {
                        return EffectResult::Unknown(
                            "Workspace reply paused for shutdown.".into(),
                        );
                    }
                    Err(error) => {
                        return settled(engine, &operation.id, attempt_no, unavailable(error))
                            .await;
                    }
                }
            }
            Ok(CompactionStep::NotNeeded) => {}
            Ok(CompactionStep::Unavailable(detail)) => {
                return settled(engine, &operation.id, attempt_no, unavailable(detail)).await;
            }
            Err(error) => return EffectResult::Unknown(error.to_string()),
        }
        let window = sweep.window(generation.windows);
        let request = match crate::agent_turns::plan_generation_window(
            engine,
            &ctx,
            &generation.route,
            &generation.binding,
            &generation.head,
            window,
        ) {
            Ok(request) => request,
            Err(error) => {
                return settled(engine, &operation.id, attempt_no, unavailable(error)).await;
            }
        };
        let prepared = PreparedReply {
            request_id: request.logical.request_id.clone(),
            head: generation.head.clone(),
            compaction_source: None,
            sweep,
        };
        if let Err(error) = save_prepared(engine, &operation.id, attempt_no, &prepared).await {
            return EffectResult::Unknown(error.to_string());
        }
        let changes = engine.state_changes();
        let progress = match engine
            .produce_and_dispatch_native_turn(request, Some(stopping.clone()))
            .await
        {
            Ok(progress) => progress,
            Err(error) => return EffectResult::Unknown(error.to_string()),
        };
        match progress.provider.result.outcome {
            Some(AttemptEnd::Completed) if progress.continuation.is_some() => {
                if sweep.complete_window() {
                    return settled(
                        engine,
                        &operation.id,
                        attempt_no,
                        completed(&intent, &progress.provider.result),
                    )
                    .await;
                }
            }
            Some(AttemptEnd::ToolCallsReady) => {
                if progress.continuation.is_none()
                    && let Err(error) = finish_tools(
                        engine,
                        &ctx,
                        &intent.session_id,
                        &prepared,
                        &progress.provider.result,
                        changes,
                        stopping,
                    )
                    .await
                {
                    return EffectResult::Unknown(error.to_string());
                }
                sweep.after_tools();
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
                if let Err(error) =
                    recover_interruption(engine, ctx.clone(), intent.session_id.clone()).await
                {
                    return EffectResult::Unknown(error.to_string());
                }
                return settled(
                    engine,
                    &operation.id,
                    attempt_no,
                    unavailable(provider_fault(&progress.provider.result)),
                )
                .await;
            }
        }
        match prepare(engine, &ctx, &intent, stopping, capacity.as_mut()).await {
            Ok(next) => generation = next,
            Err(_) if stopping.is_cancelled() => {
                return EffectResult::Unknown("Workspace reply paused for shutdown.".into());
            }
            Err(error) => {
                return settled(engine, &operation.id, attempt_no, unavailable(error)).await;
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

async fn recover_interruption(
    engine: &TurnEngine,
    ctx: InvocationContext,
    session: Id,
) -> Result<(), StorageError> {
    let engine = engine.clone();
    tokio::task::spawn_blocking(move || {
        crate::agent_turns::recover_interrupted_workspace_turn(&engine, &ctx, &session)
    })
    .await
    .map_err(invalid)?
}

fn provider_fault(result: &ProviderResult) -> &str {
    result.faults.first().map_or(
        "The selected route did not complete a reply.",
        String::as_str,
    )
}

/// Resume only the last exact request written by an earlier attempt. An admitted request
/// with an unknown outcome stays fenced against a second inference call.
async fn resume_recorded_turn(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    intent: &ReplyIntent,
    operation: &OperationRecord,
    attempt_no: i64,
    stopping: &Cancellation,
) -> Result<ToolWindowSweep, EffectResult> {
    let unknown = |error: StorageError| EffectResult::Unknown(error.to_string());
    let mut previous = None;
    for attempt in operation.attempts.iter().rev().skip(1) {
        let prepared = prepared_reply(attempt).map_err(unknown)?;
        if intent.watcher_source.is_some()
            && attempt.observations.iter().any(|observation| {
                observation["replySettlement"]["kind"] == "WorkspaceReplyUnavailable"
            })
        {
            // The exact failed request was reconciled and its carry released. Earlier
            // provider contexts may have been superseded by interruption recovery.
            return Ok(prepared.map_or_else(ToolWindowSweep::default, |reply| reply.sweep));
        }
        if prepared.is_some() {
            previous = prepared;
            break;
        }
    }
    let Some(prepared) = previous else {
        return Ok(ToolWindowSweep::default());
    };
    let mut sweep = prepared.sweep;
    let Some(recorded) = recorded_reply(engine, &operation.id, intent, &prepared)
        .await
        .map_err(unknown)?
    else {
        return Ok(sweep);
    };
    if intent.watcher_source.is_some()
        && matches!(
            recorded.provider.outcome,
            Some(AttemptEnd::Cancelled | AttemptEnd::Failed | AttemptEnd::Incomplete)
        )
    {
        // Its exact provider request has a recorded terminal failure. Start the next
        // bounded queue attempt with a fresh request and the same durable watcher input.
        return Ok(sweep);
    }
    save_prepared(engine, &operation.id, attempt_no, &prepared)
        .await
        .map_err(unknown)?;
    match recorded.provider.outcome {
        Some(AttemptEnd::Completed) if prepared.compaction_source.is_some() => {
            if recorded.published.is_none() {
                let source = prepared
                    .compaction_source
                    .ok_or_else(|| unknown(StorageError::ScopeDenied))?;
                let engine = engine.clone();
                let ctx = ctx.clone();
                tokio::task::spawn_blocking(move || {
                    finish_compaction(&engine, &ctx, &source, recorded.provider)
                })
                .await
                .map_err(|error| EffectResult::Unknown(error.to_string()))?
                .map_err(|error| EffectResult::Unknown(format!("{error:?}")))?;
            }
            Ok(sweep)
        }
        Some(AttemptEnd::ToolCallsReady) => {
            if recorded.published.is_none() {
                finish_tools(
                    engine,
                    ctx,
                    &intent.session_id,
                    &prepared,
                    &recorded.provider,
                    engine.state_changes(),
                    stopping,
                )
                .await
                .map_err(unknown)?;
            }
            sweep.after_tools();
            Ok(sweep)
        }
        Some(AttemptEnd::Completed) => {
            if recorded.published.is_none() {
                let (_, continuation) = engine
                    .continue_recorded_turn(
                        ctx,
                        &intent.session_id,
                        &prepared.head,
                        &recorded.provider,
                        None,
                        false,
                    )
                    .await
                    .map_err(unknown)?;
                if continuation.is_none() {
                    return Err(EffectResult::Unknown(
                        "The settled workspace reply lacks a continuation.".into(),
                    ));
                }
            }
            if !sweep.complete_window() {
                return Ok(sweep);
            }
            Err(settled(
                engine,
                &operation.id,
                attempt_no,
                completed(intent, &recorded.provider),
            )
            .await)
        }
        _ => Err(EffectResult::Unknown(
            "The prior provider outcome requires reconciliation.".into(),
        )),
    }
}

async fn save_prepared(
    engine: &TurnEngine,
    operation: &OperationId,
    attempt_no: i64,
    prepared: &PreparedReply,
) -> Result<(), StorageError> {
    observe(
        engine,
        operation,
        attempt_no,
        json!({"requestId": prepared.request_id, "head": prepared.head,
            "compactionSource": prepared.compaction_source, "toolWindowSweep": prepared.sweep.checkpoint()}),
    )
    .await
    .map(|_| ())
}

enum CompactionStep {
    NotNeeded,
    Published,
    Unavailable(String),
}

/// Use the same routed threshold and compaction producer as a managed agent. Recording the
/// exact immutable request before admission allows a restart to reuse its settled summary.
async fn compact_workspace(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    generation: &WorkspaceGeneration,
    operation: (&OperationId, i64),
    mut sweep: ToolWindowSweep,
    cancellation: &Cancellation,
) -> Result<CompactionStep, StorageError> {
    let window = sweep.window(generation.windows);
    let (worker, context, route, binding, head) = (
        engine.clone(),
        ctx.clone(),
        generation.route.clone(),
        generation.binding.clone(),
        generation.head.clone(),
    );
    let prepared = match tokio::task::spawn_blocking(move || {
        if crate::agent_turns::compaction_due(&worker, &context, &route, &head, window)? {
            crate::agent_turns::prepare_compaction(&worker, &context, &route, &binding, &head)
                .map(Some)
        } else {
            Ok(None)
        }
    })
    .await
    .map_err(invalid)?
    {
        Ok(prepared) => prepared,
        Err(error) => return Ok(CompactionStep::Unavailable(error.to_string())),
    };
    let Some((source, request)) = prepared else {
        return Ok(CompactionStep::NotNeeded);
    };
    let InferenceInput::Generation { context: input, .. } = &request.request.input else {
        return Err(StorageError::ScopeDenied);
    };
    let prepared = PreparedReply {
        request_id: request.logical.request_id.clone(),
        head: input.clone(),
        compaction_source: Some(source.artifact.clone()),
        sweep,
    };
    save_prepared(engine, operation.0, operation.1, &prepared).await?;
    let providers = engine.providers().ok_or(StorageError::ScopeDenied)?;
    let produced = providers
        .produce_cancellable(request, Some(cancellation.clone()))
        .await?;
    match produced.accounting {
        DriveOutcome::Refused(reason) => {
            return Ok(CompactionStep::Unavailable(format!("{reason:?}")));
        }
        DriveOutcome::CancelledBeforeDispatch => {
            return Ok(CompactionStep::Unavailable(
                "Compaction cancelled before dispatch.".into(),
            ));
        }
        _ => {}
    }
    let result = produced.result;
    if matches!(
        result.outcome,
        Some(AttemptEnd::Cancelled | AttemptEnd::Failed | AttemptEnd::Incomplete)
    ) {
        return Ok(CompactionStep::Unavailable(provider_fault(&result).into()));
    }
    let (worker, context, source) = (engine.clone(), ctx.clone(), source.artifact);
    tokio::task::spawn_blocking(move || finish_compaction(&worker, &context, &source, result))
        .await
        .map_err(invalid)?
        .map_err(|error| invalid(format!("{error:?}")))?;
    Ok(CompactionStep::Published)
}

fn finish_compaction(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &ArtifactRef,
    result: ProviderResult,
) -> Result<ArtifactRef, crate::turn_error::TurnError> {
    let binding = result.binding.clone().ok_or(StorageError::ScopeDenied)?;
    let (bytes, provenance) = engine
        .artifacts()
        .read_exact(engine.store().as_ref(), ctx, source, 8 * 1024 * 1024)
        .map_err(crate::turn_error::artifact_read_fault)?;
    let derived = &provenance["derived"];
    if derived["binding"] != json!(binding)
        || derived["sessionId"].as_str() != ctx.session_id().map(Id::as_str)
    {
        return Err(StorageError::ScopeDenied.into());
    }
    let context = serde_json::from_slice(&bytes).map_err(invalid)?;
    let observed = derived["observedSequence"]
        .as_i64()
        .ok_or(StorageError::ScopeDenied)?;
    crate::agent_turns::publish_compaction_result(
        engine,
        ctx,
        &binding,
        crate::context_epoch::EpochSource {
            context,
            artifact: source.clone(),
            observed_sequence: Some(observed),
        },
        result,
        &mut None,
        None,
    )
}

async fn finish_tools(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    session: &Id,
    prepared: &PreparedReply,
    result: &ProviderResult,
    mut changes: tokio::sync::watch::Receiver<u64>,
    stopping: &Cancellation,
) -> Result<(), StorageError> {
    let stopped = Wake::new();
    stopping.wake_on_cancel(&stopped);
    let generation = stopped.generation();
    loop {
        if stopping.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        changes.borrow_and_update();
        let started = tokio::time::Instant::now();
        let (_, continuation) = engine
            .continue_recorded_turn(ctx, session, &prepared.head, result, Some(stopping), true)
            .await?;
        if continuation.is_some() {
            return Ok(());
        }
        // Re-drive only the recorded tool identity after a possible approval or receipt change.
        tokio::select! {
            _ = changes.changed() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            _ = stopped.changed_after(generation) => return Err(StorageError::Cancelled),
        }
        tokio::time::sleep_until(started + std::time::Duration::from_millis(500)).await;
    }
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
        Ok(watcher) => settlement_outcome(watcher, value),
        Err(error) => EffectResult::Unknown(error.to_string()),
    }
}

async fn observe(
    engine: &TurnEngine,
    operation: &OperationId,
    attempt: i64,
    value: Value,
) -> Result<bool, StorageError> {
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
        if let Some(settlement) = value.get("replySettlement") {
            release_carry(
                tx,
                &current,
                settlement["kind"] == "WorkspaceReplyCompleted",
            )?;
        }
        tx.operations().observe(&operation, attempt, value)?;
        Ok(current.payload["watcherSource"].is_string())
    })
    .await
}

struct WorkspaceGeneration {
    route: RouteChoice,
    binding: NativeBinding,
    head: ArtifactRef,
    windows: usize,
}

async fn prepare(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    intent: &ReplyIntent,
    cancel: &Cancellation,
    mut capacity: Option<&mut EffectCapacity<'_>>,
) -> Result<WorkspaceGeneration, String> {
    if crate::auto_routing::needs_assignment(engine, ctx).map_err(|e| e.to_string())? {
        let (origin, material) =
            first_auto_input(engine, ctx, intent).map_err(|e| e.to_string())?;
        crate::auto_routing::assign(
            engine,
            ctx,
            origin,
            material,
            cancel,
            capacity.as_deref_mut(),
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    }
    crate::agent_turns::wait_for_running_tool_results(
        engine,
        ctx,
        &intent.session_id,
        cancel,
        capacity,
    )
    .await
    .map_err(|error| error.to_string())?;
    loop {
        let (worker, ctx, intent) = (engine.clone(), ctx.clone(), intent.clone());
        let result = tokio::task::spawn_blocking(move || prepare_prompt(&worker, &ctx, &intent))
            .await
            .map_err(|error| error.to_string())?;
        match result {
            Err(crate::turn_error::TurnError::Cooling { until_ms }) => {
                crate::auto_routing::wait_until(engine, until_ms, cancel)
                    .await
                    .map_err(|error| format!("{error:?}"))?
            }
            other => return other.map_err(|error| format!("{error:?}")),
        }
    }
}

fn first_auto_input(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    intent: &ReplyIntent,
) -> Result<(String, ProviderContext), StorageError> {
    let (entry, role) = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
        let session = session_for_reply(read, intent)?;
        if intent.user_sequence > session.last_durable_sequence() {
            return Err(StorageError::ScopeDenied);
        }
        let mut after = 0;
        while after < intent.user_sequence && after < 10_000 {
            let remaining = (intent.user_sequence - after).min(500) as u16;
            let entries = read
                .sessions()
                .entries(&intent.session_id, after, remaining)?;
            if entries.is_empty() {
                break;
            }
            for entry in entries {
                if entry.kind == "UserMessage" {
                    let role = devworks_core::settings::service::role_instructions(
                        read.configuration(),
                        AgentRole::WorkspaceCoordinator,
                        None,
                    )
                    .map_err(|_| StorageError::ScopeDenied)?;
                    return Ok((entry, role));
                }
                after = entry.sequence;
            }
        }
        Err(StorageError::ScopeDenied)
    })?;
    let source = serde_json::from_value(entry.body)
        .map_err(|error| StorageError::InvalidRecord(format!("automatic route input: {error}")))?;
    let mut context = ProviderContext {
        version: 1,
        instructions: (!role.text.is_empty())
            .then_some(TextBlock {
                text: role.text,
                cache: None,
            })
            .into_iter()
            .collect(),
        instruction_stamps: Vec::new(),
        skill_stamp: None,
        lesson_stamp: None,
        messages: Vec::new(),
        parameters: GenerationParameters::default(),
    };
    crate::tool_turn::append_context_fact(
        &mut context,
        engine.store().as_ref(),
        engine.artifacts(),
        ctx,
        &intent.session_id,
        source,
    )?;
    Ok((entry.entry_id.as_str().to_owned(), context))
}

fn prepare_prompt(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    intent: &ReplyIntent,
) -> Result<WorkspaceGeneration, crate::turn_error::TurnError> {
    let session = &intent.session_id;
    let recovered = engine.recover_native_context_head(ctx, session)?;
    if let Some((head, binding, _)) = recovered {
        read_snapshot(
            engine.store().as_ref(),
            &ReadOptions::default(),
            |snapshot| input_prefix(snapshot, intent, intent.user_sequence - 1).map(|_| ()),
        )?;
        let (head, binding, route, windows) = crate::agent_turns::prepare_generation_context(
            engine, ctx, session, head, binding, false,
        )?;
        return Ok(WorkspaceGeneration {
            route,
            binding,
            head,
            windows,
        });
    }
    let (inputs, role) = read_snapshot(
        engine.store().as_ref(),
        &ReadOptions::default(),
        |snapshot| {
            let inputs = input_prefix(snapshot, intent, 0)?;
            let role = devworks_core::settings::service::role_instructions(
                snapshot.configuration(),
                AgentRole::WorkspaceCoordinator,
                None,
            )
            .map_err(|_| StorageError::ScopeDenied)?;
            Ok((inputs, role))
        },
    )?;
    let images = inputs.iter().any(|body| match body {
        TranscriptBody::UserMessage { content } => content.iter().any(|item| {
            matches!(item, UserContent::Attachment { artifact } if artifact.media_type.starts_with("image/"))
        }),
        _ => false,
    });
    let tool_count = engine
        .providers()
        .ok_or(StorageError::ScopeDenied)?
        .schemas()
        .selected_agent_tools(ctx)?
        .len();
    let windows = crate::agent_turns::tool_window_count(tool_count);
    let route = crate::agent_turns::route_context_head(
        engine,
        ctx,
        session,
        intent.context_epoch,
        None,
        tool_count > 0,
        images,
    )?;
    let (binding, headroom, _) =
        crate::agent_turns::initial_route_binding(engine, session, &route)?;
    let mut instructions: Vec<_> = (!role.text.is_empty())
        .then_some(TextBlock {
            text: role.text,
            cache: None,
        })
        .into_iter()
        .collect();
    if windows > 1 {
        instructions.push(TextBlock {
            text: crate::agent_turns::TOOL_WINDOW_NOTICE.into(),
            cache: None,
        });
    }
    let mut context = ProviderContext {
        version: 1,
        instructions,
        instruction_stamps: Vec::new(),
        skill_stamp: None,
        lesson_stamp: None,
        messages: Vec::new(),
        parameters: GenerationParameters {
            max_output_tokens: (binding.endpoint_profile
                != EndpointProfile::CodexSubscriptionResponses)
                .then_some(headroom as u32),
            ..GenerationParameters::default()
        },
    };
    for body in inputs {
        crate::tool_turn::append_context_fact(
            &mut context,
            engine.store().as_ref(),
            engine.artifacts(),
            ctx,
            session,
            body,
        )?;
    }
    let context = engine.bind_provider_instructions(ctx, context, &[])?;
    let initial = engine.publish_initial_native_context(ctx, session, &binding, &context)?;
    let head = crate::tool_turn::append_operation_updates(
        engine.store().as_ref(),
        engine.artifacts(),
        ctx,
        session,
        &initial,
    )?;
    Ok(WorkspaceGeneration {
        route,
        binding,
        head,
        windows,
    })
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
        || session.context_epoch < intent.context_epoch
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
            if cursor == intent.user_sequence {
                let expected = match &intent.watcher_source {
                    Some(source) => {
                        snapshot.actors().actor_kind(&entry.actor)? == Some(PrincipalKind::Host)
                            && crate::watch::is_notice(&body)
                            && matches!(&body, TranscriptBody::HostNotice { source: notice, .. } if notice == source)
                    }
                    None => {
                        entry.actor == intent.creator_id
                            && matches!(&body, TranscriptBody::UserMessage { .. })
                    }
                };
                if entry.entry_id != intent.user_entry_id
                    || entry.context_epoch != intent.context_epoch
                    || !expected
                {
                    return Err(StorageError::ScopeDenied);
                }
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
