# Data, retrieval and delivery candidate evidence

1 November 2026. Architecture experiments on local macOS arm64 with Rust/Cargo 1.98.0. No production app, real inference, live GitHub writes, user repository mutations or other-platform acceptance. All temporary repositories/databases belong to their fixtures and are removed afterward.

- [Current schema/model results](schema-model-results.json): 39 generated schemas, 322 messages comprising 60 shared-control cases, 113 provider/package cases, 55 historical helper frames replayed as fixtures and 94 new data/delivery cases. All pass JSON Schema Draft 2020-12 and native typed validity expectations. Some structural schemas deliberately accept values that native cross-field checks reject. Every one of the 58 public input bindings has a conforming fixture; selected union discriminators are checked. This does not prove their runtime dispatch or authority.
- [Native DTO detail](native-results.json): normalized forms and rejection reasons for exact views, source citations, schema-only metadata, coverage, scope selectors, embedding byte shape, document locators, publications, artifacts/deletions, readiness DAGs, typed Git inputs, incomplete/queued delivery and release actions. A source-result citation is checked against its resolved view as well as against its own repeated snapshot.
- [Native model results](scenario-results.json): the existing 12 provider/session scenarios and 26 steps rerun against the current candidate binary. This is an in-memory model, not an actual provider or session adapter. The owned-helper process itself was **not** rerun in this pass; its actual execution evidence remains in the [previous report](../provider-package-contracts/README.md).
- [Native store results](native-store-results.json): 25 checks through a small compiled driver-free port boundary with actual **rusqlite 0.41.1, SQLite 3.54.1 and sqlite-vec v0.1.7**. Effective WAL/FULL/foreign-key settings are read back. Tests cover rollback, receipt replay and conflicts, atomic history/outbox changes, whole-unit replacement/deletion and endpoint filtering, scope/source checks, authorized vector ranking before top-k, dimension/nonfinite rejection, scoped FTS, stale publication and pointer comparisons, old/expired pins and access revocation before snippets. The unauthorized closest vector cannot displace the authorized result. FTS input is treated as literal text rather than exporting its query grammar.
- [Git results](git-delivery-results.json): 17 checks using actual Git 2.53.0, two disposable bare remotes and owned worktrees. They cover the all-ready barrier, dependency ordering/cycles, a lost acknowledgment after a real regular merge, observation without resubmission, changed-head blocking, preserved partial delivery, clean/owned/quiescent cleanup, a concurrent-commit expected-ref deletion race and incomplete queued/unknown states. PR/check/review/queue metadata is scripted; no GitHub API was called.
- [Refinement review](refinement-review.json): source-boundary checks, transaction-lifetime compile-fail result, current input digests and headless reader/diagram checks. Browser QA opens no user-facing tabs. Narrow visual checks supplement the editable Mermaid/SVG sources.

The store source initially failed compilation because `gen` is reserved in Rust 2024. Renaming the local variable fixed it; the current release build and compile-fail doctest pass. No runtime success is claimed for that failed build. The source-result citation fixture additionally guards against a mismatched view rather than relying on duplicated citation fields agreeing with each other.

The native store is a fixture-specific subset. It does not implement every repository, async facade, queue claim, source adapter, transaction cancellation, generation-reference collector, migration, restore or power-loss scenario. Its pin-reclamation check considers view eligibility only, not all generation/file references. The scalar sqlite-vec L2 function proves scoped correctness on tiny data; it does not qualify accelerated vector indexing or representative performance. Current upstream documentation may describe a different version from this exact probe pin. No update to that pin is implied.

Git's ordinary merge ancestry is sufficient for this fixture's chosen method. Squash/rebase, actual async ticket expiry, merge queues, server rule evaluation, auth, remote cancellation, base/head races and real PR evidence still need dedicated adapter fixtures and later authorized compatibility checks. Core policy, OS process fencing and filesystem target-race safety are not demonstrated by temporary Git ownership flags.

Reproduce from the repository root with Cargo and the established authoring Python/Node runtimes. Python `jsonschema==4.26.0` is installed only in the ignored authoring directory; none of these authoring tools becomes a shipped runtime dependency.

```sh
CARGO_TARGET_DIR="$PWD/.planning-build/data-delivery-contracts/cargo-target" cargo build --locked --release --manifest-path tools/contracts-eval/Cargo.toml
.planning-build/data-delivery-contracts/cargo-target/release/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_data_delivery_fixtures.py
python3 tools/contracts-eval/build_register.py
PYTHONPATH=.planning-build/operation-contracts/python python3 tools/contracts-eval/verify_data_delivery_contracts.py --binary .planning-build/data-delivery-contracts/cargo-target/release/devworks-contract-probe --output docs/architecture/validation/data-delivery-contracts
CARGO_TARGET_DIR="$PWD/.planning-build/data-delivery-contracts/store-target" cargo build --locked --release --manifest-path tools/store-port-eval/Cargo.toml
CARGO_TARGET_DIR="$PWD/.planning-build/data-delivery-contracts/store-target" cargo test --locked --release --doc --manifest-path tools/store-port-eval/Cargo.toml
python3 tools/contracts-eval/probe_git_delivery.py
node tools/planning/build_architecture.cjs
```

Run the store executable with a newly owned temporary path, never an existing user database:

```python
import subprocess, tempfile
from pathlib import Path
with tempfile.TemporaryDirectory(prefix="devworks-store-port-") as temporary:
    subprocess.run([
        ".planning-build/data-delivery-contracts/store-target/release/devworks-store-port-probe",
        str(Path(temporary) / "probe.sqlite"),
    ], check=True)
```

The schema/model report records source, lockfile, schema and fixture digests. Previous reports preserve historical digests/counts and should not be overwritten to suggest their larger integration scopes have now passed. At this historical checkpoint the catalog had **58 input candidates, 287 pending** (before the two SystemOne operations). The [2 November domain pass](../domain-contracts/README.md) now provides 347 input/result candidates with zero unbound inputs. This report preserves its original source digests/counts; actual retained service/authority behavior still needs qualification.
