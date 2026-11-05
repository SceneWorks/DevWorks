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
//!   cancellation request.
//! * **Package host.** [`package_host::PackageHost`] launches, monitors and stops package
//!   helpers through the process owner over the framed local protocol (`host.initialize`,
//!   `host.activate`, `host.invoke`, `host.cancel`, `host.shutdown`), reconciles them to the
//!   committed package registry, provides the quiescence barrier the package service publishes
//!   behind, and brokers package requests: an event-handler invocation changes state and admits
//!   effects only through `broker.commit_event` (never `broker.state` compare-and-set or an
//!   effectful `broker.call`). See the module documentation and core
//!   `services::packages` (trust model: a helper is a separate process, not an OS sandbox).
//! * **Receipt replies.** [`replies::reply_consumer`] registers core's receipt reply publisher
//!   as an asynchronous consumer: every recorded receipt's durable typed reply is published
//!   after commit under the claim's fence, retried and recovered without duplicates.
//! * **Resources and owned processes.** [`resources::Housekeeper`] runs the process
//!   supervisor's recovery and the resource boot reconciliation at startup, periodic sweeps with
//!   the same sleep/wake detection as the dispatcher, and the bounded shutdown of owned trees.
//!   [`runtime::boot`] drives it (see "Boot sequence").
//! * **Run root controls.** [`runs::RunSupervisor`] runs core's `ControlService` for the
//!   application lifecycle: boot recovery of run roots (activations of earlier boots
//!   interrupted, their roots held for an explicit selective Continue), supervision ticks
//!   (lease reassertion after a sleep/wake jump, bounded retry of expired leases, execution of
//!   journaled stops through the process owner, and — once a coordinator
//!   [`runs::ActivationLauncher`] is installed — claim, launch, renewal and stop signalling), the
//!   immediate stop of an archived project, and the run-control steps of shutdown. Every
//!   consumer's root-scoped deliveries pass the run-control gate ([`runs::RunGate`]), so queued
//!   work of a paused, held or stopped root waits until the root is continued.
//! * **Owned browsers.** [`browser`] composes the one `BrowserService` (`browser.*`) over the
//!   process owner: its `browser_effects` consumer executes journaled browser effects under the
//!   operation journal, its supervision loop detects exits, crashes and popups, and shutdown
//!   closes every session through the process owner before the remaining trees are stopped.
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
//!    transferred). When a browser is composed, owned browser sessions of earlier boots then
//!    settle from what this step verified (`BrowserService::recover`): a session whose tree is
//!    gone is `Failed` (or `Closed` when it was stopping) with its profile removed, one whose tree
//!    could not be verified stays `Uncertain`, and profile roots no unsettled session owns are
//!    removed.
//! 5. **Recovers run roots** ([`runs::RunSupervisor::boot`], core
//!    `ControlService::recover_boot`): every activation an earlier boot left live — running,
//!    stopping, claimed, or accepted but never launched — is recorded `Interrupted` and its
//!    feature or issue root is held (`RestartContinue`, desired `Paused`) until an explicit,
//!    authorized, selective Continue (CONTROL-10); its coordinator, assignment lineage and results
//!    are kept and its active assignments go dormant. Continuation turns left in flight are
//!    ended. Journaled stops of earlier boots are settled from what step 4 verified (a tree it
//!    could not verify stopped holds the root for reconciliation). The schedules and
//!    keep-working hooks recover in the same unit of work and cannot clear these holds.
//!    When packages are composed, the package host then removes interrupted package
//!    publications and unreferenced install directories (nothing is admitted yet, so none can be
//!    in progress) and starts every enabled, available package's helper against the committed
//!    registry (re-verifying its installed files first); its reconciliation loop starts with
//!    delivery in step 8, and shutdown gives the helpers their cooperative stop before the
//!    process owner stops the remaining trees.
//! 6. **Applies keep-awake** from the effective `power.keepAwake` setting through the
//!    composition's `PowerPort`.
//! 7. **Records boot recovery** as one `boot` diagnostic (what steps 2–6 established and what
//!    needs attention: held deliveries, missing or quarantined files, unverifiable trees, roots
//!    held by run recovery, unavailable keep-awake), through the composition's one diagnostics
//!    service. A boot that stops at any step records why (stage and error class) when the store
//!    still accepts it, so a failed recovery stays visible after the next start.
//! 8. **Starts delivery** ([`Dispatcher::run`]); every consumer was composed with the
//!    run-control gate.
//! 9. **Starts artifact collection**: one pass at once, then every
//!    [`runtime::RuntimeConfig::collect_interval`] and after every applied `artifacts.delete`
//!    admitted through the runtime. A pass removes pending blob generations no reader or pin
//!    holds (`ArtifactService::collect`) and clears derived previews (`collect_derived`). Resource
//!    housekeeping ticks start too (every [`runtime::RuntimeConfig::housekeeping_interval`]:
//!    expiry, transfer, promotion, and a `Wake` reconciliation after a sleep/wake jump), and run
//!    supervision ticks at the same interval and after every committed `runs.*` command. The
//!    diagnostics monitor starts too: the retention pass (`diagnostics.logRetentionDays`,
//!    `diagnostics.logMaxRecords`), the store inspection, the full integrity check, the
//!    write-owner probe and the sweep of stale export staging, at once and then every
//!    [`runtime::RuntimeConfig::diagnostics_interval`]. The boot report, the collection passes
//!    and the dispatcher become health sources, and the consumers' names are the queue health
//!    `workspace.diagnostics` reports.
//! 10. **Returns the [`runtime::Runtime`]**, the only handle through which commands and
//!     queries are admitted: a committed command wakes delivery for the reply intent its receipt
//!     committed, a committed `settings.save_revision` re-applies keep-awake, and a committed
//!     `workspace.record_diagnostic` raises the live-log hint.
//!
//! [`runtime::Runtime::shutdown`] (AD20) first stops the diagnostics monitor (a pass in
//! progress is awaited within the shutdown bound, so no retention, probe or sweep write starts
//! after admission closes), then closes command admission — every command holds the admission
//! fence for its whole unit of work, so closing it drains the commands already admitted and
//! refuses later ones (a `workspace.record_diagnostic` included), and none commits after the
//! snapshot that follows — then commits the run-control transition (live feature/issue work held
//! for manual Continue and asked to stop; launched coordinators get the cooperative grace), then
//! stops delivery, collection, housekeeping and run supervision, waits for in-flight deliveries,
//! stops every owned process tree within one bound ([`runtime::RuntimeConfig::shutdown_bound`]),
//! records the `Shutdown` reconciliation, records every activation still live as `Interrupted`
//! and releases keep-awake. A storage failure on the way is not fatal: the next boot holds the
//! same work from its boot epoch (step 5).
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
//! Step 5 follows steps 2 and 4 and precedes steps 8–10: it settles journaled stops from the
//! journal state step 2 classified (an interrupted `Dispatching` stop is `OutcomeUnknown`) and the
//! process state step 4 verified, and the first claim or command of the boot must already see
//! every earlier boot's feature/issue work held. It writes only run-control records, the journal
//! state of `runs.stop` operations and their history, and reads no artifact record, so it commutes
//! with step 3. Root-scoped deliveries step 2 held awaiting eligibility stay held: the run-control
//! gate refuses them until their root is continued.
//!
//! Step 4 commutes with steps 2 and 3 as well: it writes only owned-process records, resource
//! definitions and leases and their history, and reads no queue row, journal state or artifact
//! record, while queue and artifact recovery never read or write those tables. It must however
//! precede steps 5–10: until it finishes, a tree of the earlier boot may still be running and
//! holding a physical resource (the desktop, a device, a port), so neither a recovered delivery
//! nor an admitted command may launch a process or be granted that resource. Recovery kills such
//! trees and transfers their leases only once they are verified gone, so the first new work of
//! the boot never runs beside an earlier boot's writer. An operation journaled `OutcomeUnknown`
//! by step 2 stays held for reconciliation even when step 4 killed the tree that ran it: killing
//! a process is not evidence of what it did. Keep-awake (step 6) only reads settings.
//!
//! Step 7 only appends a diagnostic record and reads nothing the earlier steps write, so it
//! sits after them to report their outcome (run recovery included), and before step 8 so that
//! the record of what a boot recovered precedes every record of what it then did. The
//! diagnostics monitor of step 9 writes only diagnostic records and reads settings, store files
//! and run-control records; the retention pass never touches history, receipts, artifacts,
//! run-control records or evidence a record names.
//!
//! Nothing here establishes product acceptance: tests run the real SQLite adapter with injected
//! clocks and scripted handlers, without inference, network or containers.
pub mod browser;
pub mod diagnostics;
pub mod dispatch;
pub mod git;
pub mod indexing;
pub mod jobs;
pub mod knowledge;
pub mod package;
pub mod package_host;
pub mod replies;
pub mod resources;
pub mod runs;
pub mod runtime;
mod store;

pub use dispatch::{
    AsyncDelivery, AsyncDeliveryHandler, AsyncFuture, AsyncOutcome, Consumer, Delivery,
    DeliveryHandler, Dispatcher, DispatcherConfig, EffectExecutor, EffectFuture, EffectResult,
    Outcome, RecoveryReport, RetryPolicy, RootEligibility, TickReport, Wakeup,
};
pub use indexing::{IndexSupervisor, IndexingConfig, IndexingStatus};
pub use jobs::{Scheduler, SchedulerTick};
pub use runs::{
    ActivationLauncher, Launch, LaunchEnd, LaunchFuture, RunGate, RunSupervisor, RunTick,
};
pub use runtime::{
    BootError, BootReport, CollectionPass, Composition, IndexComposition, PackageComposition,
    RunComposition, Runtime, RuntimeConfig, ShutdownStage, boot,
};
