# Bug report: deleted nodes reappear in dense vector search after deletion flush

**Project:** OverGraph  
**Tested release:** published Rust crate `overgraph = 0.18.0`  
**Reported:** 1 November 2026  
**Status:** suspected upstream engine defect, independently reproducible through public API; internal root cause unconfirmed  
**Confidence:** high in the observed symptom and its reproduction without the DevWorks adapter; affected-version/platform range unknown

## Summary

A previously flushed vector-bearing node is deleted successfully: `get_node(victim_id)` returns `None`, and an immediate vector search excludes it. After `flush()` persists that deletion, the same dense vector search returns the deleted node’s ID again. The inconsistency survives graceful close/reopen and inspection from a separate process.

The attached standalone Cargo project reproduces this using only OverGraph public calls. It has no DevWorks code, comparison adapter, logical-ID mapping, SQLite, external database service, generated code graph, or model inference. Its other direct dependencies only support error handling and JSON output.

This report concerns **deleted-node visibility in dense vector search**. A separately discovered project-path scoping mismatch was caused by our adapter using a final-result label filter as an intermediate traversal restriction. That adapter mistake is excluded from this reproduction; no graph edges or traversal scope are used here.

## Frozen dependency and build configuration

- Registry dependency: `overgraph = { version = "=0.18.0", default-features = true }`.
- Enabled OverGraph features: `default`, including `production-concurrency` (captured in `evidence/features.txt`).
- Published crate checksum: `ab605774b959c8dacb8d73f1a768f5634d0eabd6ff7e851cda6f381e3d8c78bc`.
- Published crate `.cargo_vcs_info.json` identifies source commit `bf834023508be5a9b28b349a6982a4735d59d918`.
- Other direct dependencies: `anyhow = 1.0.104`, `serde_json = 1.0.154`. The attached lockfile freezes the complete 39-package project; it contains no DevWorks dependencies.
- Release profile: opt-level 3, LTO disabled, 16 codegen units, debug information disabled.
- No custom `RUSTFLAGS`, `CARGO_BUILD_RUSTFLAGS`, `RUSTC_WRAPPER` or `MACOSX_DEPLOYMENT_TARGET` was set.

Database configuration:

```rust
DbOptions {
    wal_sync_mode: WalSyncMode::Immediate,
    dense_vector: Some(DenseVectorConfig {
        dimension: 384,
        metric: DenseMetric::Cosine,
        hnsw: HnswConfig { m: 16, ef_construction: 200 },
    }),
    ..Default::default()
}
```

Dense search configuration: mode `Dense`, `k = 10`, `ef_search = Some(256)`, label filter `All(["Project0"])`, no traversal scope, and no sparse/hybrid settings. Remaining database/node options use the published defaults. There are only two nodes; requesting ten results should return fewer when only one live node is eligible.

## Observed environment

| Item | Recorded value |
| --- | --- |
| OS / target | macOS 27.0.1 arm64 / `aarch64-apple-darwin` |
| Hardware | Apple M5 Max, 18 cores, 128 GiB RAM |
| Storage | Local APFS filesystem |
| Rust | `rustc 1.101.0-nightly (ea137335b 2026-10-05)` |
| Rust commit | `ea137335b78829b4514bf1b4c16302f74fab8581` |
| Cargo | `cargo 1.101.0-nightly (f3865b2a4 2026-09-29)` |
| Installed toolchain name | `nightly-2026-10-06` |
| LLVM | 23.1.3 |

This is development-host evidence. Other Rust versions, OverGraph releases, operating systems, feature combinations and alternative vector-search configurations have not been qualified by this standalone test.

## Minimal triggering sequence

1. Open a fresh database with the configuration above.
2. Insert two nodes, both labeled `Symbol` and `Project0`:
   - victim: a 384-dimensional unit vector with component 0 set to 1;
   - survivor: a 384-dimensional unit vector with component 1 set to 1.
