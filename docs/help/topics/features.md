# Feature plans, tasks, and readiness

A feature groups an approved outcome and its tasks. Its List and Board views refer to the same durable records. Create or edit the feature, describe observable acceptance, choose relevant repositories, and arrange tasks and dependencies. Tasks and issues can have explicit Story/Task/Bug metadata; features, tasks and issues have authorable labels. The app does not infer type or labels from titles or membership.

## One active plan and one pending revision

A feature has one active approved plan and at most one pending replacement. Review the exact proposed content, scope, repositories, and revisions before approving it. Approval atomically promotes that snapshot. Rejection leaves the active plan unchanged. A pending draft cannot authorize additional implementation or revoke otherwise eligible work in the active plan. Editing a current draft after requesting approval can make the older proposal stale; inspect the actual reviewed snapshot.

## Readiness and delivery

Readiness is separate from plan approval. An assessment can be ready with zero findings. A blocking finding identifies the requirement or evidence gap, its concrete consequence, affected work, attempted resolution, why ordinary autonomous resolution was insufficient, and what closes it. Local findings block affected work, not every unrelated task. Continue reuses current assessments; it does not demand another full review by itself.

Finding-specific human risk acceptance records rationale and exact scope/revision. It cannot create missing prerequisites, waive permissions or required verification, or mark incomplete work complete. Notification acknowledgment and generic plan approval do not resolve readiness findings.

## Membership and controls

Linking an existing issue preserves its identity, history, worker, session, branches, and workspaces. The coordinator takes responsibility for integration. Unlinking creates a durable pause; relinking cannot clear it. A transfer reconciles at a quiescent boundary and preserves independent holds. Workflow status is separate from pause/wait state. See [Issues](help:issues), [Approvals](help:approvals), [Workflow and allocation](help:workflow), and [Work controls](help:work-controls).
