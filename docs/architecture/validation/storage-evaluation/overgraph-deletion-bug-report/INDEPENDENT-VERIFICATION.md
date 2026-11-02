# Independent verification brief

Verify or refute the attached suspected OverGraph 0.18.0 deletion/vector-visibility defect. Do not assume the report's engine attribution is correct.

1. Read BUG-REPORT.md and the standalone repro source. Confirm it uses the public engine API without DevWorks adapters, ID mappings, historical epochs or traversal scopes.
2. Check the exact registry version/checksum, Cargo.lock and default/production-concurrency features. Build with the recorded toolchain where available; record any difference.
3. Execute the four variants in fresh disposable directories. Record native victim/survivor IDs, point visibility and unfiltered vector-hit IDs before deletion, after deletion, after deletion flush and after reopen.
4. Inspect the initial-flush/no-rename database from a separate process; compare the no-initial-flush/no-rename control. Distinguish observation exit code 1 from setup-error exit code 2.
5. Check the pinned public API contract and existing upstream tests. Determine whether this is an engine defect, documented API behavior, or reproduction misuse. Investigate persistence/tombstone handling only after confirming the symptom; do not assume a root cause.
6. Return a concise verdict with confidence, exact commands/environment, raw observations, and source evidence. If it does not reproduce, explain the differences and preserve the negative evidence.

The separate cross-project graph-scope finding was an adapter mistake and is not part of this bug report. No DevWorks source changes, upstream submissions or patch implementation are requested. Keep generated databases disposable and do not reuse existing user databases.
