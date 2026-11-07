# DevWorks closeout and fresh-session handoff

The requested stopping point is the reviewed, CI-qualified merge and acceptance of
DW-83, its DW-140 contract repair, and publication of the previously committed
GitHub foundation tracked by DW-149. Then stop; another feature needs a fresh
session. The remaining DevWorks project is not complete. The final CodeGraph
acceptance reports and PR merge records carry the exact final commits and CI
results; this document explains the implementation and continuation boundaries.

## Delivery records

- [PR50](https://github.com/SceneWorks/DevWorks/pull/50): DW-83 project purge and
  DW-140 positive history-contract fixtures. Four revision-6 operations reach the
  same Core coordinator from native UI and applicable tools: prepare, paginated
  manifest, exact Human confirmation, and status. Generic Human/agent dispatch
  cannot mint confirmation. Restored frames resolve their original receipts.
- [PR52](https://github.com/SceneWorks/DevWorks/pull/52): DW-149 publishes the old
  DW-40 storage/conditional-HTTP checkpoints through `2b165d75`. This establishes
  canonical GitHub facts, endpoint cursors, one persistent polling clock, uncertain
  mutation storage, bounded conditional GETs and retained response metadata. The
  full DW-40 service, polling orchestration, runtime/UI and GH-01–12 acceptance
  remain open. A foundation merge does not complete that story.
- [PR51](https://github.com/SceneWorks/DevWorks/pull/51), merge
  `dc0386eff3cb8774d3374662cbdf2dd964447adc`: DW-139 catalog readiness fixture,
  DW-146 feature-revision regression, DW-147 shutdown fixture and DW-148 deaf-helper
  health-window fixture. Their item acceptance reports contain qualification.
- Earlier closeout deliveries: [PR48](https://github.com/SceneWorks/DevWorks/pull/48)
  (`cda0f95be4c015cfe4a75eadcae8b84efc4c83cc`) accepted DW-36 and related fixes;
  [PR49](https://github.com/SceneWorks/DevWorks/pull/49)
  (`c10d469c915692142998b1c0a77bf6acdfcce8c3`) accepted DW-88 and related fixes.

## Purge implementation and important invariants

The structural overview remains `CODEGRAPH.md`; consult CodeGraph knowledge first
and use current owning source for exact behavior. The purge implementation is in:

- Core `services/projects/purge.rs` and `ports/project_purge.rs`;
- adapter `storage/sqlite/project_purge/` and `fs/purge.rs`;
- desktop `project_purge.rs`, `memory/purge/` and `shell/project_purge.rs`;
- the conditional composition in Engine `runtime.rs` and desktop `host.rs`.

Schema38 adds current-boot effect leases over schema36 manifest storage and
schema37 staging custody. Earlier contract schemas remain compatible. The closed
native schema/ownership policy intentionally refuses unknown tables, columns,
indexes and triggers. A future migration must update that policy and its native
qualification in the same work; never bypass the refusal.

Preparation checks native ownership, authority, deleted/quiescent lifecycle,
unfinished journal operations, unsettled non-control deliveries, client producers,
managed file attribution and exact file/read witnesses. Confirmation rebuilds the
same content witness at the reviewed project and manifest revisions. Its opaque
permit cannot be deserialized from input. The logical writer keeps FK enforcement
and exact-row immutable guards. Shared content and required history headers survive
with finite minimal projections. The schema-source membership projection preserves
independent configuration and other projects' leases.

Only validated minimal purge-control publication is excluded from business row
and file witnesses, preventing a preview from staling on its own reply. Other
pending receipt publications must settle first. Never exempt arbitrary replies.

Logical deletion atomically records exact file/client effects. Recovery claims the
original bodies under current-boot leases and verifies them against the immutable
manifest. Scrubbed managed-file metadata is not reinterpreted as new authority:
only an exact committed review intent permits that recovery path. Ordinary file
inventory/removal still refuses replacements and forged current witnesses. The
actual artifact reader registry gates blob/quarantine removal. Its lock callback
must not acquire a SQLite read connection.

Native client custody checkpoints live and saved state, refuses unrecognized
shapes, uses bounded create-new/atomic writes, and verifies restored fences against
committed manifest row hashes. Interrupted writes hold cleanup. Ownership comes
from typed scopes/native IDs, never prose or a path substring. Independent paths
and exact artifact versions are keepers. The review freezes new client producers;
original unresolved commands retain their frames and receipt identities. Cleanup
waits for the live UI to discard scoped caches and acknowledge it. Old data queries
re-read at fresh snapshots, retaining cancellation options. Stale autosaves cannot
restore purged drafts. Saved manifest IDs restore status, never Human consent.

No Git cleanup is part of purge. Repository registrations, source files, branches,
worktrees and GitHub resources remain outside its physical removal authority.

## Reproducible evidence

`crates/devworks-desktop/tests/project_purge.rs` exercises the production composition
and headless native UI, including generic/stale confirmation refusal, complete
review, owned draft/paste removal, unshared blob/staging unlink, shared exact reads,
independently picked managed paths, unchanged actual shared Git state and repository
registration, ordinary/FTS-shadow SQLite privacy-sentinel scans, client autosave
fences, restart and saved-review status without renewed consent.

The adapter retained-subject case executes logical and physical owners and reads
minimal sessions, work, assignments and blockers through ordinary typed readers.
The focused purge regression set also covers every managed-file class, reader
fences, replacements, immutable guards, replay, recovery, private/shared ownership,
quiescence, schema drift and strict saved-state partitioning. The native HTTP/GitHub
foundation cases cover scoped pagination, validators/pacing, body/header bounds,
redacted metadata, canonical intake and restart-safe global timing.

Use the final item reports for exact-head results. The existing full independent
contract verifier covers 2230 fixtures and 85 schemas; DW-140 adds revision-specific
positive history continuation/empty-tail cases while retaining every verifier
assertion. Python3.14 was used for the modern verifier dependencies.

## Remaining work and tracker cautions

Re-read the live tracker before selecting work. At closeout discovery the actually
eligible ready items were DW-15, DW-22, DW-29, DW-31, DW-34, DW-35, DW-47, DW-48,
DW-53, DW-54, DW-55, DW-57 and DW-63. Eligibility/dependencies outrank priority;
DW-15 is a high-priority candidate on the coordinator/GitHub dependency path.

- DW-40: storage/transport foundation only. Its actual coordinator/tool-turn
  dependencies DW-15 and DW-20 and full orchestration/runtime/UI remain. Revalidate
  the complete approved GH requirements; do not substitute fixture evidence for
  live integration or repeat inference every polling tick.
- DW-37: the PR48 association is not acceptance of the separately permitted
  database-operation surface. Its full criteria remain unmet. Revalidate the
  current source and Confirmed/Proposed requirement states before taking it ready.
- DW-39: PR21 association is not delivery acceptance. DW-41 still gates exact-head
  review/readiness integration; its full delivery criteria remain unmet.
- Do not infer completion from `merged_awaiting_acceptance`, an old description,
  a prototype, or an incidental PR association. Check the actual service and every
  applicable source requirement.

## Workspace and operating constraints

The original checkout `/Users/michael/Repos/DevWorks` on `codex/ui-refresh` belongs
to the user and was left untouched. Session worktrees include
`/Users/michael/Repos/DevWorks-dw-83-project-purge` and
`/Users/michael/Repos/DevWorks-dw-40-github`. Preserve old branches as evidence;
start fresh work from verified `origin/main` in a new `codex/` branch/worktree.

Start a new CodeGraph session, read its brief/inbox and relevant `recall`, and read
this repository's instructions. Existing unrelated-test repairs have standing
approval; preserve intended assertions/timeouts and record meaningful changes.
Never assign work to another agent under the supplied instructions.

Use `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2`. Run one Cargo process per target
at a time; the purge target was shared at
`/Users/michael/Repos/DevWorks-dw-21-storage/target`, and the foundation target at
`/Users/michael/Repos/DevWorks-dw-52-help-jobs/target`. Avoid racing another worktree
on either target. Normal CI must not execute learned inference. Do not run blanket
`--ignored` tests or real OS keychain/trust probes; these fixtures use session
secrets and temporary stores. Preserve the explicit local-destination trust choice
in each native Git push approval. No customer project was purged by validation.
