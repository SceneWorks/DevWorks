//! Watchers (DW-94, WATCH-01a/02/03/04/10): `watch.create` launches a process through the
//! `process.execute` service on its own consumer and answers before the process exits. When the
//! effect settles, [`WatchService::tick`] records the observed outcome as one durable watcher
//! event in the same transaction as the watcher's state change, then delivers the event into the
//! owning session's input as a host-only transcript notice. The next turn that starts carries
//! it, and only that turn's completion acknowledges it (`runs` substrate `begin_turn`/`end_turn`).
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, watch as ops};
use crate::ports::process::ProcessSpec as NativeProcessSpec;
use crate::ports::storage::{
    ApplicationStore, ArtifactScope, MAX_WATCH_TAIL_BYTES, NewEvent, OperationRecord, ProcessPhase,
    ReadOptions, ReadSnapshot, StorageError, StoredProcess, StoredWatchEvent, StoredWatchOutcome,
    StoredWatcher, WatchEventState, WatcherFilter, WatcherPhase, WriteOptions, WriteTransaction,
    read_snapshot, run_write,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::provider::TranscriptBody;
use crate::protocol::records::{
    WatchDeliveryState, WatchEvent, WatchOutcome, WatchOutcomeKind, WatchProcessSource,
    WatchSourceKind, WatcherPage, WatcherRecord, WatcherState,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, EventId, Id, OperationId, OperationState,
    RevisionDimension,
};
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::artifacts::{ArtifactService, version_ref};
use crate::services::process::STOPPED_ON_REQUEST;
use crate::services::process::tools::{
    BindingRefusal, ExecuteOrigin, ProcessToolService, binding_refusal,
};
use crate::services::sessions::transcript::{BatchOutcome, SessionBatch, SessionWriter};
use crate::services::sessions::{Refusal, counter, decided, query_fault, rev, turn};
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;

/// Store bound on an event body (`watcher_events.payload_json`), with room for its envelope.
const MAX_EVENT_PAYLOAD_BYTES: usize = 200_000;
/// Host notices are read into provider context up to this many bytes (`tool_turn`).
const MAX_NOTICE_BYTES: usize = 8_000;
/// Default output tail a watcher keeps when `tail_bytes` is omitted.
pub const DEFAULT_TAIL_BYTES: u32 = 4_096;
const PAGE: u16 = 100;

/// What one [`WatchService::tick`] pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchTick {
    pub settled: usize,
    pub delivered: usize,
}

pub struct WatchService {
    store: Arc<dyn ApplicationStore>,
    processes: Arc<ProcessToolService>,
    artifacts: Arc<ArtifactService>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
}

