# Resource reservations and waits

Resources shows constrained work such as provider capacity, repository writes, compilers, indexing, and owned processes. A reservation has an owner, scope, purpose, expiry, and current state. Resource limits are separate from the independent per-feature/issue subagent allowance.

A wait is not a user Pause. Capacity can become available automatically, but no resource release clears an unrelated user, approval, dependency, budget, or reconciliation hold. Inspect the named blocker and its owner before changing it. Starting another worker cannot bypass the same physical constraint.

Renewable ownership prevents stale agents from continuing after a lease changes. A process or effect that may still be running must reach a known outcome or quiescent boundary before ownership transfers. A closed conversation alone is not that proof.

Use the app's resource operations rather than deleting lease records or killing unrelated processes. Stopping owned work should retain its output and recovery state. See [Agents](help:agents), [Controls](help:work-controls), [Browser](help:browser), and [Diagnostics](help:diagnostics).
