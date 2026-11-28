//! One background agent conversation turn that no run assignment owns: a native workspace reply
//! or a scheduled inference job run. Both prepare their first provider context from the saved
//! session inputs and role instructions, then drive routed generation, durable tool dispatch,
//! compaction and interruption recovery through the same loop. Each prepared request is recorded
//! on the owning operation's attempt before admission, so a later attempt resumes the exact
//! recorded request instead of sending a second one.
use crate::agent_turns::ToolWindowSweep;
use crate::dispatch::EffectCapacity;
use crate::providers::sink::ProviderResult;
use crate::turn_engine::TurnEngine;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::storage::{
    AttemptRecord, Cancellation, OperationRecord, ReadOptions, ReadSnapshot, StorageError,
    StoredSession, Wake, WriteOptions, WriteTransaction, read_snapshot,
};
use devworks_core::protocol::provider::{
    ArtifactRef, EndpointProfile, InferenceInput, NativeBinding, TranscriptBody, UserContent,
};
use devworks_core::protocol::records::RouteChoice;
use devworks_core::protocol::{ActorId, Id, OperationId, OperationState, ProjectId};
use devworks_core::providers::context::{GenerationParameters, ProviderContext, TextBlock};
use devworks_core::services::runs::substrate::Fenced;
use devworks_core::services::sessions::workspace_reply::ReplyIntent;
use devworks_core::services::usage::accounting::{Admission, AttemptEnd};
use devworks_core::services::usage::host::DriveOutcome;
use devworks_core::settings::catalog::AgentRole;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;

/// The most provider generations one turn drives before it gives up.
const TURN_LIMIT: usize = 64;

/// What a background turn answers: a saved workspace message, or a scheduled job's prompt.
#[derive(Clone, Debug)]
pub(crate) enum TurnSource {
    Workspace(ReplyIntent),
    Job(JobTurn),
}

/// A scheduled inference run's agent session: owned by the job's agent principal in the job's
/// project scope (no root, no assignment), whose first transcript entry is the host notice the
/// run recorded with its saved prompt.
#[derive(Clone, Debug)]
pub(crate) struct JobTurn {
    pub run_id: Id,
    pub session_id: Id,
    pub agent: ActorId,
    pub project: Option<ProjectId>,
    /// The role the host bound the job's agent with (its instructions open the context).
    pub role: AgentRole,
}

impl TurnSource {
    pub(crate) fn session(&self) -> &Id {
        match self {
            Self::Workspace(intent) => &intent.session_id,
            Self::Job(job) => &job.session_id,
        }
    }

    pub(crate) fn agent(&self) -> &ActorId {
        match self {
            Self::Workspace(intent) => &intent.agent_id,
            Self::Job(job) => &job.agent,
        }
    }

    /// The context epoch the turn's first route is planned against.
    fn context_epoch(&self) -> i64 {
        match self {
            Self::Workspace(intent) => intent.context_epoch,
            Self::Job(_) => 0,
        }
    }

    fn watcher(&self) -> bool {
        matches!(self, Self::Workspace(intent) if intent.watcher_source.is_some())
    }

    fn role_instructions(
        &self,
        read: &dyn ReadSnapshot,
    ) -> Result<devworks_core::settings::service::EffectiveInstructions, StorageError> {
        let (role, project) = match self {
            Self::Workspace(_) => (AgentRole::WorkspaceCoordinator, None),
            Self::Job(job) => (job.role, job.project.as_ref()),
        };
        devworks_core::settings::service::role_instructions(read.configuration(), role, project)
            .map_err(|_| StorageError::ScopeDenied)
    }

    /// The session, checked against the turn's owner.
    pub(crate) fn checked_session(
        &self,
        read: &dyn ReadSnapshot,
    ) -> Result<StoredSession, StorageError> {
        match self {
            Self::Workspace(intent) => crate::workspace_reply::session_for_reply(read, intent),
            Self::Job(job) => job_session(read, job),
        }
    }

    /// The ordered inputs of the first prompt after transcript position `after`.
    fn inputs(
        &self,
        read: &dyn ReadSnapshot,
        after: i64,
    ) -> Result<Vec<TranscriptBody>, StorageError> {
        match self {
            Self::Workspace(intent) => crate::workspace_reply::input_prefix(read, intent, after),
            Self::Job(job) => job_inputs(read, job).map(|body| vec![body]),
        }
    }

