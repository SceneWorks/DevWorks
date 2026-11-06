# Source, nodes, graph, and schema exploration

Repository registration, README/text summaries, search and indexing are distinct from graph/node/schema readers. Use the current operation's availability and returned source/view revision to identify what is actually being inspected. A Nodes or Graph tab can exist before its reader is installed; an unavailable reader must not be presented as an empty successful graph.

Search is scoped to the actual registered repository/project and published view. A graph edge, semantic reference, documentation claim and textual match have different evidence. Preserve ranges, path, source revision and producer when using a result to make a consequential change. Stale caches and another worktree's source cannot establish the current target.

Database schema exploration is a separate connection and snapshot boundary, not permission to execute arbitrary SQL. Never infer that a declared schema operation is supported for every database product. Read [Databases](help:databases) for the current limitations.

When a query is unavailable, inspect [Indexing](help:indexing) runs, [Repository](help:repositories) source and [Diagnostics](help:diagnostics). Do not keep changing filters to hide a missing producer. Refresh/reindex through the owning operation after fixing the actual prerequisite. Artifacts and native package evidence stay scoped even when two projects share one physical repository.
