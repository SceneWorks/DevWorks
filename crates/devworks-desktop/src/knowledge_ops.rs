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
    /// A Knowledge write (record, revise, relate, tag, lifecycle change, deletion).
    KnowledgeWrite,
    /// A graph read (bundle, entity edges, subgraph, graph query, frontier step) of this
    /// generation; a reply of an older one is ignored.
    KnowledgeGraph(u64),
    /// Publishing a summary of the graph shown (`knowledge.render_summary`), by generation.
    KnowledgeSummary(u64),
    /// Reading a published summary's text, by generation.
    KnowledgeSummaryText(u64),
    /// The receipt of the last recorded write (write ID).
    KnowledgeWriteStatus(String),
}
screen_slot!(KnowledgeSlot, Knowledge);
