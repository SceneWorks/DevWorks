# Persistence refinement evidence

1 November 2026. These are architecture mechanics probes, not production migrations, a Rust storage adapter, or completed G01/G06/G10/G12 acceptance. No inference or network provider was invoked.

- [SQL fixture at this review](schema-at-review.sql): initial relational records and indexes, with derived FTS storage. Row-spanning application policies remain explicit in the architecture.
- [SQLite mechanics results](sqlite-mechanics.json): 28 checks on bundled Python SQLite 3.53.1. Covers rollback across domain/history/queue, optimistic updates, uniqueness, deterministic competing writers/claims, stale generations, read snapshots, layer masking, conditional publication, dimension/length constraints, FTS, a database/reference-file snapshot, corruption detection and migration rollback.
- [Native scalar vector results](vector-distance.json): actual rusqlite 0.41.1, linked SQLite 3.54.1 and sqlite-vec 0.1.7. The same schema loads; the authorized set is materialized before exact ranking, while the global nearest candidate is excluded. Checks real distance and dimension errors. In-memory fixture; no production-scale or complete policy/overlay acceptance.
- [Native probe source and lockfile](vector-probe/Cargo.toml): isolated authoring crate, not an application dependency or an additional store.

From the repository root, use the bundled Python runtime for:

```sh
python3 tools/storage-eval/persistence_contract_probe.py --schema docs/architecture/validation/persistence-refinement/schema-at-review.sql --output-dir /path/to/owned/evidence
```

For the native probe:

```sh
cargo run --locked --offline --release --manifest-path docs/architecture/validation/persistence-refinement/vector-probe/Cargo.toml --target-dir tools/storage-eval/target -- docs/architecture/validation/persistence-refinement/schema-at-review.sql
```

The first command uses an owned temporary directory for all mutable database/file fixtures and saves only its report to the selected output directory. The native probe uses an in-memory database and requires locally cached dependencies for offline reproduction. Neither command accesses the user's application store, repositories, target databases, provider credentials or real models.

Outstanding acceptance includes compiled Rust repository behavior, scope/approval/dependency policies, process/resource ownership, power-loss and disk-fault behavior, actual generation pin/reclamation, backup barriers and concurrent artifact deletion, restore journal interruption, project export/import, all target operating systems and representative performance. Do not infer these from passing SQL constraints or a successful local backup API call.

The current architecture schema has since gained router request, connection and pricing records. This directory preserves the exact persistence-pass schema and its original result hashes; router-pass evidence is recorded separately.
