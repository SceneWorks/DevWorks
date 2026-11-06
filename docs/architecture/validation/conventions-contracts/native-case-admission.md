# DW-133: native Conventions case admission

Windows desktop job128164615549 passed15/17 Conventions cases. Context Help and
owned after-send index cancellation reached their original UI wait bounds. The
new Conventions binaries did not acquire the existing shared native-runtime case
budget, so independent full application boots could all run simultaneously.

Each of the17 manager/runtime and six project-scope cases now acquires the existing
two-case process budget at case entry and retains it through fixture cleanup.
A case still owns all of its internal runtimes/tasks, including intentional concurrency
and restart scenarios. The production gateway, artifacts, scoped source captures,
recorded HTTP metadata responses, custody and UI paths remain intact. No assertion,
watchdog, operation timeout or concurrency scenario was removed or enlarged.

All17 manager/runtime cases pass115.78s, including the original context Help and
owned cancellation assertions. All six scope cases pass34.34s, including exact
source-scope bulk indexing and maximum-applicability response paging. Waiting for
case admission is outside the cases' operation watchdogs, as in Help and Usage.
Fresh Windows CI must qualify the scheduling correction; local success does not
establish its Windows result or rule out a remaining semantic defect.
Strict composed checks, review/CI, merge and acceptance remain required.
No learned inference or OS credential access occurs in these fixtures.

## Requested revision in section paths (PR43 review4498460899)

Metadata-only edits retain the original indexed source revision. Section-path source spans and
artifact leases continue to use that captured owner, while the response now retains the exact
owner revision requested by the reader. The actual native manager regression reproduced the
reported UI refusal before the fix (10.94s) and passes afterward (10.99s). All18 native manager
cases pass143.08s; strict workspace/all-target checks pass40.55s. Recorded HTTP/vector fixtures
are used for routed embedding boundaries; no learned inference executes.
