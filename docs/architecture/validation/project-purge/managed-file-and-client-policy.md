# DW-83 managed files and client state

Ownership follows explicit scope, not whether a Human or agent authored content.
A draft bound to a purged session is project-owned and appears in the reviewed
purge; a workspace/Home or other-project draft remains independent. The `new`
composer draft is workspace-owned in the current native chat. Navigation's selected
project alone does not change draft ownership. This refines the earlier broad
candidate wording about preserving Human drafts.

| Managed class | Exact ownership and completion rule |
| --- | --- |
| Artifact primary bytes | Exact digest/generation of removed versions, only after every independent live reference/share/pin has been accounted for. Use the existing generation lifecycle through a reviewed-set restriction. |
| Artifact staging | `staging/<operation>.part` follows the selected operation and its recorded generation. Quiescence and fresh identity checks precede removal. Never sweep an unrelated staging operation. |
| Artifact quarantine | Validated exact digest/generation prefix plus recorded file identity for generations losing all keepers. Include every matching owned quarantined incarnation in the file manifest; keep independently referenced evidence. Never traverse symlinks. |
| Artifact preview cache | Exact preview::preview_id(version ID), including recognized temporary writes for that version. The current preview ID is derived from the exact version, not project/path text. Remove only previews of removed versions; preserve shared-version previews and unrelated cache entries. |
| Browser session profile/upload roots | Exact browser_sessions.id with project_id=P, after process ownership proves the session stopped. Inventory the managed browser/sessions/<ID> root; remove only its reviewed files/root. Browser recovery/close already removes stopped profiles; an absent root is idempotently complete. Uncertain process ownership prevents purge. |
| Diagnostic export staging | Persist ticket/project/root/file identity attribution before new staged bytes become visible. Existing ticket/path-only legacy state cannot prove project ownership after restart: preview remains held until normal diagnostic owner reconciliation proves the unattributed legacy set empty. A purge does not trigger a global staging sweep. |
| UI pasted image staging | Only a file marked owned in a selected session's draft/pending record, directly under the verified staging-ui root, and not used by any surviving draft/receipt. Picked/dropped user files are inputs and remain untouched. Workspace/new-composer paste files remain independent. |
| UI plan/convention staging | Native feature plan and requirement decision Markdown follows the plan snapshot project. Convention editor text/portable manifests follow their typed convention scope. Reserve and bind identity before private bytes; keep every surviving pipeline/receipt reference. Workspace editor staging and externally picked import files remain independent. |
| UI draft/view memory | Remove selected session-bound drafts/routing state and typed P-owned pending snapshots/commands; preserve workspace/other-project drafts and independently scoped settings. Pending original frames settle through retained receipt fences before discard; they never issue a new key. No arbitrary text matching. |
| Upload/source handles | Revoke exact project-bound in-memory handles after logical purge. Uploaded input paths and Git work-output files remain unchanged. Reopening a data root does not recreate these short-lived handles. |
| Packages/provider/global caches | Independently owned global service data remain. A project reference does not transfer ownership. |
| Git repositories/worktrees/source files | Remain unchanged, including harness-created worktrees. Removing a harness ownership record is not a Git cleanup command and does not remove the resource. |
| User exports/backups | Independently owned copies remain; active backup/read pins are explicit keepers. No claim of secure erasure or removal of external copies is made. |

Physical intents include managed-root identity, exact relative identity, measured
bytes and witness digest. Fresh inventory before confirmation detects changes; root
or child replacement, new live references, readers and unrelated paths do not grant
a deletion. An error after logical commit exposes the exact remaining intents and
retains ownership needed for restart/retry. Physical completion includes selected
staging, quarantine, preview and browser bytes, not just the primary blob path.

Client state must participate through the same host purge coordinator, including
calls initiated through agent tools. Durable file/state effects are recorded under
the single purge identity; UI-only click handlers cannot be the business owner.
