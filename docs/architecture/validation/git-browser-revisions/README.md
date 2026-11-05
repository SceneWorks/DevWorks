# Git ownership and browser source revisions

DW-38 and DW-84 share PR #21 with DW-46. The native owner supplies six operations:
`git.worktree`, `git.inspect`, `git.diff`, `git.commit`, `git.push`, and
`git.pull_request`. `git.integrate` and `git.cleanup` remain owned by DW-39.

Wire additions preserve earlier readers: grouped PR coverage starts at Git revision 2;
retained Git associations and browser source records start at revision 3; immutable
artifact source metadata starts at revision 4. Existing artifact revision 3 remains
unchanged. A source record contains repository/worktree identity, resolved commit,
dirty state, ownership epoch, and observation time. Unbound invocations record none.
Session start evidence and every published artifact version remain immutable.

The contract reports in this directory were regenerated on 2026-11-05. They qualify
67 schema documents, 354 selected operations/inputs/results, 1,313 domain fixtures
(1,033 positive and 280 negative), 1,642 combined fixtures, and 12 contract scenarios
with 26 steps. All pass. The source audit verifies the retained inventory and schema
bindings; these reports do not establish native behavior or live GitHub acceptance.

Native regressions use real temporary Git repositories, isolated worktrees, a local
bare remote, SQLite, the shared process owner, and immutable artifact readback. They
exercise create/cancel, commit, normal push, stale leases, agent-owned force after
amend, remote-default protection, symbolic task refs targeting main, ownership
transfer, grouped task PR links, and recovery without repeating an uncertain write.
GitHub transport tests verify fixed-origin requests and response-loss reconciliation
with one write; no live account or remote repository is mutated by those tests.

Browser provenance regressions exercise clean/dirty/new-commit observations, public
artifact revision-4 text and binary reads, old evidence preservation, unbound reads,
stale ownership refusal, receipt replay, and close cleanup without Git subprocesses.
The installed-browser opt-in runs the same provenance case against real Chrome.

Migration 21 remains byte-identical to the previously reviewed browser candidate.
Migration 22 adds durable owned-worktree identity. Migration 23 adds nullable browser
source records without inventing historical provenance and raises the writer floor
to 23 (reader floor 8), preventing an older writer from dropping new source evidence.

Native gate outcomes, exact source head, hosted platform checks, and final review
dispositions are recorded in PR #21 and the individual CodeGraph acceptance reports.
No model inference runs in these checks.
