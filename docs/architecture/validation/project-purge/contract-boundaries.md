# DW-83 native contract boundaries — discovery, not a frozen policy

Confidence in the facts below is high. The implementation and owning policy remain
pending; none of these observations authorizes deleting a live project.

- `gateway::unit_of_work` checks current authority before receipt lookup and compares
  operation, contract, negotiated revision and argument fingerprint before replay.
  A purged receipt must retain those fences and its deduplication identity. Erasing
  the key, fabricating another original decision, or treating a committed decision
  as a lost acknowledgement would break the existing contract.
- `history::command_receipt` requires a Recorded observation to carry the immutable
  typed reply artifact. Pending, held, abandoned and denied publication are distinct
  faults. A retained header with permanently removed private reply bytes therefore
  needs an explicit purged-content representation and old-reader behavior; returning
  Recorded without its artifact violates the current recorded rules.
- Receipt artifacts are system-owned and immutable under ordinary artifact use cases.
  Their audience is receipt-principal-bound, with fresh current authorized scope;
  project deletion cannot make them workspace-public. The reviewed purge needs its
  own narrow exception and a writer-version floor, rather than weakening ordinary
  artifact mutation or immutable-history rules.
- `ArtifactService::visible_in` preserves an explicit shared artifact's original
  project ownership and checks the surviving viewer's share. A deleted project does
  not itself require transferring that artifact to the workspace or changing its
  owner. Such a transfer could silently change its audience.
- `ArtifactLifecycle::collect` already claims exact blob generations under the writer,
  rechecks live references, respects backup and active-reader pins, unlinks outside
  SQLite, and records Unlinked or visible retryable failure. Recovery adopts previous
  claims. Purge should bind the reviewed digest set to this owner; a SQLite commit
  alone cannot honestly report physical-byte completion.
- Physical collection currently sweeps eligible global blob states. The purge's
  physical phase must be scoped to reviewed generations so unrelated pending garbage
  does not become an unreviewed effect of the confirmation.
- Migration24 eval reports and migration26 allocation replays reject all ordinary
  updates/deletes. Both have project ownership, including NoEligible replays with no
  claim. They must be previewed and explicitly handled by a manifest-guarded purge;
  project tombstoning must continue to leave them intact.
- The schema includes mixed-ownership tables: artifact shares and project repository
  memberships are edges; global repository/index/schema-source registrations are not
  owned by a project merely because its work refers to them. Operation identity is
  also a surviving FK target for independent global publications.

Source: native schema35 and the corresponding code at
`4d45aa5bed4f0b8da7b22a205d71d5073fbbc6f9`; graph discovery was used to narrow paths,
then actual current files and native DDL were inspected.

Additional verified selectors:

- The gateway's CommandDecided event records ownership from actual work targets,
  declared project targets, then current invocation lineage. Join a stored receipt's
  commandId to that event, not its scope_key: the latter encodes data root, restore
  epoch, ingress and optional worktree, with no project ownership.
- Graph and schema retention scope digests are SHA256 of the exact typed DataScope.
  Project-scoped leases can be selected from the canonical Project scope digest;
  read-pin view rows follow their pin. Owner hashes remain authority bindings, not
  a text format to reverse or a source of new authority.
- Eval reports take project ownership from their retained subject session. Their
  work/event/link evidence must match that session's project; the native records
  builder rejects a cross-project source. Allocation replays carry project_id even
  when no selection was eligible.
- Search chunks explicitly reference artifact_versions; vectors and search FTS rows
  follow their chunk. Source Git index generations and repository captures have
  separate global/repository ownership and cannot be swept merely because the
  deleted project used that repository.

ownership-map.json partitions all240 current tables into21 candidate ownership
classes. Its22 mixed-owner classes still need precise selectors and keeper rules;
its classification is not an enabled deletion plan.
