from pathlib import Path
import json, statistics, hashlib, shutil
W=Path('/Users/michael/Documents/Codex/2026-10-31/if');R=Path('/Users/michael/Repos/DevWorks');D=R/'docs/architecture';O=W/'outputs';V=D/'validation/graphqlite-evaluation';V.mkdir(parents=True,exist_ok=True)
read=lambda p:json.loads(Path(p).read_text())
old=read(W/'work/graphqlite-results-initial.json');new=read(W/'work/graphqlite-results-indexed.json');fresh=[read(W/f'work/graphqlite-eval-runs/freshness-{i:03}/result.json')for i in (8,9,10)]
cachepath=W/'work/graphqlite-eval-runs/key-measured/20261101T194050977085Z/cache-profiles';cache=read(cachepath/'manifest.json');recoverypath=next((W/'work/graphqlite-eval-runs/recovery-final').glob('*/manifest.json'));recovery=read(recoverypath)
assert not new['failures'] and not old['failures']
assert all(not r['case_errors'] for r in fresh)
assert all(r['exit_code']==0 and not r['case_errors'] for r in cache['runs'])
assert all(x['verify_exit']==0 and x['acknowledgment_seen'] for x in recovery['cases'])
ca={}
for p in ('small','source','overlay'):
 rr=[read(cachepath/Path(r['case']).name/'result.json')for r in cache['runs']if r['profile']==p]
 ca[p]={k:statistics.median(x[k]for x in rr)for k in ['nodes','edges','initial_load_ns','degree_total_ns','after_delete_reload_ns','baseline_rss_kib','loaded_rss_kib']}
 ca[p].update(bfs_p95_ns=statistics.median(x['warm_bfs_summary']['p95_ns']for x in rr),reload_insert_ns=statistics.median(x['other_connection_insert']['reload_ns']for x in rr),correctness_checks=sum(x['correctness_checks']for x in rr))
