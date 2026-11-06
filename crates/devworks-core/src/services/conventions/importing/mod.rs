//! Durable portable imports: private staged entries become public in one fenced transaction.
pub(crate) mod parser;
mod commands;
mod worker;
mod status;
use crate::{ports::{storage::*, Clock, IdSource}, services::artifacts::ArtifactService};
use std::sync::Arc;
use super::indexing::{IndexService,lifecycle::PublicationFence};
pub use commands::{PreparedImport, EFFECT_CONSUMER};
pub use worker::ImportOutcome;
pub struct ImportService {
    store: Arc<dyn ApplicationStore>, artifacts:Arc<ArtifactService>,
    ids:Arc<dyn IdSource>, clock:Arc<dyn Clock>, index:Option<Arc<IndexService>>,
    closed:Arc<PublicationFence>,
}
impl ImportService {
    pub fn new(store:Arc<dyn ApplicationStore>,artifacts:Arc<ArtifactService>,ids:Arc<dyn IdSource>,clock:Arc<dyn Clock>)->Self {
        Self {store,artifacts,ids,clock,index:None,closed:Arc::new(PublicationFence::default())}
    }
    pub fn with_index(mut self,index:Arc<IndexService>)->Self {self.index=Some(index);self}
    pub fn stop(&self) {self.closed.begin_close();}
    pub fn drain(&self) {self.closed.drain();}
    pub fn is_stopped(&self)->bool {self.closed.is_closed()}
    fn write<T:Send+'static>(&self,work:impl FnOnce(&mut dyn WriteTransaction)->Result<T,StorageError>+Send+'static)->Result<T,StorageError> {
        let _phase=self.closed.enter()?; let closed=self.closed.clone();
        run_write(self.store.as_ref(),WriteOptions::default(),move|tx| {if closed.is_closed(){return Err(StorageError::Cancelled)} work(tx)})
    }
}
