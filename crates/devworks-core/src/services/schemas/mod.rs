//! Shared metadata source owner. Source connections are selected under current project authority;
//! immutable metadata reads never open the target database or execute caller SQL.
mod access;
mod commands;
mod credentials;
mod queries;

use crate::ports::{Clock, IdSource};
use std::sync::Arc;

pub struct SchemaService {
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) clock: Arc<dyn Clock>,
    credentials: Option<credentials::Credentials>,
}
impl SchemaService {
    pub fn new(ids: Arc<dyn IdSource>, clock: Arc<dyn Clock>) -> Self {
        Self {
            ids,
            clock,
            credentials: None,
        }
    }
}