impl WatchService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        processes: Arc<ProcessToolService>,
        artifacts: Arc<ArtifactService>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            processes,
            artifacts,
            ids,
            clock,
        }
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Settles every open watcher whose process outcome is now observed, then delivers every
    /// pending event to its session. `host` must be a host context.
    pub fn tick(&self, host: &InvocationContext) -> Result<WatchTick, StorageError> {
        Ok(WatchTick {
            settled: self.settle_observed()?,
            delivered: self.deliver_pending(host)?,
        })
    }

    /// Settles every open watcher whose outcome is observed; returns how many it settled.
    pub fn settle_observed(&self) -> Result<usize, StorageError> {
        let mut settled = 0;
        let mut after: Option<Id> = None;
        loop {
            let page = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                read.watchers().open_watchers(after.as_ref(), PAGE)
            })?;
            let Some(last) = page.last() else {
                return Ok(settled);
            };
            after = Some(last.id.clone());
            for watcher in &page {
                // Most open watchers have nothing new: decide that from a snapshot and write only
                // when an outcome is observed (the write re-checks it).
                let observable = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                    let Some(operation_id) = &watcher.process_operation_id else {
                        return Ok(false);
                    };
                    let Some(operation) = read.operations().operation(operation_id)? else {
                        return Ok(false);
                    };
                    let process = read.resources().process(&operation_id.0)?;
                    let monitoring = self.processes.is_monitoring(operation_id);
                    Ok(observe(watcher, &operation, process.as_ref(), monitoring).is_some())
                })?;
                if !observable {
                    continue;
                }
                let id = watcher.id.clone();
                // Checked before the write: a monitor records its outcome before it lets go.
                let monitoring = watcher
                    .process_operation_id
                    .as_ref()
                    .is_some_and(|op| self.processes.is_monitoring(op));
                let done = run_write(&*self.store, WriteOptions::default(), move |tx| {
                    settle_in(tx, &id, monitoring)
                })?;
                settled += usize::from(done);
            }
            if page.len() < usize::from(PAGE) {
                return Ok(settled);
            }
        }
    }

    /// Delivers every pending event to its session; returns how many it delivered.
    pub fn deliver_pending(&self, host: &InvocationContext) -> Result<usize, StorageError> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(StorageError::ScopeDenied);
        }
        let mut delivered = 0;
        let mut after: Option<Id> = None;
        loop {
            let page = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                read.watchers().pending_watch_events(after.as_ref(), PAGE)
            })?;
            let Some(last) = page.last() else {
                return Ok(delivered);
            };
            after = Some(last.id.clone());
            for event in &page {
                delivered += usize::from(self.deliver(host, event)?);
            }
            if page.len() < usize::from(PAGE) {
                return Ok(delivered);
            }
        }
    }

    /// Appends one pending event to its session as a host notice whose source is the event ID,
    /// and marks it delivered in the same transaction. A replay finds it no longer pending.
    fn deliver(
        &self,
        host: &InvocationContext,
        event: &StoredWatchEvent,
    ) -> Result<bool, StorageError> {
        // Delivery to an ended session is decided with session lifecycle (DW-99); it is not
        // staged again on every pass meanwhile.
        let open = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            Ok(read
                .sessions()
                .session(&event.session_id)?
                .is_some_and(|session| !session.closed()))
        })?;
        if !open {
            return Ok(false);
        }
        let text = notice_text(event);
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                text.as_bytes(),
                json!({"kind":"WatcherEvent","eventId":event.id.as_str(),"sessionId":event.session_id.as_str()}),
            )?
            .map_err(|_| StorageError::InvalidRecord("watcher notice staging failed".into()))?;
        let pending = staged.clone();
        let service = self.artifacts.clone();
        let event_id = event.id.clone();
        let host = host.clone().with_session(event.session_id.clone());
        let committed = run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(event) = tx.watchers().watch_event(&event_id)? else {
                return Ok(false);
            };
            if event.state != WatchEventState::Pending {
                return Ok(false);
            }
            let Some(session) = tx.sessions().session(&event.session_id)? else {
                return Ok(false);
            };
            if session.closed() {
                // Delivery to an ended session is decided with session lifecycle (DW-99).
                return Ok(false);
            }
            // The notice belongs to the session's agent, whose turns read it; the watcher keeps
            // the launching actor for attribution.
            let scope = session
                .project_id
                .clone()
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
            let version = service.commit_host_bytes_in_scope(
                tx,
                &pending,
                HostArtifactPublication {
                    title: "Watcher event",
                    media_type: "text/plain; charset=utf-8",
                    provenance: json!({"kind":"WatcherEvent","eventId":event.id.as_str(),"watcherId":event.watcher_id.as_str()}),
                    scope,
                    owner: session.agent_id.clone(),
                },
            )?;
            let sequence = session.next_sequence;
            let batch = turn::batch_of(
                &event.session_id,
                tx.new_id(IdKind::TranscriptBatch),
                session.context_epoch,
                sequence,
                vec![TranscriptBody::HostNotice {
                    source: event.id.clone(),
                    artifact: version_ref(&version),
                }],
                || tx.new_id(IdKind::TranscriptEntry),
            );
            match SessionWriter::append_in(tx, &host, &SessionBatch::entries(batch))? {
                BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_) => {}
                BatchOutcome::Refused(refusal) => {
                    return Err(StorageError::InvalidRecord(format!(
                        "watcher notice refused: {refusal:?}"
                    )));
                }
            }
            let now = tx.now_ms();
            tx.watchers().deliver_watch_event(&event.id, sequence, now)
        });
        self.artifacts.settle_host_bytes(
            &*self.store,
            &staged,
            committed.as_ref().is_ok_and(|d| *d),
        );
        committed
    }

    fn record(
        &self,
        watcher: &StoredWatcher,
        event: Option<&StoredWatchEvent>,
        now_ms: i64,
    ) -> WatcherRecord {
        let live = (watcher.phase.is_open())
            .then(|| {
                watcher
                    .process_operation_id
                    .as_ref()
                    .and_then(|op| self.processes.live_tail(&op.0))
            })
            .flatten();
        record_of(watcher, event, now_ms, live)
    }

    fn visible(ctx: &InvocationContext, watcher: &StoredWatcher) -> bool {
        watcher.project_id.as_ref() == ctx.project_id()
            && (ctx.session_id() == Some(&watcher.session_id)
                || matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ))
    }

    fn decide_create(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &ops::CreateInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let Some(session_id) = ctx.session_id().cloned() else {
            return decided(
                tx,
                Err(Refusal::new(DomainErrorCode::InvalidArgument, "watcher_session", None).into()),
            );
        };
        match command.prepared::<Result<NativeProcessSpec, BindingRefusal>>() {
            Some(Ok(_)) => {}
            Some(Err(reason)) => return decided(tx, Err(binding_refusal(*reason).into())),
            None => {
                return decided(
                    tx,
                    Err(Refusal::new(DomainErrorCode::Conflict, "watch_preparation", None).into()),
                );
            }
        }
        let requested = input.tail_bytes.as_ref().map(|t| t.get());
        let tail_bytes = requested
            .unwrap_or(DEFAULT_TAIL_BYTES)
            .min(MAX_WATCH_TAIL_BYTES as u32);
        let operation = self.processes.record_execute(
            ctx,
            command.fingerprint(),
            &input.process,
            ExecuteOrigin::Watch {
                tail_bytes,
                requested,
            },
            tx,
        )?;
        let now = tx.now_ms();
        let watcher = StoredWatcher {
            id: tx.new_id(IdKind::Watcher),
            revision: 1,
            phase: WatcherPhase::Active,
            source_kind: "Process".into(),
            project_id: ctx.project_id().cloned(),
            session_id,
            actor: ctx.principal().clone(),
            process_operation_id: Some(operation.clone()),
            executable_ref: input.process.executable_ref.as_str().to_owned(),
            tail_bytes: i64::from(tail_bytes),
            outcome: None,
            duration_ms: None,
            output_tail: None,
            output_truncated: false,
            stop_requested: false,
            created_at_ms: now,
            updated_at_ms: now,
            settled_at_ms: None,
        };
        tx.watchers().insert_watcher(&watcher)?;
        history(
            tx,
            &watcher,
            Some(&operation),
            "WatcherCreated",
            json!({"processId": operation.0.as_str()}),
        )?;
        decided(
            tx,
            Ok(DomainDecision::Applied {
                result: OperationResult::WatchCreate {
                    value: record_of(&watcher, None, now, None),
                },
            }),
        )
    }

    fn decide_cancel(
        &self,
        ctx: &InvocationContext,
        input: &ops::CancelInput,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let not_found = || {
            Refusal::new(
                DomainErrorCode::NotFound,
                "watcher",
                Some(input.watcher_id.clone()),
            )
        };
        let Some(mut watcher) = tx.watchers().watcher(&input.watcher_id)? else {
            return decided(tx, Err(not_found().into()));
        };
        if !Self::visible(ctx, &watcher) {
            return decided(tx, Err(not_found().into()));
        }
        if input.expected_revision.get() != watcher.revision {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "watcher",
                watcher.id.clone(),
                input.expected_revision.clone(),
                rev(watcher.revision),
            )
            .into());
        }
        if watcher.phase != WatcherPhase::Active {
            return decided(
                tx,
                Err(Refusal::new(
                    DomainErrorCode::Conflict,
                    "watcher_state",
                    Some(watcher.id.clone()),
                )
                .into()),
            );
        }
        let now = tx.now_ms();
        let kind = if input.stop_process {
            let Some(process) = watcher.process_operation_id.clone() else {
                return decided(
                    tx,
                    Err(Refusal::new(
                        DomainErrorCode::Conflict,
                        "watcher_process",
                        Some(watcher.id.clone()),
                    )
                    .into()),
                );
            };
            // A process that already ended keeps its observed outcome: there is nothing to stop,
            // and the next pass settles the watcher with what actually happened.
            let monitoring = self.processes.is_monitoring(&process);
            let record = tx.resources().process(&process.0)?;
            let ended = tx
                .operations()
                .operation(&process)?
                .is_some_and(|op| observe(&watcher, &op, record.as_ref(), monitoring).is_some());
            if ended {
                return decided(
                    tx,
                    Ok(DomainDecision::NoChange {
                        result: OperationResult::WatchCancel {
                            value: record_of(&watcher, None, now, None),
                        },
                    }),
                );
            }
            let stop = self.processes.request_cancel(ctx, &process.0, tx)?;
            if let DomainDecision::Refused { fault } = stop.decision {
                return Ok(Decided {
                    decision: DomainDecision::Refused { fault },
                    operation_id: None,
                });
            }
            watcher.phase = WatcherPhase::Stopping;
            watcher.stop_requested = true;
            "WatcherStopRequested"
        } else {
            watcher.phase = WatcherPhase::Cancelled;
            watcher.settled_at_ms = Some(now);
            "WatcherCancelled"
        };
        watcher.revision += 1;
        watcher.updated_at_ms = now;
        tx.watchers().update_watcher(&watcher)?;
        let operation = watcher.process_operation_id.clone();
        history(tx, &watcher, operation.as_ref(), kind, json!({}))?;
        decided(
            tx,
            Ok(DomainDecision::Applied {
                result: OperationResult::WatchCancel {
                    value: record_of(&watcher, None, now, None),
                },
            }),
        )
    }
}

