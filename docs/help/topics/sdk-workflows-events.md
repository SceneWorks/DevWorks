# Workflow gates, events, and durable state

Workflow contributions declare Step or VerificationGate roles and an exact evidence schema. Verification events can carry a pass flag and evidence artifacts, but host acceptance still binds actual work/source and current policy. A helper saying passed cannot waive required tests, review, permissions, or incomplete requirements.

EventHandler contributions list supported event schemas. Process an actual delivered event under its bound subscription, invocation token, registry/state generation and checkpoint. Stable event/action identities support replay and deduplication; they do not authorize changing another subscription's state.

## Commit atomically

The SDK `broker.commit_event` binds `processSessionId`, `invocationId`, `invocationToken`, `subscriptionId`, `eventId`, `expectedCheckpoint`, `stateGeneration`, `expectedStateRevision`, optional exact `stateProposal`, and a bounded list of action-keyed intents. The host commits the processing receipt/checkpoint and accepted state/intents together. Replaying the same processing identity must not duplicate effects. A changed proposal or stale generation is refused rather than reconciled by overwriting current state.

For ordinary state use `broker.state`: Read or CompareAndSet with expected revision and exact proposal artifact. A package update that changes its state contract is a compatibility transition, not an excuse to reinterpret old state silently. Keep evidence for migration or refuse an unsupported transition.

A scheduled job can call only actual registered qualified handlers. Declaring a workflow, event, or maintenance name does not install the autonomous workflow engine. Inspect host availability before exposing it to users. See [Jobs](help:jobs), [Protocol](help:sdk-protocol), [Add-ons](help:addons), and [Recovery](help:recovery).
