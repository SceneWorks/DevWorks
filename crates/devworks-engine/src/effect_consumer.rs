//! The one consumer that serves every journaled effect service (git, process tools, containers,
//! help, desktop, convention import, MCP, browser, schemas).
//!
//! A service implements [`JournaledEffect`]; [`effect_consumer`] runs its `execute` and
//! `observe` on the blocking pool, never on an async worker, and maps the
//! [`EffectOutcome`] onto the dispatcher's [`EffectResult`]. A panic after dispatch becomes
//! `Unknown`, which the dispatcher maps to `OutcomeUnknown` and holds for reconciliation, so an
//! operation is never left `Dispatching`.
use crate::dispatch::{
    Consumer, Delivery, DeliveryHandler, EffectExecutor, EffectFuture, EffectResult, Outcome,
    ReadyFuture, RetryPolicy,
};
use devworks_core::ports::storage::{
    Cancellation, OperationRecord, StorageError, WriteTransaction,
};
use devworks_core::services::effect::{EffectOutcome, JournaledEffect};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The `Unknown` reasons used when an effect thread panics (the effect may have happened).
#[derive(Clone, Copy)]
pub struct PanicReasons {
    pub execute: &'static str,
    pub observe: &'static str,
}

/// Retry policy of most effect consumers.
pub const STANDARD_RETRY: RetryPolicy = RetryPolicy {
    base_delay_ms: 500,
    max_delay_ms: 30_000,
    max_attempts: 4,
};

fn result(outcome: EffectOutcome) -> EffectResult {
    match outcome {
        EffectOutcome::Completed(value) => EffectResult::Completed(value),
        EffectOutcome::Failed(reason) => EffectResult::Failed(reason),
        EffectOutcome::NotApplied(reason) => EffectResult::NotApplied(reason),
        EffectOutcome::Unknown(reason) => EffectResult::Unknown(reason),
    }
}

/// Cancels the attempt's token when the dispatcher drops the effect future.
struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Effects<E> {
    effect: Arc<E>,
    panics: PanicReasons,
}

impl<E: JournaledEffect> Effects<E> {
    fn run(
        &self,
        operation: OperationRecord,
        panicked: &'static str,
        call: fn(&E, &OperationRecord, &Cancellation) -> EffectOutcome,
        token: Cancellation,
    ) -> EffectFuture {
        let effect = self.effect.clone();
        Box::pin(async move {
            let _abandoned = CancelOnDrop(token.clone());
            match tokio::task::spawn_blocking(move || call(&effect, &operation, &token)).await {
                Ok(outcome) => result(outcome),
                Err(_) => EffectResult::Unknown(panicked.into()),
            }
        })
    }
}

