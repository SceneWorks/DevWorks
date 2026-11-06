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

Native runtime qualification on the assembled candidate (`415e023`, whose merge has the same tree as `411809c`) passed the workspace build, strict workspace/all-target Clippy, formatting, seven generator tests and clean regeneration. The expanded native fixture sentinel is 1,369 domain fixtures and 502 operation/mode/revision admissions; both expectations were updated deliberately after the broader run exposed their old values and the old tier-default expectation.

The adapter baseline at `eccd2e5` passed 590 unit tests and 95 integration tests, with three existing intentional ignores. Fresh final router (36) and connection (41) tests cover the four subsequently added cases and all affected adapter paths; the remaining workspace passed 656 tests in 52 suites. Together these cover 1,345 distinct native tests. The final affected UI run independently passed 65 tests, including actual background/manual inventory completion, Home/sidebar and Jobs selectors, exact pin labels and unknown-outcome recovery.

Runtime cases cover strict complete/paged inventories, hourly and reopen timing, failed/partial/stale publication, disappearance/return/disabled settings, numeric promotion and fallback, per-connection scope, preserved concrete pins and receipt replay, schema projection, migration provenance, capability/effort constraints and editable tier/configuration round trips. These use deterministic native HTTP fixtures and SQLite, with no inference. CI uses the existing eight jobs and unchanged triggers, dependencies and timeouts; no added matrix or campaign.

The automated-review repair at `1a547f5` adds three native regressions: long Unicode provider identities keep a bounded display label; edited discovery rows cannot collide with rediscovery or lose disabled settings; retained cooling pins keep recoverable blockers and retry times after promotion. Each regression reproduced the original failure before its fix. The assembled repair passed router 37/37, connections 43/43, desktop routing/connections 26/26, strict workspace/all-target Clippy, workspace build and formatting. Reusing unaffected baseline evidence, the distinct native coverage is now 1,348 tests. Contract inputs and all legacy schema bytes are unchanged by this repair.
