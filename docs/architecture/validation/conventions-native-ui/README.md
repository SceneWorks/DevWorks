# Native Conventions manager qualification

This is an implementation checkpoint, not completed DW33 acceptance. The manager uses the shared convention operations; it has no successful stand-in backend and no local convention database.

## Current native surface

| Shared operation | Native entry point |
| --- | --- |
| conventions.list | Explicit Workspace/Project scope, applicability/lifecycle, title/organization order, captured-filter paging |
| conventions.read | Captured version, Markdown/raw/portable passages, exact ranked section path, operation status, binary attachment pages |
| conventions.edit | Complete source editor, original artifact identity, separate explicit revision rebase; attachment attach/update |
| conventions.version | Attributed saved versions with paging |
| conventions.applicability | Independent applicability and Active/Retired update |
| conventions.retire | Retire document; revision-fenced cancellation of a cancellable operation |
| conventions.index | Selected convention or the observed whole-scope revision |
| conventions.import | Streaming portable creation/import, actual artifact ticket/save, durable accepted operation |
| conventions.delete | Explicit confirmation bound to the selected document revision |
| conventions.move | Independent move draft, explicit destination scope/parent/order, authoritative parent pages |
| conventions.resources | Document catalog, exact root document, revision-bound attachments |
| conventions.delete_resource | Explicit confirmation bound to the opening document/resource identity |
| conventions.search | Literal query/applicability/lifecycle, server-owned top-K, retrieval mode/gaps, exact ranked section identity |

Full source editing first collects all owner pages and checks the opening artifact length and SHA-256. Only a bounded editing passage is shaped by egui; edits replace that passage in the complete source buffer. A source page is never a document-size limit. The shared portable writer streams manifests and verifies captured source bytes. Actual artifact quotas apply to their serialized bytes.

Artifact-save and convention-command frames use the normal client command lifecycle. Unknown outcomes retain the original frame/key. Persisted continuation metadata contains genuine artifact references and staged-file paths, not a second convention store or duplicate full source. A recovered request cannot clear a newly opened draft. An accepted background operation is shown as accepted until the owner supplies its actual status; cancellation uses that status's revision and permission.

The embedding setup link only navigates to the existing provider settings. Convention scope is retained independently from the workspace embedding-space selection.

## Evidence at the source checkpoint

- `cargo check -p devworks-desktop --lib`: pass.
- `cargo clippy -p devworks-desktop --lib --no-deps -- -D warnings`: pass (core dependencies still emit known in-progress service warnings).
- Native unavailable/offline/near-limit source cases: 3/3 pass, 7.22 seconds. The first case uses the real production gateway returning the not-yet-registered owner; it does not assert successful convention business behavior.
- Native actual Help context/return case: 1/1 pass, 13.79 seconds, after correcting the new guide to avoid an unsupported decorative arrow glyph. This exercises actual local Help publication/read and preserves convention scope and unsaved source.
- Native embedding setup navigation: 1/1 pass, 6.45 seconds, with automatic accessibility checking enabled. Opening and returning retain the convention scope/source and emit no router configuration/qualification command. The embedding panel remains owned and qualified separately.
- No inference compute or remote network was used.

Logs: `/tmp/devworks-conventions-ui-check.log`, `/tmp/devworks-conventions-ui-clippy.log`, `/tmp/devworks-conventions-native-unavailable.log`, `/tmp/devworks-conventions-native-help.log`, `/tmp/devworks-conventions-native-setup.log`.

## Required acceptance still pending

Successful create/import/edit, full source/export fidelity, versions, hierarchy, applicability/lifecycle, resources, search/index, background cancellation/recovery and stale/unknown request regressions must run against the actual registered ConventionService, runtime workers and indexed SQLite store. Those shared handlers are being composed by their owners. This checkpoint does not replace that qualification or narrow the thirteen-operation scope.
