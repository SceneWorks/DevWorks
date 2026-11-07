# DW-127: native runtime case admission

Qualified locally on 2026-11-06 from composed source
`cd3fc1e51eb4df35c16122f1da4afc7aebaaa085`.

## Observed failure and diagnosis

One isolated native Help case passed in 6.06 seconds. Its actual Help worker
completed 199 documents in 4.898 seconds: staging accounted for 4.293 seconds,
metadata rendering 552 ms and projection 25 ms. Four hundred source-digest checks
totaled 140 ms over 9,499 serialized bytes and 141 registered handlers. The schema
source was already a 64-character digest; repeatedly serializing full schemas was
not the dominant path.

A default 24-case Help invocation, without another campaign from this project,
passed 10 cases and hit the original startup watchdog in 14 cases (72.72 seconds
total). Unrelated heavy host work remained present, so this does not establish a
cause from host load alone. Temporary instrumentation established continuing work:
at timeout, sampled generations had completed 121–199 of 199 staging steps, with
phase ages of 18–892 ms. Captured journals remained `Dispatching`, deliveries were
`Claimed` on their first attempt with cancellation false, and no captured journal
was `Failed` or `OutcomeUnknown`. The evidence supports explicit case resource
allocation; it does not demonstrate a globally stuck Help phase.

## Admission contract

`tests/common/native_runtime_case.rs` admits **two independent heavy cases per
integration-test binary**, using an owned RAII permit and a standard mutex/condition
variable. Each participating case acquires its permit as its first statement,
before boot, so all app/runtime locals clean up before the permit drops, including
on panic. Waiting for admission occurs before the existing operation watchdogs.

The engine adopts this contract in all 24 Help cases, including paid Conventions
cases, and both package Help cases. The guard is never acquired by an app, fixture
constructor or individual task. Existing multiple-app scenarios and internal
command, acknowledgment, cancellation and publication races retain their original
concurrency. All original assertions and 30-second operation watchdogs remain.

This is a per-binary allocation. Separate test processes and unrelated host work
still need campaign coordination. Two cases can run together; there is no single
case lock and no product staging, durability, authority or source-capture change.
Desktop Usage adopts the same API/policy in its separate owner qualification.

A failure-only borrowing guard records bounded Help metadata: head and registry
revisions, operation/attempt state, delivery ownership/cancellation, and first pages
of draft/pending staging identities. It reads at most 32 rows per inventory and
marks full pages explicitly. It selects metadata fields rather than dumping
operation payloads or source bodies; at most two reason previews contain at most
512 characters each. Its snapshot is labeled as sampled during failure cleanup.
Temporary production phase instrumentation is excluded from delivery.

## Final qualification

- Default composed Help invocation: **24/24 passed**, 119.24 seconds. Libtest's
  over-60-second case labels include admission queueing, not a changed operation
  watchdog.
- Original 264,375,372-byte signed-guide case: **1/1 passed**, 18.83 seconds.
  Largest allocation 191,466 bytes; peak additional live memory 1,433,923 bytes.
  Full-source integrity, Unicode tail, immutable history, changed source,
  cancellation/deadline and original allocation assertions remain enabled.
- Owned-permit unwind/two-overlapping-permit regression: **1/1 passed**.
- Engine all-target Clippy with `-D warnings`: passed (27.54 seconds).
- Rustfmt, diff checks and the 24-case first-statement guard audit: passed.

Commands retained the default libtest concurrency:

```sh
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test --locked -p devworks-engine \
  --test jobs help:: -- --nocapture
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo test --locked -p devworks-engine \
  --test packages help_guides::near_limit_guide -- --nocapture
cargo test --locked -p devworks-engine --test native_runtime_case
```

These are local composed-runtime results with recorded/scripted data and in-memory
secrets. No learned model execution, external provider, Git/authentication probe or
operating-system secret access was used. Windows and composed desktop acceptance
remain their own required CI/native evidence.
