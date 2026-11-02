# Shared operation architecture experiment

Candidate native Rust DTOs, generated JSON schemas, deterministic message fixtures and a SQLite receipt/control contract model. This is not the future `devworks-core` crate or a production SDK. Source/schema regeneration and tests are documented in [the evidence README](../../docs/architecture/validation/operation-contracts/README.md); semantics live in [shared operation contracts](../../docs/architecture/shared-operation-contracts.md).

`src/lib.rs` owns shared DTOs; domain modules extend them. `emit` regenerates the full schema manifest set. The fixture harness separately checks schema validity, typed semantic validity and normalized fingerprint rules. `probe_receipts.py` runs scripted transaction/race scenarios against actual SQLite. `build_register.py` keeps every retained catalog operation visible while domain schemas remain pending. No inference is executed.

Provider/session/package DTOs now live in `src/provider.rs` and `src/package.rs`, reusing generic common receipts. The owned helper in `src/package_probe.rs` is a disposable framing/RPC experiment. See [current combined evidence](../../docs/architecture/validation/provider-package-contracts/README.md) for protocol fixtures, native state models, process probes and source boundaries. No live provider or application effects are run.

Data/graph/retrieval/artifact/delivery DTOs live in `src/data_delivery.rs`. See [data/delivery evidence](../../docs/architecture/validation/data-delivery-contracts/README.md) for current combined checks, the [native store-port subset](../store-port-eval/README.md) and temporary Git probe. Historical reports preserve the source/schema digests of their original passes.


The complete catalog wire candidate now uses [domain-spec.json](domain-spec.json) as its explicit authoring source. `build_domain_contracts.py` generates and formats `src/domain_generated.rs`; `src/domain_support.rs` owns bounded scalars/collections/faults and `src/domain_rules.rs` the recorded pure cross-field checks. This large union is a schema qualification tool, not a proposed monolithic production dispatcher. Actual domain services/host authority/descriptor/SDK/provider behavior remain separate work.

Regenerate from the repository root in this order:

```sh
python3 tools/contracts-eval/build_domain_contracts.py
CARGO_TARGET_DIR="$PWD/.planning-build/data-delivery-contracts/cargo-target" cargo build --locked --release --manifest-path tools/contracts-eval/Cargo.toml
.planning-build/data-delivery-contracts/cargo-target/release/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py
PYTHONPATH=.planning-build/operation-contracts/python /path/to/authoring-python3.12 tools/contracts-eval/verify_domain_contracts.py --binary .planning-build/data-delivery-contracts/cargo-target/release/devworks-contract-probe --output docs/architecture/validation/domain-contracts
python3 tools/contracts-eval/build_domain_reference.py
node tools/planning/build_architecture.cjs
python3 tools/planning/build_implementation_plan.py
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/audit_domain_sources.py --repository /absolute/path/to/CodeGraph --output docs/architecture/validation/domain-contracts/source-binding-results.json
```

The authoring Python must match the installed jsonschema/rpds wheel ABI. The retained ignored dependency directory currently has Python 3.12 wheels; a default Python 3.14 cannot load its rpds extension. Use the matching bundled authoring interpreter or install compatible packages in a separate authoring directory. No Python runtime becomes a product dependency. Current [evidence](../../docs/architecture/validation/domain-contracts/README.md) records the observed failure and successful matching-interpreter run.

`verify_domain_contracts.py` combines the 707 new messages with the prior 322 cases, cross-checks native decoding/semantic expectations and JSON Schema, verifies every selected input/result and both modes for hybrid names, discloses legitimate shared input aliases, and reruns the 12 native stream/history scenarios. Historical helper frames are replayed as data, not claimed as new process execution. `audit_domain_sources.py` checks the frozen inventory file bytes and complete schema/ownership bindings; it does not assert retained source method semantics or actual native parity. No inference or application effects run.
