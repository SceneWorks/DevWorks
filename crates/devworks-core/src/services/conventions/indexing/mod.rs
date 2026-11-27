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
mod query_vector;
pub mod rank;
pub mod selection;
mod status;

use crate::locks::{LockExt, RwLockExt};
use crate::{
    ports::{
        Clock, IdSource,
        storage::{ApplicationStore, StorageError, WriteOptions, WriteTransaction},
    },
    services::artifacts::ArtifactService,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, RwLock, Weak,
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
    /// Other users of the selected space, told of a selection change in its unit of work.
    selection_listeners: RwLock<Vec<Weak<dyn SelectionListener>>>,
}

/// A service that keeps derived vectors of the selected embedding space (Knowledge claims) and
/// queues their production when the selection changes, inside the selection's unit of work.
pub trait SelectionListener: Send + Sync {
    fn selection_changed(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &crate::context::InvocationContext,
        fences: &[crate::ports::AuthorityFence],
    ) -> Result<(), StorageError>;
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
            selection_listeners: RwLock::new(Vec::new()),
        }
    }
    /// Registers a [`SelectionListener`] (held weakly: the composition owns it).
    pub fn add_selection_listener(&self, listener: Weak<dyn SelectionListener>) {
        self.selection_listeners.write_locked().push(listener);
    }
    pub(crate) fn selection_listeners(&self) -> Vec<Arc<dyn SelectionListener>> {
        self.selection_listeners
            .read_locked()
            .iter()
            .filter_map(Weak::upgrade)
            .collect()
    }
    /// Native composition installs only an actual qualified executor before exposing the gateway.
    pub fn set_embeddings(&self, executor: Option<Arc<dyn embedding::ConventionEmbeddings>>) {
        *self.embeddings.write_locked() = executor;
    }
    /// Stops new publication and drains admitted local metadata units. No provider/network
    /// call is ever made while this guard is held.
    pub fn stop(&self) {
        self.closed.begin_close();
        for cancellation in self.active.locked().values() {
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
        self.write_options(WriteOptions::host_state(), work)
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
        self.closed.write(self.store.as_ref(), options, work)
    }
}

struct Active<'a> {
    service: &'a IndexService,
    id: u64,
}
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.service.active.locked().remove(&self.id);
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
            let mut active = self.active.locked();
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
        let executor = self.embeddings.read_locked().clone().ok_or_else(|| {
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

pub mod cancellation;
