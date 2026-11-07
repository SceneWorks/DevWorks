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

Remaining full scope: executable coverage of every schema table and mixed owner,
typed minimal-identity keepers, atomic logical purge and durable exact physical
effects/retry, source/upload handle revocation, complete client paste/draft/view
coordination, sealed Human-only review/confirmation, integrated native acceptance,
independent review, platform CI, merge and CodeGraph acceptance. Candidate SQL
selectors are not deletion authorization. File-port support alone does not prove
project quiescence, reference/pin eligibility or full purge completion.
