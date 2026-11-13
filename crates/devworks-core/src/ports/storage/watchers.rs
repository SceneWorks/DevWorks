//! Watchers (DW-94): a watcher observes a source on behalf of its owning session and records the
//! observed outcome as one durable watcher event. The event is delivered into the session's
//! input, carried by the next turn that starts, and acknowledged only when that turn completes.
use super::StorageError;
use crate::protocol::{ActorId, Id, OperationId, ProjectId};
use serde_json::Value;

/// Bound on a stored output tail.
pub const MAX_WATCH_TAIL_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatcherPhase {
    /// Observing its source.
    Active,
    /// A stop of the launched process was requested; it settles when the stop is observed.
    Stopping,
    /// The outcome was observed and recorded as an event.
    Settled,
    /// The owner stopped observing; nothing is delivered.
    Cancelled,
}
impl WatcherPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Stopping => "Stopping",
            Self::Settled => "Settled",
            Self::Cancelled => "Cancelled",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Active" => Self::Active,
            "Stopping" => Self::Stopping,
            "Settled" => Self::Settled,
            "Cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
    /// Still observing (an outcome may still be recorded).
    pub fn is_open(self) -> bool {
        matches!(self, Self::Active | Self::Stopping)
    }
}

/// An observed outcome. `kind` is one of Succeeded, Failed, Cancelled, TimedOut, Interrupted,
/// Lost and Uncertain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredWatchOutcome {
    pub kind: String,
    pub exit_code: Option<i64>,
    pub signal: Option<i64>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredWatcher {
    pub id: Id,
    pub revision: i64,
    pub phase: WatcherPhase,
    /// `Process`, `Probe` or `Timer`.
    pub source_kind: String,
    pub project_id: Option<ProjectId>,
    pub session_id: Id,
    pub actor: ActorId,
    /// The process.execute operation (also the owned process's ID) the watcher launched.
    pub process_operation_id: Option<OperationId>,
    /// The launched (or probe) command's registered executable; `None` for a timer.
    pub executable_ref: Option<String>,
    pub tail_bytes: i64,
    pub outcome: Option<StoredWatchOutcome>,
    pub duration_ms: Option<i64>,
    /// Redacted output tail, at most [`MAX_WATCH_TAIL_BYTES`].
    pub output_tail: Option<Vec<u8>>,
    pub output_truncated: bool,
    pub stop_requested: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub settled_at_ms: Option<i64>,
    /// A probe's schedule and condition (`Probe` only).
    pub probe: Option<StoredProbe>,
    /// When a timer fires (`Timer` only).
    pub due_at_ms: Option<i64>,
    /// When a polling watcher ends `TimedOut` unless it settled first.
    pub expires_at_ms: Option<i64>,
}

/// A probe watcher's command, condition and schedule. Its current run (if any) is the watcher's
/// `process_operation_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredProbe {
    /// The first run's `process.execute` operation, whose job every later run repeats.
    pub job_operation_id: OperationId,
    pub interval_ms: i64,
    /// The exit code the condition requires (0 when neither this nor a pattern is set).
    pub exit_code: Option<i64>,
    /// A regular expression the run's output must match.
    pub output_pattern: Option<String>,
    /// Completed runs.
    pub runs: i64,
    /// When the next run starts; `None` while a run is in flight.
    pub next_run_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEventState {
    /// Recorded; not yet in the session's input.
    Pending,
    /// In the session's input; not yet acknowledged by a completed turn.
    Delivered,
    /// A turn that carried it completed.
    Acknowledged,
}
impl WatchEventState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Delivered => "Delivered",
            Self::Acknowledged => "Acknowledged",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "Pending" => Self::Pending,
            "Delivered" => Self::Delivered,
            "Acknowledged" => Self::Acknowledged,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredWatchEvent {
    pub id: Id,
    pub watcher_id: Id,
    pub session_id: Id,
    pub project_id: Option<ProjectId>,
    pub state: WatchEventState,
    /// The host-only message body delivered to the session.
    pub payload: Value,
    /// Transcript sequence of the delivered message.
    pub transcript_sequence: Option<i64>,
    /// The in-flight turn that carries it, until that turn ends.
    pub carried_turn_id: Option<Id>,
    pub created_at_ms: i64,
    pub delivered_at_ms: Option<i64>,
    pub acknowledged_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatcherFilter {
    Session(Id),
    /// Watchers in this project (`None`: watchers outside any project).
    Project(Option<ProjectId>),
}

pub trait WatcherRead {
    fn watcher(&self, id: &Id) -> Result<Option<StoredWatcher>, StorageError>;
    fn watcher_for_process(
        &self,
        operation: &OperationId,
    ) -> Result<Option<StoredWatcher>, StorageError>;
    /// Newest first (descending creation time, then ID), strictly after the exclusive
    /// `before` position `(created_at_ms, id)`; `phases` empty means every phase; limit is
    /// `1..=MAX_PAGE`.
    fn watchers(
        &self,
        filter: &WatcherFilter,
        phases: &[WatcherPhase],
        before: Option<(i64, &Id)>,
        limit: u16,
    ) -> Result<Vec<StoredWatcher>, StorageError>;
    /// Pending (undelivered) events of every session, after `after` in ID order.
    fn pending_watch_events(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredWatchEvent>, StorageError>;
    /// Open (Active or Stopping) watchers, oldest first, after `after`.
    fn open_watchers(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredWatcher>, StorageError>;
    fn watch_event(&self, id: &Id) -> Result<Option<StoredWatchEvent>, StorageError>;
    fn watch_event_for_watcher(
        &self,
        watcher: &Id,
    ) -> Result<Option<StoredWatchEvent>, StorageError>;
    /// A session's events in these states, in recording order.
    fn watch_events(
        &self,
        session: &Id,
        states: &[WatchEventState],
        limit: u16,
    ) -> Result<Vec<StoredWatchEvent>, StorageError>;
}

pub trait WatcherRepository: WatcherRead {
    fn insert_watcher(&mut self, watcher: &StoredWatcher) -> Result<(), StorageError>;
    /// Replaces the watcher when its stored revision is `watcher.revision - 1`; otherwise
    /// `LeaseLost`.
    fn update_watcher(&mut self, watcher: &StoredWatcher) -> Result<(), StorageError>;
    /// One event per watcher.
    fn insert_watch_event(&mut self, event: &StoredWatchEvent) -> Result<(), StorageError>;
    /// Pending → Delivered at `transcript_sequence`. `false` when it was not Pending.
    fn deliver_watch_event(
        &mut self,
        id: &Id,
        transcript_sequence: i64,
        at_ms: i64,
    ) -> Result<bool, StorageError>;
    /// Marks the session's delivered, unacknowledged events as carried by `turn` (replacing a
    /// carry left by an ended turn). Returns how many it carries.
    fn carry_watch_events(&mut self, session: &Id, turn: &Id) -> Result<u64, StorageError>;
    /// Ends `turn`'s carry: a completed turn acknowledges its events; any other end leaves them
    /// delivered for the next turn. Returns how many it changed.
    fn release_watch_events(
        &mut self,
        session: &Id,
        turn: &Id,
        completed: bool,
        at_ms: i64,
    ) -> Result<u64, StorageError>;
}
