# DW-83 receipt and physical completion policy

Frozen step0 version1 decision, confidence high. table-selectors.json and
storage-guard-policy.md define its owning rows and exact immutable guard.

A purge retains a compact ledger tombstone under the original actor/scope/key.
It includes operation, contract, schema revision, fingerprint, command ID, committed
sequence, original decision class, operation ID when present and purge identity.
It contains no private arguments, typed result, title, rationale, source URL or
reply bytes. Current authority and exact intent checks still precede lookup/replay.
An exact match cannot reach use-case execution; changed intent remains a conflict.
A new Purged ledger entry must be handled explicitly by gateway, scheduler recovery,
reply publisher and receipt-observation readers; it is neither missing nor corrupt.

Revision6 adds an explicit purged receipt observation and terminal command outcome.
They distinguish the original committed decision class from the permanent removal
of its private content. They never fabricate an original Refused decision, an
unknown commit or an unpublished reply. Old revision1–5 schema files remain byte
exact. An old-reader replay which cannot represent purged content receives a
permanent schema compatibility hold; its original receipt identity remains queryable
through the current native receipt lookup. The native client resolves that hold
before presenting its result, and must say that content was purged after a committed
command, rather than claiming the command did not run. No new request key or effect
is issued by this resolution. All applicable UI, native scheduling and publication
consumers handle this terminal outcome explicitly.

Only exact project-owned receipts lose content. The purge confirmation's own
receipt and minimal purge audit survive independently; preview/confirmation contain
counts, opaque identities and content digests, not copies of removed business text.
No surviving independently owned receipt is rewritten merely for mentioning P;
recognized automatic project snapshots are scrubbed by typed fields and their
project references render the minimal deleted identity. Independently owned prose and
shared content retain their own ownership; authorship alone does not override scope.

The logical purge and exact managed-byte deletion intents commit in one SQLite
transaction, including the revision-bound manifest, selected immutable-row guard,
all selected records, minimal identity substitutions and execution revocations.
External unlink is outside the writer. The manifest's exact reviewed generations
are the only allowed physical effects. Existing reference, reader and backup pin
checks apply; a changed keeper refuses stale confirmation before logical commit.
After commit, unexpected new references/pins prevent unlink and expose a recoverable
hold, never silent content loss. Recovery adopts exact recorded generation claims.
A partial physical failure remains visible and retryable under the original purge;
status cannot report complete until every intended generation is removed or has a
recorded independently owned keeper. Never promise a filesystem rollback after a
SQLite failure, and never run the global garbage sweep as an implicit purge effect.

Transient host staging follows the same rule: durable exact project attribution,
managed-root identity and file identity/digest precede any reviewed delete intent.
Legacy unattributed staging must reconcile to a proved empty state before preview,
without deleting user inputs, Git work outputs or unrelated workspace files. Workspace/other-project client drafts survive; selected session-bound drafts,
automatically persisted project caches and pending frames are removed/settled by
typed ownership. managed-file-and-client-policy.md covers all managed byte classes.

Required acceptance includes old-frame replay after restart, changed-intent conflict,
revoked authority, already-purged content lookup, no accidental reexecution by the
scheduler/publisher/client, old schema hashes, shared artifact/history survival,
physical failure/retry/restart, pins, mixed scopes, and unchanged shared repository
and source filesystem bytes. No OS credential access or learned inference is needed.

Revision6 representation: CommandReply.ReceiptPurged is terminal and carries only
PurgedReceiptOutcome. The receipt lookup remains state Recorded (the original
commit exists) with a gated purged outcome marker instead of private reply refs.
Its original command/sequence/operation must match the outer committed identity.
Earlier readers never receive a fabricated Recorded observation without a reply.