3. Call `flush()` to persist the inserted nodes.
4. Call `delete_node(victim_id)` and wait for success.
5. Verify `get_node(victim_id) == None`; a dense query using the victim’s vector currently returns only the survivor.
6. Call `flush()` again to persist the deletion.
7. Repeat point lookup and the identical dense search.
8. Close/drop the engine, reopen the same database, and repeat both reads.
9. Exit the process; open the same case from a fresh inspection process and repeat both reads.

No rename is necessary. The attached program also tests rename/no-rename and initial-flush/no-initial-flush variants as controls.

## Expected result

Once the deletion has succeeded, the victim must remain absent from both current point lookup and current dense vector search, including after flushing, closing and reopening. Since only the survivor is live, the expected returned-ID set is `{survivor_id}`.

Neither flushing a successful deletion nor reopening the store should make the deleted node eligible for current vector retrieval again. The test does not request a historical epoch or retain an explicit historical read session.

## Actual result

In the captured runs, victim ID was 1 and survivor ID was 2. The program records the actual native IDs returned by insertion; it does not assume these values.

For the initial-flush/no-rename case:

| Phase | `get_node(victim)` | Dense vector IDs |
| --- | --- | --- |
| Before deletion | Present | `[1, 2]` |
| After successful deletion, before deletion flush | Absent | `[2]` |
| After deletion flush | Absent | `[1, 2]` — victim returned |
| After graceful close/reopen | Absent | `[1, 2]` — victim returned |
| Separate inspection process | Absent | `[1, 2]` — victim returned |

The observed symptom is **stale vector retrieval**, while point lookup remains correct. Consumers can receive an ID that no longer resolves to a live node.

## Controls and repetition

Each repetition uses a fresh root directory and four separate databases. Three complete standalone repetitions reproduced this matrix:

| Initial node flush | Rename before deletion | After delete, before deletion flush | After deletion flush | After same-process reopen |
| --- | --- | --- | --- | --- |
| No | No | Victim excluded | Victim excluded | Victim excluded |
| No | Yes | Victim excluded | Victim excluded | Victim excluded |
| Yes | No | Victim excluded | **Victim returned** | **Victim returned** |
| Yes | Yes | Victim excluded | **Victim returned** | **Victim returned** |

There are 12 standalone variant executions in total. Point lookup reports the victim absent after deletion in every variant and phase. Six additional fresh-process inspections were performed: initial-flush/no-rename and no-initial-flush/no-rename for each repetition. The three affected inspections returned the stale victim; all three control inspections excluded it. Fresh-process inspection of the rename variants was not performed.

The earlier DevWorks evaluation also observed stale deleted-vector IDs across nine fresh-database cases. Those larger runs motivated this report, but the standalone evidence does not depend on their data or adapter.

## Build and reproduce independently

Extract the attached bundle into a new directory. Build from the included standalone project, keeping its manifest and lockfile intact:

```sh
cd repro
rustup run nightly-2026-10-06 cargo build --release --locked
```

The recorded toolchain must already be available, or the verifier must explicitly document a substitute toolchain. An uncached build needs access to registry dependencies; there are no inference/API credentials or external services involved. The supplied lockfile is essential for comparing the same dependency set.

The recorded build reused an existing Cargo target directory solely as a compilation cache; the project and its lockfile are independent. The commands above build into the standalone project’s normal target directory.

Create a temporary parent directory, then pass a **nonexistent child path** to the program:

```sh
overgraph_repro_parent=$(mktemp -d)
./target/release/overgraph-delete-repro run "$overgraph_repro_parent/databases" > "$overgraph_repro_parent/run.json"
```

**Exit code 1 is expected when the stale-vector symptom is observed.** If your shell uses `set -e`, capture that exit explicitly so it does not prevent inspecting the JSON. The program refuses to reuse an existing run root and never removes a directory.

