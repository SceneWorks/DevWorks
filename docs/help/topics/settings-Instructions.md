# Role instructions, skills and adoption

Workspace coordinator, feature coordinator, issue coordinator, worker, code reviewer, Dreaming and scheduled roles each have editable instruction text. Project overrides can inherit from workspace. Instructions and skills cannot grant tools, override an actual human direction, remove reviewer read-only enforcement, or bypass root controls. Continuing existing work retains history rather than starting a new identity.

## Versions and approval

Every role instruction and skill keeps immutable versions. Your own save takes effect at once. An agent can draft a change and propose it, but nothing it drafts becomes active until you approve that exact version in Approvals; a pending or rejected proposal changes nothing. A draft by an agent working in one project stays visible only in that project, and to you, until you approve it; the approved version then applies in every project. Running agents adopt a saved or approved version at their next safe step, without a restart. History shows each version, who wrote it, and whether it was approved. Restore makes an earlier approved version active again as a revision-checked change; reset to default restores the first recorded version.

## Skills

Shared skills are read in place from `~/.agents/skills` and are never modified by the app. The app's own skill folder (`skills` in the application data folder) holds local skills and same-name overrides; an override takes precedence over the shared skill with the same name. Saving or approving a change to a shared skill creates an override and leaves the shared original untouched. A change made directly in a skill folder on disk, or a skill folder seen for the first time, is recorded as a new version that waits for your confirmation, because the app cannot tell who wrote it; the previous version, if any, stays active until you choose Confirm on the new version in the skill's history. Agent commands cannot write into either skill folder. Each skill shows its origin, location, active version, any change awaiting your confirmation, and whether it is in effect or overridden. Documents, scripts and assets in a skill folder are listed with exact digests and resolved from that folder; scripts run only through the normal tools and permissions.

## Apply a change

Choose the intended scope, read the active version, edit, and save. For a revision conflict, refresh and review the newer version before retrying. Adoption timing and harness-enforced restrictions are listed in the [Complete settings reference](help:settings-reference).

See [Settings overview](help:settings), [Permissions](help:permissions), and [Recovery](help:recovery).
