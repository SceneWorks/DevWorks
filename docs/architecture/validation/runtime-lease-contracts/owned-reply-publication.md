# Native command-owned reply qualification (DW-138)

PR43/b777394b Ubuntu engine CI42314779894/job128224772588 failed the runtime boot
reply-consumer case: its target receipt existed, but its durable reply was still pending.
The fixture waited for the entire reply consumer's completed count to reach one. A reply
for an unrelated startup command can satisfy that predicate.

The strengthened native setup first commits and publishes a real unrelated sessions.start
command through the same runtime/gateway. With the old global predicate, the original target
observation deterministically fails ViewUnavailable/receipt_reply_artifact in0.53s.
The corrected predicate also requires the original target command's ReceiptReplyPublished
event. Publication appends this event atomically with the actual artifact/version, receipt
reply and queue settlement. Final typed receipt inspection remains unchanged. The original
global completion check, consumer-displacement rejection and30s watchdog remain intact.
No fake reply, product change, timeout increase or inference is involved.

All three actual native runtime cases pass2.47s, including crash/restart/exactly-once reply
recovery and deletion collection. Engine all-target strict Clippy passes0.48s. This causal
proof matches the previously unowned completion wait; fresh integrated platform CI and
review/merge acceptance remain required.
