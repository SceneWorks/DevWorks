# DevWorks storage evaluation

> Follow-up decision: SQLite is selected for app storage. The four-engine measurements below are historical. [GraphQLite plus sqlite-vec was subsequently tested](devworks-graphqlite-evaluation.html), including cache freshness after writes; GraphQLite remains unselected.

1 November 2026 · Local native results and comparison protocol · Four active candidates

**KuzuDB is excluded at Michael’s request.** The active comparison is OverGraph plus SQLite, Grafeo plus SQLite, HelixDB plus SQLite, and SQLite graph/vector storage. No production engine has been selected. These are disposable evaluation adapters, not four proposed production backends.

## Local findings

The development-Mac evaluation completed **36 fresh-database cases**: three repetitions of three profiles for each of four engines. All executables completed, but **9 cases reported correctness failures**, all in the pinned OverGraph configuration. The remaining 27 completed without a checked failure. There were 906,276 workload assertions/checks, three independent-oracle unit tests, and eight separate owned-process fault probes. This is evidence about the tested operations and configurations, not complete product acceptance.

- **OverGraph:** quick native graph reads and imports, but deleted node IDs remained in vector results before and after reopen in all nine cases. The full-source and overlay cases also returned a same-project node reached through a foreign-project bridge, outside the exact permitted graph scope. Project/view filtering on returned nodes passed; the mismatch is the permitted traversal path. These are recorded failures, not silently repaired by post-filtering. An adapter/schema/isolation change or upstream fix might address them; this experiment does not establish that OverGraph can never satisfy the contract.
- **Grafeo:** passed the checked scope, deletion and persistence cases, with strong adjacency performance. Full-source vector recall averaged about 95.4% in this configuration. Its full-source **graceful reopen took approximately 27.6 seconds**, consistently across three repetitions. The measurements establish the delay, not its root cause or irreducibility. That reopening cost matters for a desktop app that exits when closed.
- **SQLite:** passed the checked cases, with the smallest helper, fast imports and exact vector results on this corpus. Adjacency and concurrent-reader tails were slower than Grafeo/OverGraph. sqlite-vec here performs exact search; its results do not establish suitability for much larger vector collections or replace evaluation of the full retained graph-analysis surface.
- **HelixDB:** direct-source embedded execution works and passed the checked cases. Its current configuration used more memory/disk and had slower point/retrieval operations here. Project/view vector recall averaged about 78.7%; tuning and equal-recall comparisons are required before generalizing this result.

**Assessment:** SQLite and Grafeo currently have the clearest local evidence for further selection review. OverGraph’s deletion defect and our adapter’s scope misuse must be resolved before adoption, and Grafeo’s reopen behavior needs investigation. HelixDB is a working embedded candidate, with material costs in this measured configuration. No timing-only winner is declared and the original HelixDB architecture baseline remains provisional.

**Confidence:** high in the recorded local outcomes; moderate in relative performance for this exact workload; low in extrapolation to larger production projects or untested platforms.

## OverGraph failure attribution — follow-up

The two failed checks have different causes. The traversal-scope mismatch is **our adapter’s API misuse**, not evidence of an OverGraph engine defect. `label_filter` restricts result nodes; native traversal scope expands through all allowed edge types and has no intermediate project restriction in our request. In `A(project 0) → B(project 1) → C(project 0)`, returning C is consistent with the request. Our adapter must enforce DevWorks path boundaries through an appropriate scoped graph/candidate strategy. Its cost must be included when remeasured.

The deleted-vector behavior **reproduces through raw OverGraph 0.18.0 calls without any DevWorks adapter, ID mapping or oracle**. A two-node probe shows: insert and flush; delete a node; point lookup reports absent; flush the deletion; vector search again returns the deleted ID, including after close/reopen. Without the initial flush, this small probe stays correct. Rename versus no rename makes no difference. The formatted probe reproduced this pattern in three fresh directories, in addition to the original benchmark. This is high-confidence evidence of an engine defect in the tested published configuration; its exact internal root cause and affected-version range are not established.