impl CommandHandler for WatchService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        _env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        Ok(match command.input() {
            OperationInput::Watch(ops::Input::Create(input)) => {
                Some(Prepared::new(self.processes.bind_spec(&input.process, ctx)))
            }
            _ => None,
        })
    }

    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        match command.input() {
            OperationInput::Watch(ops::Input::Create(input)) => {
                self.decide_create(ctx, command, input, tx)
            }
            OperationInput::Watch(ops::Input::Cancel(input)) => self.decide_cancel(ctx, input, tx),
            _ => decided(
                tx,
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into()),
            ),
        }
    }
}

impl QueryHandler for WatchService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let fault = |error: StorageError| query_fault(error.into(), "watcher", self.correlation());
        let now = self.clock.now_ms().get();
        let result = match query.input() {
            OperationInput::Watch(ops::Input::Inspect(input)) => {
                let watcher = snapshot
                    .watchers()
                    .watcher(&input.watcher_id)
                    .map_err(fault)?
                    .filter(|w| Self::visible(ctx, w))
                    .ok_or_else(|| {
                        Refusal::new(
                            DomainErrorCode::NotFound,
                            "watcher",
                            Some(input.watcher_id.clone()),
                        )
                        .fault(self.correlation())
                    })?;
                let event = snapshot
                    .watchers()
                    .watch_event_for_watcher(&watcher.id)
                    .map_err(fault)?;
                OperationResult::WatchInspect {
                    value: self.record(&watcher, event.as_ref(), now),
                }
            }
            OperationInput::Watch(ops::Input::List(input)) => {
                let filter = match (&input.session_id, ctx.principal_kind()) {
                    (Some(session), PrincipalKind::Human | PrincipalKind::Host) => {
                        WatcherFilter::Session(session.clone())
                    }
                    (Some(session), _) if ctx.session_id() == Some(session) => {
                        WatcherFilter::Session(session.clone())
                    }
                    (None, PrincipalKind::Human | PrincipalKind::Host) => {
                        WatcherFilter::Project(ctx.project_id().cloned())
                    }
                    (None, _) if ctx.session_id().is_some() => {
                        WatcherFilter::Session(ctx.session_id().cloned().expect("checked"))
                    }
                    _ => {
                        return Err(Refusal::new(
                            DomainErrorCode::PermissionDenied,
                            "watcher_session",
                            input.session_id.clone(),
                        )
                        .fault(self.correlation()));
                    }
                };
                let phases: Vec<WatcherPhase> = input
                    .states
                    .as_ref()
                    .map(|states| states.0.iter().map(phase_of).collect())
                    .unwrap_or_default();
                let before = match &input.page.cursor {
                    Some(cursor) => Some(parse_cursor(&cursor.0).ok_or_else(|| {
                        Refusal::new(DomainErrorCode::InvalidArgument, "cursor", None)
                            .fault(self.correlation())
                    })?),
                    None => None,
                };
                let limit = u16::try_from(input.page.limit.get())
                    .unwrap_or(u16::MAX)
                    .min(crate::ports::storage::MAX_PAGE);
                let rows = snapshot
                    .watchers()
                    .watchers(
                        &filter,
                        &phases,
                        before.as_ref().map(|(at, id)| (*at, id)),
                        limit,
                    )
                    .map_err(fault)?;
                let visible: Vec<StoredWatcher> =
                    rows.into_iter().filter(|w| Self::visible(ctx, w)).collect();
                let next_cursor = (visible.len() == usize::from(limit))
                    .then(|| visible.last())
                    .flatten()
                    .map(|w| BoundedText(format!("{}:{}", w.created_at_ms, w.id.as_str())));
                let mut items = Vec::with_capacity(visible.len());
                for watcher in &visible {
                    let event = snapshot
                        .watchers()
                        .watch_event_for_watcher(&watcher.id)
                        .map_err(fault)?;
                    items.push(self.record(watcher, event.as_ref(), now));
                }
                OperationResult::WatchList {
                    value: WatcherPage {
                        items: BoundedList(items),
                        next_cursor,
                    },
                }
            }
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "operation",
                    None,
                )
                .fault(self.correlation()));
            }
        };
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

