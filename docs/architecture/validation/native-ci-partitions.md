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
