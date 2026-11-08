//! Project settings choose one backend; missing tooling never changes that choice.
use super::backend::Backend;
use crate::ports::storage::{ConfigScope, ConfigurationRead, StorageError};
use crate::protocol::ProjectId;
use crate::settings::model::{current_chain, effective_values};

pub const PREFERRED_RUNTIME: &str = "containers.preferredRuntime";

pub fn selected(
    read: &dyn ConfigurationRead,
    project: Option<&ProjectId>,
) -> Result<Backend, StorageError> {
    let scope = project.map_or(ConfigScope::Workspace, |id| {
        ConfigScope::Project(id.clone())
    });
    let layers = current_chain(read, &scope)?;
    let values = effective_values(&layers);
    let chosen = values
        .get(PREFERRED_RUNTIME)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| StorageError::InvalidRecord("missing preferred container runtime".into()))?;
    Backend::parse(chosen)
        .ok_or_else(|| StorageError::InvalidRecord("unknown preferred container runtime".into()))
}
