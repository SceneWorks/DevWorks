# Graph review corrections

Confidence: high for the native evidence below; hosted current-head CI and automatic review remain required before merge.

A native one-line C# source puts its File, Class and Method on the same line span. The regression reproduced selection of File; ascending kind priority now selects Method before Class and other containers. The same test creates a real branch overlay and proves complete scope, overview and module coverage. Before the repair, its overview and module falsely reported Partial. Both now inspect the selected generation manifests using the same manifest-coverage decoder as the enclosing graph page. Unknown analyses remain NotAnalyzed; missing repositories and exhausted budgets remain partial.

A property fixture with 1,003 small top-level keys reproduced an oversized single property page. The page now stops at 500 fragments as well as the existing byte bound, retaining the exact key/offset continuation. All 1,003 keys round-trip across pages, and the original large Unicode reconstruction test still passes.

Both property tests pass; the new native parser/SQLite/overlay regression passes (2.08 s). The full 22-case Graph native group passes (62.84 s), including large named-target lookup, overlays, source reads, cross-repository dependencies and scoped impact. Strict core/engine all-target checks pass (27.80 s). No inference executes, no timeout increases, and no property, row or scope is discarded to satisfy a bound.

## Reserve post-scan traversal work

Review 4496113728 found that a full edge scan could spend every visit before traversal examined its retained prefix. A native direct-ID fixture captures 21,000 actual CALLS edges using the real parser and SQLite. The original source cannot produce its partial result within the unchanged 30-second query bound; the repaired source returns its retained relationship and pages distinct inbound callers through one-result call-path pages, with Partial scope coverage and no invented unmapped count.

Edge materialization now receives half the remaining visits from the same budget. Its actual visits and retained memory remain charged, and its original deadline/cancellation remain in force. A phase-only visit limit becomes an explicit coverage gap while the reserved visits allow traversal/projection. Global visit limits, memory failures and cancellation remain terminal. Detail completeness, impact counts and unmapped counts retain the partial-scan boundary; the same shared edge loader supports traversal, call paths, shortest paths, detail, related nodes and impact.

All 23 native Graph cases pass (130.60 s), including the three-request large-edge regression, existing large named-target search and impact/overlay cases. Both budget regressions pass; strict core/engine all-target checks pass (26.40 s). Native platform CI and fresh automated review remain required. Confidence: high for this local evidence, without increasing any query/test timeout, visit bound or memory bound. No inference runs.

## Indexed exact endpoint lookup under the original deadline

Hosted Ubuntu runs on3aac0754 and its Conventions integration exposed the unchanged
21,000-edge traversal hitting the30-second query deadline. The native endpoint reader's
optional `OR` predicate prevented an exact node primary-key lookup, and unit restriction
occurred only after all units had entered the layer-ranking window.

The actual SQLite node path now uses a direct node predicate when an ID is supplied and
restricts an exact unit before ranking, while retaining every selected layer for that unit.
Filtering by the partition key commutes with ranking; Delete/Replace masking and full-scope
validation remain intact. No source-unit identity is assumed for unannotated endpoints.
The unchanged large native case passes39.58s including setup and three bounded queries;
all27 native indexing cases pass77.07s, and all11 graph storage cases pass11.28s including
masking, scope, pin, source, cancellation and pool reuse. Query budgets/deadlines and fixture
population remain unchanged. Hosted exact-head qualification is still required.

### Stream broad node pages without sorting repository payloads

Broad keyset reads now stream composed visible-unit metadata in repository/unit order, then
read each selected unit through the node primary key. The exact-ID path retains its indexed
predicate. Both paths use one shared SQL predicate builder, the same authorized read snapshot,
whole-unit layer masking, name/kind/trust selection, cursor order and limit+1 boundary. Read
page size, graph budgets and request deadlines are unchanged.

All 11 native graph storage cases pass (11.22 s). All 27 full-runtime indexing cases now use
the existing bounded case admission; each of the two >20k fixtures atomically owns both
permits. Their internal concurrency and assertions/timeouts are unchanged. In the full run,
both large regressions pass and 26/27 cases pass (87.16 s). The existing shutdown/reopen case
fails DataRootInUse while retaining the first boot's executor; the unchanged case passes
alone (3.50 s). Its proposed test-only physical executor teardown is tracked as DW-129 and
is awaiting user approval. No full-suite green claim is made. Adapter/engine all-target
strict checks pass (4.57 s) after covering the exclusive permit entry in the helper test.

DW-129 was approved in chat and applied exactly as prepared: the first executor is dropped
before reopening the data root, and the second boot owns a fresh executor. Every original
shutdown/process/recovery assertion and timeout remains. The complete default native indexing
suite now passes 27/27 (90.23 s), including both large regressions. The permit helper1/1 and
engine all-target strict checks (29.32 s) pass. The first full-run failure and unchanged isolated
pass remain above as provenance; no product shutdown change or proven product-cause claim is made.
