//! Watchers (DW-94, WATCH-01a/02/03/04/10): `watch.create` launches a process through the
//! `process.execute` service on its own consumer and answers before the process exits. When the
//! effect settles, [`WatchService::tick`] records the observed outcome as one durable watcher
//! event in the same transaction as the watcher's state change, then delivers the event into the
//! owning session's input as a host-only transcript notice. The next turn that starts carries
//! it, and only that turn's completion acknowledges it (`runs` substrate `begin_turn`/`end_turn`).
use crate::authority::shell::{CommandVerdict, record_verdict};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, watch as ops};
use crate::ports::process::{Probe, ProcessSpec as NativeProcessSpec};
use crate::ports::storage::{
    ApplicationStore, ArtifactScope, ConfigScope, ConfigurationRead, MAX_WATCH_TAIL_BYTES,
    NewEvent, OperationRecord, ProcessPhase, ReadOptions, ReadSnapshot, StorageError, StoredMatch,
    StoredProbe, StoredProcess, StoredWatchEvent, StoredWatchOutcome, StoredWatcher,
    WatchEventState, WatcherFilter, WatcherPhase, WriteOptions, WriteTransaction, read_snapshot,
    run_write,
};
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, Preparation, Prepared,
    QueryAnswer, QueryHandler,
};
use crate::protocol::provider::TranscriptBody;
use crate::protocol::records::{
    WatchDeliveryState, WatchEvent, WatchOutcome, WatchOutcomeKind, WatchProbeStatus,
    WatchProcessSource, WatchSourceKind, WatcherPage, WatcherRecord, WatcherState,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{
    CorrelationId, DomainErrorCode, DomainFault, EventId, Id, OperationId, OperationState,
    ProjectId, RevisionDimension,
};
use crate::services::artifacts::host::HostArtifactPublication;
use crate::services::artifacts::{ArtifactService, version_ref};
use crate::services::diagnostics::{DiagnosticService, LogEntry};
use crate::services::process::STOPPED_ON_REQUEST;
use crate::services::process::tools::{
    BindingRefusal, ExecuteOrigin, MatchCounter, ProcessToolService, binding_refusal,
};
use crate::services::runs::ControlService;
use crate::services::sessions::transcript::{BatchOutcome, SessionBatch, SessionWriter};
use crate::services::sessions::{Refusal, counter, decided, query_fault, rev, turn};
use crate::settings::catalog::{WATCH_BACKUP_ENABLED_KEY, WATCH_BACKUP_INTERVAL_KEY};
use crate::settings::catalog::{WATCH_MAX_LIFETIME_KEY, descriptor};
use crate::settings::model::{chain, current_chain};
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;

/// Store bound on an event body (`watcher_events.payload_json`), with room for its envelope.
const MAX_EVENT_PAYLOAD_BYTES: usize = 200_000;
/// Host notices are read into provider context up to this many bytes (`tool_turn`).
const MAX_NOTICE_BYTES: usize = 8_000;
/// Default output tail a watcher keeps when `tail_bytes` is omitted.
pub const DEFAULT_TAIL_BYTES: u32 = 4_096;
/// Store bound on a probe's output pattern (`watchers.probe_output_pattern`).
const MAX_PATTERN_BYTES: usize = 4_096;
/// A probe waits at most this many intervals between runs, however many have not met it.
const MAX_BACKOFF_INTERVALS: i64 = 8;

/// What preparation found for the command a watcher launches (`None`: a timer), or the file it
/// reads.
type PreparedLaunch = Result<Prepped, BindingRefusal>;
enum Prepped {
    Launch(Option<(NativeProcessSpec, CommandVerdict)>),
    File(std::path::PathBuf),
}
/// Least time between two match events when the watcher sets none.
const DEFAULT_MATCH_INTERVAL_MS: i64 = 30_000;

/// The default maximum lifetime of a polling watcher in `project` (`watch.maxLifetimeMs`).
fn max_lifetime_default(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
) -> Result<i64, StorageError> {
    let scope = project.map_or(ConfigScope::Workspace, |p| ConfigScope::Project(p.clone()));
    let layers = current_chain(read, &scope)?;
    let d = descriptor(WATCH_MAX_LIFETIME_KEY).expect("catalog key");
    let entries = chain(d, &layers).map_err(|e| {
        StorageError::IncompatibleSchema(format!("{WATCH_MAX_LIFETIME_KEY}: {e:?}"))
    })?;
    Ok(entries
        .into_iter()
        .next()
        .and_then(|e| e.value.value.0.as_i64())
        .or_else(|| d.default_value().as_i64())
        .unwrap_or(86_400_000))
}

/// How long a probe waits after its `runs`-th run did not meet its condition: one interval,
/// doubling after each further run, at most [`MAX_BACKOFF_INTERVALS`].
fn probe_backoff(interval_ms: i64, runs: i64) -> i64 {
    let factor = 1_i64 << runs.clamp(1, 4).saturating_sub(1);
    interval_ms.saturating_mul(factor.min(MAX_BACKOFF_INTERVALS))
}
const PAGE: u16 = 100;

/// What one [`WatchService::tick`] pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchTick {
    pub settled: usize,
    pub delivered: usize,
    /// The backup check, when one ran on this tick.
    pub backup: Option<BackupPass>,
}

pub struct WatchService {
    store: Arc<dyn ApplicationStore>,
    processes: Arc<ProcessToolService>,
    artifacts: Arc<ArtifactService>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    /// Root controls: a paused or held owner's events wait (WATCH-07).
    control: Option<Arc<ControlService>>,
    /// Where backup catches are recorded as missed primary deliveries (WATCH-12).
    diagnostics: Option<Arc<DiagnosticService>>,
    /// When the backup check last came due (store clock), or `None` before the first tick.
    last_backup_ms: std::sync::Mutex<Option<i64>>,
}

