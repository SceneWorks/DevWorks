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

Remaining full scope: executable coverage of every schema table and mixed owner,
typed minimal-identity keepers, atomic logical purge and durable exact physical
effects/retry, source/upload handle revocation, complete client paste/draft/view
coordination, sealed Human-only review/confirmation, integrated native acceptance,
independent review, platform CI, merge and CodeGraph acceptance. Candidate SQL
selectors are not deletion authorization. File-port support alone does not prove
project quiescence, reference/pin eligibility or full purge completion.
