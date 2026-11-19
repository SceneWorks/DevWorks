# Workflow gates, events, and durable state

Workflow contributions declare Step or VerificationGate roles and an exact evidence schema. Verification events can carry a pass flag and evidence artifacts, but host acceptance still binds actual work/source and current policy. A helper saying passed cannot waive required tests, review, permissions, or incomplete requirements.

EventHandler contributions list supported event schemas. Process an actual delivered event under its bound subscription, invocation token, registry/state generation and checkpoint. Stable event/action identities support replay and deduplication; they do not authorize changing another subscription's state.

To request an application command as a durable intent, list its exact `devworks-command/<operation>` in the handler's `intentCommands`. Adding a command expands the package's requested access. Intent execution still requires the approving person's current authority and the selected command's registered input schema. A package handler cannot run human-only commands.

## Commit atomically

The SDK `broker.commit_event` binds `processSessionId`, `invocationId`, `invocationToken`, `subscriptionId`, `eventId`, `expectedCheckpoint`, `stateGeneration`, `expectedStateRevision`, optional exact `stateProposal`, and a bounded list of action-keyed intents. The host commits the processing receipt/checkpoint and accepted state/intents together. Replaying the same processing identity must not duplicate effects. A changed proposal or stale generation is refused rather than reconciled by overwriting current state.

For ordinary state use `broker.state`: Read or CompareAndSet with expected revision and exact proposal artifact. Ship the exact state schema as a schema asset. A state proposal uses an invocation artifact with live, visible JSON bytes that validate against that schema. This check also applies to a new event commit. A package update that changes its state contract is a compatibility transition, not an excuse to reinterpret old state silently. Keep evidence for migration or refuse an unsupported transition.

A scheduled job can call only actual registered qualified handlers. Declaring a workflow, event, or maintenance name does not install the autonomous workflow engine. Inspect host availability before exposing it to users. See [Jobs](help:jobs), [Protocol](help:sdk-protocol), [Add-ons](help:addons), and [Recovery](help:recovery).

## Select required workflow gates

Use `addons.run_workflow` with a workflow ID, the exact approved step and manifest digest, an immutable input artifact, and the required gates. Each gate entry names its package, contribution, manifest digest, and selection reason. The person or authorized agent who starts the workflow selects its gates before dispatch. Later runs retain all required gates for that workflow ID. The responsible agent can add checks or select a newer approved version of a bound contribution. A new source or manifest requires fresh evidence. The host retains each run's source and gate outcomes.

The step receives the input document and returns a result artifact. The host supplies each gate with `workflowOperation`, `source`, `sourceDocument`, `stepResult`, and `stepDocument`. Each gate returns its result and emits a `Verification` event with passing evidence that satisfies its declared schema. A run with unavailable, failed, or stale required evidence cannot complete. A gate retry for the same source and step version reuses the recorded step result and checks the gates again. Existing required tests, reviews, and approvals still apply to delivery. A scheduled workflow uses the same operation and observes its durable outcome.
