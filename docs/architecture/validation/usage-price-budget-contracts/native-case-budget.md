# Native Runtime case allocation

Baseline: exact composed `cd3fc1e5`, including the earlier Usage lifecycle correction.
This is the desktop portion of the measured DW127 integration-test allocation change.

The composed run completed 16 Usage cases and timed out three startup observers. Accounting
and Usage bootstrap were complete; Help remained Claimed/Dispatching with live checkpoint
progress and no failed delivery. The separate Help diagnosis measured its 199-document native
generation at 4.898s alone, while its unbudgeted default 24-case binary produced 14 thirty-second
timeouts with checkpoints still advancing. Those are resource-allocation observations, not
permission to weaken startup completion or full-snapshot cursor semantics.

`tests/common::native_runtime_case()` owns one of two per-binary case permits, implemented with
a standard mutex and condition variable. Every one of the 19 Usage cases acquires it as its first
statement, before root/runtime construction. Rust lexical drop order retains it through harness,
all runtime/app instances, temporary-root cleanup and unwinding. The mutex is held only while
admitting/releasing a case, not while its body executes. A failed case releases its permit by RAII.

The allocation is per case, deliberately not per Engine/App: scenarios that own two apps must
retain their real overlap instead of deadlocking on a nested runtime admission. Transport gates,
query/command races, background jobs and parallel tasks within a case are unchanged. Separate
native binaries require a separately coordinated execution window; the guard is not a global
cross-process semaphore.

Source audit: removing the 19 entry guard lines reconstructs the four previous Usage test files
byte for byte (15 main cases, two official-price cases, refresh and paging). No original assertion,
internal concurrency, timeout, true-event `CursorExpired` proof, fake transport, or secret port
was changed. No runtime product code, authentication, cursor, price or accounting code changed.

The first budgeted default run completed all 19 cases in 86.05s: all three previously failing
startup observers passed, with 18 overall passes and a separate More-fixture row-count failure.
A default run with added failure-only cursor/event diagnostics passed 19/19 in 86.28s; that rerun
alone does not disposition the intermittent More premise. This pure allocation commit preserves
all prior test bodies. A separately reviewed actual writer-slot control makes the positive
More premise explicit; see `paging-snapshot-control.md`. On that controlled source the final
default Usage suite passed 19/19 in 81.25s; final strict qualification is recorded with that tail.

Tests use actual temporary SQLite/runtime composition with SessionSecrets and recorded/no-network
provider ports; no learned computation or OS secrets. No startup failure is being reclassified as
success, and no producer is disabled or marked complete by the allocation helper.