[Direct attribution, source and raw probe results](validation/storage-evaluation/direct-overgraph-attribution/attribution.json) are preserved with the evidence. A [complete standalone bug report](validation/storage-evaluation/overgraph-deletion-bug-report/BUG-REPORT.md) now includes a separate Cargo project, three fresh standalone repetitions and six fresh-process inspections; affected stores return the stale IDs in the new process, while controls remain correct. The original 36-case measurements and nine failing OverGraph cases remain unchanged. The follow-up attributes those results; it is not a rerun with a corrected adapter or an upstream fix. No upstream issue or code change has been submitted.

## Dataset and execution

The frozen fixture contains 1,077 tracked source files from CodeGraph, simple-router and dark-factory: 13,087 structural source nodes plus 2,500 isolated-project control nodes. CodeGraph had captured uncommitted changes; the source repositories were not edited by the evaluation. Tree-sitter containment and unique-name syntax-reference candidates are **not compiler-resolved calls**. The fixture has 45,759 unresolved/ambiguous syntax references and no parse-error files.

Vectors are 384-dimensional normalized deterministic mathematical fixtures, generated without model execution. Identical vectors in another project stress scope filtering. They test storage/search mechanics and numerical recall, not semantic RAG quality. The workload adds two adversarial boundary edges; the original fixture has 24,859 edges.

| Profile | Imported nodes | Imported edges | Fresh cases |
| --- | --- | --- | --- |
| small | 1,000 | 1,002 | 12 |
| source | 15,587 | 24,861 | 12 |
| overlay | 28,540 | 46,182 | 12 |

The small profile is a stratified induced subgraph (500 nodes per project). The source profile retains the captured multi-repository project and controls. The overlay profile adds one **materialized** second view with renames/deletions; it tests isolation and full-snapshot cost, not efficient delta-overlay publication. These are modest local profiles, not a large-production scale qualification.

Host: Apple M5 Max, 18 cores, 128 GiB RAM, macOS 27.0.1 arm64, APFS, AC power. Every candidate used installed `nightly-2026-10-06` (Rust 1.101 nightly), release opt-level 3, no LTO, 16 codegen units. Rayon was limited to four threads; Helix used a four-worker Tokio runtime. Engine-native worker/cache designs differ, so this is not a strict equal-CPU-budget experiment. SQLite connections have a 64 MiB cache limit. No custom Rust flags or deployment-target override was set.

Candidates ran serially with deterministic queries and randomized candidate order per repetition. The desktop was not an exclusively reserved benchmark host. Fresh processes/databases do not imply cold OS cache; no cache purge was performed. Phase order is fixed, so later phases have different cache history. Below, latency cells are **medians of the three per-run statistics**, not pooled independent samples. Full run ranges and raw per-query samples are retained. p99 is omitted below 1,000 samples; vector p95 uses only 80 queries per run and is descriptive, not an SLA.

## Full-source graph and lifecycle measurements

| Candidate | Import + index ready (s) | ID lookup median (µs) | Adjacency p95 (µs) | 3-hop p95 (µs) | Graceful reopen (s) |
| --- | --- | --- | --- | --- | --- |
| OverGraph 0.18.0 | 5.254 | 2.167 | 10.708 | 32.583 | 0.447173 |
| Grafeo 0.6.0 | 24.553 | 0.667 | 0.875 | 4.792 | 27.633631 |
| HelixDB pinned engine | 25.181 | 228.667 | 317.208 | 2,991.417 | 0.468651 |
| SQLite + sqlite-vec | 0.225 | 1.041 | 12.292 | 126.417 | 0.000374 |

Import includes graph/vector data, native index readiness and checkpoint, excluding parsing, fixture preparation and the later shared FTS build. Lookups retrieve a name by known ID, with adapter scope guards; they are not general property/text queries. Adjacency includes many leaf nodes. Reachability is the same bounded Rust traversal over each engine’s native adjacency, checked against an independent oracle. One full degree histogram exercises application-owned aggregation; full CodeGraph health/analysis operations remain unqualified.

