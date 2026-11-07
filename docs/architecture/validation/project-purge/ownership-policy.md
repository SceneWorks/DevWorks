# DW-83 ownership and purge policy — step0 version1

The owning requirement is the architecture's “Scheduling lifecycle and project
deletion” section and DW-83 AC1–3. Existing projects.delete only tombstones an
archived, quiescent project. No purge implementation exists in this branch yet.

A temporary native SqliteStore opened a new disposable data root at schema35 and
exported its schema only:222 ordinary tables,3 virtual tables,15 shadow tables and
72 triggers. schema-baseline.json records the actual table/column/FK inventory,
source commit and digest. It contains no user records. table-selectors.json now declares each ownership selector; mixed ownership,
receipt compatibility and storage guards are documented alongside it. These
policies remain a candidate until their combined freeze is recorded; an FK by itself
does not establish ownership. The temporary inspection example has been removed.

Frozen ownership principles (the companion documents settle the detailed decisions):

1. Derive ownership from a record's declared current project/scope and its actual
   owned-parent lineage. A reference to the project is not sufficient ownership.
2. Delete private owned content. Retain only necessary opaque deleted identities
   for surviving cross-scope references, plus a minimal deletion audit. The minimal
   client audit retains reviewed opaque record IDs and a digest so
   stale autosaves remain fenced across restarts; it contains no draft text or paths.
   Preserve independently authored other-scope history; redact automatically copied private
   project snapshots without rewriting other scopes' authored prose.
3. Retain shared artifact content under its existing exact access rules; never
   broaden access or silently substitute a different owner. Delete managed bytes
   only when no surviving version/reference/share/pin needs that content digest.
4. Preserve receipt request keys, argument digests and minimal outcome identity.
   Erasing private receipt content must not permit an old command to execute again.
   The revision6 purged outcome and old-reader hold follow receipt-and-completion-policy.md.
5. Bind confirmation to an immutable manifest and fresh current authority,
   quiescence, ownership and sharing witnesses. A changed affected record/share
   refuses stale confirmation. Only a native Human can confirm permanent deletion.
6. External managed-byte effects stay outside the SQLite writer. Exact
   logical commit and physical completion/recovery follow receipt-and-completion-policy.md.
7. Never change source Git resources, repository files, worktrees, branches or
   GitHub resources. Repository and global-service ownership need explicit entries.
8. Every current ordinary/virtual/shadow table must have a declared policy before
   the purge executor can be enabled; future unclassified schema additions refuse
   preview/confirmation rather than silently escaping the manifest.

Step0 is frozen as version1 with high confidence in the explicit ownership rules.
The machine-readable inventory has exactly240 table policies, with no unnamed
current class. Companion documents define mixed selectors, shared/minimal history,
receipt compatibility, immutable guards and every managed-file/client class. This is
an implementation policy, not a runtime confirmation to purge any project. New
schema/format discoveries require an explicit policy amendment before execution.

Implementation sequence:

1. Revision6 contracts and native receipt-purged compatibility, preserving every
   earlier schema; canonical admission/refusal and old-frame replay cases.
2. Durable manifest records, full ownership/witness construction, preview paging,
   current authority/quiescence and immutable reviewed revision binding.
3. Exact-row guarded logical purge and minimal reference projection; all240 table
   classes and existing immutable/FK/FTS rules participate.
4. Scoped generation/file effects, host staging/client attribution and restart-safe
   completion. No Git/source/remote cleanup.
5. Native Human review/confirmation/status UI using the same service as agents.
6. Integrated acceptance across ownership classes, shared scopes, stale/raced
   confirmations, replay/restart/failure recovery and unchanged source resources.
7. Review every finding, qualify CI on the exact head, merge and accept AC1–3.

No real project, OS credential facility, OS trust store or learned model is used by
local validation. Runtime permanent deletion always needs its own exact Human
confirmation of the manifest, irrespective of this implementation authorization.
