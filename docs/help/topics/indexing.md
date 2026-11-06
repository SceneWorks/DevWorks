# Indexing runs, sources, and diagnostics

Indexing turns registered source into a coherent published view. Use the repository's Indexing logs action to inspect structural and semantic runs, their exact source, producer, diagnostics, and outcome. A queued run, a successful helper exit, and a published usable index are different stages.

Structural parsing and semantic language packs provide different evidence. Language packs declare languages, format, prerequisites, and compatible targets. A missing compiler/runtime or an unsupported language should remain explicit; it is not an empty successful semantic index. See [Semantic language packs](help:sdk-semantic).

## Refresh and overlays

Reads use a published snapshot and revision/generation identity. Source changes, deletion, replacement, and worktree overlays must invalidate or version dependent results. Scope and overlay filtering happen before graph expansion or vector materialization. Do not treat a cache as a permission boundary.

If a run fails, inspect its bounded diagnostics and source identity, fix the actual prerequisite or unsupported input, and retry through the original operation's recovery advice. Removing an index removes managed index data rather than source files. Ended overlay records follow their retention policy; live or referenced views stay protected.

Nodes, Graph, Health, and Reviews tabs can remain unavailable until their owning reader is installed even when indexing itself works. Use the current capability report, not the presence of a tab. See [Repositories](help:repositories), [Diagnostics](help:diagnostics), and [Settings](help:settings-Storage).
