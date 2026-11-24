# Standalone issues and feature membership

An issue is a task with a project and optional feature membership. It never needs a synthetic feature or manufactured feature cards. Create it with a title, description or reproduction, expected and actual behavior where known, acceptance criteria, priority, repositories, and source issue link. Story/Task/Bug and labels are explicit metadata, independent of whether the record is displayed as an issue or feature task.

A clear actionable standalone issue can proceed without a separate feature-plan approval. Existing permissions, pauses, blockers, verification, review, and delivery policies still apply. The presence of a Start action does not establish that the autonomous runtime is configured; unavailable execution is reported rather than simulated.

## Link, unlink, or transfer

Use the shared membership edit with the current work revision. Linking makes resolution part of feature delivery and transfers supervision without creating another identity. An existing worker continues through the controlled handoff; otherwise eligible unowned work can be selected. Unlinking pauses work until explicitly resumed. Direct transfers preserve the worker/session and existing branches while changing future supervision and budget attribution at the reconciled boundary. Past spending remains historical.

## Assign the working agent

While an agent works on an issue or task, its card offers **Assign to the working agent**: the card then records that agent's live assignment, and its Agents section names the agent. **Clear assignment** removes the record. Neither starts, stops or hands over the work.

## GitHub linking and publication

Local issues stay local until you explicitly link them to an existing GitHub issue (`issues.link`) or publish them to a qualified repository (`issues.publish`). Publication journals a durable marker and reconciles lost replies through read-only observation before retrying so duplicate remote issues are not created. Clickable GitHub links carry `owner/repo` context across work details, messages, reviews, and notifications.

## Dependencies

Blockers have stable IDs. Self-links, duplicates, cycles, invalid scope, and stale revisions are refused atomically. Adding a blocker fences affected future dispatch. Finishing or removing one dependency does not clear user pauses, approval waits, budgets, or resource holds. Independent work can continue.

Use History to distinguish a membership edit, an execution stop, a tool effect, and a verified completion. See [Features](help:features), [Workflow](help:workflow), [Controls](help:work-controls), and [Recovery](help:recovery).


## Code reviews

Open **Code reviews** in the work detail to inspect each reviewed commit or local diff, its source, findings, and responses. Record a response or explain why a finding is not actionable. DevWorks keeps the original reviewed head in history when it verifies a repair. A new head or changed policy can require additional evidence before delivery.
