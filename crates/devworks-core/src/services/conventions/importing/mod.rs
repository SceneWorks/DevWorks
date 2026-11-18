//! Durable portable imports: private staged entries become public in one fenced transaction.
mod commands;
mod maintenance;
pub(crate) mod parser;
mod status;
mod worker;
use super::indexing::{IndexService, lifecycle::PublicationFence};
use crate::{
    ports::{Clock, IdSource, storage::*},
    services::artifacts::ArtifactService,
};
pub use commands::{EFFECT_CONSUMER, PreparedImport};
pub use maintenance::ImportCleanup;
use std::sync::Arc;
pub struct ImportService {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    index: Option<Arc<IndexService>>,
    closed: Arc<PublicationFence>,
}
impl ImportService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        _ids: Arc<dyn IdSource>,
        _clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            artifacts,
            index: None,
            closed: Arc::new(PublicationFence::default()),
        }
    }
    pub fn with_index(mut self, index: Arc<IndexService>) -> Self {
        self.index = Some(index);
        self
    }
    pub fn stop(&self) {
        self.closed.begin_close();
    }
    pub fn drain(&self) {
        self.closed.drain();
    }
    pub fn is_stopped(&self) -> bool {
        self.closed.is_closed()
    }
    fn write<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        self.closed
            .write(self.store.as_ref(), WriteOptions::host_state(), work)
    }
}
