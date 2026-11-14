# DW-128: complete native CI partitions

The expanded workspace exceeded the existing 45-minute whole-job capacity.
PR39 Ubuntu job 128020486988 reached that bound while progressing through passing
tests/doctests. PR40 Windows foundation job 128020573824 reached it while passing
desktop suites. Pricing assertion failures and a graph query deadline in other
jobs remain separate defects; this scheduling change does not disposition them.

Each OS now runs three complete groups through `tools/ci/native-tests.sh`:

| Group | Complete package coverage |
| --- | --- |
| foundation | All workspace members except desktop and engine: currently core, adapters and extension protocol |
| desktop | devworks-desktop |
| engine | devworks-engine |

Default `cargo test` integration tests and doctests remain enabled. The Windows
filesystem/browser/Git qualification remains unchanged in its foundation job.
That existing targeted qualification intentionally overlaps a small engine subset;
the complete package groups are disjoint. Future workspace packages enter the
foundation group automatically unless explicitly selected into another group.

The original `test (ubuntu-latest)`, `test (macos-latest)` and
`test (windows-latest)` names remain as explicit fail-closed aggregates. Each
requires all three of its platform's jobs to succeed, including cancellation or
skip refusal. Every native job retains its original 45-minute bound; all operation,
test, qualification and strict-lint bounds remain unchanged.

Local actionlint and Bash syntax checks pass. Actual Cargo metadata proves exact
coverage of all five current packages. Executing the shared entry point with an
argument-recording Cargo substitute verifies all three actual commands and the
invalid-argument exit (2); it is a configuration check, not a native test pass.
All 64 success/failure/cancelled/skipped vectors per platform fail closed except
all-success: 192 executed aggregate checks.

CI impact: nine complete test workers replace four, with three platform aggregate
jobs instead of one. Test workloads are retained; extra dependency compilation,
cache/download cost and queueing are currently unknown. Partition-specific caches
avoid competing partial cache contents. Parallel groups reduce each job's required
work, without promising total wall time or lower billed compute. There is no new
workflow or trigger, inference execution, model download, or required-check waiver.
Hosted current-head CI, automated review, merge and acceptance remain pending.

## DW-335: one macOS job

GitHub allows the organization five hosted macOS jobs at a time, and the four macOS jobs
per PR (clippy and the three groups above) took most of them. macOS now runs one job,
named `test (macos-latest)`: one build, clippy, then every workspace test through
`cargo nextest run --workspace` and the doctests through `cargo test --workspace --doc`.
`.config/nextest.toml` keeps the limits that test binaries otherwise set with in-process
mutexes. An Ubuntu job named `clippy (macos-latest)` passes only when that job's clippy
step passed, so both required check names still report. Test temp files go to a RAM disk,
because the SQLite store requests F_FULLFSYNC on macOS. That covers both `TMPDIR` and
`<target>/tmp`, which is `CARGO_TARGET_TMPDIR` for the engine and desktop test helpers. On a dev Mac the adapters unit
tests took 376 s under `cargo test`, 287 s under nextest and 119 s under nextest with the
RAM disk; CI took 513 s for them under `cargo test` (run 43554082293). Ubuntu and Windows
keep the groups above. The `pick macOS runners` job sends this one job to an idle
self-hosted `devworks-mac` runner, or to `macos-latest` when none is idle. Several of those
runners share one Mac, so the job caps nextest at three test threads and names its RAM disk
after the runner.

## DW-342: run only the partitions a PR can affect

The workspace crates form one chain, core → adapters → engine → desktop, and no crate's tests
read another crate's files outside that chain (only `docs/` and `tools/`). The `test scope` job
(`tools/ci/test-scope.sh`, tested by `tools/ci/test_test_scope.sh`) reads the PR's changed files:

| Changed paths | Partitions that run | macOS packages |
|---|---|---|
| only `crates/devworks-desktop/` | desktop | `-p devworks-desktop` |
| `crates/devworks-engine/`, maybe desktop | engine, desktop | engine, desktop |
| `crates/devworks-adapters/`, maybe later crates | all | adapters, engine, desktop |
| anything else, an empty or failed diff, or a push | all | `--workspace` |

The Ubuntu and Windows aggregate checks accept a skipped partition only when the scope left it
out, and the Ubuntu one also requires the scope job, so broken scope rules fail a required check.
If the scope job fails, every partition runs. Over the 60 PRs merged before this change, 15%
touched only desktop and 17% only engine and desktop.
