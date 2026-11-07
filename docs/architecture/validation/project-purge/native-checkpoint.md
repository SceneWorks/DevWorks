# DW-83 native implementation checkpoint

This is unfinished implementation evidence. No purge operation is routed, no purge
executor or Human confirmation UI is enabled, and none of DW-83's acceptance
criteria is complete.

The current support layer includes revision 6 contracts and terminal purged receipt
replay, migration 36 manifest storage and exact-row immutable-delete guards, durable
managed-file Writing/Live/Removing/Removed attribution, and native file inventory
and reviewed-entry removal for all seven frozen managed classes.

Diagnostic exports reserve exact project/root/name attribution before writing and
publish a verified file witness before issuing a ticket. Normal diagnostic owner
cleanup reconciles completed or missing reservations after restart. Project preview
cache writes also reserve before writing; unfinished writes block preparation and
the logical purge fence refuses new project cache writes. Previous-boot writes
cannot publish under the new boot. Workspace previews remain independent.

File witnesses bind project, store, restore epoch, owner, root/file identity,
relative entry, byte count and content digest. Transient diagnostics and paste
entries must match their published attribution, including contents. Inventory and
removal refuse replacements, wrong epochs and forged owner-relative names. Browser
trees include directories and leaf links without following their targets; new
unreviewed children prevent empty-directory removal. Native Windows joins push
validated components separately for verbatim roots. Windows runtime qualification
is still pending; a junction fixture is included for that platform.

Native macOS qualification on 2026-11-07:

- 29 real SQLite project cases passed in 22.81 seconds, including three file-effect
  cases across all seven classes, changed files, cancellation, wrong bindings,
  source/Git preservation, managed reservation/restart fences and preview recovery.
- The published transient-witness regression first failed in 0.83 seconds when
  changed bytes were incorrectly accepted as a new owned file inventory.
- 11 production Engine diagnostic cases passed in 12.91 seconds.
- Eight native artifact filesystem cases passed in 2.25 seconds.
- Workspace/all-target strict Clippy passed in 40.59 seconds.

The project storage run initially exposed an outdated new test assumption that a
restarted host could publish a previous boot's Writing reservation. The test now
asserts that refusal, retains the restart and pending-write checks, and separately
validates current-boot publication, wrong-root refusal and retirement.

The next checkpoint adds a sealed Human purge proof bound to the complete typed
input and current actor/store/restore/ingress/lineage/authority context, with a
dedicated Engine and desktop ingress. Two core proof cases and six native client
dispatch/restart cases pass. Restored purge frames preserve their exact bytes/key
but use generic receipt resolution: a stored dispatch flag cannot mint a fresh
Human confirmation. Unsupported native ingress never falls back to a generic
command. These checks qualify the proof plumbing; they do not establish the
unimplemented service's authorization, execution or Human UI acceptance.

Draft CI caught a missing generator-test expected-set update for the four new
project operations and revision 6 receipt observation. The explicit set now names
all five and checks their exact introduction revision. All 13 generator
assertion tests pass, regeneration produces no source drift, and workspace/all-target
strict Clippy passes (39.26 seconds).

All 44 desktop unit cases pass (5.38 seconds). A native graph case (0.83 seconds)
uses identical owners/view sets for Project P, another project and the workspace;
the candidate selector finds only P's lease and its two child index pins, leaving
all leases intact. All 116 candidate SQL predicates resolve without mutation
(0.74 seconds); final assembled strict checks pass (42.07 seconds). These candidates
still require the complete ownership/keeper classifier before deletion authority.

Durable row witnesses now round-trip with each preparation. They contain only an
actual table name, key/content digests and fixed action/reason enums. The immutable
manifest digest includes the canonical sorted row set. Duplicate witnesses refuse
atomically; changing a persisted row decision invalidates a prepared manifest.
All 31 project-storage cases pass (26.52 seconds), with strict workspace/all-target
checks (42.28 seconds). This persists review evidence; it does not enable the row
guard or execute the unimplemented logical purge.

The native repository now exposes a read-only candidate snapshot to the classifier.
Primary-key tuples use the exact guard encoding; full row content uses typed,
length-delimited hashing, including binary cells. Only table names and digests
leave the reader. Two scoped selection cases pass (1.43 seconds): changing another
project leaves P's snapshot unchanged, while changing P's private title without a
revision bump changes the snapshot. A real immutable allocation replay guard accepts
the candidate key hash and still refuses ordinary deletion after rollback
(0.99 seconds). Strict workspace/all-target checks pass (17.56 seconds).
Candidate types deliberately carry no action or permit; mixed-owner classification
and the guarded logical executor remain required.

