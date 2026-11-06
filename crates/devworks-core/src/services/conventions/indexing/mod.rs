//! Revision-bound convention indexing. The service owns shutdown-fenced publication and
//! cancellable embedding execution; chunking and ranking remain pure shared source rules.
pub mod chunker;
pub mod commands;
pub mod configuration;
pub mod embedding;
pub mod lifecycle;
pub mod preparation;
pub mod publisher;
pub mod query;
pub mod rank;
mod status;

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
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) embeddings: RwLock<Option<Arc<dyn embedding::ConventionEmbeddings>>>,
    closed: Arc<lifecycle::PublicationFence>,
    active: Mutex<BTreeMap<u64, crate::ports::storage::Cancellation>>,
    next_active: AtomicU64,
}
impl IndexService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        _ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            artifacts,
            clock,
            embeddings: RwLock::new(None),
            closed: Arc::new(lifecycle::PublicationFence::default()),
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
        self.closed.begin_close();
        for cancellation in self
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            cancellation.cancel();
        }
        self.closed.drain();
    }
    pub fn is_stopped(&self) -> bool {
        self.closed.is_closed()
    }
    pub(crate) fn publication<T>(
        &self,
        work: impl FnOnce() -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        let _phase = self.closed.enter()?;
        work()
    }

    pub(crate) fn write<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        self.write_options(WriteOptions::default(), work)
    }

    pub(crate) fn write_options<T, F>(
        &self,
        options: WriteOptions,
        work: F,
    ) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    {
        let _phase = self.closed.enter()?;
        let closed = self.closed.clone();
        run_write(self.store.as_ref(), options, move |tx| {
            if closed.is_closed() {
                return Err(StorageError::Cancelled);
            }
            work(tx)
        })
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
        self.embedding_batch(context, request, false)
    }
    pub(crate) fn observe_batch(
        &self,
        context: &crate::context::InvocationContext,
        request: embedding::EmbeddingRequest,
    ) -> Result<embedding::EmbeddedBatch, embedding::EmbeddingFailure> {
        self.embedding_batch(context, request, true)
    }
    fn embedding_batch(
        &self,
        context: &crate::context::InvocationContext,
        request: embedding::EmbeddingRequest,
        observe: bool,
    ) -> Result<embedding::EmbeddedBatch, embedding::EmbeddingFailure> {
        let guard = {
            if self.closed.is_closed() {
                return Err(embedding::EmbeddingFailure::Cancelled);
            }
            let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
            if active.len() >= 128 {
                return Err(embedding::EmbeddingFailure::Unavailable(
                    "Embedding service is at capacity".into(),
                ));
            }
            let id = self.next_active.fetch_add(1, Ordering::Relaxed);
            if self.closed.is_closed() {
                return Err(embedding::EmbeddingFailure::Cancelled);
            }
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
        let result = if observe {
            executor.observe(context, request)
        } else {
            executor.embed(context, request)
        };
        drop(guard);
        if self.is_stopped() {
            return Err(embedding::EmbeddingFailure::Cancelled);
        }
        result
    }
}
