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