OverGraph’s reopen includes bounded Busy/ActiveSession retries while background native shutdown releases the directory, following the pinned source’s own lifecycle test pattern. Its fast numbers remain **failed-configuration observations**, not evidence to prefer it over a correct candidate.

## Full-source vector and combined retrieval

| Candidate | Project/view mean recall@10 | Project/view KNN p95 (ms) | Graph-scope query p95 (ms) | Shared FTS + vector RRF p95 (ms) |
| --- | --- | --- | --- | --- |
| OverGraph 0.18.0 | 100.00% | 5.219 | 0.048 | 4.966 |
| Grafeo 0.6.0 | 95.38% | 3.363 | 2.556 | 3.367 |
| HelixDB pinned engine | 78.71% | 66.069 | 0.749 | 17.472 |
| SQLite + sqlite-vec | 100.00% | 6.024 | 5.688 | 5.953 |

Recall averages 240 fixed queries across the repetitions against exact scoped cosine results. The minimum single-query recall was 70% for Grafeo and 40% for HelixDB. These ANN configurations are **not matched for recall**; their speed must not be ranked as equivalent-quality retrieval. SQLite’s vec0 path is exact. OverGraph can take exact-scoring paths depending on scope/index state; configuring HNSW does not prove every query used ANN.

Graph-scope query timing excludes preparation of the eligible-ID list. OverGraph uses native traversal scope; Grafeo uses a parameterized Cypher predicate and cosine ordering; Helix uses explicit candidate traversal; SQLite uses eligible row IDs. The random graph-query set achieved recall 1.0, but the additional adversarial bridge test failed for OverGraph: random-query recall alone would have hidden that scope mismatch. These timings are not complete graph-retrieval latency.

Every candidate uses the same auxiliary SQLite FTS5 file and reciprocal-rank fusion code. Text is symbol names and provenance resolves through the frozen source manifest, including repository roots. Hybrid queries follow earlier vector queries, so their cache history differs. This is not PDF/Word extraction, document chunking or complete citation acceptance. Hybrid freshness after mutation is also not qualified here.

## Concurrency and footprint

| Candidate | 10 readers p95 (µs) | 20 readers p95 (µs) | 20 readers + writer p95 (µs) | Concurrent rename p95 (µs) |
| --- | --- | --- | --- | --- |
| OverGraph 0.18.0 | 5.500 | 3.958 | 3.875 | 3,957.917 |
| Grafeo 0.6.0 | 5.917 | 4.541 | 3.000 | 3,974.000 |
| HelixDB pinned engine | 1,335.542 | 2,604.250 | 3,255.250 | 13,301.667 |
| SQLite + sqlite-vec | 50.417 | 100.625 | 116.500 | 204.042 |

These are native query workers representing independent feature caps, not model-powered agents. A barrier starts readers and one writer; each reader performs 100 lookups while the writer performs 100 acknowledged renames on a reserved node. This is burst interference/visibility evidence, not sustained concurrent reindexing, full multi-writer isolation or fairness qualification.

| Candidate | Release helper (MiB) | Stored files, source (MiB) | Maximum process RSS, source (MiB) | Stored files, overlay (MiB) |
| --- | --- | --- | --- | --- |
| OverGraph 0.18.0 | 11.6 | 41.1 | 268.6 | 76.2 |
| Grafeo 0.6.0 | 13.8 | 26.1 | 206.0 | 48.2 |
| HelixDB pinned engine | 52.0 | 369.9 | 630.3 | 842.9 |
| SQLite + sqlite-vec | 3.0 | 27.1 | 154.0 | 49.7 |

Helper size includes common measurement code and statically linked dependencies; it is **not installed DevWorks size**. `otool -L` found only macOS system libraries/frameworks: none needs a separately installed database server, Node, Python or .NET runtime to execute. Python and Cargo are evaluation-authoring tools.

