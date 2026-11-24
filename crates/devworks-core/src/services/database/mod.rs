mod commands;
mod executor;
mod mutation;
mod mysql_compat;
pub mod parameters;
mod read;
mod rules;
pub mod sensitive;
pub mod sql_server_view;
pub use crate::ports::database::statement;

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
