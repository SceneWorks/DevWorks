# Repositories, source, and Git ownership

Register a repository in the selected project, inspect its actual location and remote, and choose whether the registration uses existing files or a supported clone flow. The base-directory preference affects new clones only; it never moves existing repositories. A project association is not permission to modify arbitrary directories.

Repository tabs separate registrations and source text from Nodes, Graph, Health, and Reviews. A tab can be present before its service is available. Use only actions the current build offers as supported. Indexing records show actual structural/semantic producers and diagnostics; an empty graph is not evidence that indexing succeeded.

## Source and worktrees

Work belongs to an identified repository/worktree and current physical source. Git commits, pushes, and PR operations bind actual branch/head, source artifacts, registered remotes, and ownership. Review the exact proposed content. Native Git protections include current ownership and protected/default branch checks; approval is not a general permission to force-push.

Never remove files or worktrees to repair a pending operation. The original action might already have happened. Inspect its journal/evidence and reconcile first. Removing an index affects managed index data, not source files. Archiving or deleting a project does not delete repositories or GitHub resources.

## A practical sequence

Register the repository, inspect its source, run an available indexing action, review diagnostics, then open the relevant work record. Let an owned task branch record changes. Verify and review the actual tested head before delivery. See [Git and delivery](help:git-delivery), [Indexing](help:indexing), [Browser evidence](help:browser), and [Permissions](help:permissions).