The read-only snapshot now covers 145 SQL predicates and 27 typed lineage anchors,
including shared/moved artifact content, legacy knowledge, actual event-attributed
receipts and reply lineage. Four native cases pass (3.35 seconds), every predicate
resolves against the actual schema (0.80 seconds), and strict checks pass (17.10
seconds). Project creation receipts are workspace-attributed; their typed project
snapshot still requires explicit redaction in the complete classifier.

Scoped native foreign-key witnesses now include hashed referring/referenced keys
and exact schema column names. They do not expand deletion ownership. Five native
cases pass (5.57 seconds), including a new independent global publication reference
that changes the snapshot while leaving the owned row set unchanged, implicit parent
keys and composite source key encoding. Strict checks pass (39.87 seconds). The
finite keeper closure and typed non-FK references still require classification.

Draft platform CI found missing fixture forwarding for project preview custody.
The native fixture now supplies its ApplicationStore and forwards the project cache
methods through fault wrappers. The original cache assertion fails before correction
(0.92 seconds); all 49 artifact cases pass after it (39.73 seconds). All 32 project
cases pass (26.73 seconds). Windows junction arguments now use native path components;
its runtime acceptance remains pending. DW-143's duplicate runtime-slot fix is also
composed: all six native convention scope cases pass (26.66 seconds). A separate
schema pin writer-clock race is repaired in DW-144 and composed here. The combined
branch passes all 15 native schema cases (12.58 seconds), all 14 database UI cases
(48.28 seconds), and workspace/all-target strict checks (42.35 seconds). These local
checks do not establish the pending Windows filesystem runtime qualification.

The local next checkpoint adds typed non-FK witnesses for the six native artifact
SubjectKey variants, index overlay runs, package intent operations, FTS chunks and
schema certificate versions. Five native cases pass (5.55 seconds), with strict
checks (38.97 seconds). The package-intent format is explicitly closed: unknown
fields or shapes refuse planning without mutating stored content (five native
cases, 6.19 seconds; strict checks, 15.91 seconds).

An initial native ownership assessment now maps the complete live table policy,
produces explicit fixed-reason unresolved rows, and only propagates retention from
surviving references. Two direct native cases pass (1.69 seconds): assessment
leaves private project data and immutable rows unchanged, ordinary immutable DELETE
remains denied, and workspace creation snapshots / unknown legacy mappings hold
planning with hashed keys only. Five native reference cases pass (6.24 seconds),
including the required minimal operation decision without changing its stored
outcome. Workspace/all-target strict checks pass (18.92 seconds), including the
canonical unsupported-adapter fixture's new read-port method. Mixed content,
receipt-control, typed lease and managed-byte partitions are still explicit holds;
this assessment is not an execution permit or full ownership qualification.

Current-scope/shared content partitioning now resolves explicit artifacts and legacy
knowledge plus their declared content lineage. Five native candidate/reference
cases pass (6.91 seconds), two direct planning cases pass (1.59 seconds), and strict
checks pass (17.11 seconds). Cases distinguish moved-in ownership from moved-to-
workspace independence, current-project membership removal from surviving shares,
private/shared revisions and tags, knowledge edges and a changed keeper decision.
Receipt artifacts, leases, independent version keepers and byte eligibility still
hold planning until their complete typed rules are implemented.

Windows managed-file support qualification is now available at published checkpoint
6531d9d7: CI 42357701451, job 128359319277, passes the all-seven-class/source-
preservation case, replacement/forged-binding refusal, published transient content
identity and project-preview fence/workspace preservation cases. The complete
native adapter library passes 882 cases with three intentional ignores. Its later
foundation failure is limited to the revision-6 corpus expectation guards. The
native corpus is now independently audited against accepted main: 31 new fixture
bodies, none removed, and six generated ID labels changed with identical bodies.
All seven recorded-fixture cases and 602 revision/mode admissions pass (0.27 seconds),
with strict checks (0.61 seconds); every CommandReply fixture bridge applies the
existing negotiated-revision validator, preserving the negative revision-5 purged
reply case. This platform support evidence does not complete DW-83 acceptance.

DW-145 is now composed: all 20 native usage cases pass (85.13 seconds), with
workspace/all-target strict checks (5.92 seconds). The controlled real-runtime hint
regression retains the original native pointer gesture and verifies that the current
history-enabled response supersedes older retained-catalog replies.

Typed receipt partitioning now recognizes actual project-owned command decisions
and the explicit workspace-created ProjectRecord snapshot, preserving replay-fence
identities without copying private keys/results into audit rows. Strict envelopes
and validated minimal revision-6 purge control receipts/events/publication intents
separate control bookkeeping from the business snapshot digest. Unexpected nested
control content remains fenced. Typed project graph/schema leases require closed
stored shapes, matching columns and exact graph/index child pairs; owner namespace
text does not grant ownership. Five native planning cases pass (6.74 seconds),
four candidate/reference cases pass (5.72 seconds), and strict workspace checks pass
(7.43 seconds). The assessment remains read-only and grants no execution permit.
The deterministic implementation register now includes all four revision-6 purge
operations; all five native registry/schema cases pass (0.57 seconds). Published
reply artifacts, independent version keepers and byte eligibility remain explicit
holds until their complete typed partitions are implemented.

