# Package process protocol and cancellation

Protocol version 1 uses four little-endian length bytes followed by exactly that many bytes of one UTF-8 JSON object. Frames are nonempty and at most 1 MiB. It is not newline-delimited JSON, JSON-RPC batch transport, or an arbitrary stdout stream. Use `frame::read_frame` and `frame::write_frame` from the SDK; partial headers/payloads are errors, not messages.

## Host lifecycle

The host sends JSON-RPC 2.0 requests `host.initialize`, `host.activate`, `host.invoke`, `host.cancel`, and `host.shutdown`. Return exactly one method-appropriate result/error with the matching request ID. Initialization negotiates a supported version and binds the expected manifest digest and contribution IDs. Activation binds registry/state/configuration revisions and includes saved configuration only when present. An invocation includes the actual process session, invocation ID/token, contribution, scope, allowed operations, registry/state/authority/control revisions, operation ID, deadline, and immutable input artifact.

Never invent or copy authority from another invocation. The host owns actor and scope; tokens are private to the actual process/invocation. Stale process sessions, foreign IDs, duplicate replies and out-of-order events are protocol failures. Dynamic data belongs in bounded exact artifacts, not an unbounded control frame.

## Calls and streams

Packages request `broker.call`, `broker.state`, or `broker.commit_event` with stable action keys. The host rechecks current permission and publication. `broker.call` checks each call against the invocation's grants. Application operations work: they run as the invoking principal and answer with a result artifact. Brokered HTTP, inference and artifact calls are not yet active and are refused as `Unavailable`, and this host records the intents of `broker.commit_event` without executing them yet. Progress and `package.event` are notifications, not terminal success. Streams respect explicit credit; a heartbeat or cancellation acknowledgment does not prove that work or effects finished.

Return Completed with exact artifacts, Failed with a code, CancelledBeforeEffect only when true, or OutcomeUnknown naming an actual operation. A cancellation after possible dispatch must not be reported as if nothing happened. Stop local work before acknowledging shutdown; ignored cancellation can cause supervised process termination, restart-budget use or quarantine.

The [echo helper](help:sdk-start) is a small real framed example. See [Event transactions](help:sdk-workflows-events), [Tools](help:sdk-tools), and [Recovery](help:recovery).
