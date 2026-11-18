//! The requests the permanent project purge review waits for. The frames are the
//! `projects.*purge*` operations ([`crate::ops::purge_prepare`] and its siblings).
use crate::backend::screen_slot;

/// The purge review's requests. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PurgeSlot {
    PurgePrepare,
    PurgePage,
    PurgeConfirm,
    PurgeStatus,
}
screen_slot!(PurgeSlot, Purge);
