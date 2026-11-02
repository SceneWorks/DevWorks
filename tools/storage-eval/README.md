# DevWorks local storage evaluation

These disposable native Rust programs compare OverGraph, Grafeo, HelixDB and SQLite. **Kuzu is excluded.** They are evaluation tooling, not the DevWorks application or four proposed production backends. No provider or model inference is invoked. Python only orchestrates owned child processes; it is not a proposed application prerequisite.

The frozen candidate pins and transitive dependencies are in `Cargo.lock`. Helix uses Git package `db` aliased as `helix-core`, plus the AST package at the same revision. Grafeo's model-generation feature is disabled. SQLite and sqlite-vec are statically linked. The local runs use installed `nightly-2026-10-06` (Rust 1.101 nightly), release opt-level 3, no LTO and 16 codegen units for every candidate. Grafeo 0.6.0 declares Rust 1.99 minimum; silently selecting an older crate on another compiler invalidates the comparison.

## Reproduce

Use a disposable work directory, outside source repositories. Preserve all attempts. The commands below create fresh directories; none deletes or reuses a previous database.

```sh
rustup run nightly-2026-10-06 cargo test --locked -p eval-common --manifest-path tools/storage-eval/Cargo.toml
rustup run nightly-2026-10-06 cargo build --release --locked --manifest-path tools/storage-eval/Cargo.toml
rustup run nightly-2026-10-06 cargo run --release --locked --manifest-path tools/storage-eval/fixture/Cargo.toml -- /path/to/disposable/source.json CodeGraph=/absolute/path/to/CodeGraph simple-router=/absolute/path/to/simple-router dark-factory=/absolute/path/to/dark-factory
python3 tools/storage-eval/run.py --fixture /path/to/disposable/source.json --work-dir /path/to/disposable/pilots --phase pilot
python3 tools/storage-eval/recovery.py --fixture /path/to/disposable/source.json --work-dir /path/to/disposable/recovery
python3 tools/storage-eval/run.py --fixture /path/to/disposable/source.json --work-dir /path/to/disposable/measured --phase measured --repeats 3
python3 tools/storage-eval/summarize.py /path/to/disposable/measured/TIMESTAMP/manifest.json --output /path/to/disposable/aggregate.json
```

The fixture generator accepts `name=/absolute/repository/path` arguments, in order. The captured evaluation used CodeGraph, simple-router and dark-factory; substitute their local paths in the command above. Regeneration from changed checkouts is a new dataset. The fixture records source heads, working-tree state, counts and an aggregate digest. A preserved fixture is necessary for identical input bytes. `run.py` records its SHA-256, lockfile digest, executable hashes, compiler/profile and raw samples. Build must finish before running the programs. The macOS runner uses `/usr/bin/time -l` to retain maximum process RSS; this includes fixture parsing and the independent oracle, not just engine memory. Other platforms need equivalent resource instrumentation.

## Semantics and limitations

- Tree-sitter captures structural containment and unique-name syntax-reference candidates. It is not a compiler-resolved call graph. Fixed 384-dimensional SHA-256-derived numerical vectors exercise mechanics and recall; they say nothing about semantic RAG quality.
- The small profile takes a project-stratified induced subgraph. The source profile retains all captured nodes. An isolated control project duplicates vectors; foreign-project edges test boundary handling. The overlay profile materializes a second view with tombstones/renames. It measures isolation and full-snapshot cost, not an optimized delta index.
- Native IDs are normalized through an adapter-owned persisted mapping. Primitive adjacency guards scope before expansion; vector results are never silently filtered to hide a failed native constraint. Queries are checked against an independent oracle, with raw recall recorded for approximate indexes.
- Graph-scoped vector query timing excludes preparation of the eligible-ID list. OverGraph uses its native traversal scope; other adapters supply the exact eligible IDs. These are different execution strategies. Do not describe this number as complete graph retrieval latency or compare ANN speed without its measured recall.
- The same auxiliary SQLite FTS5 file and reciprocal-rank fusion code are used for all engines. Text is symbol names. Source locators resolve through the frozen fixture; repository roots have repository locators. This is a retrieval mechanics probe, not document extraction/citation acceptance.
- Ten and twenty native readers run both alone and alongside one rename writer, with a barrier for the latter. The writer modifies a reserved node; readers inspect other nodes. It tests interference and acknowledged visibility, not full concurrent file reindexing or multi-writer transaction isolation. A degree histogram tests one application-owned graph aggregate.
- Deletion removes twelve nodes and verifies points, incident edges and vectors before/after reopen. Structured `case_errors` mean correctness failure even when the executable exits zero. Always inspect them before ranking timings. Some unexpected errors abort a case; preserve its stderr and failed database.
- OverGraph's production-concurrency tests document a bounded reopen retry while native shutdown releases the directory. The adapter retries only Busy/ActiveSession for at most ten seconds; the wait is included in reopen time. This is not an unbounded error suppression policy.
- Recovery probes kill an owned child with SIGKILL after a final acknowledged rename, without another engine checkpoint, then check all points and scoped vectors in a fresh process. Interrupted-import probes kill 50ms after opening and check only that the partial store can reopen/checkpoint/close; they do not prove the interrupted import was published, resumed, or an HNSW build was interrupted. No power-loss guarantee follows from a process-kill test.
- Fresh process/database does not mean cold OS file cache. No cache purge or exclusive-host assumption is made. Query samples include first-access and warm operations. Three independent databases per profile allow run-to-run ranges, not strong statistical or platform-wide claims. p99 is withheld below 1,000 samples.
- Release executable size is the complete benchmark helper, including common code. It is not installed DevWorks size. Logical storage bytes include mapping and shared lexical files; peak RSS includes fixture/oracle overhead. Source extraction is outside database timings.

