//! The native scheduled-inference pipeline (JOB-05..07): each inference job run is one agent
//! turn of the job's own principal, driven by the same background turn loop as a workspace reply
//! ([`crate::background_turn`]).
//!
//! [`ScheduledInference::start`] opens (or, on resumption, reattaches) the run's session: owned
//! by the job's agent in the job's project scope, its first transcript entry the host notice of
//! the saved prompt. The turn routes through the router, offers the agent's full selected tool
//! inventory and submits every tool call through the gateway under the agent's principal, so
//! effective tool permissions, approvals, readiness and pauses apply as for any agent. Usage is
//! attributed to the run and its session.
//!
//! When the turn ends the pipeline reports it through [`JobService::finish_inference`], once the
//! scheduler recorded the handoff (so the run names its session), and always at the run's current
//! revision. A run the scheduler already ended (cancelled, say) is left as it is.
use crate::background_turn::{JobTurn, TurnEnd, TurnHolds, TurnSource};
use crate::turn_engine::TurnEngine;
use devworks_core::context::{HostIngress, PrincipalKind};
use devworks_core::locks::LockExt;
use devworks_core::ports::IdKind;
use devworks_core::ports::storage::{
    ApplicationStore, ArtifactScope, Cancellation, ReadOptions, StorageError, StoredSession,
    WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::provider::TranscriptBody;
use devworks_core::protocol::{Counter, Id, OperationId, Revision};
use devworks_core::services::artifacts::{host::HostArtifactPublication, version_ref};
use devworks_core::services::jobs::JobService;
use devworks_core::services::jobs::runner::{
    InferenceJobs, InferenceOutcome, InferenceRefusal, InferenceStart, handed_off,
};
use devworks_core::services::sessions::MAX_HOST_NOTICE_BYTES;
use devworks_core::services::sessions::transcript::{BatchOutcome, SessionBatch, SessionWriter};
use devworks_core::services::sessions::turn;
use devworks_core::settings::catalog::AgentRole;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::task::JoinHandle;

/// The visible reason of a run handed over before the turn engine is ready.
const NOT_READY: &str = "Scheduled inference is starting: the agent turn engine is not ready yet. \
    Nothing was sent to a model.";
const STOPPING: &str = "Scheduled inference is stopping for shutdown. Nothing was sent to a model.";
const NOTHING_SENT: &str = "Nothing was sent to a model.";
const HOLD: &str = "A tool call of the scheduled agent is waiting on an approval or another \
    hold; the run continues once it is resolved.";
/// How long a finished turn waits for the scheduler to record its handoff before it leaves the
/// run to the scheduler's supervision of unsettled starts.
const REPORT_BOUND: Duration = Duration::from_secs(300);

/// The engine's [`InferenceJobs`] port.
pub(crate) struct ScheduledInference {
    engine: OnceLock<Weak<TurnEngine>>,
    jobs: Arc<JobService>,
    turns: Mutex<HashMap<String, LiveTurn>>,
    stopping: Arc<AtomicBool>,
}

struct LiveTurn {
    session: Id,
    cancel: Cancellation,
    /// Set by a requested cancellation (not by shutdown).
    requested: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl ScheduledInference {
    pub(crate) fn new(jobs: Arc<JobService>) -> Self {
        Self {
            engine: OnceLock::new(),
            jobs,
            turns: Mutex::new(HashMap::new()),
            stopping: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn bind(&self, engine: &Arc<TurnEngine>) {
        let _ = self.engine.set(Arc::downgrade(engine));
    }

    /// Pauses every live turn for shutdown: its run is recorded interrupted by the job service
    /// and resumes from its session at the next boot.
    pub(crate) fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        for turn in self.turns.locked().values() {
            turn.cancel.cancel();
        }
    }

    /// Waits for the live turns until `deadline`, then abandons the rest.
    pub(crate) async fn drain(&self, deadline: tokio::time::Instant) {
        let tasks: Vec<JoinHandle<()>> = self
            .turns
            .locked()
            .drain()
            .map(|(_, turn)| turn.task)
            .collect();
        for mut task in tasks {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if let crate::dispatch::Either::Left(()) =
                crate::dispatch::first(tokio::time::sleep(left), &mut task).await
            {
                task.abort();
            }
        }
    }

    fn engine(&self) -> Option<Arc<TurnEngine>> {
        self.engine.get().and_then(Weak::upgrade)
    }
}

impl InferenceJobs for ScheduledInference {
    fn availability(&self) -> Result<(), String> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(STOPPING.into());
        }
        self.engine().map(|_| ()).ok_or_else(|| NOT_READY.into())
    }

    fn start(&self, run: &InferenceStart) -> Result<Option<Id>, InferenceRefusal> {
        let refuse = |reason: String| InferenceRefusal {
            dispatched: false,
            reason,
        };
        let engine = self.engine().ok_or_else(|| refuse(NOT_READY.into()))?;
        if self.stopping.load(Ordering::SeqCst) {
            return Err(refuse(STOPPING.into()));
        }
        {
            let mut turns = self.turns.locked();
            turns.retain(|_, turn| !turn.task.is_finished());
            if let Some(turn) = turns.get(run.run_id.as_str()) {
                return Ok(Some(turn.session.clone()));
            }
        }
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            refuse(format!(
                "No runtime runs the scheduled agent. {NOTHING_SENT}"
            ))
        })?;
        let opened = open_session(&engine, run).map_err(|error| {
            refuse(format!(
                "The scheduled agent's session could not be opened ({error}). {NOTHING_SENT}"
            ))
        })?;
        let source = TurnSource::Job(JobTurn {
            run_id: run.run_id.clone(),
            session_id: opened.session.clone(),
            agent: run.agent.clone(),
            project: run.project_id.clone(),
            role: opened.role,
        });
        let report = Reporter {
            store: engine.store().clone(),
            jobs: self.jobs.clone(),
            run: run.run_id.clone(),
            operation: opened.operation.clone(),
            attempt: opened.attempt,
            held: AtomicBool::new(run.hold.is_some()),
        };
        let ctx = run.ctx.clone().with_session(opened.session.clone());
        let (cancel, requested, stopping) = (
            Cancellation::new(),
            Arc::new(AtomicBool::new(false)),
            self.stopping.clone(),
        );
        let (stop, asked) = (cancel.clone(), requested.clone());
        let task = handle.spawn(async move {
            let operation_id = report.operation.clone();
            let operation = match crate::store::read(engine.store(), move |read| {
                read.operations().operation(&operation_id)
            })
            .await
            {
                Ok(Some(operation)) => operation,
                Ok(None) | Err(_) => return,
            };
            let end = crate::background_turn::drive(
                &engine,
                &ctx,
                &source,
                &operation,
                report.attempt,
                &stop,
                None,
                Some(&report),
            )
            .await;
            let outcome = if asked.load(Ordering::SeqCst) {
                InferenceOutcome::Cancelled
            } else {
                match end {
                    TurnEnd::Completed(_) => InferenceOutcome::Succeeded,
                    // Shutdown paused the turn: the run resumes at the next boot.
                    _ if stopping.load(Ordering::SeqCst) => return,
                    TurnEnd::Unavailable { detail, sent: true } => InferenceOutcome::Failed(detail),
                    TurnEnd::Unavailable {
                        detail,
                        sent: false,
                    } => InferenceOutcome::Failed(format!("{detail} {NOTHING_SENT}")),
                    TurnEnd::Unknown(detail) => InferenceOutcome::Unknown(detail),
                    TurnEnd::Stopped => InferenceOutcome::Unknown(
                        "The scheduled agent stopped unexpectedly.".into(),
                    ),
                }
            };
            report.report(outcome).await;
        });
        self.turns.locked().insert(
            run.run_id.as_str().to_owned(),
            LiveTurn {
                session: opened.session.clone(),
                cancel,
                requested,
                task,
            },
        );
        Ok(Some(opened.session))
    }

    fn cancel(&self, run: &Id) -> Result<bool, String> {
        let turns = self.turns.locked();
        match turns.get(run.as_str()) {
            Some(turn) if !turn.task.is_finished() => {
                turn.requested.store(true, Ordering::SeqCst);
                turn.cancel.cancel();
                Ok(false)
            }
            _ => Ok(true),
        }
    }
}

