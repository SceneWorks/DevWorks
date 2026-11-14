# Desktop control and screen capture

DevWorks can let an agent look at your screen and operate the desktop. Reading the screen and controlling it are separate steps, and an agent can control the desktop only while you have approved that exact request.

## What the five operations do

- `desktop.inspect` reports what this computer allows: the platform, the permission state, and a bounded snapshot of the accessibility tree. Each node has an opaque target ID, a role, a name, whether it is enabled and which actions it offers. The snapshot has a revision. If the tree was too large, the snapshot says it is truncated.
- `desktop.capture` takes a PNG screenshot and stores it as an artifact in the current scope. The request must name the desktop and its current revision. A stale revision is refused with a revision conflict.
- `desktop.request_control` asks to take over the desktop for a stated reason and a lease length in milliseconds. It is refused as unsupported when neither input nor accessibility is available on this computer, and as a conflict when the desktop revision is stale.
- `desktop.input` sends one action: a click at screen coordinates with a mouse button, typed text, a key with modifiers, a scroll, or an action on an accessibility target (invoke an action, set a value, or focus).
- `desktop.release_control` ends the lease early.

## Consent

Every control request waits for you. The approval card reads "Allow desktop control" followed by the agent's reason, and it shows the desktop, its revision, the lease length and the permission state. A request made without an approved decision is refused with ApprovalRequired. An agent cannot approve its own request; only a person's decision counts, and it applies to that one request.

When you approve, the request takes the single desktop resource for the lease length. If another holder has it, the request waits in line and the desktop stays in a partial state until the lease is granted. Approval ends with the lease. When the lease runs out or you release it, the agent must ask again.

Each `desktop.input` must carry the lease ID, the lease's fence number, the approval's ID and the desktop revision. If any of these no longer matches (the lease expired, was released or passed to another holder, the owner or session differs, or the approval is not a person's approval of this lease), the input is refused with LeaseLost, PermissionDenied or a revision conflict. An accepted input is queued as a durable operation and runs once. If DevWorks cannot tell afterwards whether the input took effect, the outcome is reported as unknown. It is never repeated.

An agent's screenshot also needs a live, approved lease. A screenshot you request yourself does not.

## Permissions of the operating system

Screen capture and input are separate operating-system permissions, and having one does not give the other. When a permission is missing, the action fails with a permission-required result that names the missing capability. Grant it in the system settings, then try again. A platform that cannot do something reports Unsupported. DevWorks does not simulate a missing capability.

An action on an accessibility target works only against the snapshot it came from. If the screen changed since the snapshot, the action fails with a changed-target result. Inspect again and use the new IDs.

## Work in a task worktree

When the request runs inside a task's Git worktree, DevWorks records the tested repository state with the screenshot or the input. If the repository changed between the request and the action, the request is refused with SourceChanged.

## Which agents can use it

The desktop tools are off by default for agents. Turn them on in the agent tool selection under [Agent tools](help:settings-Tools) before an agent can call them. A read-only reviewer cannot request control or send input, because both change state. Read the reason before you approve, and release control when the work is done. See [Resources](help:resources), [Approvals](help:approvals), [Browser](help:browser) and [Permissions](help:permissions).
