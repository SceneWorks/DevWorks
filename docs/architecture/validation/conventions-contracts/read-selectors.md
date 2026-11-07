# Exact hierarchy, slug lookup and controlled reads

Retained reference: CodeGraph `afe73560215bbe7593fc59451de5cc3237e3eeca`.
`WikiService.cs:238-256` resolves exact hierarchical parents. The MCP tool in
`CodeGraphMcpServer.cs:1442-1471` instead tries a literal root slug, then a unique
fuzzy slug/title match; its resource handler at `:116-137` requires the exact
root slug. Current `conventions.read.path` preserves hierarchy separately from
`slug`; it never splits, trims, drops or decodes literal components. Resource
Document lookup remains root-exact only.

Nine production native admission tests and eleven focused native-probe/schema
cases cover current admission, old-revision
refusal, selector exclusivity, explicit scope, 1–4 components, the inclusive
200-character component bound, and legal empty/NUL/separator values. Missing
scope is a pure owning semantic refusal (the shape schema alone permits it),
recorded explicitly in `path-fixtures.json` rather than claiming schema parity
that does not exist. All 56 legacy schema documents remain byte-identical.

Real gateway/service/SQLite/managed-file tests in
`devworks-engine/tests/conventions_import/service.rs` establish:

- A fuzzy literal `parent/child` match cannot capture the exact hierarchical
  `path:["parent","child"]`; a root whose literal slug contains `/` takes exact
  precedence only for the slug selector. An absent child does not fall back.
- Empty parent slugs and child slugs containing NUL or `/` retain exact component
  identity; splitting that child into separate components returns NotFound.
- A production Index consumer publishes the genuine generation/chunk used by
  SectionPath reads. The real first page is valid; deliberately constructed
  correctly bound cursors with an out-of-range piece/offset or a mid-UTF-8-scalar
  literal position return CursorExpired without panicking.
- A real controlled native file read is paused after query admission, cancelled
  through its actual token, then released. Its typed query reply is Cancelled;
  an interrupted I/O wrapper cannot relabel it StorageUnavailable.

These tests use the registered ConventionService and actual production import and
index consumers. There are no direct catalog inserts, synthetic successful
handlers, provider calls, learned compute or timeout increases.
A separate real Project fixture retains inherited Workspace guidance, refuses
ambiguous same-slug roots across inherited scopes, and keeps explicit Workspace
lookup confined to general guidance. The combined seven-case child suite
(including the two order regressions) passed in 7.59 seconds. Integrated UI and full runtime acceptance remain separate gates.

Strict Clippy for the core library/admission test and engine import/service test
target passed on the final source (43.31 seconds). The 13 generator tests passed;
repeating the complete source/schema/register/core/fixture regeneration produced
zero drift. Only the qualified revision-3 domain request schema changes.
