//! Vector retrieval over Knowledge (RAG-04/07/08): the seam a sqlite-vec index plugs into.
//!
//! Every embedding (and query embedding) is inference and goes through the internal router, so a
//! vector index is usable only when an embedding route exists and its space is indexed. Until
//! then retrieval is lexical only (SQLite FTS5) and says so: no caller may report a vector or
//! hybrid result that did not run. Scope filtering always happens before candidates are
//! materialized, in the store, for lexical and vector candidates alike.

/// What retrieval can run right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetrievalMode {
    /// Lexical and vector candidates over one compatible, indexed embedding space.
    Hybrid { embedding_space: String },
    /// Lexical (FTS5) only, with the reason vector retrieval is unavailable.
    LexicalOnly { reason: &'static str },
}

/// A Knowledge vector index.
pub trait VectorIndex: Send + Sync {
    /// The mode retrieval runs in now.
    fn mode(&self) -> RetrievalMode;
}

/// The production index until an embedding route and indexed space exist: lexical only.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoVectorIndex;
impl VectorIndex for NoVectorIndex {
    fn mode(&self) -> RetrievalMode {
        RetrievalMode::LexicalOnly {
            reason: "NoEmbeddingRoute",
        }
    }
}
