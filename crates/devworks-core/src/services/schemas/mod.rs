//! Shared metadata source owner. Source connections are selected under current project authority;
//! immutable metadata reads never open the target database or execute caller SQL.
mod access;
mod certificates;
mod commands;
mod credentials;
pub mod indexing;
mod pins;
mod queries;

use crate::ports::{Clock, IdSource};
use std::sync::Arc;

pub struct SchemaService {
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) clock: Arc<dyn Clock>,
    credentials: Option<credentials::Credentials>,
    runtime: Option<Arc<indexing::Runtime>>,
}
impl SchemaService {
    pub fn new(ids: Arc<dyn IdSource>, clock: Arc<dyn Clock>) -> Self {
        Self {
            ids,
            clock,
            credentials: None,
            runtime: None,
        }
    }
}
