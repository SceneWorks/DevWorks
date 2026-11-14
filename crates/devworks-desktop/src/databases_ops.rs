//! Database source frames: the shared `schemas.*` operations at wire revision 5.
use crate::{
    backend::{Backend, screen_slot},
    ops,
};
use serde_json::Value;

/// The Databases requests the screen waits for. The names are persisted (see
/// [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DatabasesSlot {
    /// A source inventory page (list sequence, appended to the shown page).
    DatabaseList(u64, bool),
    /// A schema object page (read sequence, appended).
    DatabaseObjects(u64, bool),
    /// A relationship page (read sequence, appended).
    DatabaseRelations(u64, bool),
    /// Staging a source's password before its configure command.
    DatabasePrepare,
    /// Preparing a certificate file before its `artifacts.save`.
    DatabaseCaPrepare,
    /// The certificate's `artifacts.save`.
    DatabaseCa,
    /// A source command (its operation).
    DatabaseCommand(String),
}
screen_slot!(DatabasesSlot, Databases);

pub fn query(b: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::query_frame(b, op, body), 5)
}
pub fn command(b: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::command_frame(b, op, body), 5)
}
