//! Why a native agent turn stopped, and whether its activation repeats with backoff or stops
//! with one blocker.
use devworks_core::ports::storage::{StorageError, Unavailable};
use devworks_core::protocol::RetryAdvice;
use devworks_core::providers::decode::DecodeErrorKind;
use devworks_core::services::artifacts::lifecycle::ExactReadRefusal;
use devworks_core::services::runs::substrate::ActivationFailure;
use devworks_core::services::usage::accounting::AttemptEnd;

#[derive(Debug)]
pub enum TurnError {
    /// A store read or write failed, including an artifact read (see [`artifact_read_fault`]).
    Store(StorageError),
    /// The provider attempt ended without a settled continuation. `faults` are the recorded
    /// `Kind:code` provider errors of the attempt.
    Provider {
        outcome: Option<AttemptEnd>,
        faults: Vec<String>,
    },
    /// A command the turn issued for itself was refused, rejected or left unconfirmed; the
    /// gateway's retry advice decides.
    Command { detail: String, retry: RetryAdvice },
    /// A blocking worker the turn waited on panicked or was aborted.
    Worker(String),
}

impl From<StorageError> for TurnError {
    fn from(error: StorageError) -> Self {
        Self::Store(error)
    }
}

impl TurnError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Store(StorageError::Cancelled))
    }

    /// Repeating the turn may succeed: the activation retries with backoff.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Store(error) => storage_retryable(error),
            Self::Provider { outcome, faults } => provider_retryable(*outcome, faults),
            Self::Command { retry, .. } => *retry != RetryAdvice::Never,
            Self::Worker(_) => true,
        }
    }

    pub fn activation_failure(&self) -> ActivationFailure {
        let detail = format!("native agent turn: {self:?}");
        if self.retryable() {
            ActivationFailure::Transient(detail)
        } else {
            ActivationFailure::Deterministic(detail)
        }
    }
}

/// A lost scope, a missing row, an invalid or corrupt record cannot change by waiting. Every
/// other storage error (busy, unavailable, lost lease, unconfirmed commit) can.
pub(crate) fn storage_retryable(error: &StorageError) -> bool {
    !matches!(
        error,
        StorageError::ScopeDenied
            | StorageError::InvalidRecord(_)
            | StorageError::NotFound { .. }
            | StorageError::StorageUnavailable {
                cause: Unavailable::Corrupt,
                ..
            }
    )
}

/// Provider errors that a later attempt can outlive: an unreachable or overloaded endpoint, a
/// cooldown, or a generic upstream fault.
const TRANSIENT_PROVIDER_FAULTS: [DecodeErrorKind; 4] = [
    DecodeErrorKind::Unreachable,
    DecodeErrorKind::Overloaded,
    DecodeErrorKind::Cooldown,
    DecodeErrorKind::Provider,
];

/// A failed or cut-off attempt retries when every recorded provider error is transient (a
/// transport failure records none). Any other outcome, or an authentication, permission, model,
/// context-limit, refusal or protocol error, repeats the same way.
fn provider_retryable(outcome: Option<AttemptEnd>, faults: &[String]) -> bool {
    matches!(outcome, Some(AttemptEnd::Failed | AttemptEnd::UnknownTail))
        && faults.iter().all(|fault| {
            let kind = fault.split(':').next().unwrap_or_default();
            TRANSIENT_PROVIDER_FAULTS
                .iter()
                .any(|transient| format!("{transient:?}") == kind)
        })
}

/// The storage error of an exact artifact read. A version the caller cannot see stays a scope
/// denial and an over-bound version an invalid record; a blob the store cannot read now is an
/// I/O fault, so a turn that reads it retries.
pub(crate) fn artifact_read_fault(refusal: ExactReadRefusal) -> StorageError {
    match refusal {
        ExactReadRefusal::NotFound => StorageError::ScopeDenied,
        ExactReadRefusal::TooLarge { bytes, max } => {
            StorageError::InvalidRecord(format!("artifact of {bytes} bytes exceeds {max}"))
        }
        ExactReadRefusal::Unreadable => {
            StorageError::unavailable(Unavailable::Io, "artifact content unreadable")
        }
        ExactReadRefusal::Storage(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(outcome: AttemptEnd, faults: &[&str]) -> TurnError {
        TurnError::Provider {
            outcome: Some(outcome),
            faults: faults.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    #[test]
    fn transient_provider_failures_retry_and_rejections_do_not() {
        assert!(provider(AttemptEnd::Failed, &[]).retryable());
        assert!(provider(AttemptEnd::UnknownTail, &[]).retryable());
        assert!(provider(AttemptEnd::Failed, &["Overloaded:overloaded_error"]).retryable());
        assert!(provider(AttemptEnd::Failed, &["Unreachable:provider_error"]).retryable());
        assert!(!provider(AttemptEnd::Failed, &["Authentication:invalid_api_key"]).retryable());
        assert!(
            !provider(
                AttemptEnd::Failed,
                &["Overloaded:x", "ContextLimit:too_long"]
            )
            .retryable()
        );
        assert!(!provider(AttemptEnd::Incomplete, &[]).retryable());
        assert!(!provider(AttemptEnd::Completed, &[]).retryable());
    }

    #[test]
    fn unreadable_artifacts_retry_and_hidden_ones_do_not() {
        assert!(storage_retryable(&artifact_read_fault(
            ExactReadRefusal::Unreadable
        )));
        assert!(!storage_retryable(&artifact_read_fault(
            ExactReadRefusal::NotFound
        )));
        assert!(!storage_retryable(&StorageError::unavailable(
            Unavailable::Corrupt,
            "stored row"
        )));
        assert!(storage_retryable(&StorageError::Busy));
    }

    #[test]
    fn command_retry_advice_decides() {
        let refused = |retry| TurnError::Command {
            detail: "route".into(),
            retry,
        };
        assert!(refused(RetryAdvice::SameCommand).retryable());
        assert!(refused(RetryAdvice::ResolveReceipt).retryable());
        assert!(!refused(RetryAdvice::Never).retryable());
    }
}
