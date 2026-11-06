# Graph review corrections

Confidence: high for the native evidence below; hosted current-head CI and automatic review remain required before merge.

A native one-line C# source puts its File, Class and Method on the same line span. The regression reproduced selection of File; ascending kind priority now selects Method before Class and other containers. The same test creates a real branch overlay and proves complete scope, overview and module coverage. Before the repair, its overview and module falsely reported Partial. Both now inspect the selected generation manifests using the same manifest-coverage decoder as the enclosing graph page. Unknown analyses remain NotAnalyzed; missing repositories and exhausted budgets remain partial.

A property fixture with 1,003 small top-level keys reproduced an oversized single property page. The page now stops at 500 fragments as well as the existing byte bound, retaining the exact key/offset continuation. All 1,003 keys round-trip across pages, and the original large Unicode reconstruction test still passes.

Both property tests pass; the new native parser/SQLite/overlay regression passes (2.08 s). The full 22-case Graph native group passes (62.84 s), including large named-target lookup, overlays, source reads, cross-repository dependencies and scoped impact. Strict core/engine all-target checks pass (27.80 s). No inference executes, no timeout increases, and no property, row or scope is discarded to satisfy a bound.
