# Agents, sessions, and execution availability

The Agents & sessions destination is the place for coordinator/worker assignment visibility. A screen entry or registered protocol DTO alone is not a running agent supervisor. If this build reports the assignment/delegation view unavailable, child delegation, durable agent inboxes, and the autonomous coordinator loop must not be assumed to run.

## Existing control foundations

Root run controls, durable sessions, ownership generations, instruction history, and operation reconciliation are separate from an agent's visible conversation. A session can remain open after an activation stops. Inspect the actual run and its blockers rather than treating an open chat as liveness.

Each feature and standalone issue has its own configured subagent allowance, default 10. There is no global subagent-count cap. This setting does not itself launch workers. Real provider capacity, repository writes, compiler/indexing work, resources, dependencies, and permissions can still restrict dispatch.

## Continuity and safe recovery

Continuing existing work preserves logical history and reconciles current ownership. A new work item starts a fresh session with a scoped brief; another conversation is not permission to duplicate an existing execution owner. Membership handoff preserves an existing worker/session only through the native quiescent, generation-fenced boundary. Late results from an older generation cannot become current completion evidence.

A message, acknowledgment, or child report is not acceptance. Completion needs the approved outcome and verification evidence. Use [Controls](help:work-controls), [Readiness](help:features), [Resources](help:resources), and [Diagnostics](help:diagnostics) to understand a wait or interruption.