/// What one watcher backup check did (WATCH-12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackupPass {
    /// Outcomes delivered by the backup check because their first delivery was missed.
    pub caught: usize,
    /// Owners reminded to check a watcher that stalled or whose source cannot be established.
    pub reminded: usize,
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
            control: None,
            diagnostics: None,
            last_backup_ms: std::sync::Mutex::new(None),
        }
    }

    /// Records each backup catch as a missed primary delivery in diagnostics as well as history.
    pub fn with_diagnostics(mut self, diagnostics: Arc<DiagnosticService>) -> Self {
        self.diagnostics = Some(diagnostics);
        self
    }

    /// Holds delivery to an owner whose root is paused or held until it may run again (Continue),
    /// when the waiting events are delivered in the order they were recorded.
    pub fn with_root_control(mut self, control: Arc<ControlService>) -> Self {
        self.control = Some(control);
        self
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }

    /// Settles every open watcher whose process outcome is now observed, then delivers every
    /// pending event to its session. `host` must be a host context.
    pub fn tick(&self, host: &InvocationContext) -> Result<WatchTick, StorageError> {
        let settled = self.settle_observed()?;
        let delivered = self.deliver_pending(host)?;
        let backup = self.backup_if_due(host)?;
        Ok(WatchTick {
            settled,
            delivered,
            backup,
        })
    }

    /// Advances every open watcher: settles those whose outcome is observed (a process's end, a
    /// probe's condition holding, a timer's due time, a polling watcher's lifetime), starts each
    /// probe run that is due and schedules the next after a run that did not meet its condition.
    /// Returns how many watchers it settled.
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
                settled += usize::from(self.advance(watcher)?);
            }
            if page.len() < usize::from(PAGE) {
                return Ok(settled);
            }
        }
    }

    /// Runs the watcher backup check when it is due (WATCH-12): when it is on, its interval has
    /// passed since it last came due (read on every tick, so a changed interval applies to the next
    /// check), and some owner still has an unfinished watcher or an undelivered event. `None` when
    /// no check ran.
    pub fn backup_if_due(
        &self,
        host: &InvocationContext,
    ) -> Result<Option<BackupPass>, StorageError> {
        let now = self.clock.now_ms().get();
        let (enabled, interval) = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            backup_settings(read.configuration())
        })?;
        {
            let mut last = self
                .last_backup_ms
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let Some(since) = *last else {
                *last = Some(now);
                return Ok(None);
            };
            if !enabled || now - since < interval {
                return Ok(None);
            }
            *last = Some(now);
        }
        let unfinished = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            Ok(!read.watchers().open_watchers(None, 1)?.is_empty()
                || !read.watchers().pending_watch_events(None, 1)?.is_empty())
        })?;
        if !unfinished {
            return Ok(None);
        }
        self.backup_pass(host).map(Some)
    }

    /// The watcher backup check itself. Each open watcher of an owner that may run is checked by
    /// a path independent of the primary pass: an outcome the primary pass never settled is
    /// settled now, a recorded tree the OS reports gone is settled Lost, and an owner whose
    /// watcher stalled (a probe more than two intervals past its due run) or whose source cannot
    /// be established is reminded to check it; a source that is still running wakes no one.
    /// Every event still undelivered is then delivered as a backup delivery, with its own event
    /// ID, and recorded as a missed primary delivery.
    pub fn backup_pass(&self, host: &InvocationContext) -> Result<BackupPass, StorageError> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(StorageError::ScopeDenied);
        }
        let mut pass = BackupPass::default();
        let mut after: Option<Id> = None;
        loop {
            let page = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                read.watchers().open_watchers(after.as_ref(), PAGE)
            })?;
            let Some(last) = page.last() else {
                break;
            };
            after = Some(last.id.clone());
            for watcher in &page {
                if !self.owner_may_run(watcher)? {
                    continue;
                }
                if let Some(reason) = self.backup_check(watcher)? {
                    pass.reminded += usize::from(self.remind(host, watcher, reason)?);
                }
            }
            if page.len() < usize::from(PAGE) {
                break;
            }
        }
        let mut after: Option<(i64, Id)> = None;
        loop {
            let page = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                read.watchers()
                    .pending_watch_events(after.as_ref().map(|(at, id)| (*at, id)), PAGE)
            })?;
            let Some(last) = page.last() else {
                break;
            };
            after = Some((last.created_at_ms, last.id.clone()));
            for event in &page {
                pass.caught += usize::from(self.deliver(host, event, true)?);
            }
            if page.len() < usize::from(PAGE) {
                break;
            }
        }
        Ok(pass)
    }

    /// Whether `watcher`'s owner is open and may run (not paused or held): a held owner is
    /// skipped by the backup check as by delivery.
    fn owner_may_run(&self, watcher: &StoredWatcher) -> Result<bool, StorageError> {
        let session = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.sessions().session(&watcher.session_id)
        })?;
        let Some(session) = session.filter(|s| !s.closed()) else {
            return Ok(false);
        };
        match (self.control.clone(), session.root_id) {
            (Some(control), Some(root)) => run_write(&*self.store, WriteOptions::default(), {
                move |tx| control.root_permits_dispatch(tx, &root)
            }),
            _ => Ok(true),
        }
    }

    /// Checks one open watcher's source by the backup path; returns why its owner must be
    /// reminded, if it must. An outcome found here is settled (and later delivered as a backup).
    fn backup_check(&self, watcher: &StoredWatcher) -> Result<Option<&'static str>, StorageError> {
        let now = self.clock.now_ms().get();
        // A probe past its lifetime ends TimedOut, stalled or not: no reminder for it.
        if watcher.expires_at_ms.is_some_and(|end| now >= end) {
            self.advance(watcher)?;
            return Ok(None);
        }
        if watcher.source_kind == "Probe"
            && let Some(probe) = &watcher.probe
            && probe
                .next_run_at_ms
                .is_some_and(|next| now - next > probe.interval_ms.saturating_mul(2))
        {
            return Ok(Some(
                "the probe has not run for more than twice its interval",
            ));
        }
        // Settles whatever the primary pass would have: an observed end, a due timer.
        if self.advance(watcher)? {
            return Ok(None);
        }
        let Some(run) = &watcher.process_operation_id else {
            return Ok(None);
        };
        // A registered monitor is not proof the source is healthy (it may be stuck): the OS is
        // asked either way.
        let monitoring = self.processes.is_monitoring(run);
        let record = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.resources().process(&run.0)
        })?;
        let Some(record) = record else {
            // Not launched yet: still pending, nothing to establish.
            return Ok(None);
        };
        match record.phase {
            ProcessPhase::Running | ProcessPhase::Stopping => {
                match self.processes.probe_watched_run(&record) {
                    Some(Probe::Alive) => Ok(None),
                    // Its monitor may be about to record the real outcome, or may be stuck: the
                    // host cannot tell, so the owner is asked to check rather than told Lost.
                    Some(Probe::Gone) if monitoring => Ok(Some(
                        "its process has ended but its outcome was not recorded",
                    )),
                    Some(Probe::Gone) => {
                        let id = watcher.id.clone();
                        let run = run.clone();
                        run_write(&*self.store, WriteOptions::default(), move |tx| {
                            lost_in(tx, &id, &run)
                        })?;
                        Ok(None)
                    }
                    Some(Probe::Unknown(_)) | None => {
                        Ok(Some("its process's status cannot be established"))
                    }
                }
            }
            ProcessPhase::Uncertain | ProcessPhase::Lost => {
                Ok(Some("its process's status cannot be established"))
            }
            _ => Ok(None),
        }
    }

    /// Wakes `watcher`'s owner with a reminder to check it: a host notice whose source is the
    /// watcher, recorded in history. It is not a watcher event and changes no state.
    fn remind(
        &self,
        host: &InvocationContext,
        watcher: &StoredWatcher,
        reason: &'static str,
    ) -> Result<bool, StorageError> {
        let text = format!(
            "Watcher reminder: watcher {id} ({kind}) needs checking: {reason}. Inspect it with \
             watch.inspect and decide whether to keep or cancel it.",
            id = watcher.id.as_str(),
            kind = watcher.source_kind,
        );
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                text.as_bytes(),
                json!({"kind":"WatcherReminder","watcherId":watcher.id.as_str(),"sessionId":watcher.session_id.as_str()}),
            )?
            .map_err(|_| StorageError::InvalidRecord("watcher reminder staging failed".into()))?;
        let pending = staged.clone();
        let service = self.artifacts.clone();
        let id = watcher.id.clone();
        let host = host.clone().with_session(watcher.session_id.clone());
        let control = self.control.clone();
        let committed = run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(watcher) = tx.watchers().watcher(&id)? else {
                return Ok(false);
            };
            let Some(session) = tx.sessions().session(&watcher.session_id)? else {
                return Ok(false);
            };
            if !watcher.phase.is_open() || session.closed() {
                return Ok(false);
            }
            // As for delivery: an owner paused or held meanwhile receives nothing.
            if let (Some(control), Some(root)) = (&control, &session.root_id)
                && !control.root_permits_dispatch(tx, root)?
            {
                return Ok(false);
            }
            let scope = session
                .project_id
                .clone()
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
            let version = service.commit_host_bytes_in_scope(
                tx,
                &pending,
                HostArtifactPublication {
                    title: "Watcher reminder",
                    media_type: "text/plain; charset=utf-8",
                    provenance: json!({"kind":"WatcherReminder","watcherId":watcher.id.as_str()}),
                    scope,
                    owner: session.agent_id.clone(),
                },
            )?;
            let batch = turn::batch_of(
                &watcher.session_id,
                tx.new_id(IdKind::TranscriptBatch),
                session.context_epoch,
                session.next_sequence,
                vec![TranscriptBody::HostNotice {
                    source: watcher.id.clone(),
                    artifact: version_ref(&version),
                }],
                || tx.new_id(IdKind::TranscriptEntry),
            );
            match SessionWriter::append_in(tx, &host, &SessionBatch::entries(batch))? {
                BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_) => {}
                BatchOutcome::Refused(refusal) => {
                    return Err(StorageError::InvalidRecord(format!(
                        "watcher reminder refused: {refusal:?}"
                    )));
                }
            }
            let operation = watcher.process_operation_id.clone();
            history(
                tx,
                &watcher,
                operation.as_ref(),
                "WatcherReminder",
                json!({"reason": reason}),
            )?;
            Ok(true)
        });
        self.artifacts.settle_host_bytes(
            &*self.store,
            &staged,
            committed.as_ref().is_ok_and(|d| *d),
        );
        committed
    }

    /// The matches found since the watcher last recorded them: the running process's count so
    /// far, or the complete lines appended to the file since its scanned offset.
    fn match_update(&self, watcher: &StoredWatcher) -> Option<MatchUpdate> {
        let rule = watcher.matching.as_ref()?;
        match watcher.source_kind.as_str() {
            "File" => {
                let path = watcher.file_path.as_deref()?;
                let (to, found) = scan_file(path, rule.scanned, &rule.pattern)?;
                (to != rule.scanned).then_some(MatchUpdate {
                    from: rule.scanned,
                    to,
                    total: rule.total + found,
                })
            }
            _ => {
                let run = watcher.process_operation_id.as_ref()?;
                let total = self.processes.live_match_count(&run.0)?;
                (total > rule.total).then_some(MatchUpdate {
                    from: rule.scanned,
                    to: rule.scanned,
                    total,
                })
            }
        }
    }

    /// The last pass of an orderly shutdown, after owned process trees were stopped (WATCH-08):
    /// a process watcher whose process the shutdown stopped ends `Interrupted`, and a probe whose
    /// run it stopped forgets that run and runs again as soon as DevWorks reopens. Timers and
    /// waiting probes need nothing: they resume on reopen. Returns how many watchers it changed.
    pub fn close_at_shutdown(
        &self,
        stopped: &std::collections::HashSet<Id>,
        deadline: std::time::Instant,
    ) -> Result<usize, StorageError> {
        let mut changed = 0;
        let mut after: Option<Id> = None;
        loop {
            // Bounded by the shutdown: what is left is observed by the next boot instead.
            if std::time::Instant::now() >= deadline {
                return Ok(changed);
            }
            let page = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                read.watchers().open_watchers(after.as_ref(), PAGE)
            })?;
            let Some(last) = page.last() else {
                return Ok(changed);
            };
            after = Some(last.id.clone());
            for watcher in &page {
                // Only a run whose tree this shutdown stopped: one stopped earlier (its own time
                // limit, a requested stop) keeps the outcome it recorded.
                if !watcher
                    .process_operation_id
                    .as_ref()
                    .is_some_and(|run| stopped.contains(&run.0))
                    || std::time::Instant::now() >= deadline
                {
                    continue;
                }
                let id = watcher.id.clone();
                changed += usize::from(run_write(
                    &*self.store,
                    WriteOptions::default(),
                    move |tx| interrupt_in(tx, &id),
                )?);
            }
            if page.len() < usize::from(PAGE) {
                return Ok(changed);
            }
        }
    }

    /// One open watcher's step; `true` when it settled.
    fn advance(&self, watcher: &StoredWatcher) -> Result<bool, StorageError> {
        // Most open watchers have nothing new: decide that from a snapshot and write only when
        // something is due or observed (the write re-checks it).
        let now = self.clock.now_ms().get();
        // A watcher lives as long as its owner (WATCH-07): an ended session or an archived
        // project cancels it and stops what it launched.
        let owner_ended = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            owner_ended(read.sessions(), read.projects(), watcher)
        })?;
        if owner_ended.is_some() {
            let id = watcher.id.clone();
            let step = run_write(&*self.store, WriteOptions::default(), move |tx| {
                end_with_owner_in(tx, &id)
            })?;
            if let Step::Expired(Some(run)) = &step {
                self.processes.stop_watched_run(&run.0);
            }
            return Ok(false);
        }
        let observed = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
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
        // New matches: counted over the live output by the run, or read from the file.
        let update = self.match_update(watcher);
        let flush = watcher.matching.as_ref().is_some_and(|m| {
            m.each
                && m.pending > 0
                && m.last_event_at_ms.is_none_or(|at| {
                    now - at >= m.min_interval_ms.unwrap_or(DEFAULT_MATCH_INTERVAL_MS)
                })
        });
        let due = update.is_some()
            || flush
            || match watcher.source_kind.as_str() {
                "Timer" => watcher.due_at_ms.is_some_and(|due| now >= due),
                "File" => watcher.expires_at_ms.is_some_and(|end| now >= end),
                "Probe" => {
                    watcher.expires_at_ms.is_some_and(|end| now >= end)
                        || (watcher.process_operation_id.is_none()
                            && watcher
                                .probe
                                .as_ref()
                                .and_then(|p| p.next_run_at_ms)
                                .is_some_and(|next| now >= next))
                }
                _ => false,
            };
        if !observed && !due {
            return Ok(false);
        }
        let id = watcher.id.clone();
        // Checked before the write: a monitor records its outcome before it lets go.
        let monitoring = watcher
            .process_operation_id
            .as_ref()
            .is_some_and(|op| self.processes.is_monitoring(op));
        let processes = self.processes.clone();
        let step = run_write(&*self.store, WriteOptions::default(), move |tx| {
            step_in(tx, &id, monitoring, &processes, update)
        })?;
        if let Step::Expired(Some(run)) = &step {
            // The lifetime ended with a run in flight: the watcher already settled TimedOut, and
            // the run's tree is stopped now (a failed stop leaves it to its own time limit).
            self.processes.stop_watched_run(&run.0);
        }
        Ok(matches!(step, Step::Settled | Step::Expired(_)))
    }

    /// Delivers every pending event to its session; returns how many it delivered.
    pub fn deliver_pending(&self, host: &InvocationContext) -> Result<usize, StorageError> {
        if host.principal_kind() != PrincipalKind::Host {
            return Err(StorageError::ScopeDenied);
        }
        let mut delivered = 0;
        // In the order the events were recorded, so a held owner's events arrive in order once
        // it may run again.
        let mut after: Option<(i64, Id)> = None;
        loop {
            let page = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
                read.watchers()
                    .pending_watch_events(after.as_ref().map(|(at, id)| (*at, id)), PAGE)
            })?;
            let Some(last) = page.last() else {
                return Ok(delivered);
            };
            after = Some((last.created_at_ms, last.id.clone()));
            for event in &page {
                delivered += usize::from(self.deliver(host, event, false)?);
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
        backup: bool,
    ) -> Result<bool, StorageError> {
        // An ended session's events are never delivered (its watchers are cancelled), so they
        // are not staged on every pass.
        let session = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            read.sessions().session(&event.session_id)
        })?;
        let Some(session) = session.filter(|session| !session.closed()) else {
            return Ok(false);
        };
        // A held owner's events wait without staging a notice on every pass; the write below
        // checks again, against a pause that lands meanwhile.
        if let (Some(control), Some(root)) = (self.control.clone(), session.root_id.clone()) {
            let permitted = run_write(&*self.store, WriteOptions::default(), move |tx| {
                control.root_permits_dispatch(tx, &root)
            })?;
            if !permitted {
                return Ok(false);
            }
        }
        let mut text = notice_text(event);
        if backup {
            text = format!(
                "Backup delivery: this watcher event's first delivery was missed; the watcher \
                 backup check found it. Act on it as on any watcher event.\n{text}"
            );
        }
        let staged = self
            .artifacts
            .stage_host_bytes(
                &*self.store,
                text.as_bytes(),
                json!({"kind":"WatcherEvent","eventId":event.id.as_str(),"sessionId":event.session_id.as_str(),"backup":backup}),
            )?
            .map_err(|_| StorageError::InvalidRecord("watcher notice staging failed".into()))?;
        let pending = staged.clone();
        let service = self.artifacts.clone();
        let event_id = event.id.clone();
        let host = host.clone().with_session(event.session_id.clone());
        let control = self.control.clone();
        let diagnostics = self.diagnostics.clone();
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
                // An ended session's watchers are cancelled; its events are never delivered.
                return Ok(false);
            }
            // A paused (or otherwise held) owner receives nothing until it may run again: the
            // event waits, and no notice arrives to start a turn.
            if let (Some(control), Some(root)) = (&control, &session.root_id)
                && !control.root_permits_dispatch(tx, root)?
            {
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
                    provenance: json!({"kind":"WatcherEvent","eventId":event.id.as_str(),"watcherId":event.watcher_id.as_str(),"backup":backup}),
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
            let delivered = tx
                .watchers()
                .deliver_watch_event(&event.id, sequence, now)?;
            if backup && delivered {
                record_backup_catch(tx, diagnostics.as_deref(), &event)?;
            }
            Ok(delivered)
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
        let refuse = |tx: &mut dyn WriteTransaction, code, target: &'static str| {
            decided(tx, Err(Refusal::new(code, target, None).into()))
        };
        let Some(session_id) = ctx.session_id().cloned() else {
            return refuse(tx, DomainErrorCode::InvalidArgument, "watcher_session");
        };
        let mut file_path = None;
        match command.prepared::<PreparedLaunch>() {
            Some(Ok(Prepped::File(path))) => file_path = Some(path.to_string_lossy().into_owned()),
            Some(Ok(Prepped::Launch(Some((bound, verdict))))) => {
                // Probe and launch commands pass the shell preflight at creation (WATCH-10),
                // not only when each run launches.
                if matches!(verdict, CommandVerdict::Blocked { .. }) {
                    record_verdict(tx, ctx, "watch.create", &bound.argv(), verdict)?;
                    return refuse(tx, DomainErrorCode::PermissionDenied, "process_preflight");
                }
            }
            Some(Ok(Prepped::Launch(None))) => {}
            Some(Err(reason)) => return decided(tx, Err(binding_refusal(*reason).into())),
            None => return refuse(tx, DomainErrorCode::Conflict, "watch_preparation"),
        }
        let now = tx.now_ms();
        let requested = input.tail_bytes.as_ref().map(|t| t.get());
        let tail_bytes = requested
            .unwrap_or(DEFAULT_TAIL_BYTES)
            .min(MAX_WATCH_TAIL_BYTES as u32);
        let matching = match &input.matching {
            Some(rule) => {
                let pattern = rule.pattern.0.clone();
                if regex::Regex::new(&pattern).is_err() {
                    return refuse(tx, DomainErrorCode::InvalidArgument, "matching_pattern");
                }
                Some(StoredMatch {
                    pattern,
                    each: rule.each,
                    min_interval_ms: Some(
                        rule.min_interval_ms
                            .as_ref()
                            .map_or(DEFAULT_MATCH_INTERVAL_MS, |ms| ms.get()),
                    ),
                    scanned: 0,
                    pending: 0,
                    total: 0,
                    last_event_at_ms: None,
                })
            }
            None => None,
        };
        let output_pattern = matching.as_ref().map(|m| m.pattern.clone());
        let launch = |spec, tx: &mut dyn WriteTransaction| {
            self.processes.record_execute(
                ctx,
                command.fingerprint(),
                spec,
                ExecuteOrigin::Watch {
                    tail_bytes,
                    requested,
                    pattern: output_pattern.clone(),
                },
                tx,
            )
        };
        let mut watcher = StoredWatcher {
            id: tx.new_id(IdKind::Watcher),
            revision: 1,
            phase: WatcherPhase::Active,
            source_kind: String::new(),
            project_id: ctx.project_id().cloned(),
            session_id,
            actor: ctx.principal().clone(),
            process_operation_id: None,
            executable_ref: None,
            tail_bytes: i64::from(tail_bytes),
            outcome: None,
            duration_ms: None,
            output_tail: None,
            output_truncated: false,
            stop_requested: false,
            created_at_ms: now,
            updated_at_ms: now,
            settled_at_ms: None,
            probe: None,
            due_at_ms: None,
            expires_at_ms: None,
            matching,
            file_path: None,
        };
        if input.file.is_some() {
            let Some(path) = file_path.take() else {
                return refuse(tx, DomainErrorCode::Conflict, "watch_preparation");
            };
            let lifetime = match &input.max_lifetime_ms {
                Some(ms) => ms.get(),
                None => max_lifetime_default(tx.configuration(), ctx.project_id())?,
            };
            watcher.source_kind = "File".into();
            watcher.tail_bytes = 0;
            watcher.file_path = Some(path);
            watcher.expires_at_ms = Some(now.saturating_add(lifetime));
        } else if let Some(process) = &input.process {
            watcher.source_kind = "Process".into();
            watcher.executable_ref = Some(process.executable_ref.as_str().to_owned());
            watcher.process_operation_id = Some(launch(process, tx)?);
        } else if let Some(probe) = &input.probe {
            let pattern = probe.condition.output_pattern.as_ref().map(|p| p.0.clone());
            if pattern
                .as_deref()
                .is_some_and(|p| p.len() > MAX_PATTERN_BYTES || regex::Regex::new(p).is_err())
            {
                return refuse(tx, DomainErrorCode::InvalidArgument, "probe_output_pattern");
            }
            let lifetime = match &input.max_lifetime_ms {
                Some(ms) => ms.get(),
                None => max_lifetime_default(tx.configuration(), ctx.project_id())?,
            };
            let first = launch(&probe.process, tx)?;
            watcher.source_kind = "Probe".into();
            watcher.executable_ref = Some(probe.process.executable_ref.as_str().to_owned());
            watcher.process_operation_id = Some(first.clone());
            watcher.expires_at_ms = Some(now.saturating_add(lifetime));
            watcher.probe = Some(StoredProbe {
                job_operation_id: first,
                interval_ms: probe.interval_ms.get(),
                exit_code: probe.condition.exit_code.map(i64::from),
                output_pattern: pattern,
                runs: 0,
                next_run_at_ms: None,
            });
        } else if let Some(timer) = &input.timer {
            let due = match (&timer.at_ms, &timer.after_ms) {
                (Some(at), _) => at.get(),
                (None, Some(after)) => now.saturating_add(after.get()),
                (None, None) => return refuse(tx, DomainErrorCode::InvalidArgument, "timer"),
            };
            watcher.source_kind = "Timer".into();
            watcher.tail_bytes = 0;
            watcher.due_at_ms = Some(due);
        } else {
            return refuse(tx, DomainErrorCode::InvalidArgument, "watcher_source");
        }
        tx.watchers().insert_watcher(&watcher)?;
        let operation = watcher.process_operation_id.clone();
        history(
            tx,
            &watcher,
            operation.as_ref(),
            "WatcherCreated",
            json!({
                "sourceKind": watcher.source_kind,
                "processId": operation.as_ref().map(|o| o.0.as_str()),
                "dueAtMs": watcher.due_at_ms,
                "expiresAtMs": watcher.expires_at_ms,
            }),
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
        // A probe between runs has nothing running to stop: stopping it just ends it.
        let probe = watcher.source_kind == "Probe";
        // A probe whose run has already ended (or that waits between runs) has nothing running
        // to stop: stopping it just ends it, so no further run starts.
        let run_ended = match &watcher.process_operation_id {
            Some(process) => {
                let monitoring = self.processes.is_monitoring(process);
                let record = tx.resources().process(&process.0)?;
                tx.operations()
                    .operation(process)?
                    .is_some_and(|op| observe(&watcher, &op, record.as_ref(), monitoring).is_some())
            }
            None => probe,
        };
        let kind = if input.stop_process && !(probe && run_ended) {
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
            if run_ended {
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
                // The command a process or probe watcher launches, bound and preflighted outside
                // the write (the preflight resolves paths on disk). A timer launches nothing.
                let spec = input
                    .process
                    .as_ref()
                    .or(input.probe.as_ref().map(|p| &p.process));
                let prepared: PreparedLaunch = match &input.file {
                    Some(file) => self
                        .processes
                        .resolve_watched_file(&file.path, ctx.project_id())
                        .map(Prepped::File),
                    None => spec
                        .map(|spec| {
                            let bound = self.processes.bind_spec(spec, ctx)?;
                            let verdict = self
                                .processes
                                .preflight(&bound, &spec.working_directory.root_id)
                                .map_err(|_| BindingRefusal::ChangedRoot)?;
                            Ok((bound, verdict))
                        })
                        .transpose()
                        .map(Prepped::Launch),
                };
                Some(Prepared::new(prepared))
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

/// What one write step did to an open watcher.
#[derive(Debug)]
enum Step {
    Nothing,
    Settled,
    /// A probe run started or the next one was scheduled.
    Advanced,
    /// A polling watcher reached its maximum lifetime, with the run then in flight (if any).
    Expired(Option<OperationId>),
}

/// Whether the backup check is on, and its interval.
fn backup_settings(read: &dyn ConfigurationRead) -> Result<(bool, i64), StorageError> {
    let layers = current_chain(read, &ConfigScope::Workspace)?;
    let value = |key: &str| {
        let d = descriptor(key).expect("catalog key");
        chain(d, &layers)
            .ok()
            .and_then(|e| e.into_iter().next())
            .map(|e| e.value.value.0)
            .unwrap_or_else(|| d.default_value())
    };
    Ok((
        value(WATCH_BACKUP_ENABLED_KEY).as_bool().unwrap_or(true),
        value(WATCH_BACKUP_INTERVAL_KEY)
            .as_i64()
            .unwrap_or(900_000)
            .max(60_000),
    ))
}

/// Records a backup delivery of `event` as a missed primary delivery: in the watcher's history
/// and, when composed, in diagnostics.
fn record_backup_catch(
    tx: &mut dyn WriteTransaction,
    diagnostics: Option<&DiagnosticService>,
    event: &StoredWatchEvent,
) -> Result<(), StorageError> {
    let Some(watcher) = tx.watchers().watcher(&event.watcher_id)? else {
        return Ok(());
    };
    let operation = watcher.process_operation_id.clone();
    history(
        tx,
        &watcher,
        operation.as_ref(),
        "WatcherBackupCatch",
        json!({"eventId": event.id.as_str(), "missed": "primary delivery"}),
    )?;
    if let Some(diagnostics) = diagnostics {
        let mut entry = LogEntry::new(
            "watch.backup",
            "missed_primary_delivery",
            crate::protocol::records::Severity::Warning,
            format!(
                "The watcher backup check delivered event {} of watcher {}: its primary delivery was missed.",
                event.id.as_str(),
                watcher.id.as_str()
            ),
        );
        entry.project_id = watcher.project_id.clone();
        entry.target_kind = Some("watcher".into());
        entry.target_id = Some(watcher.id.clone());
        entry.operation_id = operation;
        diagnostics
            .log_in(tx, &entry)
            .map_err(|refusal| match refusal {
                crate::services::diagnostics::LogRefusal::Storage(error) => error,
                crate::services::diagnostics::LogRefusal::Invalid(why) => {
                    StorageError::InvalidRecord(why.into())
                }
            })?;
    }
    Ok(())
}

/// Settles an open watcher whose recorded tree the OS reports gone without an observed outcome:
/// Lost, found by the backup check.
fn lost_in(
    tx: &mut dyn WriteTransaction,
    id: &Id,
    run: &OperationId,
) -> Result<bool, StorageError> {
    let Some(watcher) = tx.watchers().watcher(id)? else {
        return Ok(false);
    };
    if !watcher.phase.is_open() || watcher.process_operation_id.as_ref() != Some(run) {
        return Ok(false);
    }
    let now = tx.now_ms();
    let duration = Some((now - watcher.created_at_ms).max(0));
    let lost = outcome(
        "Lost",
        None,
        None,
        Some(
            "the process ended without its outcome being observed; found by the watcher backup check",
        ),
    );
    finish_in(tx, watcher, lost, duration, None, false, Some(run))?;
    Ok(true)
}

/// Why `watcher`'s owner no longer exists for it: its session ended, or its project was archived
/// (or deleted). `None` while both are open.
fn owner_ended(
    sessions: &dyn crate::ports::storage::SessionRead,
    projects: &dyn crate::ports::storage::ProjectRead,
    watcher: &StoredWatcher,
) -> Result<Option<&'static str>, StorageError> {
    let session_ended = sessions
        .session(&watcher.session_id)?
        .is_none_or(|session| session.closed());
    let project_closed = match &watcher.project_id {
        Some(project) => projects
            .project(project)?
            .is_none_or(|p| crate::services::runs::closed_to_work(&p.lifecycle)),
        None => false,
    };
    Ok(ended_because(session_ended, project_closed))
}

fn ended_because(session_ended: bool, project_closed: bool) -> Option<&'static str> {
    if session_ended {
        Some("the owning session ended")
    } else if project_closed {
        Some("the owning project was archived")
    } else {
        None
    }
}

/// Cancels an open watcher whose owner ended (WATCH-07), with a stop of what it launched: no
/// further probe run starts, a queued process is not launched, and the run in flight (if any) is
/// returned for the caller to stop. Nothing is delivered: the owner is gone.
fn end_with_owner_in(tx: &mut dyn WriteTransaction, id: &Id) -> Result<Step, StorageError> {
    let Some(mut watcher) = tx.watchers().watcher(id)? else {
        return Ok(Step::Nothing);
    };
    if !watcher.phase.is_open() {
        return Ok(Step::Nothing);
    }
    let session_ended = tx
        .sessions()
        .session(&watcher.session_id)?
        .is_none_or(|session| session.closed());
    let project_closed = match &watcher.project_id {
        Some(project) => tx
            .projects()
            .project(project)?
            .is_none_or(|p| crate::services::runs::closed_to_work(&p.lifecycle)),
        None => false,
    };
    let Some(reason) = ended_because(session_ended, project_closed) else {
        return Ok(Step::Nothing);
    };
    let run = watcher.process_operation_id.clone();
    let in_flight = match &run {
        Some(op) => tx
            .operations()
            .operation(op)?
            .is_some_and(|op| !watch_outcome_recorded(&op)),
        None => false,
    };
    let now = tx.now_ms();
    watcher.phase = WatcherPhase::Cancelled;
    watcher.stop_requested = true;
    watcher.settled_at_ms = Some(now);
    if let Some(probe) = watcher.probe.as_mut() {
        probe.next_run_at_ms = None;
    }
    update_in(tx, &mut watcher, now)?;
    history(
        tx,
        &watcher,
        run.as_ref(),
        "WatcherCancelled",
        json!({"reason": reason, "stopProcess": true}),
    )?;
    Ok(Step::Expired(run.filter(|_| in_flight)))
}

/// Applies the shutdown's stop of `id`'s current run, which the shutdown reported stopping: it
/// ended it if the record says the stop found it running (or could not verify it stopped). A run
/// that had already exited keeps its own outcome for the next pass.
fn interrupt_in(tx: &mut dyn WriteTransaction, id: &Id) -> Result<bool, StorageError> {
    let Some(mut watcher) = tx.watchers().watcher(id)? else {
        return Ok(false);
    };
    let Some(run) = watcher.process_operation_id.clone() else {
        return Ok(false);
    };
    if !watcher.phase.is_open() || watcher.stop_requested {
        return Ok(false);
    }
    let stopped_by_shutdown = tx.resources().process(&run.0)?.is_some_and(|p| {
        matches!(p.phase, ProcessPhase::Uncertain)
            || (p.phase == ProcessPhase::Reaped && p.detail.as_deref() == Some(STOPPED_ON_REQUEST))
    });
    if !stopped_by_shutdown {
        return Ok(false);
    }
    let now = tx.now_ms();
    if watcher.source_kind == "Probe" {
        if let Some(probe) = watcher.probe.as_mut() {
            probe.next_run_at_ms = Some(now);
        }
        watcher.process_operation_id = None;
        update_in(tx, &mut watcher, now)?;
        history(
            tx,
            &watcher,
            Some(&run),
            "WatcherProbeInterrupted",
            json!({"processId": run.0.as_str()}),
        )?;
        return Ok(true);
    }
    let duration = Some((now - watcher.created_at_ms).max(0));
    let interrupted = outcome(
        "Interrupted",
        None,
        None,
        Some("DevWorks shut down and stopped the process"),
    );
    finish_in(tx, watcher, interrupted, duration, None, false, Some(&run))?;
    Ok(true)
}

/// Advances one open watcher in `tx`: the watcher's state change, its event and its history
/// commit together.
fn step_in(
    tx: &mut dyn WriteTransaction,
    id: &Id,
    monitoring: bool,
    processes: &ProcessToolService,
    update: Option<MatchUpdate>,
) -> Result<Step, StorageError> {
    let Some(mut watcher) = tx.watchers().watcher(id)? else {
        return Ok(Step::Nothing);
    };
    if !watcher.phase.is_open() {
        return Ok(Step::Nothing);
    }
    let now = tx.now_ms();
    // Matches first: a first-match watcher completes on its first, an each-match watcher records
    // them and reports them no more often than its minimum interval.
    // A pass that settles the watcher reports no intermediate event first: its pending matches
    // ride on the outcome, so the outcome is never delivered ahead of a report saying the
    // source is still watched. An intermediate report does not end the pass otherwise.
    let ending = match watcher.source_kind.as_str() {
        "File" => watcher.expires_at_ms.is_some_and(|end| now >= end),
        "Process" => match watcher.process_operation_id.clone() {
            Some(run) => match tx.operations().operation(&run)? {
                Some(operation) => {
                    let process = tx.resources().process(&run.0)?;
                    observe(&watcher, &operation, process.as_ref(), monitoring).is_some()
                }
                None => false,
            },
            None => false,
        },
        _ => false,
    };
    if watcher.matching.is_some()
        && let Some(Step::Settled) = matches_in(tx, &mut watcher, update, now, !ending)?
    {
        return Ok(Step::Settled);
    }
    match watcher.source_kind.as_str() {
        "Timer" => {
            if watcher.due_at_ms.is_none_or(|due| now < due) {
                return Ok(Step::Nothing);
            }
            let duration = Some((now - watcher.created_at_ms).max(0));
            let fired = outcome("Succeeded", None, None, Some("the timer fired"));
            finish_in(tx, watcher, fired, duration, None, false, None)?;
            Ok(Step::Settled)
        }
        "Probe" => probe_step_in(tx, watcher, monitoring, processes, now),
        "File" => {
            if watcher.expires_at_ms.is_none_or(|end| now < end) {
                return Ok(Step::Advanced);
            }
            let duration = Some((now - watcher.created_at_ms).max(0));
            let detail = if watcher.matching.as_ref().is_some_and(|m| m.each) {
                "the file watcher reached its maximum lifetime"
            } else {
                "the pattern did not match the file within the watcher's maximum lifetime"
            };
            let ended = outcome("TimedOut", None, None, Some(detail));
            finish_in(tx, watcher, ended, duration, None, false, None)?;
            Ok(Step::Settled)
        }
        _ => {
            let Some(operation_id) = watcher.process_operation_id.clone() else {
                return Ok(Step::Nothing);
            };
            let Some(operation) = tx.operations().operation(&operation_id)? else {
                return Ok(Step::Nothing);
            };
            let process = tx.resources().process(&operation_id.0)?;
            let Some(observed) = observe(&watcher, &operation, process.as_ref(), monitoring) else {
                return Ok(Step::Nothing);
            };
            // The final count covers the output after the last pass read the live count.
            if let (Some(rule), Some(total)) = (&watcher.matching, final_matches(&operation))
                && total > rule.total
            {
                let update = MatchUpdate {
                    from: rule.scanned,
                    to: rule.scanned,
                    total,
                };
                if let Some(step) = matches_in(tx, &mut watcher, Some(update), now, false)? {
                    return Ok(step);
                }
            }
            let mut ended = observed.outcome;
            if watcher.matching.as_ref().is_some_and(|m| !m.each) {
                let detail = match ended.detail.take() {
                    Some(detail) => format!("{detail}; the output never matched the pattern"),
                    None => "the output never matched the pattern".into(),
                };
                // The stored detail is bounded (1,024 characters); the explanation must fit.
                ended.detail = Some(detail.chars().take(1024).collect());
            }
            finish_in(
                tx,
                watcher,
                ended,
                observed.duration_ms,
                observed.tail,
                observed.truncated,
                Some(&operation_id),
            )?;
            Ok(Step::Settled)
        }
    }
}

/// A probe's step: its lifetime first, then its run in flight (settle when the condition holds,
/// else schedule the next run after its backoff), then a due run.
fn probe_step_in(
    tx: &mut dyn WriteTransaction,
    mut watcher: StoredWatcher,
    monitoring: bool,
    processes: &ProcessToolService,
    now: i64,
) -> Result<Step, StorageError> {
    let Some(mut probe) = watcher.probe.clone() else {
        return Ok(Step::Nothing);
    };
    let duration = Some((now - watcher.created_at_ms).max(0));
    if watcher.expires_at_ms.is_some_and(|end| now >= end) {
        let run = watcher.process_operation_id.clone();
        let detail = format!(
            "the probe's condition did not hold within its maximum lifetime ({} runs)",
            probe.runs
        );
        let expired = outcome("TimedOut", None, None, Some(&detail));
        finish_in(tx, watcher, expired, duration, None, false, run.as_ref())?;
        // A run whose outcome is already recorded needs no stop.
        let in_flight = match &run {
            Some(op) => tx
                .operations()
                .operation(op)?
                .is_some_and(|op| !watch_outcome_recorded(&op)),
            None => false,
        };
        return Ok(Step::Expired(run.filter(|_| in_flight)));
    }
    if let Some(run) = watcher.process_operation_id.clone() {
        let Some(operation) = tx.operations().operation(&run)? else {
            return Ok(Step::Nothing);
        };
        let process = tx.resources().process(&run.0)?;
        let Some(observed) = observe(&watcher, &operation, process.as_ref(), monitoring) else {
            return Ok(Step::Nothing);
        };
        let exit = observed.outcome.exit_code;
        let completed = exit.is_some() || observed.outcome.kind == "TimedOut";
        // A stop was requested (watch.cancel with stopProcess): the run's end is the outcome.
        if !completed || watcher.phase == WatcherPhase::Stopping {
            // The probe could not run (refused, lost or uncertain): that is its outcome.
            finish_in(
                tx,
                watcher,
                observed.outcome,
                duration,
                observed.tail,
                observed.truncated,
                Some(&run),
            )?;
            return Ok(Step::Settled);
        }
        probe.runs += 1;
        if exit.is_some() && condition_holds(&probe, exit, run_output(&operation).as_deref()) {
            let detail = format!("the probe's condition held on run {}", probe.runs);
            let held = outcome("Succeeded", exit, None, Some(&detail));
            watcher.probe = Some(probe);
            finish_in(
                tx,
                watcher,
                held,
                duration,
                observed.tail,
                observed.truncated,
                Some(&run),
            )?;
            return Ok(Step::Settled);
        }
        probe.next_run_at_ms =
            Some(now.saturating_add(probe_backoff(probe.interval_ms, probe.runs)));
        watcher.output_tail = observed.tail;
        watcher.output_truncated = observed.truncated;
        watcher.process_operation_id = None;
        let payload = json!({
            "runs": probe.runs,
            "processId": run.0.as_str(),
            "exitCode": exit,
            "nextRunAtMs": probe.next_run_at_ms,
        });
        watcher.probe = Some(probe);
        update_in(tx, &mut watcher, now)?;
        history(tx, &watcher, Some(&run), "WatcherProbeRan", payload)?;
        return Ok(Step::Advanced);
    }
    if probe.next_run_at_ms.is_none_or(|next| now < next) {
        return Ok(Step::Nothing);
    }
    let run = processes.record_watch_rerun(&probe.job_operation_id, tx)?;
    probe.next_run_at_ms = None;
    watcher.probe = Some(probe);
    watcher.process_operation_id = Some(run.clone());
    update_in(tx, &mut watcher, now)?;
    history(
        tx,
        &watcher,
        Some(&run),
        "WatcherProbeStarted",
        json!({"processId": run.0.as_str()}),
    )?;
    Ok(Step::Advanced)
}

/// Matches found since a watcher last recorded them: its new total, and for a file the bytes
/// scanned (complete lines) from `from` to `to`.
#[derive(Clone, Copy, Debug)]
struct MatchUpdate {
    from: i64,
    to: i64,
    total: i64,
}

/// Applies `update` to `watcher`'s match rule and records what it calls for: `Some(step)` when
/// the watcher settled (its first match) or reported matches, `None` to continue its source's
/// own step. The watcher row is written whenever its match state changed.
fn matches_in(
    tx: &mut dyn WriteTransaction,
    watcher: &mut StoredWatcher,
    update: Option<MatchUpdate>,
    now: i64,
    report: bool,
) -> Result<Option<Step>, StorageError> {
    let Some(mut rule) = watcher.matching.clone() else {
        return Ok(None);
    };
    let mut changed = false;
    // Read outside this write: applied only if nothing recorded them meanwhile.
    if let Some(update) = update
        && update.from == rule.scanned
        && update.total >= rule.total
        && (update.total > rule.total || update.to != rule.scanned)
    {
        rule.pending += update.total - rule.total;
        rule.total = update.total;
        rule.scanned = update.to;
        changed = true;
    }
    if !rule.each && rule.total > 0 {
        rule.pending = 0;
        watcher.matching = Some(rule);
        let duration = Some((now - watcher.created_at_ms).max(0));
        let run = watcher.process_operation_id.clone();
        let matched = outcome("Succeeded", None, None, Some("the pattern matched"));
        finish_in(
            tx,
            watcher.clone(),
            matched,
            duration,
            None,
            false,
            run.as_ref(),
        )?;
        return Ok(Some(Step::Settled));
    }
    let interval = rule.min_interval_ms.unwrap_or(DEFAULT_MATCH_INTERVAL_MS);
    if report
        && rule.each
        && rule.pending > 0
        && rule.last_event_at_ms.is_none_or(|at| now - at >= interval)
    {
        let count = rule.pending;
        rule.pending = 0;
        rule.last_event_at_ms = Some(now);
        watcher.matching = Some(rule);
        update_in(tx, watcher, now)?;
        let mut payload = bounded_payload(watcher, now)?;
        payload["matchCount"] = json!(count);
        let event = StoredWatchEvent {
            id: tx.new_id(IdKind::Event),
            watcher_id: watcher.id.clone(),
            session_id: watcher.session_id.clone(),
            project_id: watcher.project_id.clone(),
            state: WatchEventState::Pending,
            payload,
            transcript_sequence: None,
            carried_turn_id: None,
            created_at_ms: now,
            delivered_at_ms: None,
            acknowledged_at_ms: None,
            match_count: Some(count),
        };
        tx.watchers().insert_watch_event(&event)?;
        let run = watcher.process_operation_id.clone();
        history(
            tx,
            watcher,
            run.as_ref(),
            "WatcherMatched",
            json!({"eventId": event.id.as_str(), "matches": count}),
        )?;
        return Ok(Some(Step::Advanced));
    }
    if changed {
        watcher.matching = Some(rule);
        update_in(tx, watcher, now)?;
    }
    Ok(None)
}

/// The final match count a watched run's monitor recorded with its outcome.
fn final_matches(operation: &OperationRecord) -> Option<i64> {
    operation.attempts.iter().rev().find_map(|attempt| {
        attempt
            .observations
            .iter()
            .rev()
            .find_map(|o| o.pointer("/watchOutcome/matches").and_then(Value::as_i64))
    })
}

/// The matches of `pattern` in the complete lines appended to the file at `path` since byte
/// `from` (from the start again if the file shrank): the offset scanned to, and the count.
/// `None` while the file cannot be read (it may not exist yet).
fn scan_file(path: &str, from: i64, pattern: &str) -> Option<(i64, i64)> {
    use std::io::{Read, Seek, SeekFrom};
    const MAX_READ: u64 = 16 * 1024 * 1024;
    // The path was resolved under its registered root (directory canonical, no symlink); it
    // must still resolve to itself, so a symlink placed there later cannot lead outside.
    if std::fs::canonicalize(path).ok()? != std::path::Path::new(path) {
        return None;
    }
    // Only a regular file is read, opened without blocking (a FIFO with no writer would stall
    // every watcher's pass); the opened file is checked again.
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let len = i64::try_from(metadata.len()).ok()?;
    let from = if len < from { 0 } else { from };
    if len == from {
        return Some((from, 0));
    }
    file.seek(SeekFrom::Start(from as u64)).ok()?;
    let mut bytes = Vec::new();
    file.take(((len - from) as u64).min(MAX_READ))
        .read_to_end(&mut bytes)
        .ok()?;
    let end = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(i) => i + 1,
        // A line longer than one read is counted in pieces, so the scan moves past it.
        None if bytes.len() as u64 == MAX_READ => bytes.len(),
        None => return Some((from, 0)),
    };
    let mut counter = MatchCounter::new(pattern)?;
    counter.feed(&bytes[..end]);
    counter.finish();
    Some((from + end as i64, counter.count()))
}