    /// Rechecks the owner's inputs before a recovered context head continues.
    fn check_recovered(&self, read: &dyn ReadSnapshot) -> Result<(), StorageError> {
        match self {
            Self::Workspace(intent) => {
                crate::workspace_reply::input_prefix(read, intent, intent.user_sequence - 1)
                    .map(|_| ())
            }
            Self::Job(job) => job_inputs(read, job).map(|_| ()),
        }
    }

    /// Whether a published context's provenance belongs to this turn. A workspace session is
    /// shared by its replies, so each publication names its reply; a job session belongs to its
    /// one run, and its inference attempt is already checked against the session and agent.
    fn published_by(&self, derived: &Value, operation: &OperationId) -> bool {
        match self {
            Self::Workspace(_) => derived["workspaceReply"] == json!(operation.0),
            Self::Job(_) => true,
        }
    }

    /// The material an automatic route classifies: the role instructions and the first input.
    fn first_auto_input(
        &self,
        read: &dyn ReadSnapshot,
    ) -> Result<(String, String, TranscriptBody), StorageError> {
        match self {
            Self::Workspace(intent) => {
                let session = self.checked_session(read)?;
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
                            let role = self.role_instructions(read)?;
                            let body = serde_json::from_value(entry.body).map_err(|error| {
                                StorageError::InvalidRecord(format!(
                                    "automatic route input: {error}"
                                ))
                            })?;
                            return Ok((entry.entry_id.as_str().to_owned(), role.text, body));
                        }
                        after = entry.sequence;
                    }
                }
                Err(StorageError::ScopeDenied)
            }
            Self::Job(job) => {
                let body = job_inputs(read, job)?;
                let role = self.role_instructions(read)?;
                Ok((job.run_id.as_str().to_owned(), role.text, body))
            }
        }
    }
}