results={'title':'DevWorks GraphQLite and sqlite vec evaluation','date':'2026-11-01','state':'Evaluation completed; SQLite selected by user, GraphQLite not selected','versions':{'graphqlite':'0.12.0','sqlite_vec':'0.1.7','rusqlite':'0.41.1','sqlite':'3.54.1'},'inference':False,'indexed_adapter':new,'original_native_id_adapter':old,'native_cache_profiles':ca,'cache_manifest':cache,'freshness_repetitions':fresh,'freshness_binary':read(W/'work/graphqlite-eval-runs/freshness-guard-final-manifest.json')['binary_sha256'],'recovery':recovery,'confidence':'High for reproduced local outcomes, moderate for general performance ranking, target-platform qualification incomplete'}
(D/'graphqlite-evaluation-results.json').write_text(json.dumps(results,indent=2)+'\n')
met=lambda p,e,k:new['profiles'][p][e]['metrics'][k]['median']
checks=sum(int(x['metrics']['correctness_checks']['median'])*x['runs_with_results']for p in new['profiles'].values()for x in p.values())
lines=['# DevWorks GraphQLite and sqlite vec evaluation','',
'GraphQLite 0.12.0 and sqlite-vec 0.1.7 passed the shared graph, vector, scope, mutation, deletion and reopen workload on this Mac. Native cached algorithms can return stale results after writes or rollback unless the application refreshes their snapshot. SQLite remains the selected storage engine; this evaluation does not select GraphQLite.',
'',
'**Recommendation:** use application-owned SQLite graph tables and sqlite-vec for the core store. GraphQLite is viable as a query and analysis dependency if Cypher and its algorithms justify their cost, but its cache must have an explicit owner and refresh contract. It did not improve the common workload performance. Confidence is high for the recorded local results and moderate for general performance conclusions; Windows, Linux, minimum macOS, production scale and the complete retained CodeGraph operation surface are not qualified.',
'',
'## What ran','',
'Native Rust release helpers ran serially on 1 November 2026 on an Apple M5 Max, 18 logical cores and 128 GiB RAM, macOS 27.0.1 arm64, APFS and AC power. GraphQLite and the plain-SQLite baseline used the same bundled SQLite 3.54.1, rusqlite 0.41.1, sqlite-vec 0.1.7, WAL, synchronous FULL and a 64 MiB page-cache setting per connection. Rust was nightly-2026-10-06, opt-level 3, no LTO and 16 codegen units. No model or provider inference ran.',
'',
'The frozen fixture contains 1,077 files from CodeGraph, simple-router and dark-factory. Tree-sitter supplies structural relationships and unique-name reference candidates, not compiler-resolved calls. The source graph has 13,087 source nodes plus 2,500 control-project nodes. Scope boundary controls bring the source workload to 15,587 nodes and 24,861 edges. The small profile has 1,000 nodes and 1,002 edges; the materialized overlay has 28,540 nodes and 46,182 edges. Vectors are deterministic normalized 384-dimensional numeric fixtures, not semantic embeddings.',
'',
'Fixture SHA-256: `84588880e378beb11ed839c91ecbce5d7e33b9002b60aec6fd4c600447a56034`. The exact compressed input is included in the evidence archive. Re-extracting changed repositories produces a different test.',
'',
'Each matrix comprises two backends × three profiles × three fresh database/process repetitions. The original native-ID adapter and a revised indexed external-ID adapter both ran all 18 cases. The revised matrix is the primary comparison; original results are preserved. Filesystem caches were not purged. Table values are medians of three per-run summaries; a p95 column is the median of three p95 values, not a pooled percentile or confidence interval.',
'',
'## Correctness and recovery','',
'| Check | Observed result |','|---|---|',
f'| Revised common workload | 18/18 cases, {checks:,} explicit checks, no process or correctness failures |',
f'| Original adapter workload | 18/18 cases, {checks:,} explicit checks, no process or correctness failures |',
'| Point lookup and bounded graph traversal | Exact independent-oracle matches, including project/view boundary controls |',
'| Project/view and graph-scoped vector recall@10 | 1.000 mean and minimum; no underfilled queries, both backends, every profile |',
'| Rename/read concurrency | 10 and 20 readers, with and without a rename writer; expected values and acknowledged final write verified |',
'| Deletion | 12 nodes per case; points, incident edges and vectors absent before and after reopen |',
'| Hybrid FTS and vector fusion | Scope and source-locator checks passed; shared FTS5 auxiliary database and shared fusion code |',
'| Final process-kill recovery | 4/4 probes: two acknowledged-write checks and two partial-import reopen checks |',
'| Native cached analysis on revised stores | 9/9 profiles; 136,038 checks of BFS, degrees, counts and reload restoration |',
'| Final cache-coherence scenarios | 3/3 repetitions, no guarded-result mismatch; raw stale-cache controls reproduced |',
'| Shared oracle unit tests | 3 passed |','',
'Each acknowledged-write recovery probe kills an owned process after its final rename, without a following checkpoint. A fresh process verifies 1,000 names and 28 scoped-vector checks. Partial-import probes kill 50 ms after opening and verify reopen/checkpoint/close, with no import-complete marker. These are process-crash probes, not power-loss durability or automatic job-resume proofs.',
'',
'## Common workload performance','',
'GraphQLite stores graph rows through its public bulk API and queries them through parameterized Cypher. The baseline uses application-owned indexed tables and prepared SQL. Both use exact vec0 KNN. The common bounded BFS expands scoped adjacency on every hop; it does not use GraphQLite’s whole-database algorithm cache.',
'']
for p,label in [('small','Small'),('source','Full source'),('overlay','Materialized overlay')]:
 lines += [f'### {label}','', '| Operation | GraphQLite plus sqlite vec | Plain SQLite plus sqlite vec |','|---|---:|---:|']
 for title,key,unit,div in [
 ('Import and index ready','import_and_index_ready_ns','s',1e9),('Point lookup median','lookup__median_ns','µs',1e3),('Neighbors p95','neighbors__p95_ns','µs',1e3),('Scoped depth 3 reach p95','reach_depth_3__p95_ns','µs',1e3),('Project/view vector KNN p95','knn_project_view_scope__p95_ns','ms',1e6),('Hybrid retrieval p95','hybrid_fts_vector_rrf__p95_ns','ms',1e6),('20 readers total wall time','concurrent_20_wall_ns__median_ns','ms',1e6),('20 readers plus writer wall time','reader_writer_20_wall_ns__median_ns','ms',1e6),('Scoped degree histogram','health_degree_histogram_wall_ns__median_ns','ms',1e6),('Graceful reopen','reopen_ns','ms',1e6),('Logical storage bytes','database_logical_bytes','MiB',1024**2),('Process RSS after queries','post_query_rss_kib','MiB',1024)]:
  lines.append(f'| {title} | {met(p,"graphqlite",key)/div:,.3f} {unit} | {met(p,"sqlite",key)/div:,.3f} {unit} |')
 lines+=['']