struct OpenedSession {
    session: Id,
    operation: OperationId,
    attempt: i64,
    role: AgentRole,
}

fn derived_id(prefix: &str, run: &Id) -> Id {
    Id::new(
        format!("{prefix}-{}", run.as_str())
            .chars()
            .take(128)
            .collect::<String>(),
    )
    .expect("run IDs are ID-safe")
}

/// The run's session, created on its first start; a resumed run reattaches the session it
/// recorded. Its first entry is a host notice with the saved prompt's text: the host reads the
/// prompt (a person's saved artifact, which the agent's own scope need not see) and publishes it
/// as the agent's copy in the session's scope, naming the saved version in its provenance.
fn open_session(engine: &TurnEngine, run: &InferenceStart) -> Result<OpenedSession, StorageError> {
    let store: &dyn ApplicationStore = engine.store().as_ref();
    let info = store.info();
    let session = run
        .session_id
        .clone()
        .unwrap_or_else(|| derived_id("job-session", &run.run_id));
    let host = HostIngress::new(store)
        .context(
            info.host_actor.clone(),
            PrincipalKind::Host,
            run.run_id.clone(),
            Id::new("scheduled-inference").expect("static host identity"),
            Counter::new(info.restore_epoch).map_err(|_| StorageError::ScopeDenied)?,
            Revision::new(1).expect("positive revision"),
        )
        .with_session(session.clone())
        .with_lineage(run.project_id.clone(), None, None);
    let exists = read_snapshot(store, &ReadOptions::default(), |read| {
        Ok(read.sessions().session(&session)?.is_some())
    })?;
    let staged = if exists {
        None
    } else {
        let (bytes, _) = engine
            .artifacts()
            .read_exact(store, &host, &run.prompt, MAX_HOST_NOTICE_BYTES)
            .map_err(|refusal| {
                StorageError::InvalidRecord(format!("the saved prompt is unreadable: {refusal:?}"))
            })?;
        std::str::from_utf8(&bytes)
            .map_err(|_| StorageError::InvalidRecord("the saved prompt is not text".into()))?;
        let staged = engine
            .artifacts()
            .stage_host_bytes(store, &bytes, prompt_provenance(&session, run))?
            .map_err(StorageError::InvalidRecord)?;
        Some(staged)
    };
    let (run_id, agent, project, artifacts, pending, provenance) = (
        run.run_id.clone(),
        run.agent.clone(),
        run.project_id.clone(),
        engine.artifacts().clone(),
        staged.clone(),
        prompt_provenance(&session, run),
    );
    let committed = run_write(store, WriteOptions::host_state(), move |tx| {
        let stored = tx
            .jobs()
            .job_run(&run_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let operation = stored.operation_id.ok_or(StorageError::ScopeDenied)?;
        let attempt = tx
            .operations()
            .operation(&operation)?
            .and_then(|record| record.latest_attempt().map(|attempt| attempt.attempt_no))
            .ok_or(StorageError::ScopeDenied)?;
        let binding = tx
            .authority()
            .principal(&agent)?
            .filter(|binding| {
                binding.active
                    && binding.kind == PrincipalKind::Agent
                    && binding.project_id == project
            })
            .ok_or(StorageError::ScopeDenied)?;
        let role = binding.role.unwrap_or(AgentRole::Scheduled);
        let opened = OpenedSession {
            session: session.clone(),
            operation,
            attempt,
            role,
        };
        if let Some(existing) = tx.sessions().session(&session)? {
            if existing.closed()
                || existing.agent_id != agent
                || existing.root_id.is_some()
                || existing.project_id != project
            {
                return Err(StorageError::ScopeDenied);
            }
            return Ok((opened, false));
        }
        let staged = pending.as_ref().ok_or(StorageError::ScopeDenied)?;
        let now = tx.now_ms();
        tx.sessions().insert_session(&StoredSession {
            id: session.clone(),
            conversation_id: derived_id("job-conversation", &run_id),
            agent_id: agent.clone(),
            project_id: project.clone(),
            root_id: None,
            title: "Scheduled job run".into(),
            context_epoch: 0,
            revision: 1,
            created_by: host.principal().clone(),
            created_at_ms: now,
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        })?;
        let version = artifacts.commit_host_bytes_in_scope(
            tx,
            staged,
            HostArtifactPublication {
                title: "Scheduled job prompt",
                media_type: "text/plain; charset=utf-8",
                provenance: provenance.clone(),
                scope: project
                    .clone()
                    .map_or(ArtifactScope::Workspace, ArtifactScope::Project),
                // The agent's own copy: a project artifact is visible to its owner.
                owner: agent.clone(),
            },
        )?;
        let batch = turn::batch_of(
            &session,
            tx.new_id(IdKind::TranscriptBatch),
            0,
            1,
            vec![TranscriptBody::HostNotice {
                source: run_id.clone(),
                artifact: version_ref(&version),
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        match SessionWriter::append_in(tx, &host, &SessionBatch::entries(batch))? {
            BatchOutcome::Accepted(_) => Ok((opened, true)),
            BatchOutcome::Replayed(_) => Err(StorageError::ScopeDenied),
            BatchOutcome::Refused(reason) => Err(StorageError::InvalidRecord(format!(
                "the prompt notice was refused: {reason:?}"
            ))),
        }
    });
    if let Some(staged) = &staged {
        engine
            .artifacts()
            .settle_host_bytes(store, staged, matches!(&committed, Ok((_, true))));
    }
    committed.map(|(opened, _)| opened)
}

fn prompt_provenance(session: &Id, run: &InferenceStart) -> serde_json::Value {
    serde_json::json!({"kind":"ScheduledJobPrompt","sessionId":session,"jobRunId":run.run_id,
        "prompt":run.prompt})
}

/// Reports a run's turn to the job service under the attempt the turn runs.
struct Reporter {
    store: Arc<dyn ApplicationStore>,
    jobs: Arc<JobService>,
    run: Id,
    operation: OperationId,
    attempt: i64,
    /// Whether the run is recorded holding on one of its tool calls (or was resumed holding).
    held: AtomicBool,
}

enum Reported {
    Done,
    /// The scheduler has not recorded the handoff yet.
    NotHandedOff,
}

impl Reporter {
    async fn report(&self, outcome: InferenceOutcome) {
        let deadline = tokio::time::Instant::now() + REPORT_BOUND;
        let mut wait = Duration::from_millis(10);
        loop {
            let (jobs, run, operation, attempt, outcome) = (
                self.jobs.clone(),
                self.run.clone(),
                self.operation.clone(),
                self.attempt,
                outcome.clone(),
            );
            let reported =
                crate::store::write(&self.store, WriteOptions::settlement(), move |tx| {
                    let Some(current) = tx.jobs().job_run(&run)? else {
                        return Ok(Reported::Done);
                    };
                    if !matches!(current.status.as_str(), "Running" | "Holding")
                        || current.operation_id.as_ref() != Some(&operation)
                    {
                        return Ok(Reported::Done);
                    }
                    let Some(record) = tx.operations().operation(&operation)? else {
                        return Ok(Reported::Done);
                    };
                    if record
                        .latest_attempt()
                        .is_none_or(|latest| latest.attempt_no != attempt)
                    {
                        // A later attempt owns the run now.
                        return Ok(Reported::Done);
                    }
                    if !handed_off(&record) {
                        return Ok(Reported::NotHandedOff);
                    }
                    match jobs.finish_inference(tx, &run, current.revision, &outcome) {
                        Ok(_) | Err(StorageError::LeaseLost { .. }) => Ok(Reported::Done),
                        Err(error) => Err(error),
                    }
                })
                .await;
            if let Ok(Reported::Done) = reported {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(1));
        }
    }
}

impl TurnHolds for Reporter {
    fn held(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if !self.held.swap(true, Ordering::SeqCst) {
                self.report(InferenceOutcome::Holding(HOLD.into())).await;
            }
        })
    }
    fn resumed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if self.held.swap(false, Ordering::SeqCst) {
                self.report(InferenceOutcome::Resumed).await;
            }
        })
    }
}