fn job_session(read: &dyn ReadSnapshot, job: &JobTurn) -> Result<StoredSession, StorageError> {
    let session = read
        .sessions()
        .session(&job.session_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if session.closed()
        || session.agent_id != job.agent
        || session.root_id.is_some()
        || session.project_id != job.project
        || !read
            .authority()
            .principal(&job.agent)?
            .is_some_and(|binding| {
                binding.active
                    && binding.kind == PrincipalKind::Agent
                    && binding.project_id == job.project
            })
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(session)
}

/// The job session's first entry: the host notice of the run's saved prompt (only the host
/// appends notices, under the run's identity).
fn job_inputs(read: &dyn ReadSnapshot, job: &JobTurn) -> Result<TranscriptBody, StorageError> {
    job_session(read, job)?;
    let entry = read
        .sessions()
        .entries(&job.session_id, 0, 1)?
        .into_iter()
        .next()
        .filter(|entry| entry.sequence == 1 && entry.kind == "HostNotice")
        .ok_or(StorageError::ScopeDenied)?;
    let body: TranscriptBody = serde_json::from_value(entry.body).map_err(invalid)?;
    match &body {
        TranscriptBody::HostNotice { source, .. } if source == &job.run_id => Ok(body),
        _ => Err(StorageError::ScopeDenied),
    }
}

/// How a background turn ended.
#[derive(Debug)]
pub(crate) enum TurnEnd {
    /// The final assistant turn of the conversation was published.
    Completed(Box<ProviderResult>),
    /// The turn cannot complete; `sent` when a model request may already have been sent.
    Unavailable { detail: String, sent: bool },
    /// What the turn did cannot be established yet; it needs reconciliation.
    Unknown(String),
    /// The turn's cancellation was requested and it stopped.
    Stopped,
}

/// Reports what the turn waits on (a held tool call, a refused admission that clears later) and
/// its release. An owner without one ends the turn on a refused admission instead of waiting.
pub(crate) trait TurnHolds: Send + Sync {
    fn held(&self, reason: String) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
    fn resumed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// The visible reason of a turn whose tool call waits.
const TOOL_HOLD: &str = "A tool call of the agent is waiting on a hold that clears later (an \
    approval, a pause, capacity); the run continues once it is resolved.";

/// Why an inference admission refusal is a hold that clears later (a feature budget, a paused
/// or otherwise ineligible root), or `None` for a refusal that ends the turn.
fn admission_hold(admission: &Admission) -> Option<String> {
    match admission {
        Admission::Blocked(block) => Some(format!(
            "The model request is held by its feature budget ({block:?}); the run continues \
             once the budget allows it."
        )),
        Admission::Fenced(Fenced::Blocked(eligibility)) => Some(format!(
            "The model request is held: its work is paused or not eligible now ({:?}); the run \
             continues once it is.",
            eligibility.conditions
        )),
        _ => None,
    }
}

/// Waits for a committed state change (or the 30 s recheck), and at least until `started` +
/// `pace`; `Err(Cancelled)` once `stopping` is cancelled.
async fn wait_for_change(
    changes: &mut tokio::sync::watch::Receiver<u64>,
    stopping: &Cancellation,
    started: tokio::time::Instant,
    pace: std::time::Duration,
) -> Result<(), StorageError> {
    let stopped = Wake::new();
    stopping.wake_on_cancel(&stopped);
    let generation = stopped.generation();
    if stopping.is_cancelled() {
        return Err(StorageError::Cancelled);
    }
    tokio::select! {
        _ = changes.changed() => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
        _ = stopped.changed_after(generation) => return Err(StorageError::Cancelled),
    }
    tokio::select! {
        _ = tokio::time::sleep_until(started + pace) => Ok(()),
        _ = stopped.changed_after(generation) => Err(StorageError::Cancelled),
    }
}

/// The first and longest pause between retries of a request whose admission is held.
const HOLD_PACE: (std::time::Duration, std::time::Duration) = (
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(300),
);

#[derive(Clone)]
pub(crate) struct PreparedTurn {
    pub(crate) request_id: Id,
    pub(crate) head: ArtifactRef,
    pub(crate) compaction_source: Option<ArtifactRef>,
    pub(crate) sweep: ToolWindowSweep,
}

pub(crate) fn prepared_turn(attempt: &AttemptRecord) -> Result<Option<PreparedTurn>, StorageError> {
    attempt
        .observations
        .iter()
        .rev()
        .find(|value| value.get("requestId").is_some())
        .map(|value| {
            Ok(PreparedTurn {
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

pub(crate) fn invalid(error: impl ToString) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}

pub(crate) struct RecordedTurn {
    pub(crate) provider: ProviderResult,
    pub(crate) published: Option<ArtifactRef>,
}

/// The provider result of `prepared`'s exact request, if it was admitted, and the context head
/// this turn published from it.
pub(crate) async fn recorded_turn(
    engine: &TurnEngine,
    operation: &OperationId,
    source: &TurnSource,
    prepared: &PreparedTurn,
) -> Result<Option<RecordedTurn>, StorageError> {
    let (engine_for_read, operation, source, prepared) = (
        engine.clone(),
        operation.clone(),
        source.clone(),
        prepared.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let engine = engine_for_read;
        let admitted = read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
            let session = source.checked_session(read)?;
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
                if inference.actor_id != *source.agent()
                    || inference.session_id.as_ref() != Some(source.session())
                    || inference.request_id != prepared.request_id
                {
                    return Err(StorageError::ScopeDenied);
                }
                if let Some(compaction) = &prepared.compaction_source {
                    let binding = provider.binding.as_ref().ok_or(StorageError::ScopeDenied)?;
                    let next = binding
                        .context_epoch
                        .get()
                        .checked_add(1)
                        .ok_or(StorageError::ScopeDenied)?;
                    let Some(epoch) = read.sessions().epoch(source.session(), next)? else {
                        return Ok(None);
                    };
                    let artifact: ArtifactRef =
                        serde_json::from_value(epoch.context).map_err(invalid)?;
                    let version = read
                        .artifacts()
                        .version(&artifact.version_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    let derived = &version.source["derived"];
                    if !source.published_by(derived, &operation)
                        || derived["sourceContext"] != json!(compaction)
                        || derived["reason"] != "Compaction"
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    return Ok(Some(artifact));
                }
                let Some(head) = read
                    .sessions()
                    .provider_context_head(source.session())?
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
                if !source.published_by(&version.source["derived"], &operation) {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(Some(artifact))
            })?
        } else {
            None
        };
        Ok(Some(RecordedTurn {
            provider,
            published,
        }))
    })
    .await
    .map_err(invalid)?
}

/// The operation `operation` while `attempt` is its current dispatching attempt.
pub(crate) fn dispatching_attempt(
    tx: &mut dyn WriteTransaction,
    operation: &OperationId,
    attempt: i64,
    kind: &'static str,
) -> Result<OperationRecord, StorageError> {
    let current = tx
        .operations()
        .operation(operation)?
        .ok_or(StorageError::ScopeDenied)?;
    if current.state != OperationState::Dispatching
        || current
            .latest_attempt()
            .is_none_or(|entry| entry.attempt_no != attempt)
    {
        return Err(StorageError::LeaseLost {
            kind,
            id: operation.0.as_str().to_owned(),
        });
    }
    Ok(current)
}

async fn save_prepared(
    engine: &TurnEngine,
    operation: &OperationId,
    attempt_no: i64,
    prepared: &PreparedTurn,
) -> Result<(), StorageError> {
    let operation = operation.clone();
    let value = json!({"requestId": prepared.request_id, "head": prepared.head,
        "compactionSource": prepared.compaction_source,
        "toolWindowSweep": prepared.sweep.checkpoint()});
    crate::store::write(engine.store(), WriteOptions::observed(), move |tx| {
        dispatching_attempt(tx, &operation, attempt_no, "background_turn")?;
        tx.operations()
            .observe(&operation, attempt_no, value.clone())
    })
    .await
    .map(|_| ())
}

/// Drives one turn of `source` under `operation`'s current attempt `attempt_no` until it ends.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &TurnSource,
    operation: &OperationRecord,
    attempt_no: i64,
    stopping: &Cancellation,
    mut capacity: Option<EffectCapacity<'_>>,
    holds: Option<&dyn TurnHolds>,
) -> TurnEnd {
    let mut sent = false;
    let mut sweep = match resume_recorded_turn(
        engine, ctx, source, operation, attempt_no, stopping, holds, &mut sent,
    )
    .await
    {
        Ok(sweep) => sweep,
        Err(end) => return end,
    };
    let unavailable = |detail: String, sent: bool| TurnEnd::Unavailable { detail, sent };
    let mut generation =
        match prepare(engine, ctx, source, stopping, capacity.as_mut(), &mut sent).await {
            Ok(value) => value,
            Err(_) if stopping.is_cancelled() => return TurnEnd::Stopped,
            Err(error) => return unavailable(error, sent),
        };
    let mut generations = 0;
    let mut hold_pace = HOLD_PACE.0;
    while generations < TURN_LIMIT {
        if stopping.is_cancelled() {
            return TurnEnd::Stopped;
        }
        match compact(
            engine,
            ctx,
            &generation,
            (&operation.id, attempt_no),
            sweep,
            stopping,
            &mut sent,
        )
        .await
        {
            Ok(CompactionStep::Published) => {
                match prepare(engine, ctx, source, stopping, capacity.as_mut(), &mut sent).await {
                    Ok(next) => generation = next,
                    Err(_) if stopping.is_cancelled() => return TurnEnd::Stopped,
                    Err(error) => return unavailable(error, sent),
                }
            }
            Ok(CompactionStep::NotNeeded) => {}
            Ok(CompactionStep::Unavailable(detail)) => return unavailable(detail, sent),
            Err(error) => return TurnEnd::Unknown(error.to_string()),
        }
        let window = sweep.window(generation.windows);
        let request = match crate::agent_turns::plan_generation_window(
            engine,
            ctx,
            &generation.route,
            &generation.binding,
            &generation.head,
            window,
        ) {
            Ok(request) => request,
            Err(error) => return unavailable(error.to_string(), sent),
        };
        let prepared = PreparedTurn {
            request_id: request.logical.request_id.clone(),
            head: generation.head.clone(),
            compaction_source: None,
            sweep,
        };
        if let Err(error) = save_prepared(engine, &operation.id, attempt_no, &prepared).await {
            return TurnEnd::Unknown(error.to_string());
        }
        let mut changes = engine.state_changes();
        changes.borrow_and_update();
        let started = tokio::time::Instant::now();
        // A scheduled run's request is admitted only while the run is not holding, so a hold
        // the run records (a waiting tool call, a resumed held run, a held admission) is
        // released before each request; the owner reports a release only when it holds.
        if let Some(holds) = holds {
            holds.resumed().await;
        }
        sent = true;
        let progress = match engine
            .produce_and_dispatch_native_turn(request, Some(stopping.clone()))
            .await
        {
            Ok(progress) => progress,
            Err(error) => return TurnEnd::Unknown(error.to_string()),
        };
        if let (None, Some(holds), DriveOutcome::Refused(admission)) = (
            &progress.provider.result.outcome,
            holds,
            &progress.provider.accounting,
        ) && let Some(reason) = admission_hold(admission)
        {
            // Nothing was admitted: wait for a state change, then retry the same request, paced
            // so a lasting hold is retried (and its release and hold recorded) at most once per
            // pause, doubling to five minutes.
            holds.held(reason).await;
            if wait_for_change(&mut changes, stopping, started, hold_pace)
                .await
                .is_err()
            {
                return TurnEnd::Stopped;
            }
            hold_pace = (hold_pace * 2).min(HOLD_PACE.1);
            continue;
        }
        hold_pace = HOLD_PACE.0;
        generations += 1;
        match progress.provider.result.outcome {
            Some(AttemptEnd::Completed) if progress.continuation.is_some() => {
                if sweep.complete_window() {
                    return TurnEnd::Completed(Box::new(progress.provider.result));
                }
            }
            Some(AttemptEnd::ToolCallsReady) => {
                if progress.continuation.is_none()
                    && let Err(error) = finish_tools(
                        engine,
                        ctx,
                        source.session(),
                        &prepared,
                        &progress.provider.result,
                        changes,
                        stopping,
                        holds,
                    )
                    .await
                {
                    if stopping.is_cancelled() {
                        return TurnEnd::Stopped;
                    }
                    return TurnEnd::Unknown(error.to_string());
                }
                sweep.after_tools();
            }
            Some(AttemptEnd::UnknownTail) | Some(AttemptEnd::Completed) => {
                return TurnEnd::Unknown(
                    "The model request's outcome is unknown (its response did not end \
                     verifiably), so what it did must be reconciled before the run is retried."
                        .into(),
                );
            }
            None if matches!(
                progress.provider.accounting,
                DriveOutcome::AlreadyDispatched
            ) =>
            {
                return TurnEnd::Unknown("The provider request is already dispatched.".into());
            }
            _ => {
                if let Err(error) =
                    recover_interruption(engine, ctx.clone(), source.session().clone()).await
                {
                    return TurnEnd::Unknown(error.to_string());
                }
                return unavailable(provider_fault(&progress.provider.result).into(), sent);
            }
        }
        match prepare(engine, ctx, source, stopping, capacity.as_mut(), &mut sent).await {
            Ok(next) => generation = next,
            Err(_) if stopping.is_cancelled() => return TurnEnd::Stopped,
            Err(error) => return unavailable(error, sent),
        }
    }
    unavailable("The turn exceeded its tool-turn limit.".into(), sent)
}

pub(crate) async fn recover_interruption(
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

pub(crate) fn provider_fault(result: &ProviderResult) -> &str {
    result.faults.first().map_or(
        "The selected route did not complete a reply.",
        String::as_str,
    )
}

/// Resume only the last exact request written by an earlier attempt. An admitted request
/// with an unknown outcome stays fenced against a second inference call.
#[allow(clippy::too_many_arguments)]
async fn resume_recorded_turn(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &TurnSource,
    operation: &OperationRecord,
    attempt_no: i64,
    stopping: &Cancellation,
    holds: Option<&dyn TurnHolds>,
    sent: &mut bool,
) -> Result<ToolWindowSweep, TurnEnd> {
    let unknown = |error: StorageError| TurnEnd::Unknown(error.to_string());
    let mut previous = None;
    for attempt in operation.attempts.iter().rev().skip(1) {
        let prepared = prepared_turn(attempt).map_err(unknown)?;
        if source.watcher()
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
    // An earlier attempt prepared a request: it may have been sent.
    *sent = true;
    let mut sweep = prepared.sweep;
    let Some(recorded) = recorded_turn(engine, &operation.id, source, &prepared)
        .await
        .map_err(unknown)?
    else {
        return Ok(sweep);
    };
    if source.watcher()
        && matches!(
            recorded.provider.outcome,
            Some(AttemptEnd::Cancelled | AttemptEnd::Failed | AttemptEnd::Incomplete)
        )
    {
        // Its exact provider request has a recorded terminal failure. Start the next
        // bounded queue attempt with a fresh request and the same durable watcher input.
        return Ok(sweep);
    }
    if matches!(source, TurnSource::Job(_))
        && matches!(
            recorded.provider.outcome,
            Some(AttemptEnd::Cancelled | AttemptEnd::Failed | AttemptEnd::Incomplete)
        )
    {
        // A resumed scheduled run: its exact request ended without effect (an interrupted
        // stream, say). The interruption is reconciled into the session, and the run continues
        // with a fresh request from that head.
        recover_interruption(engine, ctx.clone(), source.session().clone())
            .await
            .map_err(unknown)?;
        return Ok(sweep);
    }
    save_prepared(engine, &operation.id, attempt_no, &prepared)
        .await
        .map_err(unknown)?;
    match recorded.provider.outcome {
        Some(AttemptEnd::Completed) if prepared.compaction_source.is_some() => {
            if recorded.published.is_none() {
                let compaction = prepared
                    .compaction_source
                    .ok_or_else(|| unknown(StorageError::ScopeDenied))?;
                let engine = engine.clone();
                let ctx = ctx.clone();
                tokio::task::spawn_blocking(move || {
                    finish_compaction(&engine, &ctx, &compaction, recorded.provider)
                })
                .await
                .map_err(|error| TurnEnd::Unknown(error.to_string()))?
                .map_err(|error| TurnEnd::Unknown(format!("{error:?}")))?;
            }
            Ok(sweep)
        }
        Some(AttemptEnd::ToolCallsReady) => {
            if recorded.published.is_none() {
                finish_tools(
                    engine,
                    ctx,
                    source.session(),
                    &prepared,
                    &recorded.provider,
                    engine.state_changes(),
                    stopping,
                    holds,
                )
                .await
                .map_err(|error| {
                    if stopping.is_cancelled() {
                        TurnEnd::Stopped
                    } else {
                        unknown(error)
                    }
                })?;
            }
            sweep.after_tools();
            Ok(sweep)
        }
        Some(AttemptEnd::Completed) => {
            if recorded.published.is_none() {
                let (_, continuation) = engine
                    .continue_recorded_turn(
                        ctx,
                        source.session(),
                        &prepared.head,
                        &recorded.provider,
                        None,
                        false,
                    )
                    .await
                    .map_err(unknown)?;
                if continuation.is_none() {
                    return Err(TurnEnd::Unknown(
                        "The settled turn lacks a continuation.".into(),
                    ));
                }
            }
            if !sweep.complete_window() {
                return Ok(sweep);
            }
            Err(TurnEnd::Completed(Box::new(recorded.provider)))
        }
        outcome => Err(TurnEnd::Unknown(format!(
            "The interrupted turn's last model request has an unknown outcome ({outcome:?}); \
             what it did must be reconciled before the run is retried."
        ))),
    }
}

enum CompactionStep {
    NotNeeded,
    Published,
    Unavailable(String),
}

/// Use the same routed threshold and compaction producer as a managed agent. Recording the
/// exact immutable request before admission allows a restart to reuse its settled summary.
async fn compact(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    generation: &Generation,
    operation: (&OperationId, i64),
    mut sweep: ToolWindowSweep,
    cancellation: &Cancellation,
    sent: &mut bool,
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
    let prepared = PreparedTurn {
        request_id: request.logical.request_id.clone(),
        head: input.clone(),
        compaction_source: Some(source.artifact.clone()),
        sweep,
    };
    save_prepared(engine, operation.0, operation.1, &prepared).await?;
    let providers = engine.providers().ok_or(StorageError::ScopeDenied)?;
    *sent = true;
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

/// Re-drives the recorded tool calls of a settled turn until its continuation is published. A
/// call still waiting (on an approval, say) is reported held, and released when it continues
/// (the owner reports each change once).
#[allow(clippy::too_many_arguments)]
async fn finish_tools(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    session: &Id,
    prepared: &PreparedTurn,
    result: &ProviderResult,
    mut changes: tokio::sync::watch::Receiver<u64>,
    stopping: &Cancellation,
    holds: Option<&dyn TurnHolds>,
) -> Result<(), StorageError> {
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
            // Also releases a hold the turn was resumed under.
            if let Some(holds) = holds {
                holds.resumed().await;
            }
            return Ok(());
        }
        if let Some(holds) = holds {
            holds.held(TOOL_HOLD.into()).await;
        }
        // Re-drive only the recorded tool identity after a possible approval or receipt change.
        wait_for_change(
            &mut changes,
            stopping,
            started,
            std::time::Duration::from_millis(500),
        )
        .await?;
    }
}

struct Generation {
    route: RouteChoice,
    binding: NativeBinding,
    head: ArtifactRef,
    windows: usize,
}

async fn prepare(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &TurnSource,
    cancel: &Cancellation,
    mut capacity: Option<&mut EffectCapacity<'_>>,
    sent: &mut bool,
) -> Result<Generation, String> {
    if crate::auto_routing::needs_assignment(engine, ctx).map_err(|e| e.to_string())? {
        let (origin, material) =
            first_auto_input(engine, ctx, source).map_err(|e| e.to_string())?;
        // Automatic routing may classify the input with a model.
        *sent = true;
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
        source.session(),
        cancel,
        capacity,
    )
    .await
    .map_err(|error| error.to_string())?;
    loop {
        let (worker, ctx, turn) = (engine.clone(), ctx.clone(), source.clone());
        let result = tokio::task::spawn_blocking(move || prepare_prompt(&worker, &ctx, &turn))
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

fn instructions(role: String) -> Vec<TextBlock> {
    (!role.is_empty())
        .then_some(TextBlock {
            text: role,
            cache: None,
        })
        .into_iter()
        .collect()
}

fn first_auto_input(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &TurnSource,
) -> Result<(String, ProviderContext), StorageError> {
    let (origin, role, body) =
        read_snapshot(engine.store().as_ref(), &ReadOptions::default(), |read| {
            source.first_auto_input(read)
        })?;
    let mut context = ProviderContext {
        version: 1,
        instructions: instructions(role),
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
        source.session(),
        body,
    )?;
    Ok((origin, context))
}

fn prepare_prompt(
    engine: &TurnEngine,
    ctx: &InvocationContext,
    source: &TurnSource,
) -> Result<Generation, crate::turn_error::TurnError> {
    let session = source.session();
    let recovered = engine.recover_native_context_head(ctx, session)?;
    if let Some((head, binding, _)) = recovered {
        read_snapshot(
            engine.store().as_ref(),
            &ReadOptions::default(),
            |snapshot| source.check_recovered(snapshot),
        )?;
        let (head, binding, route, windows) = crate::agent_turns::prepare_generation_context(
            engine, ctx, session, head, binding, false,
        )?;
        return Ok(Generation {
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
            Ok((
                source.inputs(snapshot, 0)?,
                source.role_instructions(snapshot)?,
            ))
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
        source.context_epoch(),
        None,
        tool_count > 0,
        images,
    )?;
    let (binding, headroom, _) =
        crate::agent_turns::initial_route_binding(engine, session, &route)?;
    let mut instructions = instructions(role.text);
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
    Ok(Generation {
        route,
        binding,
        head,
        windows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::services::usage::budget::BudgetBlock;

    #[test]
    fn a_budget_block_is_a_hold_and_a_stop_or_changed_source_ends_the_turn() {
        let held = admission_hold(&Admission::Blocked(BudgetBlock::Reached)).unwrap();
        assert!(held.contains("feature budget"), "{held}");
        assert!(admission_hold(&Admission::Fenced(Fenced::StopRequested)).is_none());
        assert!(admission_hold(&Admission::SourceChanged("changed".into())).is_none());
    }
}
