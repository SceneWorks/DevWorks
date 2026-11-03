//! DevWorks asynchronous engine: durable event delivery, supervision and recovery.
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
//!   before any expired-claim recovery can reassign them (see [`dispatch`] for per-platform
//!   behaviour and the safe fallback when no jump is visible). Root-scoped work recovered from
//!   an earlier boot is held until a consumer's [`RootEligibility`] confirms it may resume.
//! * **Package event handlers.** [`package::commit_event`] is the host side of
//!   `broker.commit_event`: state CAS, typed effect intents, the processing receipt and the
//!   subscription checkpoint commit in one transaction, and an identical replay returns the
//!   original receipt. It refuses a stale or cancelled invocation (fence and cancellation
//!   epoch) without writing. A package process reaches it through [`AsyncDeliveryHandler`]: the
//!   dispatcher keeps the claim live with a renewed lease while the handler runs and signals a
//!   cancellation request. The package process host itself is a later item.
//! * **Receipt replies.** [`replies::reply_consumer`] registers core's receipt reply publisher
//!   as an asynchronous consumer: every recorded receipt's durable typed reply is published
//!   after commit under the claim's fence, retried and recovered without duplicates.
//! * **Resources and owned processes.** [`resources::Housekeeper`] runs the process
//!   supervisor's recovery and the resource boot reconciliation at startup, periodic sweeps with
//!   the same sleep/wake detection as the dispatcher, and the bounded shutdown of owned trees.
//!   [`runtime::boot`] drives it (see "Boot sequence").
//! * **Diagnostics.** [`diagnostics`] records boot recovery (and a boot that stopped) in the core
//!   diagnostic log, registers the boot report, artifact collection and dispatcher as health
//!   sources of `workspace.diagnostics`, and runs the retention and monitor passes
//!   ([`diagnostics::Monitor`]).
//!
//! # Boot sequence
//!
//! Restart correctness depends on what runs before the first claim, so there is one entry point,
//! [`runtime::boot`], and the composition root uses nothing else to start the engine. It takes
//! one [`runtime::Composition`] of core ports and services (one store for everything; the native
//! process launcher and power facility arrive as the core `ProcessHost` and `PowerPort` ports, so
//! the engine still depends on no adapter) and, in this fixed order:
//!
//! 1. **Composes delivery.** The dispatcher is built with every required consumer — the receipt
//!    reply consumer ([`replies::reply_consumer`], over the composition's one artifact service and
//!    store) — plus the caller's consumers. A missing required consumer would leave every
//!    `Recorded` receipt's reply pending forever, so the caller cannot omit it; a caller consumer
//!    reusing its name is refused. Nothing is claimed yet.
//! 2. **Recovers the queue and operation journal** ([`Dispatcher::recover`]): interrupted
//!    operations of earlier boots are classified (`Preflighted` → `RetryPending`, `Dispatching`
//!    → `OutcomeUnknown`), then their claims are requeued, held for reconciliation or settled
//!    `Cancelled`. This must precede the first claim of the boot.
//! 3. **Recovers artifacts** (`ArtifactService::recover`): interrupted staging operations of
//!    earlier boots are completed or failed from durable metadata, staged files and orphaned
//!    generations (a crash between a reply file and its metadata) are removed, interrupted
//!    removals finished and present generations verified.
//! 4. **Recovers owned processes and resource leases** ([`resources::Housekeeper::boot`]): the
//!    machine resources ([`runtime::RuntimeConfig::machine_resources`], the desktop by default)
//!    are registered; every process tree an earlier boot recorded is reconciled by its native
//!    identity (pid plus birth token): a live one is killed with its group or job, a vanished one
//!    is `Reaped`, an unverifiable one stays `Uncertain`, a launch without an identity is `Lost`;
//!    then the resource boot reconciliation runs (earlier boots' waiters cancelled, logical grants
//!    marked owner-unreachable, physical grants uncertain until their trees are settled, then
//!    transferred).
//! 5. **Applies keep-awake** from the effective `power.keepAwake` setting through the
//!    composition's `PowerPort`.
//! 6. **Records boot recovery** as one `boot` diagnostic (what steps 2–5 established and what
//!    needs attention: held deliveries, missing files, unverifiable trees, unavailable
//!    keep-awake), through the composition's one diagnostics service. A boot that stops at any
//!    step records why (stage and error class) when the store still accepts it, so a failed
//!    recovery stays visible after the next start.
//! 7. **Starts delivery** ([`Dispatcher::run`]).
//! 8. **Starts artifact collection**: one pass at once, then every
//!    [`runtime::RuntimeConfig::collect_interval`] and after every applied `artifacts.delete`
//!    admitted through the runtime. A pass removes pending blob generations no reader or pin
//!    holds (`ArtifactService::collect`) and clears derived previews (`collect_derived`). Resource
//!    housekeeping ticks start too (every [`runtime::RuntimeConfig::housekeeping_interval`]:
//!    expiry, transfer, promotion, and a `Wake` reconciliation after a sleep/wake jump). The
//!    diagnostics monitor starts too: the retention pass (`diagnostics.logRetentionDays`,
//!    `diagnostics.logMaxRecords`), the full integrity check and the write-owner probe, at once
//!    and then every [`runtime::RuntimeConfig::diagnostics_interval`]. The boot report, the
//!    collection passes and the dispatcher become health sources, and the consumers' names are
//!    the queue health `workspace.diagnostics` reports.
//! 9. **Returns the [`runtime::Runtime`]**, the only handle through which commands and queries
//!    are admitted: a committed command wakes delivery for the reply intent its receipt
//!    committed, a committed `settings.save_revision` re-applies keep-awake, and a committed
//!    `workspace.record_diagnostic` raises the live-log hint.
//!
//! [`runtime::Runtime::shutdown`] stops delivery, collection, housekeeping and the diagnostics
//! monitor, waits for in-flight deliveries, then stops every owned process tree within one bound
//! ([`runtime::RuntimeConfig::shutdown_bound`]), records the `Shutdown` reconciliation and
//! releases keep-awake.
//!
//! Steps 2 and 3 commute. Queue recovery writes only queue claims and the journal state of
//! non-terminal operations; artifact recovery resolves only staging operations (which commit as
//! `Dispatching`, never `Preflighted`) and decides each from whether its version metadata
//! committed, whatever non-terminal state it is in (`Dispatching` or the `OutcomeUnknown` that
//! classification gives it), and it touches no queue row. A reply delivery carries no operation,
//! so its prior-boot claim is requeued, never held, in either order. Either order therefore
//! ends with the same claims requeued, every staging operation terminal, the orphan removed and
//! the reply published exactly once (adapter test
//! `boot_recovery_orders_commute_and_publish_the_reply_once`). The order above is fixed so a
//! boot is reproducible.
//!
//! Step 4 commutes with steps 2 and 3 as well: it writes only owned-process records, resource
//! definitions and leases and their history, and reads no queue row, journal state or artifact
//! record, while queue and artifact recovery never read or write those tables. It must however
//! precede steps 7–9: until it finishes, a tree of the earlier boot may still be running and
//! holding a physical resource (the desktop, a device, a port), so neither a recovered delivery
//! nor an admitted command may launch a process or be granted that resource. Recovery kills such
//! trees and transfers their leases only once they are verified gone, so the first new work of
//! the boot never runs beside an earlier boot's writer. An operation journaled `OutcomeUnknown`
//! by step 2 stays held for reconciliation even when step 4 killed the tree that ran it: killing
//! a process is not evidence of what it did. Keep-awake (step 5) only reads settings.
//!
//! Step 6 only appends a diagnostic record and reads nothing the earlier steps write, so it
//! sits after them to report their outcome, and before step 7 so that the record of what a
//! boot recovered precedes every record of what it then did. The diagnostics monitor of step 8
//! writes only diagnostic records and reads settings and store files; the retention pass
//! never touches history, receipts, artifacts or evidence a record names.
//!
//! Nothing here establishes product acceptance: tests run the real SQLite adapter with injected
//! clocks and scripted handlers, without inference, network or containers.
pub mod diagnostics;
pub mod dispatch;
pub mod package;
pub mod replies;
pub mod resources;
pub mod runtime;
mod store;

pub use dispatch::{
    AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Consumer, Delivery,
    DeliveryHandler, Dispatcher, DispatcherConfig, EffectExecutor, EffectFuture, EffectResult,
    Outcome, RecoveryReport, RetryPolicy, RootEligibility, TickReport, Wakeup,
};
pub use runtime::{
    BootError, BootReport, CollectionPass, Composition, Runtime, RuntimeConfig, boot,
};
