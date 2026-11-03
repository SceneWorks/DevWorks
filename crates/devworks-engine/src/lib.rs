//! DevWorks asynchronous engine: durable event delivery, supervision and recovery (DW-003).
//!
//! # Boundary rationale (architecture "Component and dependency structure")
//!
//! * **Dependencies.** Core and the public extension protocol only. Persistence is the core
//!   [`devworks_core::ports::storage::ApplicationStore`] port injected at the composition root;
//!   the engine never names SQLite or SQL. The clock is the injected core
//!   [`devworks_core::ports::Clock`]. There is no global state.
//! * **The database owns recovery; channels only move work** ("Asynchronous event delivery and
//!   channel reuse"). Required deliveries are queue rows committed with their state change. The
//!   [`Dispatcher`] is woken by a coalescing Tokio `Notify` (best effort, after commit) and by a
//!   bounded periodic sweep that sleeps at most until the next saved ready time or lease expiry,
//!   so a lost wakeup delays work by at most one sweep interval and never loses it. A `watch`
//!   channel carries "queue state may have changed" hints for UI; a missed hint is repaired by a
//!   durable query, never by replay.
//! * **Bounded dispatch.** The dispatcher claims only as many deliveries as it has free worker
//!   permits, so saturation queues durably instead of buffering in memory.
//! * **Atomic handlers, journaled effects.** A [`DeliveryHandler`] runs inside one write
//!   transaction with the delivery's completion: its database effects, follow-up intents and the
//!   consumer checkpoint commit together or not at all (at-least-once delivery, idempotent
//!   application). External effects run outside any transaction through an [`EffectExecutor`]
//!   under the shared operation journal: `Dispatching` is committed before the effect starts, so
//!   a crash or lost result leaves `OutcomeUnknown`, which is held for observation and explicit
//!   reconciliation and never replayed blindly.
//! * **Recovery before admission.** [`Dispatcher::recover`] classifies interrupted operations and
//!   reclaims prior-boot claims before the first claim of a new boot; sleep/wake is detected by
//!   comparing the wall clock with Tokio's monotonic clock and repairs the running owner's leases
//!   before any expired-claim recovery can reassign them.
//! * **Package event handlers.** [`package::commit_event`] is the host side of
//!   `broker.commit_event`: state CAS, typed effect intents, the processing receipt and the
//!   subscription checkpoint commit in one transaction, and an identical replay returns the
//!   original receipt. The package process host itself is a later item.
//!
//! Nothing here establishes product acceptance: tests run the real SQLite adapter with injected
//! clocks and scripted handlers, without inference, network or containers.
pub mod dispatch;
pub mod package;
mod store;

pub use dispatch::{
    Consumer, Delivery, DeliveryHandler, Dispatcher, DispatcherConfig, EffectExecutor,
    EffectFuture, EffectResult, Outcome, RecoveryReport, RetryPolicy, TickReport, Wakeup,
};
