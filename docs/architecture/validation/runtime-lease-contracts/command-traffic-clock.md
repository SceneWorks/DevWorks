# Native parked-lease clock qualification (DW-136)

Main ac75da46 push CI42305496146 failed Windows engine job128197424199 in the
parked command-traffic lease case;22/23 delivery cases passed. The unchanged local
suite passes23/23 in15.71s. The test advances an injected store clock while Tokio
supervision and the real SQLite writer run on independent clocks.

A controlled650ms writer hold reproduces the same unchanged lease-expiry assertion
with the original test (1.32s). Keeping all30 original20ms advance/notification/sleep
iterations, the correction waits after each200ms of injected time for its due parked
renewal, continuously sending20ms notifications while waiting. This prevents the
injected clock from outrunning a delayed writer. It does not stop command traffic
while awaiting renewal. The same controlled writer hold then passes1.80s.

Restoring the old reset-on-every-notification renewal timer makes the corrected case
fail the original30s watchdog (30.02s). Thus the added synchronization still detects
timer starvation. The actual monotonic product implementation is unchanged; temporary
writer-delay and old-timer experiments are removed from the final diff. All lease,
recovery, generation and exact-once assertions remain unchanged, as do lease lengths,
traffic steps,20ms intervals and30s watchdog.

Final native delivery suite:23/23 pass16.50s. Engine all-target strict Clippy:19.40s.
The writer-delay mechanism is demonstrated with high confidence; attribution of the
original Windows scheduling delay remains medium confidence because its log did not
record writer latency. Fresh reviewed Windows/all-platform CI remains required.
No learned inference, real credentials or OS trust changes are involved.