/// The events an in-flight turn carries: delivered to its session before the turn began and
/// acknowledged only when it completes.
pub fn turn_input(
    read: &dyn ReadSnapshot,
    session: &Id,
    turn: &Id,
) -> Result<Vec<StoredWatchEvent>, StorageError> {
    Ok(read
        .watchers()
        .watch_events(
            session,
            &[WatchEventState::Delivered],
            crate::ports::storage::MAX_PAGE,
        )?
        .into_iter()
        .filter(|event| event.carried_turn_id.as_ref() == Some(turn))
        .collect())
}

/// Settles one open watcher whose process outcome is observed: the watcher's state change, its
/// event and its history commit together. `false` when nothing is observed yet.
fn settle_in(
    tx: &mut dyn WriteTransaction,
    id: &Id,
    monitoring: bool,
) -> Result<bool, StorageError> {
    let Some(mut watcher) = tx.watchers().watcher(id)? else {
        return Ok(false);
    };
    if !watcher.phase.is_open() {
        return Ok(false);
    }
    let Some(operation_id) = watcher.process_operation_id.clone() else {
        return Ok(false);
    };
    let Some(operation) = tx.operations().operation(&operation_id)? else {
        return Ok(false);
    };
    let process = tx.resources().process(&operation_id.0)?;
    let Some(observed) = observe(&watcher, &operation, process.as_ref(), monitoring) else {
        return Ok(false);
    };
    let now = tx.now_ms();
    watcher.phase = WatcherPhase::Settled;
    watcher.revision += 1;
    watcher.outcome = Some(observed.outcome);
    watcher.duration_ms = observed.duration_ms;
    watcher.output_tail = observed.tail;
    watcher.output_truncated = observed.truncated;
    watcher.updated_at_ms = now;
    watcher.settled_at_ms = Some(now);
    tx.watchers().update_watcher(&watcher)?;
    let event = StoredWatchEvent {
        id: tx.new_id(IdKind::Event),
        watcher_id: watcher.id.clone(),
        session_id: watcher.session_id.clone(),
        project_id: watcher.project_id.clone(),
        state: WatchEventState::Pending,
        payload: bounded_payload(&watcher, now)?,
        transcript_sequence: None,
        carried_turn_id: None,
        created_at_ms: now,
        delivered_at_ms: None,
        acknowledged_at_ms: None,
    };
    tx.watchers().insert_watch_event(&event)?;
    let outcome = watcher.outcome.as_ref().map(|o| o.kind.clone());
    history(
        tx,
        &watcher,
        Some(&operation_id),
        "WatcherSettled",
        json!({"eventId": event.id.as_str(), "outcome": outcome}),
    )?;
    Ok(true)
}

