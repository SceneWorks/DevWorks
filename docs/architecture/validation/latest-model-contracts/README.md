# DW-108 latest model catalog contract qualification

Confidence: high for the schema, typed-message and ownership checks recorded here. These checks run no inference and do not establish runtime inventory, allocation, connection policy or UI acceptance; DW-108's integrated native evidence owns those behaviors.

`router.catalog` adds schema revision 3 and a query-only flat `RouteCatalogModel`. Its optional gated metadata reports family identity, plural concrete serving-scope targets, known inventory availability and a safe reason. Original `RouteModel` configuration inputs are unchanged. Both prior catalog projections retain their recorded shape and names through `earlierName: RouteModel` and `earlierBefore: 3`; virtual alias rows are omitted by the native handler for older clients. Existing named-model intent carries all eight reserved aliases without a new input union variant.

The [legacy digests](legacy-schema-sha256.json) record all 56 revision-1/2 schema files from immutable DW-71 head `bffce8562a9b77ae8f192775b1dfd96adbb7d694`. Every file remains byte-identical after regeneration; no golden exception was added. The native revision regression checks these digests. The native fixture admission expectation deliberately grows from 498 to 502: catalog, route, pin and availability each add revision 3. Route/pin retain revisions [1,3], since revision 2 was never qualified for them. Operation and mode counts remain unchanged.

The [schema/model report](schema-model-results.json), [strict native results](native-results.json) and [scenario results](scenario-results.json) preserve source digests and complete probe output. Fixtures cover plural targets, missing and unknown inventory, legacy rejection of new fields, null refusal for gated fields and rejection of caller-injected catalog facts in configuration. RouteChoice snapshots separately retain resolved upstream model/profile and the identity origin, including truthful current-row migration observations and unknown old identity; prior receipt JSON remains untouched. Seven generator tests include the explicit rename boundary and configuration separation.

Reproduce using an authoring Python with matching jsonschema/rpds dependencies:

```sh
python3 tools/contracts-eval/build_domain_contracts.py
cargo build --locked --manifest-path tools/contracts-eval/Cargo.toml --target-dir /tmp/devworks-latest-contract-probe
/tmp/devworks-latest-contract-probe/debug/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py --date 2026-11-05
python3 tools/core-codegen/generate.py
python3 tools/contracts-eval/build_domain_reference.py
python3 tools/planning/build_implementation_plan.py
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/verify_domain_contracts.py --binary /tmp/devworks-latest-contract-probe/debug/devworks-contract-probe --output docs/architecture/validation/latest-model-contracts --date 2026-11-05
python3 -m unittest discover -s tools/core-codegen -p 'test_*.py'
cargo test --locked -p devworks-core --test registry --test schema_revisions --test recorded_fixtures
cargo fmt --all --check
```

The Rust workspace tests run on the integrated DW-108 branch with its updated catalog handler. Probe and schema checks remain independently executable. The regenerated architecture/plan owns 377 requirements, 356 operations in 49 families and 77 slices, with zero unowned requirements or operations; approved Word source and prior visual QA remain unchanged by this API revision.
