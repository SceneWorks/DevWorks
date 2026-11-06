# Workflow statuses and eligible allocation

Projects have authorable statuses with stable identities, names, order, and explicit workflow semantics. Use the workflow controls rather than relying on a status's display name to decide whether work is ready, blocked, under review, or complete. Feature boards and issues use the selected project's workflow.

Removing an occupied status requires an explicit destination for its work. A rename does not remove history. Moving a card cannot bypass plan approval, verification, review, or delivery policy, and a workflow status is separate from execution Pause or wait.

## Select work safely

The native next-work operations inspect a project or feature queue and create a revision-bound reservation. Selection considers current eligibility and blockers before priority. Story/Task/Bug and labels are explicit filters, not words guessed from a title. Label matching uses whole labels without case sensitivity. A reservation is not an active worker and cannot prove liveness.

Use the returned claim identity and revision for inspect/release. Lost acknowledgment must replay the same operation identity rather than reserve a second owner. Current queue/work generations fence stale claims. A transfer or closed session can retire an old reservation; it must not leave a permanent phantom owner.

Keep-working defaults off. When supported and explicitly enabled, it selects one feature or standalone issue at a time per project; different projects are independent. Reopening requires manual restart, and later queue changes must not silently turn it back on. If the current runtime does not offer automatic execution, queue inspection and reservations remain distinct from launching work.

See [Issues](help:issues), [Controls](help:work-controls), and [Agents](help:agents).