Inspect `run.json` for the actual victim ID. The captured runs used ID 1. With that ID, inspect the affected case in a completely new process:

```sh
./target/release/overgraph-delete-repro inspect "$overgraph_repro_parent/databases/flush-true-rename-false" 1
```

Expected observed-bug output includes `point_exists: false`, `deleted_id_in_vectors: true`, `vector_ids: [1, 2]` and `observed_stale_vector: true`. Use the actual ID from your run if it differs.

Then inspect the negative control:

```sh
./target/release/overgraph-delete-repro inspect "$overgraph_repro_parent/databases/flush-false-rename-false" 1
```

The observed control output has the victim absent from point lookup and vector results, with `observed_stale_vector: false` and exit code 0.

Program exit-code contract:

| Code | Meaning |
| --- | --- |
| 0 | Executed cases completed without observing an absent node returned by vector search |
| 1 | That inconsistency was observed; JSON contains the observations |
| 2 | Setup or database-execution error; inspect stderr rather than counting it as a reproduction |

The reopen helper retries only the exact `Busy` / `ActiveSession` directory-admission states for at most ten seconds, following OverGraph’s lifecycle-test pattern. It does not retry writes, alter results, or filter vector hits.

## Evidence included

- `repro/Cargo.toml`, `repro/Cargo.lock`, `repro/src/main.rs`: complete standalone source and dependency pins.
- `evidence/run-001.json` through `run-003.json`: all four variants and their native IDs/observations per repetition.
- `evidence/inspect-*-flushed-true.json`: three separate-process affected-case observations.
- `evidence/inspect-*-flushed-false.json`: three separate-process controls.
- Matching `.stderr` files: captured diagnostics, empty in successful executions.
- `evidence/verification.json`: commands, exit codes, host/compiler details, input hashes, published package checksum and reference executable hash.
- `evidence/locked-build.log`, `evidence/build.log`, `evidence/features.txt`: build and enabled-feature evidence.
- `INDEPENDENT-VERIFICATION.md`: a ready-to-paste verification brief.
- `SHA256SUMS`: package-file integrity hashes. Executable bytes and generated databases are excluded; build and create fresh disposable databases.

The reference standalone executable SHA-256 is `ce93ebd2a0a09d80216657d9400e0e37187bedcd96e8cf68474a4f4c10242178`. Binary identity is provenance, not a requirement that another machine’s build be byte-identical.

## Source references and investigation leads

Pinned upstream source:

- [Published source revision](https://github.com/Bhensley5/overgraph/tree/bf834023508be5a9b28b349a6982a4735d59d918)
- [Public engine operations](https://github.com/Bhensley5/overgraph/blob/bf834023508be5a9b28b349a6982a4735d59d918/src/engine/mod.rs)
- [Node-deletion planning](https://github.com/Bhensley5/overgraph/blob/bf834023508be5a9b28b349a6982a4735d59d918/src/engine/write.rs)
- [Dense search and candidate visibility](https://github.com/Bhensley5/overgraph/blob/bf834023508be5a9b28b349a6982a4735d59d918/src/engine/read.rs)
- [Existing dense deleted-node exclusion test](https://github.com/Bhensley5/overgraph/blob/bf834023508be5a9b28b349a6982a4735d59d918/src/engine/tests/read.rs#L14790-L14855)

The existing test flushes the initial nodes, deletes one, and searches **before flushing the deletion**. The standalone failure extends that sequence across the deletion-flush and reopen boundaries. Relevant areas to inspect include `plan_delete_node`, `vector_search_dense_with_scope`, `score_dense_candidate_ids` and persisted deletion visibility. These are investigation leads, not a verified internal cause or proposed patch.

No upstream code was modified, no workaround was applied to search results, and no issue or patch has been submitted. The independent session should confirm or refute the observation and API expectations before proposing a fix. First affected version, feature-specific cause, production impact beyond this operation, and behavior on other platforms remain unknown.
