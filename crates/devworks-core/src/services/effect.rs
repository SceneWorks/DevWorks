//! The one result and the one trait every journaled effect service shares. The engine's single
//! effect consumer runs any [`JournaledEffect`] on a blocking thread and maps its
//! [`EffectOutcome`] onto the dispatcher's result; services own only the effect itself.
use crate::ports::storage::{Cancellation, OperationRecord};
use serde_json::Value;
use std::time::Duration;

/// Observed outcome of one journaled effect.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectOutcome {
    /// The effect happened; the value carries the evidence (typically `{"result": ...}`).
    Completed(Value),
    /// Deterministic failure; nothing (or nothing more) to retry.
    Failed(String),
    /// Evidence that nothing happened; safe to retry under the consumer's policy.
    NotApplied(String),
    /// The effect may or may not have happened; the journal holds it for reconciliation.
    Unknown(String),
}

/// A service whose journaled operations the dispatcher executes and observes.
///
/// Both methods block and run on a blocking thread, never on the async runtime. `observe` must
/// not perform the effect again.
pub trait JournaledEffect: Send + Sync + 'static {
    /// Performs the effect of a `Dispatching` operation once.
    fn execute(&self, operation: &OperationRecord) -> EffectOutcome;
    /// Reads the target for an operation whose outcome is unknown, using its saved identity.
    fn observe(&self, operation: &OperationRecord) -> EffectOutcome;
    /// [`execute`](Self::execute) with a token the consumer cancels when the dispatcher
    /// abandons the attempt. Only effects that can stop early override this.
    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        _abandoned: &Cancellation,
    ) -> EffectOutcome {
        self.execute(operation)
    }
    /// [`observe`](Self::observe) with the same abandonment token.
    fn observe_cancellable(
        &self,
        operation: &OperationRecord,
        _abandoned: &Cancellation,
    ) -> EffectOutcome {
        self.observe(operation)
    }
    /// How long `operation` may wait, parked without a worker permit, for
    /// [`must_wait`](Self::must_wait) to turn false. `None` (the default) dispatches at once.
    fn wait_bound(&self, _operation: &OperationRecord) -> Option<Duration> {
        None
    }
    /// Whether `operation` is still ordered behind an unsettled predecessor.
    fn must_wait(&self, _operation: &OperationRecord) -> bool {
        false
    }
}