/// The event body, its encoded size within the store's bound: JSON escaping can grow a tail
/// several times, so the tail keeps its end and loses its front until the record fits.
fn bounded_payload(watcher: &StoredWatcher, now: i64) -> Result<Value, StorageError> {
    let mut record = record_of(watcher, None, now, None);
    loop {
        let value = serde_json::to_value(&record)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        if value.to_string().len() <= MAX_EVENT_PAYLOAD_BYTES {
            return Ok(value);
        }
        let Some(tail) = record.output_tail.as_mut() else {
            return Err(StorageError::InvalidRecord(
                "watcher event exceeds its bound".into(),
            ));
        };
        let chars = tail.0.chars().count();
        tail.0 = tail.0.chars().skip(chars.div_ceil(2).max(1)).collect();
        record.output_truncated = true;
    }
}

struct Observed {
    outcome: StoredWatchOutcome,
    duration_ms: Option<i64>,
    tail: Option<Vec<u8>>,
    truncated: bool,
}

fn outcome(
    kind: &str,
    exit_code: Option<i64>,
    signal: Option<i64>,
    detail: Option<&str>,
) -> StoredWatchOutcome {
    StoredWatchOutcome {
        kind: kind.into(),
        exit_code,
        signal,
        detail: detail.map(|d| d.chars().take(1024).collect()),
    }
}

