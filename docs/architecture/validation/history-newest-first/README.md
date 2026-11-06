# Newest-first project and card history (DW-89)

`history.events` revision 3 adds optional `newestFirst`. With `true`, the first read
uses the existing decision indexes in descending committed-sequence order and each
continuation reads strictly before the last sequence. Cursor bindings include direction,
project/root, principal, negotiated revision and the existing store/restore/scope fences.
Revision 1 and 2 stay forward-only; all 67 schemas already on main ac72886b remain byte-exact.
No migration or full-feed materialization is needed. Production SQL's EXPLAIN check confirms
the project decision index with a sequence bound.

Project Activity and independent card feeds initially request 200 newest changes. The visible
older-page controls request one additional page. A refresh atomically replaces at most the
number of pages the user loaded; it never walks the full feed implicitly. Project, card and
request generation checks discard stale responses. Failed continuation leaves loaded rows
visible and exposes Reload history, which starts from the newest page again. Filters apply
to the explicitly loaded history, as the caption states.

Native acceptance evidence:

- All 53 SQLite session/history cases pass (36.78 s), including backwards continuation after
  a concurrent append, legacy ordering/projections, direction/project/root/principal cursor
  refusal and indexed SQL. The older unsupported-revision check now uses revision 4 because
  revision 3 is intentionally registered; every assertion remains.
- All 23 native desktop work-screen cases pass (33.49 s). The 10,000-event fixture stores
  real recorded decisions through the write owner, then counts actual history queries over
  the production gateway: one request shows the newest 200, explicit older-page actions
  load project and separate card pages. Its extended final case also drives the inline card
  control and actual CursorExpired recovery (7.08 s).
- Ten generator tests pass (9.281 s); generated source reproduces without further drift.
  The expected history revision set now includes 3. Core, adapters and desktop all-target
  strict checks pass (30.78 s); formatting and diff checks pass.

The added native fixture takes 7.08 s on the development host plus shared compilation;
platform CI costs are not yet measured. Existing watchdogs remain. It executes no inference,
network calls, real Keychain operations or live provider requests. Final remote review,
all-platform CI, merge and tracker acceptance remain separate gates.

CI correctly caught missing revision-3 recorded admission/gated-member fixtures. The source
fixture generator now adds seven positive/negative cases (root/project newest, explicit forward,
before-revision refusal, null and string refusal), preserving every pre-existing case. The full
native recorded-contract suite passes 7/7 in 0.14 s, covering all 1,519 domain fixtures and all
542 operation/mode/revision admissions. The independent DTO probe passes the same recorded
corpus; JSON Schema validation separately checks the seven new shapes. Native API/UI paging
proofs remain unchanged. The expected gated-member list now names newestFirst explicitly.
