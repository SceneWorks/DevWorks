# Reviewed Git child-process policy

DW-114 binds only reviewed `add`/`commit` writes to the native no-child-process
policy. Ordinary Git mutations and push retain their existing process policy.
The generic policy prevents child creation; it is not a filesystem, network, or
in-place executable sandbox.

Before an approval proposal is published, the Git owner executes the actual Git
`--version` under the same guarded policy and authority used by the reviewed
write. Unsupported policy, privileged execution, or a launcher that requires a
child produces `UnsupportedCapability` (`git_reviewed_process_policy`) instead
of a reviewable proposal. Cancellation, timeout, storage failure, and uncertainty
retain their existing meanings. Preparation does this I/O before the writer;
the approval writer preserves the owner's typed refusal without doing I/O.

The supported Git floor remains 2.31. On Windows, the trusted configured Git's
`--exec-path` identifies its own `mingw64`, `mingw32`, `clangarm64`, or `ucrt64`
distribution. The owner selects that distribution's `bin/git.exe`, then qualifies
that actual candidate under the guard. Relative paths, another installation,
path traversal, and the `cmd/git.exe` launcher are not accepted as candidates.
Only exact read-only `--version` and `--exec-path` discovery arguments are newly
recognized by shell authority; option overrides and extra commands remain held.

Reviewed writes explicitly disable hook paths, configured hook events, signing,
preload workers, automatic maintenance, and parallel index/pack/checkout workers.
The configuration witness remains checked before and after each mutation, but
the native process policy enforces the late-child boundary. The original exact
committed tree, message, and parent checks still determine success and recovery.

## Local evidence

The recorded checks use base `bda0a6e443648660bb1c2620c345f4c73e7aed90` plus
caller checkpoint `cd2ba4804c11e8b172d6da79803d5240a4872441` and the source
hashes in `native-results.json`. The machine was unprivileged uid 501 on macOS
27.0.1 (Darwin arm64), with `git version 2.54.0 (Apple Git-157)`.

All five reviewed-commit native tests pass. They execute the original Agent's
saved command only after the dedicated native Human approval, through the real
SQLite store, gateway, process supervisor, and native process adapter. The late
hook fixture installs a new configured `pre-commit` hook at the final process
spawn boundary, after the last source witness. Its marker is absent under the
guard; the resulting changed-source/uncertain operation is not replayed. A later
unguarded commit on the same repository executes the hook and creates the marker,
confirming this installed Git supports the exercised hook mechanism.

A separate controlled `EnforcementUnavailable` at guarded version qualification
produces the exact typed refusal, publishes no proposal, leaves HEAD unchanged,
and creates no hook marker. This is a service-boundary failure test, not evidence
that every privileged native environment was exercised locally. All eight native
approval tests pass after preserving typed owner refusals. The initial full Git
binary passed the eight other existing Git cases; its only failing case was the
new refusal-reason assertion, which was repaired and passed in the final focused
run. The final five-case run also retains the normal hook/signer, changed-witness,
and unexpected tree/message recovery regressions.

Git floor/Windows candidate parsing and the exact discovery authority test pass.
Strict Clippy for the core library and engine Git test, formatting, and whitespace
checks pass. No inference or external network is used by these tests.

Confidence is high for this macOS caller integration and its exercised native
late-hook boundary. Windows candidate derivation is covered by portable tests,
not a local Windows execution. Linux/Windows kernel policy and privileged/setid
qualification require their native process-owner tests and exact-head platform
CI; this report makes no cross-platform runtime claim.
