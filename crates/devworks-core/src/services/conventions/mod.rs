//! Versioned authored conventions: one scoped owner for native UI, agents and scheduled
//! indexing. Convention data never grants authority or replaces explicit user instructions.
pub(crate) mod access;
pub(crate) use hierarchy::parent_path;
mod commands;
mod handlers;
mod hierarchy;
pub mod importing;
pub mod indexing;
mod manifest;
pub mod model;
mod operation;
mod passages;
mod paths;
pub(crate) use paths::path_source;
mod preparation;
mod queries;

use crate::ports::storage::ApplicationStore;
use crate::ports::{Clock, IdSource};
use crate::services::artifacts::ArtifactService;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// One actual catalog owner. Native composition registers handlers only once the complete
/// service is composed; the index owner is shared by reads, commands and durable workers.
pub struct ConventionService {
    pub(crate) artifacts: Arc<ArtifactService>,
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) index: Arc<indexing::IndexService>,
    pub(crate) imports: Arc<importing::ImportService>,
    pub(crate) stopping: AtomicBool,
}
impl ConventionService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        artifacts: Arc<ArtifactService>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let index = Arc::new(indexing::IndexService::new(
            store.clone(),
            artifacts.clone(),
            ids.clone(),
            clock.clone(),
        ));
        let imports = Arc::new(
            importing::ImportService::new(
                store.clone(),
                artifacts.clone(),
                ids.clone(),
                clock.clone(),
            )
            .with_index(index.clone()),
        );
        Self {
            artifacts,
            ids,
            clock,
            index,
            imports,
            stopping: AtomicBool::new(false),
        }
    }
    pub fn indexing(&self) -> Arc<indexing::IndexService> {
        self.index.clone()
    }
    pub fn imports(&self) -> Arc<importing::ImportService> {
        self.imports.clone()
    }
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.imports.stop();
        self.index.stop();
        self.imports.drain();
    }
    pub fn is_stopped(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

pub const OPERATIONS: [&str; 13] = [
    "conventions.list",
    "conventions.read",
    "conventions.edit",
    "conventions.version",
    "conventions.applicability",
    "conventions.retire",
    "conventions.index",
    "conventions.import",
    "conventions.delete",
    "conventions.move",
    "conventions.resources",
    "conventions.delete_resource",
    "conventions.search",
];
