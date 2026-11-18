//! The requests the permanent project purge review waits for, and the frame builder that pins
//! them to the purge schema revision. The frames are the `projects.*purge*` operations.
use crate::backend::{Backend, screen_slot};
use crate::ops;
use serde_json::{Value, json};

/// The purge review's requests. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PurgeSlot {
    PurgePrepare,
    PurgePage,
    PurgeConfirm,
    PurgeStatus,
}
screen_slot!(PurgeSlot, Purge);

/// A purge operation's frame at schema revision 6.
pub(crate) fn purge_frame(b: &dyn Backend, op: &str, body: Value, query: bool) -> Vec<u8> {
    let bytes = if query {
        ops::query_frame(b, op, body)
    } else {
        ops::command_frame(b, op, body)
    };
    let mut frame: Value = serde_json::from_slice(&bytes).expect("native frame");
    frame["schemaRevision"] = json!(6);
    serde_json::to_vec(&frame).expect("native frame")
}
