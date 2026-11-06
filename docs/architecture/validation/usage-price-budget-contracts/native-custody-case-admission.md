# DW-134: bound independent Runs fixtures during custody qualification

PR41 macOS engine job128156515707 passed75/76 Runs cases; the managed-source
custody case reached its unchanged30-second watchdog after its expected scripted
source-panic scenario. That panic is deliberate and proves no product failure by
itself. The original focused case passes unchanged in5.61s on the development Mac.

The Runs binary's independent native SQLite/application fixtures did not acquire the
existing process case budget. Its75 textual test functions now acquire the established
two-case admission guard before fixture setup, including the physical-owner recovery
case; platform configuration expands these to76 cases on this host. Internal tasks,
barriers, multi-app scenarios and native child processes retain their original behavior.

The managed-source case retains all seven coordinator, worker, bounded shutdown,
membership, panic and manual-custody scenarios. Every original assertion, the30-second
outer watchdog and2-second shutdown check remain unchanged. Failure-only diagnostics
now report scenario, phase and elapsed time, without inference inputs or credentials.
No learned inference runs: sources are recorded metadata frames and deterministic
port fixtures, alongside the actual native process-identity qualification.

All76 native Runs cases pass39.02s at default invocation, including the original
custody case. This is development-host evidence; confidence in the resource-pressure
diagnosis is medium pending current-head macOS CI. Any remaining semantic failure
still fails the original assertions and watchdogs. Strict checks, review/CI, merge
and acceptance remain required.
