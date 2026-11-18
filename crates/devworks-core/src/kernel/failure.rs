//! The typed refusal and failure every core use case returns, and the one mapping from a storage
//! error to its wire fault code and retry advice. The gateway, the services' query faults and
//! the web store all answer a storage error through [`storage_fault`].
use crate::ports::storage::{StorageError, Unavailable};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, HoldId, Id, RetryAdvice};

/// A typed refusal: a durable `Refused` decision for commands, a fault for queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: DomainErrorCode,
    pub retry: RetryAdvice,
    pub target_kind: &'static str,
    pub target_id: Option<Id>,
    /// Holds that block the request (configuration writes name them).
    pub blockers: Vec<HoldId>,
}
impl Refusal {
    pub fn new(code: DomainErrorCode, target_kind: &'static str, target_id: Option<Id>) -> Self {
        Self {
            code,
            retry: RetryAdvice::Never,
            target_kind,
            target_id,
            blockers: Vec::new(),
        }
    }
    pub fn retry(mut self, retry: RetryAdvice) -> Self {
        self.retry = retry;
        self
    }
    pub fn fault(self, correlation_id: CorrelationId) -> DomainFault {
        DomainFault {
            code: self.code,
            retry: self.retry,
            correlation_id,
            target_id: self.target_id,
            target_kind: Some(BoundedText(self.target_kind.to_owned())),
            revision_dimension: None,
            expected_revision: None,
            current_revision: None,
            blocker_ids: BoundedList(self.blockers),
            diagnostic_id: None,
        }
    }
    /// The query fault of a storage error, with `target_kind` as its target.
    pub fn of_storage(error: &StorageError, target_kind: &'static str) -> Self {
        let (code, retry) = storage_fault(error);
        Self::new(code, target_kind, None).retry(retry)
    }
}

