//! Typed graph request targets and the shared cursor lease header.
use crate::kernel::failure::Failure;
use crate::operations::{self, OperationInput};
use crate::ports::storage::StorageError;
use crate::protocol::{
    Id,
    data::{GraphQuery, GraphReadRequest},
    records::CodeSourceSelection,
};

pub(crate) fn graph_input(input: &OperationInput) -> Option<&GraphReadRequest> {
    use operations::code::Input::*;
    match input {
        OperationInput::Code(
            Search(i)
            | Architecture(i)
            | Dependency(i)
            | Impact(i)
            | Source(i)
            | Traverse(i)
            | GraphScopeStatus(i)
            | Overview(i)
            | NodesByFile(i)
            | NodeDetail(i)
            | FileImpact(i)
            | GraphSchema(i)
            | CallPath(i)
            | DataLineage(i)
            | Consumers(i)
            | Publishers(i)
            | Snippet(i)
            | RepositoryDependencies(i),
        ) => Some(i),
        _ => None,
    }
}

pub(crate) fn target_repository(query: &GraphQuery) -> Option<&Id> {
    match query {
        GraphQuery::Search { selection, .. } => selection.as_ref()?.repository_id.as_ref(),
        GraphQuery::Source {
            selection: Some(CodeSourceSelection::File { repository_id, .. }),
            ..
        }
        | GraphQuery::RepositoryDependencies { repository_id } => Some(repository_id),
        GraphQuery::Overview { repository_id } => repository_id.as_ref(),
        GraphQuery::FileImpact {
            repository_id,
            selection,
            ..
        }
        | GraphQuery::NodesByFile {
            repository_id,
            selection,
            ..
        } => selection
            .as_ref()
            .map(|target| &target.repository_id)
            .or(repository_id.as_ref()),
        _ => None,
    }
}

/// The untrusted header only locates a lease. Ownership/current authority and the full
/// binding are checked before any result can be published; it never selects another view.
pub(crate) fn cursor_pin(cursor: &str) -> Result<Option<Id>, Failure> {
    if cursor.starts_with("m1.") {
        return Ok(None);
    }
    let mut parts = cursor.splitn(3, '.');
    if parts.next() != Some("g1") {
        return Err(StorageError::CursorExpired.into());
    }
    let pin = parts.next().ok_or(StorageError::CursorExpired)?;
    if parts.next().is_none() {
        return Err(StorageError::CursorExpired.into());
    }
    Ok(Some(
        Id::new(pin.to_owned()).map_err(|_| StorageError::CursorExpired)?,
    ))
}

pub(crate) fn is_graph_read(input: &OperationInput) -> bool {
    graph_input(input).is_some()
        || matches!(
            input,
            OperationInput::Repositories(operations::repositories::Input::Nodes(_))
        )
}