Maximum RSS comes from `/usr/bin/time -l` and includes fixture parsing, duplicate oracle data and queries, not just the engine. Raw reports also capture baseline/import/post-query RSS. Stored logical bytes include the ID map and common lexical file, after the tested mutation/checkpoint/close lifecycle. No equal forced-compaction procedure was applied; these are measured lifecycle costs, not theoretical minimum storage or write-amplification estimates. Startup/idle behavior of the actual app remains untested.

## Recovery, failures and evidence

All four engines passed a SIGKILL probe after an acknowledged rename with **no subsequent engine checkpoint**. Fresh processes checked all 1,000 names plus scoped vector results (1,028 checks per engine). All four also reopened/checkpointed/closed stores killed 50 ms after opening while an import was in progress; no import-complete marker had been published. That second probe proves partial-store reopen only, not successful generation publication, indexing-job resume, interruption specifically inside HNSW construction, or power-loss safety.

Failed pilots are preserved. Corrections to our adapters are distinguished from backend findings: Grafeo requires detach-delete for connected nodes; Helix Disk roots must exist; OverGraph needs the documented bounded lifecycle retry; an early shared provenance assertion incorrectly rejected valid repository-root locators. None of those initial mistakes is used as a candidate disqualification. OverGraph’s stale vector IDs and foreign-bridge graph-scope result remained after those corrections and reproduced in the measured runs.

- [Machine-readable aggregate and run ranges](storage-evaluation-results.json)
- [Candidate status and remaining gates](storage-evaluation-status.json)
- [Historical Helix dependency probes and later runtime qualification](helix-embedding-validation.json)
- [Reproduction and raw evidence record](validation/storage-evaluation/README.md) — the full review export archive includes the exact compressed fixture, manifests, raw samples, failed pilots, fault-probe logs, source/toolchain hashes and evaluation source. It excludes build caches and generated databases.
- Canonical evaluation tooling: `tools/storage-eval/` in the DevWorks repository; its README explains commands, semantics and limits.

**Unresolved before a production choice:** full retained CodeGraph query/analysis inventory (G11), arbitrary property/type/search/pagination behavior, complete incremental file reindexing and publication reconciliation, optimized/multiple overlays, larger representative projects, matched-recall tuning, sustained read/write contention, broader fault boundaries and power-loss durability, migration/manual-backup restoration, app idle/startup behavior, and Windows/Linux/minimum-macOS packaging. These gates remain visible in G01; local benchmarking does not mark them complete. The architecture still has one eventual production graph/vector implementation behind its port, with SQLite authority and file artifacts unchanged.

## Candidate pins and source inspection

| Candidate | Frozen implementation | Persistence/vector configuration | Source licence metadata |
| --- | --- | --- | --- |
| OverGraph | Published `overgraph = 0.18.0` | Immediate WAL; dense cosine 384; HNSW M16/ef-construction 200, ef-search 256; no prune policy configured | MIT OR Apache-2.0 |
| Grafeo | Published `grafeo = 0.6.0` | Persistent Sync WAL; selected vector/text/file features; cosine 384, M16/ef-construction 200, ef-search 256; model generation disabled | Apache-2.0 |
| HelixDB | Git package `db`, alias `helix-core`, revision `6f2a69653772b60ea099d8d36a02a1d4408147ec` | Embedded Disk defaults with writer flush; tenant scope; wait for vector-index lifecycle success | Repository Apache-2.0 |
| SQLite | rusqlite 0.41.1, bundled SQLite 3.54.1, sqlite-vec 0.1.7 | WAL synchronous FULL; indexed graph tables; vec0 exact cosine KNN, native scope partition | SQLite core public domain; sqlite-vec MIT/Apache metadata |

