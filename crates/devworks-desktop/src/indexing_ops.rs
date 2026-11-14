//! The requests of repository indexing. Their frames are the shared `indexing.*` builders in
//! [`crate::ops`].
use crate::backend::screen_slot;

/// The indexing requests the screen waits for. The names are persisted (see
/// [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IndexingSlot {
    /// A repository's index status (repository ID).
    IndexStatus(String),
    /// Index runs (repository ID, or `*` for the selected scope).
    IndexRuns(String),
    /// An index, rebuild or overlay command of one repository (repository ID).
    IndexCommand(String),
    /// A run cancellation (run ID).
    IndexCancel(String),
    /// An overlay retirement (overlay ID).
    IndexRetire(String),
    /// The section configuration save.
    IndexSourcesSave,
    /// A discovery scan.
    IndexDiscover,
}
screen_slot!(IndexingSlot, Indexing);