lines += ['Concurrent wall time includes reader connection setup and 100 lookups per reader; lookup distributions time individual queries after setup. The mixed run adds 100 acknowledged renames. These are adapter-level costs, not proof of an engine locking defect. Each reader has its own connection. The test does not model sustained full-file reindexing or many concurrent writers.',
'',
'Graph-constrained vector timing excludes construction of the eligible-ID set. Full-source project/view vector p95 is 5.975 ms versus 5.783 ms; both execute the same exact-search strategy and achieve 100% recall. This is neither ANN qualification nor a RAG-quality test.',
'',
'Logical bytes include the graph/vector files, mapping and shared lexical files. RSS includes fixture parsing, oracle and harness state; it is not database-only memory. The revised release helpers are 4,622,432 bytes for GraphQLite and 3,160,256 bytes for plain SQLite; these are benchmark binaries, not predicted app sizes.',
'',
'### Query shape materially changes GraphQLite performance','',
'The first adapter used `WHERE id(n) = $native`. On the source profile it measured 436.709 µs median point lookup and 773.875 µs neighbor p95. Replacing that predicate with the published indexed pattern `MATCH (n {id: $key})` reduced those values to 46.375 µs and 152.875 µs. Import and reopen costs remained similar. Both versions passed correctness. The initial penalty was partly our query choice and must not be presented as unavoidable GraphQLite overhead.',
'',
'A separate fixed-point probe used 100 warmups and 2,000 measured samples per query shape and retained generated SQL from EXPLAIN. Medians were 437 µs for the native-ID predicate, 45.208 µs for the external-ID property pattern and 10,937.166 µs for `WHERE n.id = $external`. This diagnostic supports choosing the indexed form; it does not replace the varied-workload matrix.',
'',
'## Native cached algorithm performance','',
'This separate profile uses GraphQLite’s native cached BFS and degree API on the whole post-deletion database. It validates 80 depth-3 BFS queries against a global oracle and every node’s raw-edge in/out/total degree. It deliberately includes all projects/views. Its scope and deduplication semantics differ from the common scoped BFS and degree histogram, so the numbers are not a direct speedup comparison.',
'',
'| Post-deletion profile | Nodes / edges | Initial cache load | Warm BFS p95 | All-node degree analysis | Reload after insert | Reload after delete |','|---|---:|---:|---:|---:|---:|---:|']
for p,x in ca.items():lines.append(f'| {p} | {x["nodes"]:,} / {x["edges"]:,} | {x["initial_load_ns"]/1e6:.3f} ms | {x["bfs_p95_ns"]/1e3:.3f} µs | {x["degree_total_ns"]/1e6:.3f} ms | {x["reload_insert_ns"]/1e6:.3f} ms | {x["after_delete_reload_ns"]/1e6:.3f} ms |')
lines += ['',
'For the source profile, process RSS rose from a median 43.844 MiB before explicit cache load to 49.828 MiB afterward. Overlay RSS rose from 73.438 MiB to 80.703 MiB. These are observed whole-process deltas, not isolated cache allocations. Explicit reload after another connection’s insert and after deletion restored oracle results and original graph/vector cardinality on every store. The original nine stores were also profiled independently and passed the same 136,038 checks.',
'',
'## Cache freshness after writes','',
'Raw native cached BFS retained its snapshot in several tested cases. This is a cache-coherence hazard for the app, not evidence of committed SQLite row corruption. The documented explicit-load/reload model means DevWorks must not assume automatic invalidation across every API and connection. The public Cypher and public bulk paths behaved differently.',
'',
'| Scenario | Raw native observation | Explicit or guarded refresh |','|---|---|---|',
'| Same-connection Cypher node/edge insert and edge deletion | Fresh in tested traces | Correct |',
'| Another connection commits an edge or deletes a node | BFS stale | Correct |',
'| Same-connection public bulk helpers | Writer cache stale after bulk graph writes | Correct |',
'| Another connection uses public bulk helpers | Reader cache stale | Correct |',
'| Load cache inside write transaction, then roll back | BFS retained rolled-back node | Correct after reload; guarded read refuses write transaction |',
'| Read snapshot ends after another writer commits | Cache remained on prior snapshot | Correct |',
'| Rollback, then commit with reused numeric generation | Native BFS still returned ghost node | Correct; guarded protocol rejected dirty read and refreshed |',
'| Another writer has not committed or rolls back | Uncommitted graph/vector data stayed invisible to reader | Correct |',
'| Edge weight changes from total path cost 9 to 16 | Dijkstra returned 16 without explicit CSR reload | Correct; not all algorithms share BFS freshness behavior |',
'| Graph and vector writes in one explicit transaction | Commit visible; rolled-back rows absent after reopen | Correct |',
'| No intervening change | Guard reused cache without reload | Correct |','',
'The strongest control starts with cached reachability `[a, bulk, r, y]`, loads an uncommitted `ghost`, rolls back, then commits a different node reachable only from an unrelated root. The transactional numeric generation deliberately returns to the same value as the dirty generation. Native cached BFS still reports `ghost`; guarded BFS returns exactly `[a, bulk, r, y]`. Repeated three times. A generation counter alone is insufficient.',
'',
'The tested guard tracks transactional generation, `PRAGMA data_version` and `sqlite3_total_changes64` for its connection. A managed read during a write transaction is rejected and invalidates the guard stamp. Changed stamps trigger explicit reload; unchanged read-only snapshots reuse the cache. The protocol assumes exclusive ownership of cache lifecycle and controlled graph mutations. Production reads must bind version check, reload and algorithm execution to one coherent read snapshot and prevent an unrelated caller from replacing the cache. The scenario tests exercise interleavings at defined boundaries, not every concurrent race.',
'',
'Whole-database native algorithms are not a permission or project/view boundary. The common adapter’s per-hop filtering passed scope controls, but no production project-scoped CSR cache, atomic index-generation publication or optimized branch overlay was implemented.',
'',
'## Packaging and capability boundaries','',
'The published `graphqlite = "=0.12.0"` crate with its default `bundled-extension` feature embeds platform-specific extension bytes, extracts the selected library and dynamically loads it. The tested macOS arm64 library is 854,528 bytes and local `otool` inspection shows only libSystem linkage. No Python, Node, database server or GraphQLite service was required by the native helper. This is not a fully static extension build.',
'',
'Use an app-owned, versioned extension directory and validate trusted packaged bytes before loading. The evaluation supplies an isolated `GRAPHQLITE_EXTENSION_DIR` per process/case: the default loader may clean other extracted versions in its shared directory. The release payload, signing/notarization, Windows loading, Linux libc compatibility and minimum supported OS still require target packaging tests.',
'',
'The published archive’s VCS metadata records revision `995a8b9bc25cdd86d4c2b50ff7ff87f0eb236629` with `dirty: true`; a clean checkout at that revision must not be called byte-identical to this tested package. Runtime reports GraphQLite 0.12.0 and SQLite 3.54.1. Capability flags are recorded, but flags do not prove every advertised Cypher construct or graph algorithm was validated.',
'',
'Public bulk import commits chunks through the package helpers; graph and vector import batches are not published as one atomic whole-index generation. The explicit graph/vector mutation transaction tests passed, which is a narrower guarantee. Full schema migration, backup/restore, indexing publication, sustained update load and large representative graphs remain application acceptance work.',
'',
'## Evidence and reproduction','',
'[Machine-readable results](graphqlite-evaluation-results.json) contain both workload matrices, cache summaries, final freshness traces and recovery records. [Validation index](validation/graphqlite-evaluation/README.md) lists retained raw records and source snapshots. The downloadable evidence archive also includes the exact compressed fixture and published crate payload; generated database files and build caches are excluded.',
'',
'The indexed workload executable hash is `01eca79c5ad5e3091dcc9e7dbb8b113c0fb5e2379de6bbd61aa8c09f5e2786b6`. The original native-ID executable hash is `42d86fdab4e5ff948bac61666e4d92644f82e1d0134ebd084a73dd97a0d0fccb`. The final guard/cache/recovery executable hash is `a58b9444838ece5aa2f4d69f82aad5e3db2f9582cd92ca239b22f1f089453f71`. The guard was strengthened after common timing runs; the timed adapter/common/cache-profile source files are unchanged, and each full source snapshot is preserved. The baseline executable hash is `e9d4ec918fe3b57c35e5f0e61c295bfbfc49a39f15bc9053e4b2a667fabd3fe9`.',
'',
'From the archived `harness-final` directory, build before running:',
'',
'```sh',
'rustup run nightly-2026-10-06 cargo build --release --locked -p eval-graphqlite -p eval-sqlite',
'rustup run nightly-2026-10-06 cargo test --locked -p eval-common',
'python3 run.py --fixture /absolute/source.json --work-dir /fresh/measured --phase measured --repeats 3 --engines graphqlite,sqlite',
'python3 graphqlite_cache.py /fresh/measured/TIMESTAMP/manifest.json',
'GRAPHQLITE_EXTENSION_DIR=/fresh/extension target/release/eval-graphqlite --freshness /fresh/freshness',
'python3 recovery.py --fixture /absolute/source.json --work-dir /fresh/recovery --engines graphqlite,sqlite',
'python3 summarize.py /fresh/measured/TIMESTAMP/manifest.json --output /fresh/results.json',
'```','',
'Every output root must be disposable and new. Freshness is a diagnostic: raw stale controls are expected and are reported explicitly even when `case_errors` is empty. A passing diagnostic means those controls and managed results matched their declared expectations; it does not mean native caching stayed fresh automatically.',
'',
'Primary package and interface references: [GraphQLite source](https://github.com/colliery-io/graphqlite), [SQL interface and explicit cache reload](https://colliery-io.github.io/graphqlite/reference/sql-interface/), [other SQLite extensions](https://colliery-io.github.io/graphqlite/how-to/other-extensions/), [native algorithms](https://colliery-io.github.io/graphqlite/reference/algorithms/), [sqlite-vec source](https://github.com/asg017/sqlite-vec). Performance and observed behavior above come from the retained local runtime evidence.','']
(D/'devworks-graphqlite-evaluation.md').write_text('\n'.join(lines))
print(json.dumps({'common_checks_per_matrix':checks,'cache_checks_revised':sum(x['correctness_checks']for x in ca.values()),'report':str(D/'devworks-graphqlite-evaluation.md')}))
