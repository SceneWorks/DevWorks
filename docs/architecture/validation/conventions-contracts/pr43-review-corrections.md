# PR43 review corrections

The exact `aa3558c9` review identified five material gaps. The corrections preserve current authority, source/version fences, bounds and the existing public schemas.

- Project edits and source-backed breadcrumbs now carry the already-authorized owner's actual scope. The actual desktop Human has no project lineage. Native baseline reproduced three NotFound failures; the corrected four-case suite passes, including current/raw/resource sources, historical preservation, search/SectionPath and wrong-scope refusal.
- Hybrid fusion computes actual compatible cosine for every admitted lexical candidate. The frozen implementation's sparse-score approximation is explicitly corrected: dense top4K limits candidate discovery, not scoring. A lexical-best chunk outside four dense-only neighbors now scores0.935 and remains first at topK1/2/50. Excluded dense-only rows cannot beat the retained topK under the same stable tie ordering. Scoped eligibility and full-corpus lexical statistics remain unchanged.
- Configuration freshness excludes only transient invocation identity. Real new-invocation and app shutdown/drop/reopen recovery reuse the original paid query with unchanged attempts, preparation, balance and HTTP count. Actual selection changes still expire the request.
- Production maintenance releases terminal private import captures in bounded pages, under the existing publication fence and actual terminal journal checks. Deleting references and invoking the shared blob-release helper occur in the same transaction; the normal collector performs physical removal. Live checkpoints, original caller input and published/history references remain intact. The task starts at boot and stops with its owning service.

On the composed delivery source, strict workspace all-target Clippy passes (12.97s), along with formatting and zero-drift regeneration. Native desktop project-scope cases4/4 pass (9.68s), full import suite20/20 passes (22.31s), and actual paid query/job cases6/6 pass (27.06s). The separate SQL retrieval suite6/6 and affected query suite6/6 pass; recorded vectors execute no learned model computation. Both missing runtime wiring and missing physical blob-release marking have causal failures at the unchanged watchdog bounds.

These local results close the identified source gaps. Fresh exact-head code/security review, all-platform CI, merge and integrated CodeGraph acceptance remain required.

The subsequent `65908cad` review identified two more gaps, now corrected locally:

- Compatible metadata revisions preserve the original One/All index operation, generation headers and accounted embedding preparation. One compares the exact captured/current indexed source; All captures a private semantic source-set revision. Active body, title, membership, scope and lifecycle changes advance that witness, including both scopes of a move. Public scope revisions retain their existing meaning. Legacy journals without the witness keep their strict fence, and unchanged inactive snapshots retain their prior no-op behavior. Publication still rechecks current authority, controls, source identity, head and owner CAS. Native queued/running/lost-ack and partial32-to33-source cases preserve the original requests without a second dispatch; actual source changes refuse.
- Retaining convention bytes as Explicit artifacts now invokes the existing destination quota helper inside the same command transaction. A late subtree refusal after a copied root/resource prefix rolls back catalog, artifact usage, resources, public scope revisions and private index source revisions. Visible foreign-scope uploads also obey the destination cap; duplicate retained digests still count once.

The composed source passes all23 import/quota cases (25.45s) and all9 new index compatibility cases (17.25s). Independent existing publisher14, query/custody6, selection2 and migration32 upgrade/reopen1 cases pass. Strict workspace all-target Clippy passes (78s). The unpublished migration32 checksum is `1a860d2cb4e54abeb61cd07a3c07520de4c6fbb148c774758211ea98b14b6dbb`; migrations1–31 and33 remain byte-identical.

Contract integration now admits all1800 recorded cases and584 mode/revision bindings; the seven native fixture, five registry and eight schema-revision tests pass, including all56 legacy schema byte checks. Positive recorded results now include the actual required source/retrieval/qualification metadata. The existing DW-119 extension is retained in canonical plan inputs, so both catalog and plan regeneration preserve it. All377 requirement records,558 frozen source entries and79 slice acceptance obligations remain unchanged; no retained entry is invented for the native extension.

## Immutable import publication and reader continuation

The import publication transaction records its exact typed result in the existing operation observation journal. A worker that loses the commit acknowledgment recovers that immutable result after checking current access to the original scope. Later edits, index progress or scope moves cannot substitute the current convention head. Native SQLite/gateway tests lose the actual publication acknowledgment, edit or move the published convention, and compare the recovered result byte-for-byte with the original. Both fail before the repair and pass after it. All 25 native import tests pass (26.49 s), including existing reopen, control, permission and quota cases.

The desktop reader retains the cursor-free initial query intent separately from its canonical navigation target. Next/Previous reuse the original slug, hierarchy path, section and optional revision rather than changing the cursor binding to the returned ID. Native UI coverage exercises slug, exact path and refreshed-ID entry, exact Unicode/CRLF passage positions, forward/back navigation and absence of visible errors. All 17 Conventions desktop tests pass (45.38 s). The existing native macOS linker compact-unwind warning remains nonfatal. No timeout, scope check, cursor binding or assertion is weakened; no model inference executes.

Confidence: high for these local native results. Final assembled strict checks and fresh published-head CI/review remain required before merge and acceptance.

## Query-centered lexical excerpts

Finding 4496607421 is repaired in the production SQLite ranked-chunk reader. It passes the
actual weighted query terms to the existing Unicode-safe, whitespace-normalizing 360-character
excerpt helper; empty/dense-only queries retain its prefix fallback. Immutable chunk text,
ranking scores, scope and provenance do not change.

The native regression reaches a real published chunk with a lexical match beyond character
1,400. The old path returns only filler and fails; the corrected path includes the matching
phrase, stays within 360 characters and preserves exact stored source. All 23 SQLite convention
cases pass in 24.96 s; adapters all-target strict checks pass in 30.66 s. Earlier fixture setup
errors (invalid term formatting, then mismatched token count) were corrected before the causal
RED result. Existing query and test bounds remain unchanged; no inference compute runs.

## Response-byte paging and exact bulk-index source revisions

Search hits use the same serialized-row budget as catalog pages, leaving16KiB for the reply
envelope, cursor and retrieval metadata. The continuation offset advances by the hits actually
retained. A real native project fixture with128 distinct256-character applicability labels and
more than50 published chunks pages all50 selected hits without repetition; each actual serialized
reply stays within the protocol frame limit. No field is truncated and topK is unchanged.

`ConventionPage` and `ConventionSearchPage` expose optional `sourceScopeRevision` at revision4.
It is the exact target scope's current source revision. Existing `scopeRevision` still binds
inherited listing pages or the search index. The native manager retains the two independently
and uses the source revision for “Index this scope”, including after search. Shared page-result
users list/search/version/resources register4; unchanged inputs reuse the revision3 shape.

All6 actual desktop project-scope cases pass (16.17s), including clicks that accept and complete
bulk project indexing after both listing and search. All earlier revision1/2/3 schema documents
are byte-identical; four unmerged revision4 output documents add the gated member. All1800
existing fixture objects remain identical. Four revision4 input admissions, four outputs and
six refusal cases add14 fixtures: native recorded suite7/7 passes (0.25s), independently checked
JSON Schema14/14 passes, and generator13/13 passes (11.11s). Native admission covers588 exact
operation/mode/revision bindings. Final composed workspace strict passes (38.19s), including
current-main prompt picker, provider identity-index and approved routine fixture corrections.
Fresh exact-head hosted CI and automatic review remain required before merge/acceptance.
