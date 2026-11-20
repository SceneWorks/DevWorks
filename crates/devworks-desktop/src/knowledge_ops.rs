//! The requests the Knowledge screen waits for. The `knowledge.*` frames are in
//! [`crate::knowledge_screen`].
use crate::backend::screen_slot;

/// The Knowledge requests. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum KnowledgeSlot {
    Documents(crate::documents_ops::DocumentSlot),
    KnowledgeList(u64),
    /// One Knowledge entry with its evidence and history (claim ID).
    KnowledgeRead(String),
    /// Knowledge counts and retrieval mode.
    KnowledgeDiagnostics,
    /// The preview of a delete by source (the source).
    KnowledgePreview(String),
    /// Storing an import file as the user's artifact.
    KnowledgeImportSave,
    /// The exact preview of an import.
    KnowledgeImportPreview,
    /// The confirmed import.
    KnowledgeImport,
    /// A Knowledge write (record, revise, tag, lifecycle change, deletion).
    KnowledgeWrite,
}
screen_slot!(KnowledgeSlot, Knowledge);