/// Maps what the process supervisor observed to a watcher outcome. `None` while nothing is
/// observed: a launch or poll alone is never success (WATCH-04).
fn observe(
    watcher: &StoredWatcher,
    operation: &OperationRecord,
    process: Option<&StoredProcess>,
    monitoring: bool,
) -> Option<Observed> {
    let duration_ms = process
        .filter(|p| matches!(p.phase, ProcessPhase::Reaped))
        .map(|p| (p.updated_at_ms - p.created_at_ms).max(0));
    let effect = operation.latest_attempt().and_then(|attempt| {
        attempt.observations.iter().rev().find_map(|observation| {
            observation
                .get("effect")
                .or_else(|| observation.get("reconciliation"))
                .cloned()
        })
    });
    let stopped_on_request = process.is_some_and(|p| {
        p.phase == ProcessPhase::Reaped && p.detail.as_deref() == Some(STOPPED_ON_REQUEST)
    });
    let plain = |outcome| Observed {
        outcome,
        duration_ms,
        tail: None,
        truncated: false,
    };
    // A watched process's effect ends at launch; its monitor records what it observed.
    let monitored = operation.attempts.iter().rev().find_map(|attempt| {
        attempt
            .observations
            .iter()
            .rev()
            .find_map(|o| o.get("watchOutcome").cloned())
    });
    match operation.state {
        OperationState::ObservedComplete => {
            let launched = effect
                .as_ref()
                .and_then(|e| e.pointer("/evidence/launched"))
                == Some(&Value::Bool(true));
            let evidence = match (monitored, launched) {
                (Some(evidence), _) => evidence,
                (None, true) if monitoring => return None,
                // No monitor recorded it: the process record says what became of it.
                (None, true) => {
                    if watcher.stop_requested && stopped_on_request {
                        return Some(plain(outcome("Cancelled", None, None, None)));
                    }
                    let (kind, detail) = match process.map(|p| (p.phase, p.detail.as_deref())) {
                        Some((ProcessPhase::Reaped, Some("killed by boot recovery"))) => {
                            ("Interrupted", "DevWorks restarted and stopped the process")
                        }
                        Some((ProcessPhase::Uncertain, _)) => {
                            ("Uncertain", "the process could not be verified stopped")
                        }
                        Some((ProcessPhase::Failed, _)) => {
                            ("Failed", "the process failed before a verified result")
                        }
                        Some((ProcessPhase::Reaped | ProcessPhase::Lost, _)) => (
                            "Lost",
                            "the process ended without its outcome being observed",
                        ),
                        // Still running under an owner that no monitor observes (an earlier
                        // boot's tree awaiting recovery): not observed yet.
                        _ => return None,
                    };
                    return Some(plain(outcome(kind, None, None, Some(detail))));
                }
                (None, false) => effect
                    .as_ref()
                    .and_then(|e| e.get("evidence"))
                    .cloned()
                    .unwrap_or(Value::Null),
            };
            if let Some(reason) = evidence
                .get("unknown")
                .or_else(|| evidence.get("failed"))
                .and_then(Value::as_str)
            {
                let kind = if evidence.get("failed").is_some() {
                    "Failed"
                } else {
                    "Uncertain"
                };
                return Some(plain(outcome(kind, None, None, Some(reason))));
            }
            let exit_code = evidence
                .pointer("/result/value/exitCode")
                .and_then(Value::as_i64);
            let signal = evidence.get("signal").and_then(Value::as_i64);
            let tail = evidence
                .get("tailBase64")
                .and_then(Value::as_str)
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .map(|bytes| keep_end(bytes, MAX_WATCH_TAIL_BYTES));
            let truncated = evidence
                .get("tailTruncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || tail.as_ref().is_some_and(|(_, cut)| *cut);
            let tail = tail.map(|(bytes, _)| bytes);
            let lost = evidence.get("outputLost").and_then(Value::as_bool) == Some(true);
            // Measured on a monotonic clock around the run; the store-clock span otherwise.
            let duration_ms = evidence
                .get("durationMs")
                .and_then(Value::as_i64)
                .or(duration_ms);
            // A requested stop is the cause only when the supervisor recorded that the stop
            // reaped a still-running tree; a process that exited on its own first keeps its exit.
            let kind = if watcher.stop_requested && stopped_on_request {
                outcome("Cancelled", None, signal, None)
            } else if evidence.get("timedOut").and_then(Value::as_bool) == Some(true) {
                outcome(
                    "TimedOut",
                    exit_code,
                    signal,
                    Some("the process exceeded its timeout and was stopped"),
                )
            } else if lost
                && process.and_then(|p| p.detail.as_deref()) == Some("killed by boot recovery")
            {
                outcome(
                    "Interrupted",
                    None,
                    None,
                    Some("DevWorks restarted and stopped the process"),
                )
            } else if lost && exit_code.is_none() {
                outcome(
                    "Lost",
                    None,
                    None,
                    Some(
                        "the process ended while DevWorks was not running; its exit was not observed",
                    ),
                )
            } else {
                match exit_code {
                    Some(0) => outcome("Succeeded", Some(0), None, None),
                    Some(code) => outcome("Failed", Some(code), signal, None),
                    None => outcome(
                        "Failed",
                        None,
                        signal,
                        Some(if signal.is_some() {
                            "terminated by a signal"
                        } else {
                            "the process ended without an exit code"
                        }),
                    ),
                }
            };
            Some(Observed {
                outcome: kind,
                duration_ms,
                tail,
                truncated,
            })
        }
        OperationState::Failed => {
            let cause = effect
                .as_ref()
                .and_then(|e| e.get("cause"))
                .and_then(Value::as_str)
                .unwrap_or("the process could not be launched");
            Some(plain(outcome("Failed", None, None, Some(cause))))
        }
        OperationState::CancelledBeforeDispatch => Some(plain(outcome(
            "Cancelled",
            None,
            None,
            Some("cancelled before the process launched"),
        ))),
        OperationState::OutcomeUnknown => match process.map(|p| p.phase) {
            Some(ProcessPhase::Lost) => Some(plain(outcome(
                "Lost",
                None,
                None,
                Some("the process was lost before its exit was observed"),
            ))),
            Some(ProcessPhase::Uncertain) => Some(plain(outcome(
                "Uncertain",
                None,
                None,
                process.and_then(|p| p.detail.as_deref()),
            ))),
            _ => None,
        },
        _ => None,
    }
}

fn phase_of(state: &WatcherState) -> WatcherPhase {
    match state {
        WatcherState::Active => WatcherPhase::Active,
        WatcherState::Stopping => WatcherPhase::Stopping,
        WatcherState::Settled => WatcherPhase::Settled,
        WatcherState::Cancelled => WatcherPhase::Cancelled,
    }
}

fn state_of(phase: WatcherPhase) -> WatcherState {
    match phase {
        WatcherPhase::Active => WatcherState::Active,
        WatcherPhase::Stopping => WatcherState::Stopping,
        WatcherPhase::Settled => WatcherState::Settled,
        WatcherPhase::Cancelled => WatcherState::Cancelled,
    }
}

fn outcome_kind(kind: &str) -> WatchOutcomeKind {
    match kind {
        "Succeeded" => WatchOutcomeKind::Succeeded,
        "Failed" => WatchOutcomeKind::Failed,
        "Cancelled" => WatchOutcomeKind::Cancelled,
        "TimedOut" => WatchOutcomeKind::TimedOut,
        "Interrupted" => WatchOutcomeKind::Interrupted,
        "Lost" => WatchOutcomeKind::Lost,
        _ => WatchOutcomeKind::Uncertain,
    }
}

fn parse_cursor(cursor: &str) -> Option<(i64, Id)> {
    let (at, id) = cursor.split_once(':')?;
    Some((at.parse().ok()?, Id::new(id.to_owned()).ok()?))
}

fn text(bytes: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(bytes);
    lossy.chars().take(65_536).collect()
}

fn record_of(
    watcher: &StoredWatcher,
    event: Option<&StoredWatchEvent>,
    now_ms: i64,
    live_tail: Option<(Vec<u8>, bool)>,
) -> WatcherRecord {
    let (tail, truncated) = match live_tail {
        Some((bytes, truncated)) => (Some(bytes), truncated),
        None => (watcher.output_tail.clone(), watcher.output_truncated),
    };
    let end = watcher.settled_at_ms.unwrap_or(now_ms);
    WatcherRecord {
        id: watcher.id.clone(),
        revision: rev(watcher.revision),
        state: state_of(watcher.phase),
        source_kind: WatchSourceKind::Process,
        process: watcher.process_operation_id.as_ref().and_then(|op| {
            Some(WatchProcessSource {
                process_id: op.0.clone(),
                executable_ref: Id::new(watcher.executable_ref.clone()).ok()?,
            })
        }),
        session_id: watcher.session_id.clone(),
        project_id: watcher.project_id.clone(),
        created_at_ms: counter(watcher.created_at_ms),
        settled_at_ms: watcher.settled_at_ms.map(counter),
        elapsed_ms: counter(end - watcher.created_at_ms),
        outcome: watcher.outcome.as_ref().map(|o| WatchOutcome {
            kind: outcome_kind(&o.kind),
            exit_code: o.exit_code.and_then(|c| i32::try_from(c).ok()),
            signal: o.signal.and_then(|s| i32::try_from(s).ok()),
            detail: o
                .detail
                .as_ref()
                .filter(|d| !d.is_empty())
                .map(|d| BoundedText(d.chars().take(4096).collect())),
        }),
        duration_ms: watcher.duration_ms.map(counter),
        output_tail: tail.map(|bytes| BoundedText(text(&bytes))),
        output_truncated: truncated,
        stop_requested: watcher.stop_requested,
        event: event.map(|e| WatchEvent {
            id: e.id.clone(),
            state: match e.state {
                WatchEventState::Pending => WatchDeliveryState::Pending,
                WatchEventState::Delivered => WatchDeliveryState::Delivered,
                WatchEventState::Acknowledged => WatchDeliveryState::Acknowledged,
            },
            created_at_ms: counter(e.created_at_ms),
            delivered_at_ms: e.delivered_at_ms.map(counter),
            acknowledged_at_ms: e.acknowledged_at_ms.map(counter),
        }),
    }
}

/// The host-only message an agent reads: watcher, source, outcome, duration and output tail.
fn notice_text(event: &StoredWatchEvent) -> String {
    let p = &event.payload;
    let s = |pointer: &str| p.pointer(pointer).and_then(Value::as_str).unwrap_or("");
    // Counters are serialized as decimal strings.
    let n = |pointer: &str| {
        p.pointer(pointer).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
    };
    let mut outcome = s("/outcome/kind").to_owned();
    if let Some(code) = n("/outcome/exitCode") {
        outcome.push_str(&format!(" (exit code {code})"));
    }
    if let Some(signal) = n("/outcome/signal") {
        outcome.push_str(&format!(" (signal {signal})"));
    }
    let mut text = format!(
        "Watcher event {event}: watcher {watcher} observed its process end: {outcome}.\n\
         Source: process {process} ({executable}).",
        event = event.id.as_str(),
        watcher = event.watcher_id.as_str(),
        process = s("/process/processId"),
        executable = s("/process/executableRef"),
    );
    if let Some(duration) = n("/durationMs") {
        text.push_str(&format!(" Duration: {duration} ms."));
    }
    if !s("/outcome/detail").is_empty() {
        text.push_str(&format!("\nDetail: {}", s("/outcome/detail")));
    }
    if let Some(tail) = p.get("outputTail").and_then(Value::as_str) {
        let mut omitted = p.get("outputTruncated").and_then(Value::as_bool) == Some(true);
        // The whole notice must fit what a provider context reads; keep the tail's end.
        let heading = "\nOutput tail (earlier output omitted):\n";
        let budget = MAX_NOTICE_BYTES.saturating_sub(text.len() + heading.len());
        let mut start = tail.len().saturating_sub(budget);
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        omitted |= start > 0;
        text.push_str(if omitted { heading } else { "\nOutput tail:\n" });
        text.push_str(&tail[start..]);
    }
    text.truncate(floor_boundary(&text, MAX_NOTICE_BYTES));
    text
}

/// The last `max` bytes, starting at a UTF-8 boundary when the cut lands inside a character;
/// `true` when bytes were dropped (redaction can make a tail longer than its budget).
fn keep_end(bytes: Vec<u8>, max: usize) -> (Vec<u8>, bool) {
    if bytes.len() <= max {
        return (bytes, false);
    }
    let mut start = bytes.len() - max;
    let limit = (start + 3).min(bytes.len());
    while start < limit && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    (bytes[start..].to_vec(), true)
}

fn floor_boundary(text: &str, max: usize) -> usize {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn history(
    tx: &mut dyn WriteTransaction,
    watcher: &StoredWatcher,
    operation: Option<&OperationId>,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event,
        actor: watcher.actor.clone(),
        project_id: watcher.project_id.clone(),
        root_id: None,
        operation_id: operation.cloned(),
        aggregate_kind: "watcher".into(),
        aggregate_id: watcher.id.as_str().into(),
        aggregate_revision: watcher.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ActorId;

    fn watcher(tail: Vec<u8>) -> StoredWatcher {
        let id = |s: &str| Id::new(s).unwrap();
        StoredWatcher {
            id: id("w"),
            revision: 2,
            phase: WatcherPhase::Settled,
            source_kind: "Process".into(),
            project_id: None,
            session_id: id("s"),
            actor: ActorId(id("a")),
            process_operation_id: Some(OperationId(id("op"))),
            executable_ref: "sh".into(),
            tail_bytes: 65_536,
            outcome: Some(outcome("Failed", Some(1), None, None)),
            duration_ms: Some(5),
            output_tail: Some(tail),
            output_truncated: false,
            stop_requested: false,
            created_at_ms: 0,
            updated_at_ms: 1,
            settled_at_ms: Some(1),
        }
    }

    #[test]
    fn an_escaping_heavy_tail_still_fits_the_event_bound_and_keeps_its_end() {
        let mut tail = vec![0u8; 65_535];
        tail.push(b'Z');
        let payload = bounded_payload(&watcher(tail), 1).unwrap();
        assert!(payload.to_string().len() <= MAX_EVENT_PAYLOAD_BYTES);
        assert!(payload["outputTail"].as_str().unwrap().ends_with('Z'));
        assert_eq!(payload["outputTruncated"], json!(true));
    }

    #[test]
    fn a_notice_fits_what_a_provider_context_reads_and_keeps_the_tail_end() {
        let mut tail = "x".repeat(65_000);
        tail.push_str("THE END");
        let event = StoredWatchEvent {
            id: Id::new("e").unwrap(),
            watcher_id: Id::new("w").unwrap(),
            session_id: Id::new("s").unwrap(),
            project_id: None,
            state: WatchEventState::Pending,
            payload: bounded_payload(&watcher(tail.into_bytes()), 1).unwrap(),
            transcript_sequence: None,
            carried_turn_id: None,
            created_at_ms: 1,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
        };
        let text = notice_text(&event);
        assert!(text.len() <= MAX_NOTICE_BYTES);
        assert!(text.ends_with("THE END"));
        assert!(text.contains("(earlier output omitted)"));
        assert!(text.contains("Failed (exit code 1)") && text.contains("Duration: 5 ms"));
    }
}

#[cfg(test)]
mod bound_tests {
    use super::keep_end;

    #[test]
    fn a_bounded_tail_keeps_its_end_at_a_character_boundary() {
        assert_eq!(keep_end(b"short".to_vec(), 8), (b"short".to_vec(), false));
        assert_eq!(
            keep_end(b"0123456789".to_vec(), 4),
            (b"6789".to_vec(), true)
        );
        // "é" is two bytes; a cut through it starts after it.
        let (kept, cut) = keep_end("aéb".as_bytes().to_vec(), 2);
        assert_eq!((kept, cut), (b"b".to_vec(), true));
    }
}
