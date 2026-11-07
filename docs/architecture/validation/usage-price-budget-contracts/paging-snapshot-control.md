# Controlled no-event premise for the price More gesture

Qualified source: composed `cd3fc1e5` plus the separate desktop case allocation `6f23eb8a`.
The production Usage controller is unchanged in this tail.

The first budgeted composed run observed 90 price rows where the More fixture expected 105.
That failure did not capture its error or event stamps. A diagnostics-only run passed; the
original uncontrolled 90-row cause therefore remains **unknown**. It is not attributed to
`CursorExpired`, a particular producer, or a product defect on the basis of a rerun.

Independently, the positive fixture had not actually controlled its no-event premise: waiting
for startup does not prevent future real background writes between the first query and its
continuation. Both price and Usage cursors intentionally fence the full event snapshot.

The fixture now acquires the actual SQLite writer admission slot using an owned no-op
transaction, then explicitly reads a fresh first price page. It holds that slot across the
single native More gesture, its controlled best-effort hint, and the continuation read. WAL
readers continue using the real gateway while real background producers remain active/queued.
No producer is disabled, marked complete or replaced; no domain response is manufactured.
The existing final 105-row/two-page/unchanged-event-sequence assertions remain intact.

The writer control has a bounded admission observation using the existing `SETTLE_BOUND`.
Its RAII destructor releases/cancels the no-op transaction and joins the owned worker through
normal return and unwinding. It is declared after the apps/harness, so it releases before
cleanup needs the writer. The existing true-event `CursorExpired` negative remains unchanged.
There are no authorization, restore, cursor, accounting, price or timeout changes.

Causal qualification of this same controlled fixture:

- Temporarily restoring the old partial-page replacement condition: **RED, 14.44s**, generation
  3 supersedes generation 2 during the pointer frame. The live writer control releases/joins
  during that assertion's unwind; the native process exits normally with the expected failure.
- Correct production condition: **GREEN, 14.16s**, real 105 rows, two pages, same event sequence.
- Temporary production calibration is restored byte-for-byte; it is not part of the commit.
- Full default composed Usage suite: **19/19 passed, 81.25s**, no ignored/skipped/filtered cases
  and no `--test-threads` override. The only independent-case allocation is the owned two-case
  guard; every scenario's original internal concurrency and time bounds remain intact.
- Desktop all-target strict Clippy (`--no-deps -- -D warnings`): **passed, 28.10s**.
- Desktop formatting and `git diff --check`: passed. No production files differ from the
  composed baseline in either the case-allocation commit or this fixture-control tail.

Tests retain actual temporary SQLite/runtime composition, SessionSecrets and recorded/no-network
provider ports. No learned execution, Git/auth probes or OS secrets are involved. Native and
strict campaigns run in an exclusively coordinated window, with two build jobs, incremental
compilation disabled and the existing target; disk remained above the 8 GiB stop floor.

## Older positive paging case (DW-137)

PR43/b777394b CI42314779894 Ubuntu-desktop128224772973 observed90 versus105 rows in
`native_price_paging_keeps_loaded_rows_and_draft_until_explicit_catalog_reload`.
This separate older case did not use the writer control above. The original uninstrumented
failure still does not establish a particular event producer or product defect.

It now reuses the same real writer hold and explicitly reloads its first page before the
single More gesture. The hold ends after validating/capturing the105-tariff draft, before
the test's deliberately changed source command. All original105rows/twopages/draft/background
preservation/explicit-reload assertions remain. The real event cursor and product controller
are unchanged, and no producer is disabled or marked complete. The existing true-event
CursorExpired negative and same-frame-hint positive remain in the full suite.

Complete default native Usage UI suite:19/19 pass84.50s. Desktop all-target strict Clippy:
34.61s. All original bounds and internal concurrency remain intact; fresh reviewed platform
CI and merge acceptance are still required. SessionSecrets/recorded sources only, no inference.
