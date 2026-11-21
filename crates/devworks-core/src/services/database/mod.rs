mod commands;
mod executor;
mod mutation;
mod mysql_compat;
pub mod parameters;
mod read;
mod rules;
pub mod sensitive;
pub mod statement;

pub use executor::{DataExecutor, DatabaseReadRunner};
use executor::{Selected, check_fences};
pub use mutation::EFFECT_CONSUMER;

use crate::gateway::AdmittedQuery;
use crate::ports::database::data::{DataFailure, FailureKind};
use crate::ports::storage::StorageError;
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault};
use crate::services::kernel::{Refusal, query_fault};

fn fault(query: &AdmittedQuery, code: DomainErrorCode) -> DomainFault {
    Refusal::new(code, "database_data", None).fault(CorrelationId(query.query_id().0.clone()))
}

fn store_fault(query: &AdmittedQuery, error: StorageError) -> DomainFault {
    query_fault(
        error.into(),
        "database_data",
        CorrelationId(query.query_id().0.clone()),
    )
}

fn native_storage(error: StorageError) -> DataFailure {
    let failure = match error {
        StorageError::Cancelled => FailureKind::Cancelled,
        StorageError::ScopeDenied => FailureKind::PermissionDenied,
        StorageError::RevisionConflict(_) => FailureKind::StaleSchema,
        _ => FailureKind::Unavailable,
    };
    DataFailure::Refused(failure)
}

fn native_failure(error: DataFailure) -> DomainErrorCode {
    let reason = match error {
        DataFailure::Refused(reason)
        | DataFailure::RolledBack(reason)
        | DataFailure::Partial { reason, .. } => reason,
        DataFailure::Unknown { .. } => return DomainErrorCode::OutcomeUnknown,
    };
    match reason {
        FailureKind::InvalidTarget | FailureKind::UnsupportedProfile => {
            DomainErrorCode::UnsupportedTarget
        }
        FailureKind::UnsupportedStatement
        | FailureKind::UnsupportedValue
        | FailureKind::ResultLimit => DomainErrorCode::InvalidArgument,
        FailureKind::PermissionDenied => DomainErrorCode::PermissionDenied,
        FailureKind::StaleSchema => DomainErrorCode::SourceChanged,
        FailureKind::Constraint => DomainErrorCode::Conflict,
        FailureKind::Cancelled | FailureKind::Deadline => DomainErrorCode::Cancelled,
        FailureKind::Unavailable => DomainErrorCode::ExternalFailure,
    }
}

/// Source permission is checked before a command's receipt lookup, including replay-only grants.
pub(crate) fn mutation_fence(
    snapshot: &dyn crate::ports::storage::ReadSnapshot,
    ctx: &crate::context::InvocationContext,
    input: &crate::operations::OperationInput,
) -> Result<Option<crate::ports::AuthorityFence>, StorageError> {
    use crate::operations::{OperationInput, database::Input};
    use crate::services::schemas::data;
    use statement::DataMode;
    let (id, mode, scope) = match input {
        OperationInput::Database(Input::ExecuteWrite(input)) => {
            (&input.connection_id, DataMode::Write, input.scope.as_ref())
        }
        OperationInput::Database(Input::ApplyMigration(input)) => (
            &input.connection_id,
            DataMode::Migration,
            input.scope.as_ref(),
        ),
        OperationInput::Database(Input::DirectSchemaChange(input)) => {
            (&input.connection_id, DataMode::Schema, input.scope.as_ref())
        }
        _ => return Ok(None),
    };
    let ctx = executor::scope_context(ctx, scope)?;
    let revision = snapshot
        .schemas()
        .schema_source(id)?
        .ok_or(StorageError::ScopeDenied)?
        .connection
        .revision;
    data::source(snapshot, &ctx, id, &revision, mode)?;
    Ok(Some(crate::ports::AuthorityFence::DatabaseSource {
        id: id.clone(),
        revision: revision.get(),
    }))
}
