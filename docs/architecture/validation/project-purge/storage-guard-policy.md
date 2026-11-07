# DW-83 storage guard and retained identity policy

The complete selector inventory is table-selectors.json: all240 current tables,
including the three virtual FTS owners and their15 shadow tables. It is tied to
schema35's observed schema digest. No broad FK traversal grants deletion. The
explicit service ownership sets determine selected rows first; foreign keys only
identify required ordering and surviving minimal identity references.

Purge uses a new migration with both reader and writer floors raised. Old builds
cannot interpret compact receipt tombstones or minimal identity rows as full
business records. Existing revision1–5 wire readers on a current build remain
supported under the explicit compatibility policy.

Existing immutable DELETE guards remain effective for ordinary transactions. A
connection-local, default-deny purge guard permits only the exact manifest-selected
table, canonical primary-key digest and Delete/Scrub action while the reviewed purge
storage method runs. It is not a caller capability, global flag, persisted leftover
marker or blanket trigger disable. A scoped Rust guard resets it on success, error
or unwind. Reader connections cannot enable it. Trigger exceptions check that exact
row membership; ordinary immutable UPDATE rules stay intact unless a specifically
reviewed minimal identity requires a matching Scrub action. The migration does not
change the historical checksums of migrations1–35.

Cyclic owned FKs may be deferred within this one writer transaction; foreign key
enforcement stays enabled and every constraint must pass at commit. FTS deletion
uses the virtual owner, maintaining its shadow records; it never edits shadow
storage directly. Ordinary cleanup APIs retain their existing limits and semantics.
The purge method cannot accidentally opt an unrelated collector into its guard.

Only identities needed by surviving typed references remain. The project always
retains its ID, Deleted lifecycle, monotonic revision/scope fences and deletion time,
with a fixed deleted label and no private title/description. A new purged-subject
registry makes native queries render minimal deleted identities before ordinary
record decoding. Surviving references do not reconstruct the old name or payload.

Known cross-owner keeper cases are explicit:

- A kept shared/workspace artifact's origin project/work references may require
  minimal project/work headers. Keep its independently shared artifact/version
  content and exact ACL, not the deleted work's brief, caller reference or title.
- A kept package processing commit may need the event's opaque ID/sequence/type
  and original outcome identity. Remove that event's private payload and root text.
- A global schema publication, source refresh, router qualification or other
  independent publication may need an operation ID. Retain its static capability,
  original terminal state and monotonic revision; scrub private payload and revoke
  any executable lineage. Never relabel an observed outcome Unknown.
- A kept typed integration/intake link keeps only remote/local opaque identities and
  an honest deleted local reference. Its remote resource and independently authored
  history remain. Automatically copied project titles/body snapshots are removed.
- A retained workspace artifact's native transcript link may require a minimal
  closed session header and its conversation identity. Keep only opaque IDs,
  monotonic counters and lifecycle attribution; remove the session title, pin,
  root/private transcript and checkpoint content. A rootless project session still
  owns its conversation through sessions.conversation_id. This is an explicit
  finite keeper case, not a generic FK retention or deletion rule.
- A surviving workspace/shared artifact may reference an assignment, project-private
  resource, or blocker. Keep a minimal terminal header for that exact typed subject.
  Assignment headers retain their required root/work/session identities but clear
  execution, parent, checkpoint and approved-scope text. Resource headers keep the
  project and monotonic fences with an opaque deleted label, without original name,
  canonical private key or labels. Blocker headers become cleared placeholders with
  no reason, origin text or clearance payload; nullable root/assignment/operation
  links are cleared. The native purged-subject registry prevents these placeholders
  from being decoded or reused as live work/resource/control records.
- A surviving work header may require a workflow-status identity: retain only the
  ID/project, fixed deleted label and valid static kind fields. It is a reference
  placeholder, not a live status or allocatable work item.

The keeper closure is finite and reported in the manifest. Every additional
surviving reference is evaluated against the actual typed owner, never guessed
from a matching string or FK alone. Unknown formats block preview until mapped.
Manifest-owned row keys may themselves contain private text (for example source
references): durable plans/audit retain only canonical key and row digests; exact
keys are materialized from a verified snapshot for execution and discarded. No
private source key is copied into a surviving purge receipt.

Scope-owned records and all their declared session/work/convention/knowledge/usage
lineage are removed unless an explicit independently owned share or required minimal
identity applies. Shared legacy knowledge entries retain their content and exact
surviving shares. Repository/source/index registrations, help, provider/router
catalogs and package state keep their independent owners. Remove only typed P access
edges or copied P snapshots in those records; for example schema_sources.projectAccess
loses P without deleting a workspace connection or its other projects' access.
Settings secret values remain under the existing shared reference-aware collector:
removing project settings references does not authorize a namespace-wide delete.

The schema allowlist includes the purge's new own tables when the migration is
introduced. Any later unclassified table or changed owning shape fails closed at
preview/confirmation. Native acceptance must seed every current ownership class,
prove ordinary immutable guards still reject deletion, and verify complete private
sentinel removal with shared/other-project/workspace keepers and source bytes intact.

The ownership assessment digest includes the actual compiled finite minimal-header
allowlist and retained/cleared FK column policy. Changing those rules invalidates an
older preparation even when no stored project row changed.