Published receipt partitioning is validated against the actual native reply publisher:
its exact schema/command/actor/version binding selects the private reply row,
receipt-retained artifact and content lineage; an altered schema remains unresolved.
Six native planning cases pass (8.52 seconds), with strict checks (17.35 seconds).
The replay tombstone retains actor/operation foreign-key identities explicitly.

Current draft cba36ad8 Windows foundation also passes its complete adapter library:
884 cases, no failures, three intentional ignores (793.15 seconds), before the three
known registry expectations fail. Those expectations and implementation-register
generation are corrected in d19c04e7; platform qualification of later classifier
changes remains pending. PR49 merged as c10d469c with all20 checks green; its exact
merged tree equals the pre-merge integration tree. DW88 and DW143/144/145 are accepted.

Blob assessment now distinguishes selected unshared generations from surviving live
versions, independent references and all unproved backup/reader pins. It never
claims or unlinks a file. A controlled legacy-reference regression reproduced the
incorrect pre-closure byte decision (1.94 seconds); running byte assessment after
the finite retention closure corrects it. All seven native planning cases pass
(12.32 seconds), including shared-content survival, new-pin staleness, pin release,
unknown independent-reference holds and unchanged live blob state. Workspace/all-
target strict checks pass (17.22 seconds). The sealed physical executor and its
reader/generation rechecks are still required before any byte deletion is possible.

Mixed metadata now resolves exact typed grant revocation, intake deduplication
identity, owned integration edges, and global source heads according to all final
surviving basis/evidence references. Workspace/other-project grants and history
remain outside ownership, malformed bindings/states hold, and sensitive source text
never enters audit rows. Managed metadata holds unfinished writes and forged
owners; live attribution is distinct from native file/epoch/content inventory.
Settled Removed metadata is included for logical deletion. Candidate policy hashes
now include compiled anchors and predicates as well as the frozen policy source.
All eleven native planning cases pass (16.72 seconds), four candidate cases pass
(5.64 seconds), and workspace/all-target strict checks pass (16.51 seconds).

Global schema sources with exact typed projectAccess(P) edges now participate in
assessment. Closed stored source bodies and matching header/configuration columns
are required; the independent registration and other-project configuration remain
keepers. A typed activeOperationId witness retains only the required operation
identity through the finite closure. All twelve native planning cases pass
(17.68 seconds), four candidate cases pass (5.50 seconds), and strict checks pass
(38.89 seconds). No access edge is changed by this assessment. The purpose-bound
writer must remove only P's edge, advance source CAS revision, and preserve the
unchanged metadata configuration revision/view/other-project leases; integrated
native acceptance of that write is still pending.

The actual native reply publisher now participates in the control stability proof,
including a Deleted project's workspace-fallback reply artifact. Exact actor/command/
schema, closed host provenance and staging source, and the native canonical reply
byte digest are required before its minimal artifact/version/blob/pin metadata can
be excluded from business staleness. Unknown provenance remains a hold. The new
native case initially failed (1.51 seconds); correcting the actual stored host-kind
binding and validating the real deleted-project publication scope resolves it.
All thirteen native planning cases pass (19.51 seconds), with strict checks
(15.94 seconds). This does not route prepare/purge or grant any execution permit.

Managed staging has a current-boot, single-bind created-identity seam between
create-new and private byte writes. Publication must match the bound handle identity;
recovery returns it across reopen, while another boot cannot continue the original
write. Older reservations remain readable with no created identity. Four native
managed cases pass (2.46 seconds), with strict checks (7.25 seconds). Session-bound
paste production and its normal-owner recovery still need to use this seam before
client state is complete.

Remaining full scope: executable coverage of every schema table and mixed owner,
typed minimal-identity keepers, atomic logical purge and durable exact physical
effects/retry, source/upload handle revocation, complete client paste/draft/view
coordination, sealed Human-only review/confirmation, integrated native acceptance,
independent review, platform CI, merge and CodeGraph acceptance. Candidate SQL
selectors are not deletion authorization. File-port support alone does not prove
project quiescence, reference/pin eligibility or full purge completion.

Native session ownership now includes conversations belonging to explicitly
project-scoped sessions even when the session has no work root. A surviving
workspace artifact's real transcript link retains only a minimal closed session
and conversation header; it does not retain private session text or transcripts.
A project-root session without explicit project scope remains an unresolved
legacy mapping. The missing-conversation regression failed before correction
(1.11 seconds); all 15 planning cases pass (21.61 seconds), and strict
workspace/all-target checks pass (16.90 seconds). This remains read-only
classification; the destructive executor and client coordination are pending.