impl<E: JournaledEffect> EffectExecutor for Effects<E> {
    fn resumes_after_observation(&self) -> bool {
        self.effect.resumes_after_observation()
    }
    fn observes_delivery_cancellation(&self) -> bool {
        self.effect.observes_delivery_cancellation()
    }
    fn execute(&self, operation: OperationRecord) -> EffectFuture {
        self.run(
            operation,
            self.panics.execute,
            E::execute_cancellable,
            Cancellation::new(),
        )
    }
    fn execute_cancellable(
        &self,
        operation: OperationRecord,
        cancellation: Cancellation,
    ) -> EffectFuture {
        if self.effect.observes_delivery_cancellation() {
            self.run(
                operation,
                self.panics.execute,
                E::execute_cancellable,
                cancellation,
            )
        } else {
            self.execute(operation)
        }
    }
    fn observe(&self, operation: OperationRecord) -> EffectFuture {
        self.run(
            operation,
            self.panics.observe,
            E::observe_cancellable,
            Cancellation::new(),
        )
    }
    fn ready(&self, operation: &OperationRecord) -> Option<ReadyFuture> {
        self.effect.wait_bound(operation)?;
        let (effect, operation) = (self.effect.clone(), operation.clone());
        Some(Box::pin(async move {
            let Some(bound) = effect.wait_bound(&operation) else {
                return;
            };
            let deadline = Instant::now() + bound;
            loop {
                let (e, o) = (effect.clone(), operation.clone());
                let pending = tokio::task::spawn_blocking(move || e.must_wait(&o))
                    .await
                    .unwrap_or(false);
                if !pending || Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }))
    }
}

/// The delivery itself has nothing to apply: the journaled effect is the work.
struct Settle;
impl DeliveryHandler for Settle {
    fn apply(&self, _: &mut dyn WriteTransaction, _: &Delivery) -> Result<Outcome, StorageError> {
        Ok(Outcome::Completed)
    }
}

/// The consumer `name` over `effect`. Callers set the fields that differ per service
/// (`max_in_flight`, a different `handler`) on the returned consumer.
pub fn effect_consumer<E: JournaledEffect>(
    name: &str,
    effect: Arc<E>,
    panics: PanicReasons,
    retry: RetryPolicy,
    max_in_flight: Option<usize>,
) -> Consumer {
    Consumer {
        name: name.into(),
        handler: Arc::new(Settle),
        effect: Some(Arc::new(Effects { effect, panics })),
        async_handler: None,
        eligibility: None,
        stop_grace_ms: 5_000,
        retry,
        max_in_flight,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn on_async_worker() -> bool {
        std::panic::catch_unwind(|| tokio::runtime::Handle::current().block_on(async {})).is_err()
    }

    /// Records whether each call ran on an async worker. Blocking the runtime from inside it
    /// panics, which a blocking-pool thread allows.
    #[derive(Default)]
    struct Probe {
        on_async_thread: Mutex<Vec<(&'static str, bool)>>,
        panic_execute: bool,
        wait: bool,
    }
    impl Probe {
        fn note(&self, call: &'static str) {
            self.on_async_thread
                .lock()
                .unwrap()
                .push((call, on_async_worker()));
        }
    }
    impl JournaledEffect for Probe {
        fn execute(&self, _: &OperationRecord) -> EffectOutcome {
            self.note("execute");
            assert!(!self.panic_execute, "boom");
            EffectOutcome::Completed(serde_json::json!(1))
        }
        fn observe(&self, _: &OperationRecord) -> EffectOutcome {
            self.note("observe");
            EffectOutcome::NotApplied("none".into())
        }
        fn wait_bound(&self, _: &OperationRecord) -> Option<Duration> {
            self.wait.then_some(Duration::from_millis(30))
        }
        fn must_wait(&self, _: &OperationRecord) -> bool {
            self.note("must_wait");
            true
        }
    }

    fn operation() -> OperationRecord {
        OperationRecord {
            id: devworks_core::protocol::OperationId(
                devworks_core::protocol::Id::new("op").unwrap(),
            ),
            root_id: None,
            assignment_id: None,
            capability: "test.effect".into(),
            schema_version: 1,
            args_digest: String::new(),
            payload: serde_json::json!({}),
            state: devworks_core::protocol::OperationState::Dispatching,
            revision: 1,
            created_at_ms: 0,
            attempts: Vec::new(),
        }
    }

    fn effects(probe: &Arc<Probe>) -> Effects<Probe> {
        Effects {
            effect: probe.clone(),
            panics: PanicReasons {
                execute: "execute panicked",
                observe: "observe panicked",
            },
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_and_observe_run_off_the_async_threads() {
        let probe = Arc::new(Probe {
            wait: true,
            ..Probe::default()
        });
        assert!(on_async_worker(), "the probe must detect the async runtime");
        let effects = effects(&probe);
        assert_eq!(
            effects.execute(operation()).await,
            EffectResult::Completed(serde_json::json!(1))
        );
        assert_eq!(
            effects.observe(operation()).await,
            EffectResult::NotApplied("none".into())
        );
        effects.ready(&operation()).expect("ordered").await;
        let calls = probe.on_async_thread.lock().unwrap().clone();
        assert!(calls.iter().any(|c| c.0 == "execute"));
        assert!(calls.iter().any(|c| c.0 == "observe"));
        assert!(calls.iter().any(|c| c.0 == "must_wait"));
        assert!(
            calls.iter().all(|(_, on_async)| !on_async),
            "an effect ran on the async runtime: {calls:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panic_after_dispatch_is_unknown_not_stuck() {
        let probe = Arc::new(Probe {
            panic_execute: true,
            ..Probe::default()
        });
        assert_eq!(
            effects(&probe).execute(operation()).await,
            EffectResult::Unknown("execute panicked".into())
        );
    }

    #[test]
    fn unordered_effects_dispatch_at_once() {
        let probe = Arc::new(Probe::default());
        assert!(effects(&probe).ready(&operation()).is_none());
    }
}
