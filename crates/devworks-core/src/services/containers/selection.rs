//! Project settings choose one backend; missing tooling never changes that choice.
use super::backend::Backend;
use crate::ports::storage::{ConfigScope, ConfigurationRead, StorageError};
use crate::protocol::{Id, ProjectId};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionError {
    UnknownEngine,
    NotSelected,
}

/// A caller's engine ID is an assertion of the effective project choice, not a fallback hint.
pub fn require_selected(
    read: &dyn ConfigurationRead,
    project: &ProjectId,
    requested: &Id,
) -> Result<Result<Backend, SelectionError>, StorageError> {
    let Some(engine) = Backend::parse(requested.as_str()) else {
        return Ok(Err(SelectionError::UnknownEngine));
    };
    if engine != selected(read, Some(project))? {
        return Ok(Err(SelectionError::NotSelected));
    }
    Ok(Ok(engine))
}