/// Whether a completed probe run meets its condition: the required exit code (0 when neither a
/// code nor an output pattern is set), and a match of the pattern anywhere in its output.
fn condition_holds(probe: &StoredProbe, exit: Option<i64>, output: Option<&[u8]>) -> bool {
    let code = match (probe.exit_code, &probe.output_pattern) {
        (Some(code), _) => exit == Some(code),
        (None, None) => exit == Some(0),
        (None, Some(_)) => true,
    };
    let output = match &probe.output_pattern {
        None => true,
        Some(pattern) => regex::Regex::new(pattern).is_ok_and(|re| {
            output.is_some_and(|bytes| re.is_match(&String::from_utf8_lossy(bytes)))
        }),
    };
    code && output
}

/// The redacted output a watched run's monitor captured, if recorded.
fn run_output(operation: &OperationRecord) -> Option<Vec<u8>> {
    operation.attempts.iter().rev().find_map(|attempt| {
        attempt.observations.iter().rev().find_map(|o| {
            o.pointer("/watchOutcome/outputBase64")
                .and_then(Value::as_str)
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        })
    })
}

/// Whether a probe run has ended: refused or cancelled before it launched, or its monitor
/// recorded its outcome. A run still queued, dispatching or running has not.
fn watch_outcome_recorded(operation: &OperationRecord) -> bool {
    matches!(
        operation.state,
        OperationState::Failed | OperationState::CancelledBeforeDispatch
    ) || operation.attempts.iter().any(|a| {
        a.observations
            .iter()
            .any(|o| o.get("watchOutcome").is_some())
    })
}

