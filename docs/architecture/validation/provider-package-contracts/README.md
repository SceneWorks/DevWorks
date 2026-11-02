# Provider/session/package candidate verification

Historical provider/package pass. The later [data/delivery report](../data-delivery-contracts/README.md) records the current combined schema/input counts; counts and hashes below preserve this pass.

1 November 2026. This is architecture qualification evidence at explicitly limited layers. No live provider connection, real inference, production application, broker effect, installed package, secret access or native sandbox was exercised.

- [Schema/model results](schema-model-results.json): 24 generated candidate schemas; 228 typed/schema message checks comprising 60 prior shared-control fixtures, 113 provider/session/package fixtures and 55 frames actually exchanged with the native helper. Sixteen public operation bindings also pass selected-discriminator checks, including rejection of an Install body selected through Disable and a Continue body selected through Start.
- [Native DTO detail](native-results.json): normalized forms and rejection reasons. The 113 new fixtures include all eight contribution categories, manifests/dependencies/files, raw codec versus host cost classification, native continuation/attempt bindings, session message authority, transcript ordering, package RPC/credits/state/lifecycle requests and responses.
- [Native stream/history scenarios](scenario-results.json): 12 deterministic in-memory scenarios and 26 steps covering default terminal readiness, incomplete/EOF evidence, duplicate calls, changed attempts, foreign opaque items, out-of-order tool results, one response per native call, later operation events, batch replay/conflicts, failure atomicity and explicit epochs. This is not actual provider wire parsing or the production session repository.
- [Native helper process results](package-process-results.json): 40 checks, including repeated handshake assertions, with 55 typed frames over actual owned stdin/stdout pipes. Checks cover fragmented input, pre-activation refusal, broker round trips, cancel acknowledgment versus outcome, late uncertainty, stale/changed generations, a quiescent activation barrier, independent invocations, invocation-isolated credit windows, replay/overflow/cancel credit refusal, request namespaces, duplicate/unknown responses and eight malformed framing cases. Every owned process exited or was killed/reaped by its fixture owner.
- [Actual frame fixtures](actual-frame-fixtures.json) retain deterministic, nonsecret toy values for reproduction. Host policy, grant tables, artifacts and possible callback effects are scripted. The helper does not read real artifact bytes, validate signatures or operate a real application target.

The initial cross-account attempt fixture accidentally shared the same Python binding object on both sides, so its mutation changed both. The fixture was corrected to separate those objects; the current combined run rejects the foreign item. A reporting-path issue was also corrected. Neither was a live-provider observation or a production defect.

Build host is local macOS arm64, Rust/Cargo 1.98.0; exact resolved dependencies are in `tools/contracts-eval/Cargo.lock`. Current probe pins include Serde 1.0.229, serde_json 1.0.154, Schemars 1.2.3 and semver 1.0.28. JSON Schema validation uses authoring-only jsonschema 4.26.0. No Python, Node, schema-generator runtime or development toolchain is implied as a shipped app requirement.

Reproduce from the repository root using the established bundled authoring runtimes and `NODE_PATH` where necessary:

```sh
CARGO_TARGET_DIR="$PWD/.planning-build/provider-package-contracts/cargo-target" cargo build --locked --release --manifest-path tools/contracts-eval/Cargo.toml
.planning-build/provider-package-contracts/cargo-target/release/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_protocol_fixtures.py
python3 tools/contracts-eval/probe_package_process.py --binary .planning-build/provider-package-contracts/cargo-target/release/devworks-contract-probe --output docs/architecture/validation/provider-package-contracts
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/build_register.py
PYTHONPATH=.planning-build/operation-contracts/python python3 tools/contracts-eval/verify_protocol_contracts.py --binary .planning-build/provider-package-contracts/cargo-target/release/devworks-contract-probe --output docs/architecture/validation/provider-package-contracts
```

Install authoring `jsonschema==4.26.0` in that ignored Python directory if absent. The earlier operation-contract evidence preserves its own historical source/schema digests; this report is the current combined candidate run. Regenerate the schema manifest after DTO changes and keep the family register aligned with the catalog.

Still unqualified: actual SSE/provider request/continuation codecs and subscription auth, native JCS, real session repository/crash semantics, all SDK contribution execution, host token/role/permission enforcement, signed packages and dependency closure, actual atomic registry/state publication/rollback, complete credit consumption/bulk transport/queue overload, OS process-tree/sandbox/credential behavior and the supported target matrix. A wire/schema experiment is not evidence that those services already work. Remaining domain schemas are explicit: 47 families/345 catalog operations, 16 concrete public input candidates and 329 pending inputs. Internal router/codec/package messages are separately typed and do not inflate that public count.