/// A use-case failure: a typed refusal or a storage error. A revision conflict becomes the
/// gateway's durable `RevisionConflict`; any other storage error rolls a command back.
#[derive(Clone, Debug)]
pub enum Failure {
    Refused(Refusal),
    Storage(StorageError),
}
impl From<StorageError> for Failure {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<Refusal> for Failure {
    fn from(r: Refusal) -> Self {
        Self::Refused(r)
    }
}
impl Failure {
    /// The query fault of this failure: a refusal as it is, a storage error through
    /// [`storage_fault`] with `target_kind` as its target.
    pub fn query_fault(self, target_kind: &'static str, correlation: CorrelationId) -> DomainFault {
        match self {
            Self::Refused(r) => r.fault(correlation),
            Self::Storage(e) => Refusal::of_storage(&e, target_kind).fault(correlation),
        }
    }
}

impl Failure {
    /// The storage error of this failure, for a host-internal caller that has no refusal to
    /// answer with (a host notice, a long-poll driver). A refused rule keeps its kind: a denial
    /// is `ScopeDenied`, a missing row `NotFound`, an expired cursor `CursorExpired`, and any
    /// other rule an `InvalidRecord` naming its target.
    pub fn into_storage(self) -> StorageError {
        match self {
            Failure::Storage(error) => error,
            Failure::Refused(r) => match r.code {
                DomainErrorCode::PermissionDenied => StorageError::ScopeDenied,
                DomainErrorCode::CursorExpired => StorageError::CursorExpired,
                DomainErrorCode::NotFound => StorageError::NotFound {
                    kind: r.target_kind,
                    id: r
                        .target_id
                        .map(|id| id.as_str().to_owned())
                        .unwrap_or_default(),
                },
                code => StorageError::InvalidRecord(format!("{code:?}: {}", r.target_kind)),
            },
        }
    }
}

/// The wire fault code and retry advice of a storage error. Only what a later identical request
/// can outlive retries: an expired cursor needs a new request from a refreshed position, and a
/// cancellation, an unpublished view, a schema this build cannot use, a request that breaks a
/// persistence invariant and a stored row that cannot be read all repeat the same way.
pub fn storage_fault(e: &StorageError) -> (DomainErrorCode, RetryAdvice) {
    match e {
        StorageError::InvalidRecord(_)
        | StorageError::StorageUnavailable {
            cause: Unavailable::Corrupt,
            ..
        } => (DomainErrorCode::StorageUnavailable, RetryAdvice::Never),
        StorageError::IncompatibleSchema(_) => {
            (DomainErrorCode::SchemaMismatch, RetryAdvice::Never)
        }
        StorageError::ViewUnavailable => (DomainErrorCode::ViewUnavailable, RetryAdvice::Never),
        StorageError::CursorExpired => (
            DomainErrorCode::CursorExpired,
            RetryAdvice::RefreshAndNewRequest,
        ),
        StorageError::Cancelled => (DomainErrorCode::Cancelled, RetryAdvice::Never),
        StorageError::RevisionConflict(_)
        | StorageError::NotFound { .. }
        | StorageError::ScopeDenied
        | StorageError::Busy
        | StorageError::LeaseLost { .. }
        | StorageError::Duplicate { .. }
        | StorageError::StorageUnavailable { .. }
        | StorageError::CommitUnknown => (
            DomainErrorCode::StorageUnavailable,
            RetryAdvice::SameCommand,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Revision, RevisionDimension};

    /// One error of every variant. The match has no wildcard arm, so a new variant does not
    /// compile until it is listed here and given its row in the table below.
    fn every_variant() -> Vec<StorageError> {
        let all = vec![
            StorageError::conflict(
                RevisionDimension::Store,
                "row",
                Id::new("row-1").unwrap(),
                Revision::new(1).unwrap(),
                Revision::new(2).unwrap(),
            ),
            StorageError::NotFound {
                kind: "row",
                id: "row-1".into(),
            },
            StorageError::ScopeDenied,
            StorageError::Busy,
            StorageError::Cancelled,
            StorageError::ViewUnavailable,
            StorageError::CursorExpired,
            StorageError::LeaseLost {
                kind: "lease",
                id: "lease-1".into(),
            },
            StorageError::Duplicate {
                kind: "row",
                id: "row-1".into(),
            },
            StorageError::InvalidRecord("bad row".into()),
            StorageError::IncompatibleSchema("newer".into()),
            StorageError::unavailable(Unavailable::Io, "disk"),
            StorageError::unavailable(Unavailable::Corrupt, "unreadable row"),
            StorageError::CommitUnknown,
        ];
        for error in &all {
            match error {
                StorageError::RevisionConflict(_)
                | StorageError::NotFound { .. }
                | StorageError::ScopeDenied
                | StorageError::Busy
                | StorageError::Cancelled
                | StorageError::ViewUnavailable
                | StorageError::CursorExpired
                | StorageError::LeaseLost { .. }
                | StorageError::Duplicate { .. }
                | StorageError::InvalidRecord(_)
                | StorageError::IncompatibleSchema(_)
                | StorageError::StorageUnavailable { .. }
                | StorageError::CommitUnknown => {}
            }
        }
        all
    }

    #[test]
    fn every_storage_error_has_one_fault_code_and_retry_advice() {
        use DomainErrorCode as C;
        use RetryAdvice as R;
        let table = [
            (C::StorageUnavailable, R::SameCommand),
            (C::StorageUnavailable, R::SameCommand),
            (C::StorageUnavailable, R::SameCommand),
            (C::StorageUnavailable, R::SameCommand),
            (C::Cancelled, R::Never),
            (C::ViewUnavailable, R::Never),
            (C::CursorExpired, R::RefreshAndNewRequest),
            (C::StorageUnavailable, R::SameCommand),
            (C::StorageUnavailable, R::SameCommand),
            (C::StorageUnavailable, R::Never),
            (C::SchemaMismatch, R::Never),
            (C::StorageUnavailable, R::SameCommand),
            (C::StorageUnavailable, R::Never),
            (C::StorageUnavailable, R::SameCommand),
        ];
        let all = every_variant();
        assert_eq!(all.len(), table.len());
        for (error, expected) in all.iter().zip(table) {
            assert_eq!(storage_fault(error), expected, "{error:?}");
            // Every mapped fault is a valid wire fault.
            let fault =
                Refusal::of_storage(error, "row").fault(CorrelationId(Id::new("c").unwrap()));
            crate::protocol::support::Check::check(&fault).unwrap();
        }
    }
}