fn update_in(
    tx: &mut dyn WriteTransaction,
    watcher: &mut StoredWatcher,
    now: i64,
) -> Result<(), StorageError> {
    watcher.revision += 1;
    watcher.updated_at_ms = now;
    tx.watchers().update_watcher(watcher)
}

/// Settles `watcher` with `outcome`: its state change, its one event and its history.
fn finish_in(
    tx: &mut dyn WriteTransaction,
    mut watcher: StoredWatcher,
    outcome: StoredWatchOutcome,
    duration_ms: Option<i64>,
    tail: Option<Vec<u8>>,
    truncated: bool,
    operation: Option<&OperationId>,
) -> Result<(), StorageError> {
    let now = tx.now_ms();
    watcher.phase = WatcherPhase::Settled;
    watcher.revision += 1;
    watcher.outcome = Some(outcome);
    watcher.duration_ms = duration_ms;
    if tail.is_some() {
        watcher.output_tail = tail;
        watcher.output_truncated = truncated;
    }
    if let Some(probe) = watcher.probe.as_mut() {
        probe.next_run_at_ms = None;
    }
    // Matches not yet reported ride on the outcome, so none is silently lost.
    let unreported = watcher
        .matching
        .as_mut()
        .map_or(0, |m| std::mem::take(&mut m.pending));
    watcher.updated_at_ms = now;
    watcher.settled_at_ms = Some(now);
    tx.watchers().update_watcher(&watcher)?;
    let mut payload = bounded_payload(&watcher, now)?;
    if unreported > 0 {
        payload["matchCount"] = json!(unreported);
    }
    let event = StoredWatchEvent {
        id: tx.new_id(IdKind::Event),
        watcher_id: watcher.id.clone(),
        session_id: watcher.session_id.clone(),
        project_id: watcher.project_id.clone(),
        state: WatchEventState::Pending,
        payload,
        transcript_sequence: None,
        carried_turn_id: None,
        created_at_ms: now,
        delivered_at_ms: None,
        acknowledged_at_ms: None,
        match_count: None,
    };
    tx.watchers().insert_watch_event(&event)?;
    let kind = watcher.outcome.as_ref().map(|o| o.kind.clone());
    history(
        tx,
        &watcher,
        operation,
        "WatcherSettled",
        json!({"eventId": event.id.as_str(), "outcome": kind}),
    )?;
    Ok(())
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
        source_kind: match watcher.source_kind.as_str() {
            "Probe" => WatchSourceKind::Probe,
            "Timer" => WatchSourceKind::Timer,
            "File" => WatchSourceKind::File,
            _ => WatchSourceKind::Process,
        },
        process: watcher.process_operation_id.as_ref().and_then(|op| {
            Some(WatchProcessSource {
                process_id: op.0.clone(),
                executable_ref: Id::new(watcher.executable_ref.clone()?).ok()?,
            })
        }),
        probe: watcher.probe.as_ref().map(|p| WatchProbeStatus {
            interval_ms: counter(p.interval_ms),
            runs: counter(p.runs),
            next_run_at_ms: p.next_run_at_ms.map(counter),
        }),
        due_at_ms: watcher.due_at_ms.map(counter),
        expires_at_ms: watcher.expires_at_ms.map(counter),
        matches: watcher.matching.as_ref().map(|m| counter(m.total)),
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
    let (observed, source) = match s("/sourceKind") {
        "Timer" => (
            "its timer fire".to_owned(),
            format!("timer due at {} ms", n("/dueAtMs").unwrap_or_default()),
        ),
        "File" => (
            "its file watch end".to_owned(),
            "a file under the project's registered root".to_owned(),
        ),
        "Probe" => (
            "its probe end".to_owned(),
            format!(
                "probe command {} after {} runs (last process {})",
                s("/process/executableRef"),
                n("/probe/runs").unwrap_or_default(),
                s("/process/processId"),
            ),
        ),
        _ => (
            "its process end".to_owned(),
            format!(
                "process {} ({})",
                s("/process/processId"),
                s("/process/executableRef")
            ),
        ),
    };
    let reported = n("/matchCount");
    let mut text = if event.match_count.is_some() {
        // An intermediate match event: the source is still being watched.
        format!(
            "Watcher event {event}: watcher {watcher} observed {count} new matches of its \
             pattern ({total} so far); it is still watching.\nSource: {source}.",
            event = event.id.as_str(),
            watcher = event.watcher_id.as_str(),
            count = reported.unwrap_or_default(),
            total = n("/matches").unwrap_or_default(),
        )
    } else {
        format!(
            "Watcher event {event}: watcher {watcher} observed {observed}: {outcome}.\n\
             Source: {source}.",
            event = event.id.as_str(),
            watcher = event.watcher_id.as_str(),
        )
    };
    if event.match_count.is_none()
        && let Some(total) = n("/matches")
    {
        text.push_str(&format!(" Matches: {total} in all"));
        if let Some(count) = reported {
            text.push_str(&format!(", {count} not reported before"));
        }
        text.push('.');
    }
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

    fn id(s: &str) -> Id {
        Id::new(s).unwrap()
    }

    fn watcher(tail: Vec<u8>) -> StoredWatcher {
        StoredWatcher {
            id: id("w"),
            revision: 2,
            phase: WatcherPhase::Settled,
            source_kind: "Process".into(),
            project_id: None,
            session_id: id("s"),
            actor: ActorId(id("a")),
            process_operation_id: Some(OperationId(id("op"))),
            executable_ref: Some("sh".into()),
            tail_bytes: 65_536,
            outcome: Some(outcome("Failed", Some(1), None, None)),
            duration_ms: Some(5),
            output_tail: Some(tail),
            output_truncated: false,
            stop_requested: false,
            created_at_ms: 0,
            updated_at_ms: 1,
            settled_at_ms: Some(1),
            probe: None,
            due_at_ms: None,
            expires_at_ms: None,
            matching: None,
            file_path: None,
        }
    }

    #[test]
    fn a_notice_names_what_its_watcher_observed() {
        let mut timer = watcher(Vec::new());
        timer.source_kind = "Timer".into();
        timer.executable_ref = None;
        timer.process_operation_id = None;
        timer.output_tail = None;
        timer.due_at_ms = Some(5_000);
        timer.outcome = Some(outcome("Succeeded", None, None, Some("the timer fired")));
        let mut probe = watcher(b"ready".to_vec());
        probe.source_kind = "Probe".into();
        probe.outcome = Some(outcome("Succeeded", Some(0), None, None));
        probe.probe = Some(StoredProbe {
            job_operation_id: OperationId(id("op")),
            interval_ms: 1_000,
            exit_code: None,
            output_pattern: None,
            runs: 3,
            next_run_at_ms: None,
        });
        let text = |w: &StoredWatcher| {
            notice_text(&StoredWatchEvent {
                id: id("e"),
                watcher_id: w.id.clone(),
                session_id: w.session_id.clone(),
                project_id: None,
                state: WatchEventState::Pending,
                payload: bounded_payload(w, 1).unwrap(),
                transcript_sequence: None,
                carried_turn_id: None,
                created_at_ms: 1,
                delivered_at_ms: None,
                acknowledged_at_ms: None,
                match_count: None,
            })
        };
        let fired = text(&timer);
        assert!(
            fired.contains("observed its timer fire: Succeeded"),
            "{fired}"
        );
        assert!(fired.contains("timer due at 5000 ms"), "{fired}");
        let held = text(&probe);
        assert!(
            held.contains("observed its probe end: Succeeded (exit code 0)"),
            "{held}"
        );
        assert!(held.contains("probe command sh after 3 runs"), "{held}");
    }

    /// After a crash nothing stopped the tree in an orderly way: boot recovery kills it, and the
    /// watcher reports Interrupted (WATCH-08), not the kill's missing exit code.
    #[test]
    fn a_process_killed_by_boot_recovery_is_interrupted() {
        use crate::ports::storage::{AttemptBinding, AttemptRecord};
        let mut watcher = watcher(Vec::new());
        watcher.phase = WatcherPhase::Active;
        watcher.outcome = None;
        let launched = OperationRecord {
            id: OperationId(id("op")),
            root_id: None,
            assignment_id: None,
            capability: "process.execute".into(),
            schema_version: 1,
            args_digest: String::new(),
            payload: json!({}),
            state: OperationState::ObservedComplete,
            revision: 2,
            created_at_ms: 0,
            attempts: vec![AttemptRecord {
                id: crate::protocol::AttemptId(id("attempt")),
                attempt_no: 1,
                boot_id: id("earlier-boot"),
                binding: AttemptBinding {
                    execution_id: None,
                    owner_generation: None,
                    control_revision: None,
                    policy_revision: None,
                },
                state: OperationState::ObservedComplete,
                observations: vec![json!({"effect": {"evidence": {"launched": true}}})],
            }],
        };
        let killed = StoredProcess {
            id: id("op"),
            boot_id: id("earlier-boot"),
            phase: ProcessPhase::Reaped,
            pid: Some(1),
            birth: Some("b".into()),
            group: None,
            platform: "unix".into(),
            program: "sh".into(),
            argv_sha256: String::new(),
            operation_id: Some(OperationId(id("op"))),
            lease_id: None,
            actor: ActorId(id("a")),
            session_id: None,
            project_id: None,
            root_id: None,
            assignment_id: None,
            exit_code: None,
            detail: Some("killed by boot recovery".into()),
            created_at_ms: 0,
            updated_at_ms: 1,
            revision: 3,
        };
        let observed = observe(&watcher, &launched, Some(&killed), false).expect("observed");
        assert_eq!(observed.outcome.kind, "Interrupted");
    }

    #[test]
    fn a_queued_or_running_probe_run_is_in_flight_until_it_ends() {
        let run = |state| OperationRecord {
            id: OperationId(id("op")),
            root_id: None,
            assignment_id: None,
            capability: "process.execute".into(),
            schema_version: 1,
            args_digest: String::new(),
            payload: json!({}),
            state,
            revision: 1,
            created_at_ms: 0,
            attempts: Vec::new(),
        };
        // Still to be stopped when a probe's lifetime ends: queued, dispatching or launched.
        for state in [
            OperationState::IntentRecorded,
            OperationState::Preflighted,
            OperationState::Dispatching,
            OperationState::ObservedComplete,
        ] {
            assert!(!watch_outcome_recorded(&run(state)), "{state:?}");
        }
        for state in [
            OperationState::Failed,
            OperationState::CancelledBeforeDispatch,
        ] {
            assert!(watch_outcome_recorded(&run(state)), "{state:?}");
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("devworks-scan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// A line longer than one read is passed in pieces; matches after it are still found.
    #[test]
    fn a_file_scan_moves_past_a_line_longer_than_one_read() {
        let dir = scratch("long-line");
        let path = dir.join("big.log");
        let mut bytes = vec![b'x'; 16 * 1024 * 1024 + 10];
        bytes.extend_from_slice(b"\nERROR here\n");
        std::fs::write(&path, &bytes).unwrap();
        let path = path.to_string_lossy().into_owned();
        let (to, found) = scan_file(&path, 0, "ERROR").unwrap();
        assert_eq!(
            (to, found),
            (16 * 1024 * 1024, 0),
            "the oversized window is passed"
        );
        let (end, found) = scan_file(&path, to, "ERROR").unwrap();
        assert_eq!((end, found), (bytes.len() as i64, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A symlink placed at the watched path, or anything but a regular file, is never read.
    #[cfg(unix)]
    #[test]
    fn a_file_scan_reads_only_the_regular_file_at_its_own_path() {
        let dir = scratch("special");
        let outside = scratch("outside").join("secret.log");
        std::fs::write(&outside, b"ERROR secret\n").unwrap();
        let link = dir.join("link.log");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert_eq!(
            scan_file(&link.to_string_lossy(), 0, "ERROR"),
            None,
            "symlink refused"
        );
        let fifo = dir.join("pipe.log");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        // Returns at once (no writer would block an ordinary open).
        assert_eq!(
            scan_file(&fifo.to_string_lossy(), 0, "ERROR"),
            None,
            "FIFO refused"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(outside.parent().unwrap());
    }

    #[test]
    fn a_probe_condition_needs_its_exit_code_and_its_pattern() {
        let probe = |exit_code, output_pattern: Option<&str>| StoredProbe {
            job_operation_id: OperationId(id("op")),
            interval_ms: 1_000,
            exit_code,
            output_pattern: output_pattern.map(str::to_owned),
            runs: 0,
            next_run_at_ms: None,
        };
        assert!(condition_holds(&probe(None, None), Some(0), None));
        assert!(!condition_holds(&probe(None, None), Some(1), None));
        assert!(condition_holds(&probe(Some(3), None), Some(3), None));
        assert!(condition_holds(
            &probe(None, Some("ready")),
            Some(1),
            Some(b"ready\n")
        ));
        assert!(!condition_holds(
            &probe(None, Some("^ready$")),
            Some(0),
            Some(b"not ready")
        ));
        assert!(!condition_holds(
            &probe(Some(0), Some("up")),
            Some(1),
            Some(b"up")
        ));
        assert_eq!(
            (1..=6)
                .map(|runs| probe_backoff(1_000, runs))
                .collect::<Vec<_>>(),
            vec![1_000, 2_000, 4_000, 8_000, 8_000, 8_000]
        );
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
            match_count: None,
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
