//! Revision-bound convention indexing. The service owns shutdown-fenced publication and
//! cancellable embedding execution; chunking and ranking remain pure shared source rules.
pub mod chunker;
pub mod embedding;
pub mod preparation;
pub mod rank;

use crate::{
    ports::{
        Clock, IdSource,
        storage::{ApplicationStore, StorageError, WriteOptions, WriteTransaction, run_write},
    },
    services::artifacts::ArtifactService,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

pub struct IndexService {
    pub(crate) store: Arc<dyn ApplicationStore>,
    pub(crate) artifacts: Arc<ArtifactService>,
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) embeddings: RwLock<Option<Arc<dyn embedding::ConventionEmbeddings>>>,
    closed: Arc<RwLock<bool>>,
    active: Mutex<BTreeMap<u64, crate::ports::storage::Cancellation>>,
    next_active: AtomicU64,
}
impl IndexService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            artifacts,
            ids,
            clock,
            embeddings: RwLock::new(None),
            closed: Arc::new(RwLock::new(false)),
            active: Mutex::new(BTreeMap::new()),
            next_active: AtomicU64::new(0),
        }
    }
    /// Native composition installs only an actual qualified executor before exposing the gateway.
    pub fn set_embeddings(&self, executor: Option<Arc<dyn embedding::ConventionEmbeddings>>) {
        *self.embeddings.write().unwrap_or_else(|p| p.into_inner()) = executor;
    }
    /// Stops new publication and drains admitted local metadata units. No provider/network
    /// call is ever made while this guard is held.
    pub fn stop(&self) {
        *self.closed.write().unwrap_or_else(|p| p.into_inner()) = true;
        for cancellation in self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            cancellation.cancel();
        }
    }
    pub fn is_stopped(&self) -> bool {
        *self.closed.read().unwrap_or_else(|p| p.into_inner())
    }
    pub(crate) fn publication<T>(
        &self,
        work: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let closed = self.closed.read().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(StorageError::Cancelled);
        }
        let result = work();
        drop(closed);
        result
    }
    pub(crate) fn write<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        self.publication(|| run_write(self.store.as_ref(), WriteOptions::default(), work))
    }
}

struct Active<'a> {
    service: &'a IndexService,
    id: u64,
}
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.service
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}
impl IndexService {
    pub(crate) fn embed_batch(
        &self,
        context: &crate::context::InvocationContext,
        request: embedding::EmbeddingRequest,
    ) -> Result<embedding::EmbeddedBatch, embedding::EmbeddingFailure> {
        let guard = {
            let closed = self.closed.read().unwrap_or_else(|p| p.into_inner());
            if *closed {
                return Err(embedding::EmbeddingFailure::Cancelled);
            }
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            if active.len() >= 128 {
                return Err(embedding::EmbeddingFailure::Unavailable(
                    "Embedding service is at capacity".into(),
                ));
            }
            let id = self.next_active.fetch_add(1, Ordering::Relaxed);
            active.insert(id, request.cancellation.clone());
            Active { service: self, id }
        };
        let executor = self
            .embeddings
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| {
                embedding::EmbeddingFailure::Unavailable("NoConfiguredEmbeddingRoute".into())
            })?;
        let result = executor.embed(context, request);
        drop(guard);
        if self.is_stopped() {
            return Err(embedding::EmbeddingFailure::Cancelled);
        }
        result
    }
}