Grafeo 0.6.0 declares Rust 1.99 minimum; the default local Rust 1.98 resolver otherwise selected an older version, so all candidates were explicitly pinned and built with the same installed newer compiler. This is a build-toolchain consideration, not a runtime prerequisite for end users. Helix’s published client feature mismatch is a separate historical registry fact; the pinned direct engine works locally. Dependencies, manifests and licence identifiers were inspected; this is not a complete transitive distribution audit.

Primary project sources: [OverGraph](https://github.com/Bhensley5/overgraph), [Grafeo](https://github.com/GrafeoDB/grafeo), [Helix pinned engine](https://github.com/HelixDB/helix-db/blob/6f2a69653772b60ea099d8d36a02a1d4408147ec/crates/db/src/lib.rs), [sqlite-vec Rust integration](https://alexgarcia.xyz/sqlite-vec/rust.html). Vendor performance descriptions are not used as measured results. [Kuzu’s archived repository](https://github.com/kuzudb/kuzu) remains a historical reference only.

## Production acceptance protocol and adapter contract

Freeze a canonical dataset manifest with entity/edge IDs, labels/properties, project/repository membership, source revisions, branch deltas, deletions/renames, document/chunk references and vector model/dimension identity. Record input file digests, counts and distributions. All engines ingest the same logical data and return the same normalized IDs, paths, provenance and distances within a declared numeric tolerance.

Use captured representative code graphs with small, typical and large project profiles, including a multi-repository project. Parsing/semantic extraction is performed once outside the database timing. Include its output digest; do not time different parsers against different engines. A benchmark source export is a disposable fixture, not permission to import old code indexes into the product. Freeze the retained query/analysis inventory under G11 before using the results for the final architecture decision.

Use fixed previously generated embeddings where available, without issuing inference requests. Otherwise use explicitly labeled deterministic numeric fixtures for vector mechanics/scaling. Such fixtures test storage/search, not semantic RAG quality. Ground-truth nearest neighbors come from numerical distance computation against the exact allowed scope; no model execution is involved.

Implement the smallest comparison adapter that expresses the required operations. It owns source-schema translation, imports, snapshots/overlays, query execution, transaction boundaries and diagnostics. Common Rust code owns request generation, correctness checks and measurement. SQL/Cypher/native operation trees can differ; semantics and scope cannot. Include adapter validation/filtering cost in end-to-end results. Raw engine microbenchmarks may be reported separately.

| Workload | Required behavior and correctness oracle | Measurements |
| --- | --- | --- |
| Bulk graph import | Same nodes, edges and properties; durable publication before success | load throughput, wall time, peak memory, disk bytes and write amplification where observable |
| Incremental indexing | Changed/deleted/renamed files; local worktree changes; no baseline leakage | batch latency, write throughput, query interference and publication lag |
| Symbol/entity lookup | ID, exact name, label/property and bounded search | latency distribution, throughput and returned-ID equality |
| Neighborhood/navigation | Typed incoming/outgoing links, filters, pagination and source locators | latency by degree/result size; bounded-memory behavior |
| Dependency/impact traversal | Multiple hop depths, reverse impact, cycles, cross-repository edges | latency, visited nodes/edges, exact reachability/path result checks and limits |
| Branch overlay queries | Baseline plus replacements, tombstones and renames across multiple active worktrees | publication cost, query latency, storage overhead and absence of masked/stale IDs |
| Health/analysis queries | Retained aggregate and graph-analysis operations with equivalent semantics | total analysis time, working memory and agreement with the reference algorithm |
| Vector retrieval | Same vectors/metric and eligible project/repository/snapshot/document scope | latency, throughput, recall@k against exact scoped results, build/update time, memory and disk size |
| Combined retrieval | Same graph constraints and lexical/vector fusion policy; scoped passage/citation results | end-to-end latency, candidate counts, citation integrity and visibility/freshness |
| Concurrent work | Single reader, ten logical subagents in one feature, and two independent features with ten each; background index writer | read/write throughput, tail latency, fairness, queue time, projection lag and memory |
| Reopen/recovery | Graceful close/reopen, process kill at recorded write/publication boundaries and interrupted index build | reopen/reconciliation time, acknowledged-write retention, orphan handling and duplicate/missing effects |
| Packaging/idle | Actual standalone release artifacts and local storage; no active indexing or requests | installed bytes, loaded libraries, startup/idle memory and CPU, startup latency and required runtime files |

SQLite continues to own the same work/history/queue fixture for the three graph-engine candidates. Keep shared SQLite lexical search constant for the baseline hybrid comparison; benchmark optional native FTS separately if considering an architecture change. The SQLite-only candidate may combine graph/vector and operational writes in one file or use separate files: record which design is measured, its transaction guarantees and index-write contention. Do not credit a different topology without measuring it.

## Measurement protocol

1. **Preflight first.** Pin source releases/commits, features, transitive lockfiles, compiler/build flags and extension artifacts. Compile and link a minimal native executable, then prove disk write/read/reopen and required query features before performance runs. Unsupported behavior is an explicit result, not a fast empty query.
2. **Freeze correctness and consistency.** Verify reference results for every measured request and revision. Compare equivalent committed visibility and durability; report eventual/stale reads separately. Approximate vector search is compared at matched measured recall, with exact baselines reported independently. Reject cross-scope leakage before using a performance result.
3. **Use release builds.** Record optimization/LTO settings, actual hardware/OS, filesystem, power state, thread limits and engine cache budgets. Keep foreground workload controlled and record competing work. Development check time is never a runtime benchmark. No GPU or model inference is required.
4. **Separate phases.** Time source preparation, import, index construction, activation and steady-state queries separately. An index declaration returning early is not a finished index build. Record wait-for-ready and durable publication time.
5. **Distinguish cold states.** Report process-start, database reopen and warm-cache queries separately. Reopening a process does not prove the OS file cache is cold. Genuine cold-I/O measurements need a controlled documented host procedure; do not purge the user's desktop caches as an incidental benchmark step.
6. **Repeat and vary order.** Use repeated fresh databases, deterministic query seeds and randomized candidate/run order. Keep raw per-query samples, errors, warmup policy and sample counts. Report medians and p95/p99 only with enough observations and uncertainty; do not hide timeouts or calculate strong tail claims from a few samples.
7. **Exercise concurrency fairly.** Apply comparable resource budgets and record actual engine worker counts. Measure useful completed operations plus wait time. The two-feature case tests independent caps; it does not introduce a global agent-count policy. Serialize only operations required by each engine's actual write model, reporting that cost.
8. **Keep recovery separate.** Run fault cases against disposable databases under owned process control. Record acknowledged versus uncertain effects, replay/publication reconciliation and data checksums. A fast but unrecoverable or incorrect result fails acceptance.
9. **Retain evidence.** Save run manifests, dataset/query/lockfile digests, versions, raw samples, resource observations, query errors and correctness results. Preserve failed attempts. Human summary tables derive from this evidence rather than hand-entered timing claims.

## Selection and resulting architecture

Require the no-server/no-mandatory-runtime contract, source/index scoping, required operations, persistence/recovery and target packaging before ranking performance. Compare latency distributions, indexing responsiveness, memory/disk footprint, migration/backup behavior, maintenance and implementation complexity together. Do not reduce selection to a single throughput score or assume a native graph database must outperform indexed relational storage on every query.

OverGraph is a promising fit, not a selected engine. Grafeo, HelixDB and SQLite remain in the same evaluation. Kuzu is excluded. Local build or reopen success is partial qualification; full acceptance work remains. SQLite-only is viable only if it passes the complete retained capability surface and workload scale; do not quietly drop graph analyses or scope semantics.

The selected implementation remains behind the existing graph/vector adapter boundary. SQLite keeps operational authority, artifacts remain files, and agent/router/UI contracts are unchanged. If SQLite-only is selected, simplify cross-store publication only where actual transaction boundaries permit it. Record the final choice and evidence in AD 02/G01 before creating the complete implementation breakdown.

