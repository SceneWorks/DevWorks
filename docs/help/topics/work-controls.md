# Start, Pause, Continue, Stop, and waits

Controls act on a work root and retain durable history. Start begins or reconciles the root's activation; Continue reuses established identity and current history. They are idempotent against work already active. A feature must have current approved scope and readiness before implementation, while a clear standalone issue has no separate feature-plan approval requirement.

Pause is a human control, not a task status. There is no separate manual child Pause workflow and no global subagent-count pause. Continue resumes the coordinator and eligible descendants while preserving each independent dependency, approval, resource, budget, ownership, and reconciliation hold. Clearing one reason never clears another service's blocker.

## Redirect and interrupt

A redirect is a recorded instruction with received, acknowledged, and incorporated stages. Saving it is not proof it has reached the next safe step. Incompatible work is fenced; compatible work can continue within approved scope. Stop and project archival interrupt owned activity and retain uncertain outcomes for reconciliation.

After app closure or restore, stopped work requires the appropriate manual Continue/restart. A provider connection reconnecting, a notification being read, or a project being restored is not authorization to resume execution.

Inspect the run and named blocker to understand a wait. An expired capacity wait differs from a durable user pause. Before retrying an uncertain mutation, confirm the original receipt and external effect. Never report a child as complete merely because its process or conversation stopped. See [Recovery](help:recovery), [Resources](help:resources), and [Readiness](help:features).
