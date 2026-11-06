# Exact graph contracts

DW-28 retains all twenty owned operations. Revision 1 and its recorded specialized
envelopes remain supported; revision 3 adds typed selected arguments and results.
Contract qualification does not establish GraphService, storage, UI or runtime
acceptance. Source parity is pinned to CodeGraph
`afe73560215bbe7593fc59451de5cc3237e3eeca`.

`GraphReadRequest` retains scope, limits and cursor. Exactly one explicit `view`
or graph-specific `current:true` is supplied. Current means the actual caller's
worktree mapping; it never silently selects Published. `repositories.nodes` has
the equivalent `views`/`current` choice. The host applies scope before expansion,
source lookup, counting and snippets, and binds continuation to the normalized
operation, arguments, authority and immutable complete view set.

The existing query discriminator stays selected by the registered operation.
Revision-3 `selection` records are alternatives to the recorded legacy fields:
search declares RankedNodes, NodeList or Unified; named call/impact/model/event
selection does not require callers to first discover an ID; file selection
carries the exact relative path and optional containing line/depth. Source
selection retains node-window, whole-file and file-line-range reads; zero file
range endpoints mean beginning/end, as in the retained tool. Bounded source
text includes exact byte and line positions and continues through the page
cursor, including fragments of a single long line. No legal requested source
window is silently truncated to the page size.

Current results are closed `GraphResult` alternatives containing the generated
`Code*` records listed in `field-table.md`. Rich nodes include actual repository,
view and unit identities. Source-less external/package nodes have no fabricated
citation. Edge endpoint identity is repository + unit + node; relationships may
not point outside the returned scoped graph. Canonical graph unit keys preserve both relative file paths and the directory namespace
(`/`, `/src`); file source locations retain relative paths. Nodes expose the exact opaque
stored content identity and trust revision. A raw SHA-256 is optional and only appears
when actual captured source supplies it. Properties are exact serialized
JSON fragments with offsets/total bytes, permitting inspection of large values
without replacing them with truncated strings. Completion and continuation must
remain explicit when bounded work stops.

Only unavailable ScopeDetails, declared static SchemaDetails, or metadata-only
RepositoryOverview may omit GraphPage.view. Each has a real read stamp and actual
missing-view reason; no fake pin or baseline view is invented. Metadata-only
overviews explicitly mark graph coverage unavailable, have no dependency rows and
leave graph totals unknown. Observed/private schema metadata and indexed facts
require actual authorized scoped views. An invalid explicit view cannot select
this exception as a fallback.
Existing freshness, language, semantic-pack and overlay records carry lifecycle
states instead of collapsing them into generic coverage.

Trust revision 3 binds repository + unit + node, exactly one opaque content identity or known captured SHA-256, exact
view digest and trust revision. The immutable view identity is not caller
permission. Revision 1 retains its recorded arguments and requires unambiguous
host resolution. New optional members reject explicit null, and exact-one rules
are checked by both schema and native readers.

Legacy Nodes/Graph/Source/Schema/ScopeStatus results remain byte-compatible.
Current basic nodes/graphs/source and real schema artifacts can project to those
shapes when all required actual identities/citations exist. A current aggregate
that cannot be faithfully represented is refused at an earlier revision; no
invented citation, omitted repository hit or fake zero aggregate is published.

Implementation source baseline: `06ff92ac`. Native service and platform acceptance
are recorded separately by the owning implementation, after actual qualification.


## Qualification boundary

The native contract suites exercise all twenty selected revision-3 operations,
strict present-null and partial mixed selectors, directory/opaque trust witnesses,
cross-repository snapshots, source-less nodes, unavailable own views, verbatim
empty/NUL/long-line source, property offsets and faithful legacy projection.
The inherited five Help projection tests qualify the composed source without
changing Help behavior. Current Rust semantic counts are omitted when no pass
was recorded; they are not invented measured zeros.

`retained-source-audit.json` records the per-operation frozen arguments/results
and quirks, while `source-binding-results.json` verifies inventory bytes and
ownership. Generated field/reference/architecture/plan readers retain all 377
requirements and twenty graph operation owners. These checks do not claim a
native graph service, Graph SQL implementation, source-capture adapter, UI,
MCP exposure, or platform acceptance.


The composed qualification passes 37 native core tests (12 graph, 5 Help, 7
recorded-fixture, 5 registry and 8 schema-revision), 11 generator tests, 70 schema
documents, 2,084 message fixtures and 12 scenarios/26 steps. The register admits
570 operation/mode/revision bindings. All 56 legacy schema bytes remain exact;
125 generated artifacts regenerate without drift. The current source has 419
generated domain types, 359 operations, 78 slices and 377 requirements, with no
unowned operations, requirements or retained source entries. Strict scoped Clippy,
formatting and whitespace checks pass. The approved requirements Word document
is unchanged; no new Word layout or native runtime acceptance is inferred.


## Partial scope and internal-module reads

Current broad reads pin the available authorized repository views and report each
missing repository with a reason in required revision-3 `scopeCoverage`. The
graph-only `GraphCompletion.Partial` marks finished available work with incomplete
logical scope; bounded work still reports its actual limit/deadline/cancellation.
The existing retrieval completion enum is unchanged. Explicit views and targeted
unavailable repositories do not silently substitute another graph. An incomplete
current result cannot project to a fictitious complete legacy result.

Revision-3 `repositories.nodes` requires the caller's actual selected `scope`;
legacy omission resolves through the host context. Current and explicit pins must
be compared with that scope and fresh project membership. Repository membership
does not select a project on the caller's behalf.

Architecture and overview expose typed internal-module records. Repository plus
actual module name identifies the retained analysis key, with an indexed composite
node reference when available. Observed node-kind counts and endpoint/service/table
drill-down references describe the selected index, not whole-program semantic
totals. Missing recorded analysis is NotAnalyzed/Unavailable with no fabricated
summary, confidence, model, source revision or zero semantic totals. Existing
recorded endpoint/service/table/external-dependency descriptions and their actual
model/time/source provenance remain typed. Program totals appear only when
explicitly recorded. Metadata-only overview contains no module/graph fallback facts.

These read shapes support the existing repository/module exploration requirements;
DW-29's later analysis producer is not impersonated. The existing primary ownership
of `repositories.get` is preserved while its frozen project-analysis response is
supporting evidence for these module read fields.


Current trust changes likewise require the selected scope and may carry an actual
retained caller pin ID. An explicitly invalid pin never selects another view.
Legacy old-view execution needs its retained caller witness; a new current view
can validate its actual immutable lifecycle without a pin. These are references
for current scope/membership validation, not caller-minted authority.

The bounded package-total delta (`b34c1f71`) makes `CodeArchitecture.packageCount` optional: it is present only when the aggregate completed. The two added probe fixtures and one native DTO regression passed, including strict present-null refusal; all 56 legacy schemas remain byte-identical. This adds two domain fixtures (1757 total), with the same 570 operation/mode/revision admissions. See [package-total-qualification.json](package-total-qualification.json). Prior full-pass reports retain their original source checkpoints; this delta does not establish GraphService execution.
