# Usage native CI lifecycle correction

Source audit began at PR39 head `87cf92b3829d9247a21dff9132ba470269351241`.
Qualification uses its fast-forward descendant, the inherited Help head `75b02053`, in an
isolated worktree. The correction changes only the native Usage controller and its own fixtures;
no core cursor, authorization, restore, pricing or accounting semantics are changed.

CI evidence: Windows job `127950007789` failed at `usage.rs:793` (`refresh.is_none()`);
Ubuntu jobs `127951931038` and `127952089947` failed at `usage.rs:829` (90 instead of 105
price rows) and `usage.rs:1280` (`CursorExpired`, read revisions 46/49).

The controller counted a partial first catalog page as replaceable because `price_pages == 1`.
A hint consumed before the More gesture could start a full reload, increment the query
generation and disable/supersede the original cursor append. A partial first page now gets
the same bounded revision probe as an already loaded multi-page set. The retained page and
its continuation survive until explicit reload or a real cursor refusal.

The causal native fixture controls only a best-effort hint and delays real gateway reads.
It supplies no successful domain result. The exact event sequence is asserted unchanged.
Old source is RED (generation 1 becomes 2 in the pointer frame, 13.24s); corrected source is
GREEN (one cursor append, all 105 tariffs, two pages, unchanged event sequence, 13.05s).

Two fixture assumptions also crossed actual lifecycle boundaries:

- Provider publication/error persistence precedes the refresh operation's terminal journal
  transition. The fixture now observes the UI consuming the real terminal operation before
  retaining its original completion/error assertions. It uses the existing `SETTLE_BOUND`.
- Both Usage and Price cursors deliberately bind the full event-sequence snapshot. A quiet
  GUI does not settle native Usage bootstrap or Help generation. Positive stable-paging
  fixtures first observe actual Usage bootstrap/accounting recovery and, where registered,
  the Help consumer's successful completion and settled queue. No background owner is
  disabled, manually completed or replaced. No fixed delay or timeout increase is used.

The Usage fixture additionally commits a real unrelated event and requires `CursorExpired`,
retained rows/context and an unchanged unsent filter draft. The full-snapshot fences remain
intact; there is no silent page restart or frontend rewriting of the backend context.

Validation on the qualified inherited source:

- Full native Usage suite: **19/19 passed, 38.03s**, default runner, no skipped/ignored cases.
- Desktop all-target strict Clippy, `--no-deps -- -D warnings`: **passed, 26.05s**.
- Desktop formatting and `git diff --check`: passed.

The actual temporary SQLite/runtime and common `SessionSecrets` configuration are used.
Public price HTTP responses are recorded; there is no network, learned inference, Git/auth
probe or OS-secret-store access. Builds used the exclusively reserved existing target,
`CARGO_INCREMENTAL=0`, two build jobs; free disk remained above the 8 GiB stop threshold.
These local macOS results do not substitute for fresh exact-head Windows/Linux CI.
