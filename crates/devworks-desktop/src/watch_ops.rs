//! Watcher frames (`watch.list`, `watch.cancel`) and the requests the watcher views wait for.
use crate::frame::{self, Negotiate};
use crate::{
    backend::{Backend, screen_slot},
    ops::{self, Failure},
};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::operations::{OperationInput as In, watch};
use devworks_core::protocol::RootId;
use devworks_core::protocol::records::{WatcherPage, WatcherRecord};
use devworks_core::protocol::results::OperationResult;

/// Watchers one page request asks for.
pub const WATCH_PAGE: u16 = 50;

/// Which watcher list a request is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum WatchView {
    /// Every watcher (the Watchers screen).
    All,
    /// The selected conversation's watchers (the chat sidebar's Current tab).
    Session,
    /// The selected feature's watchers (the feature detail).
    Feature,
}

/// The watcher requests the views wait for. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum WatchSlot {
    /// A page of one view (list generation, appended to the shown page).
    WatcherList(WatchView, u64, bool),
    /// `watch.cancel` of one watcher (its ID).
    WatcherCancel(String),
}
screen_slot!(WatchSlot, Watch);

/// What a list shows: every watcher, one session's or one feature's sessions'.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchScope {
    All,
    Session(String),
    Feature(String),
}

/// Most watchers one read asks for (the protocol's page bound).
pub const WATCH_MAX_ROWS: usize = 500;

/// Up to `limit` watchers in `scope` after `cursor`, newest first.
pub fn list(
    backend: &dyn Backend,
    scope: &WatchScope,
    cursor: Option<&str>,
    limit: usize,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let (session_id, feature_id) = match scope {
            WatchScope::All => (None, None),
            WatchScope::Session(id) => (Some(frame::id(id)?), None),
            WatchScope::Feature(id) => (None, Some(RootId(frame::id(id)?))),
        };
        Some(In::Watch(watch::Input::List(watch::ListInput {
            session_id,
            feature_id,
            states: None,
            page: frame::page(limit.clamp(1, WATCH_MAX_ROWS) as u32, cursor)?,
        })))
    })
}

/// Stops observing `watcher` at the revision shown; `stop_process` also stops what it launched.
pub fn cancel(backend: &dyn Backend, watcher: &WatcherRecord, stop_process: bool) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Watch(watch::Input::Cancel(watch::CancelInput {
            watcher_id: watcher.id.clone(),
            expected_revision: watcher.revision.clone(),
            stop_process,
        })))
    })
}

pub fn page(outcome: QueryOutcome) -> Result<WatcherPage, Failure> {
    match ops::query_result(outcome)? {
        (OperationResult::WatchList { value }, _) => Ok(value),
        (other, _) => Err(ops::unexpected("watch.list", &other)),
    }
}

pub fn cancelled(outcome: CommandOutcome) -> Result<WatcherRecord, Failure> {
    match ops::command_result(outcome)? {
        OperationResult::WatchCancel { value } => Ok(value),
        other => Err(ops::unexpected("watch.cancel", &other)),
    }
}