The full retained CodeGraph operation inventory, large production datasets, delta publication/reconciliation, power-loss durability, migration/backup and Windows/Linux/minimum-macOS packaging remain architecture acceptance gates. This harness must not be used to claim those tests passed.

## Raw OverGraph attribution probe

`overgraph/examples/correctness_probe.rs` uses only raw OverGraph public calls: no comparison adapter, logical-ID map or oracle. Build with `cargo build --release --locked --manifest-path tools/storage-eval/Cargo.toml -p eval-overgraph --example correctness_probe` using the recorded toolchain. Run `tools/storage-eval/target/release/examples/correctness_probe /fresh/disposable/root`. It records four deletion variants (initial flush yes/no and rename yes/no), before/after deletion flush and reopen, plus an explicit three-node cross-project path. It is a diagnostic that records an observed defect, not a regression test that assumes future versions must retain the defect.

The result-label versus intermediate-path distinction is an adapter mistake. The flushed-deletion vector result independently reproduces in the engine; the root cause is not established. Adding this example does not change or rerun the measured workload.


## GraphQLite follow-up

The GraphQLite 0.12.0 helper links the same bundled SQLite 3.54.1 and sqlite-vec 0.1.7 as the baseline. Graph data uses public bulk helpers and parameterized indexed Cypher patterns, vectors use the same SQLite file/connection, and shared scoped BFS does not use the global algorithm cache. Published native extension bytes are extracted and loaded at runtime; this is not a fully static graph extension.

```sh
rustup run nightly-2026-10-06 cargo build --release --locked --manifest-path tools/storage-eval/Cargo.toml -p eval-graphqlite -p eval-sqlite
python3 tools/storage-eval/run.py --fixture /absolute/source.json --work-dir /fresh/measured --phase measured --repeats 3 --engines graphqlite,sqlite
python3 tools/storage-eval/graphqlite_cache.py /fresh/measured/TIMESTAMP/manifest.json
GRAPHQLITE_EXTENSION_DIR=/fresh/extension tools/storage-eval/target/release/eval-graphqlite --freshness /fresh/freshness
python3 tools/storage-eval/recovery.py --fixture /absolute/source.json --work-dir /fresh/recovery --engines graphqlite,sqlite
```

Every destination must be owned and disposable. The runners isolate `GRAPHQLITE_EXTENSION_DIR`; do the same when invoking examples manually, since default extension-cache extraction may clean other versions. `graphqlite_cache.py` operates on completed generated common-workload stores only, inserting and removing its temporary fixtures. Do not point it at production databases.

`--freshness` records native stale-cache controls as well as managed behavior. Empty `case_errors` does not imply automatic native freshness. The guard requires generation/data-version/own-change tracking, rejects write-transaction cache reads and must own cache lifecycle. Native algorithms span the whole test database and are not a project/view security boundary. The query probe example retains EXPLAIN SQL for three point-query shapes. See `docs/architecture/devworks-graphqlite-evaluation.md` for all measured outcomes and limitations; SQLite was selected by the user, GraphQLite is not selected.
