# Graph review corrections

Confidence: high for the native evidence below; hosted current-head CI and automatic review remain required before merge.

A native one-line C# source puts its File, Class and Method on the same line span. The regression reproduced selection of File; ascending kind priority now selects Method before Class and other containers. The same test creates a real branch overlay and proves complete scope, overview and module coverage. Before the repair, its overview and module falsely reported Partial. Both now inspect the selected generation manifests using the same manifest-coverage decoder as the enclosing graph page. Unknown analyses remain NotAnalyzed; missing repositories and exhausted budgets remain partial.

A property fixture with 1,003 small top-level keys reproduced an oversized single property page. The page now stops at 500 fragments as well as the existing byte bound, retaining the exact key/offset continuation. All 1,003 keys round-trip across pages, and the original large Unicode reconstruction test still passes.

Both property tests pass; the new native parser/SQLite/overlay regression passes (2.08 s). The full 22-case Graph native group passes (62.84 s), including large named-target lookup, overlays, source reads, cross-repository dependencies and scoped impact. Strict core/engine all-target checks pass (27.80 s). No inference executes, no timeout increases, and no property, row or scope is discarded to satisfy a bound.

## Reserve post-scan traversal work

Review 4496113728 found that a full edge scan could spend every visit before traversal examined its retained prefix. A native direct-ID fixture captures 21,000 actual CALLS edges using the real parser and SQLite. The original source cannot produce its partial result within the unchanged 30-second query bound; the repaired source returns its retained relationship and pages distinct inbound callers through one-result call-path pages, with Partial scope coverage and no invented unmapped count.

Edge materialization now receives half the remaining visits from the same budget. Its actual visits and retained memory remain charged, and its original deadline/cancellation remain in force. A phase-only visit limit becomes an explicit coverage gap while the reserved visits allow traversal/projection. Global visit limits, memory failures and cancellation remain terminal. Detail completeness, impact counts and unmapped counts retain the partial-scan boundary; the same shared edge loader supports traversal, call paths, shortest paths, detail, related nodes and impact.

All 23 native Graph cases pass (130.60 s), including the three-request large-edge regression, existing large named-target search and impact/overlay cases. Both budget regressions pass; strict core/engine all-target checks pass (26.40 s). Native platform CI and fresh automated review remain required. Confidence: high for this local evidence, without increasing any query/test timeout, visit bound or memory bound. No inference runs.
