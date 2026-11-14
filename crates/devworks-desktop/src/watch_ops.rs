//! Watcher frames (`watch.list`, `watch.cancel`) and the requests the watcher views wait for.
use crate::{
    backend::{Backend, screen_slot},
    ops::{self, Failure},
};
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::protocol::records::{WatcherPage, WatcherRecord};
use devworks_core::protocol::results::OperationResult;
use serde_json::{Value, json};

/// Watch operations were introduced at schema revision 8.
pub const WATCH_REVISION: u64 = 8;
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
    let mut page = json!({"limit": limit.clamp(1, WATCH_MAX_ROWS)});
    if let Some(cursor) = cursor {
        page["cursor"] = json!(cursor);
    }
    let mut body: Value = json!({"page": page});
    match scope {
        WatchScope::All => {}
        WatchScope::Session(id) => body["sessionId"] = json!(id),
        WatchScope::Feature(id) => body["featureId"] = json!(id),
    }
    ops::at_revision(
        ops::query_frame(backend, "watch.list", body),
        WATCH_REVISION,
    )
}

/// Stops observing `watcher` at the revision shown; `stop_process` also stops what it launched.
pub fn cancel(backend: &dyn Backend, watcher: &WatcherRecord, stop_process: bool) -> Vec<u8> {
    ops::at_revision(
        ops::command_frame(
            backend,
            "watch.cancel",
            json!({
                "watcherId": watcher.id,
                "expectedRevision": watcher.revision,
                "stopProcess": stop_process,
            }),
        ),
        WATCH_REVISION,
    )
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
