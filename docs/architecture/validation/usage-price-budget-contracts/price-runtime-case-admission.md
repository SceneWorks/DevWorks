# DW135: native price fixture admission and failure phases

PR41 cfe9492b Windows-engine job128199808179 passed17/19 price cases, with both
price-shutdown qualifications reaching the original30s failure watchdog. All five tests that
boot the full native runtime now acquire the existing two-case permit before any boot and keep
it through cleanup. Internal recovery/race scenarios, every assertion, the30s watchdog and2s
shutdown checks remain unchanged. Store-only price cases retain their existing concurrency.

The original full19 suite passed locally25.80s; failure-only instrumentation without admission
changes passed25.53s; with admission, all19 passed25.33s. A temporary timing probe on the actual
two-scenario shutdown case completed9.26s: source startup owners took about3.7s per boot,
with each bounded app shutdown about106ms. The timing prints were removed. This supports
contention as a plausible Windows explanation (medium confidence), not a proven causal repair.
Fresh exact-head Windows CI remains required. Strict workspace/all-target checks pass34.03s.

A bounded failure-only guard records the current phase and startup Help/receipt/bootstrap
metadata if the future is cancelled or fails. It never changes ordering, adds a retry, increases
a timeout or prints source/catalog/document bodies. Both original shutdown scenarios and the
controlled independent-receipt history race remain actual native executions.
