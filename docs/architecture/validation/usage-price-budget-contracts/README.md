# DW-21 usage, prices and budget contract qualification

Confidence: high for typed/schema and ownership evidence here. No inference runs. Native pricing adapters, attempt accounting, deduplication, budget admission/Continue, historical bootstrap and UI acceptance remain separate integrated DW-21 evidence.

The [frozen retained-source audit](retained-source-audit.json) records exact source hashes, arguments, reports and quirks.

The register remains 359 operations and 364 operation/mode bindings, with 546 operation/mode/revision admissions. Usage, price status, price override and explicit connection billing authoring/query gain revision 3; price refresh retains its sufficient original command/result. All 56 existing revision-1/2 schemas remain byte-identical to the [immutable baseline](../latest-model-contracts/legacy-schema-sha256.json), without exceptions or hardcoded generic definition numbers.

Usage supports complete bounded Attempts, the five frozen report modes and filter discovery. Native nullable metrics, unknown tails, partial monetary coverage, original reported currency, API-equivalent estimates and raw legacy observation subtotals remain explicit. Nondeduplicable old facts never become canonical attempt spend or gain fabricated historical ownership. Report/option rows preserve full retained source arguments/results through query/read-revision-bound pages; totals describe the full selected population. Exact tariffs preserve service/context/modality/endpoint/TTL/region and units, source/history provenance and explicit override clearing. Unpriced model rows expose an authoritative positive initial CAS revision. Feature budgets use all existing creation/edit/atomic/staged/import surfaces, remain feature-only, and expose bootstrap and monetary coverage.

The [schema/model](schema-model-results.json), [strict typed](native-results.json) and [scenario](scenario-results.json) reports retain the full baseline of 1,905 fixture messages, including 1,576 domain cases (372 negative), all 67 schema documents and 12 recorded scenarios/26 steps. The current set has 1,907 messages (1,578 domain, 373 negative); the two added digest cases and all four changed budget cases pass the focused [plan-editor native/schema delta](plan-editor-delta-results.json). Eleven generator tests pass. Four new native adapter regressions prove exact legacy usage/page round-trip, refusal to invent legacy counters, and refusal to turn Clear into a zero rate. Three allocation projection and two legal large-review regressions remain green. These are pure protocol checks, not learned execution or source behavioral acceptance.

The generic legacy alias normalization now resolves a referenced numbered family member before existing structural equality folding. The failed initial emission exposed this gap; no old schema was accepted with changed bytes. A malformed historical fixture ID was also corrected to the existing opaque-ID grammar. The successful complete pass is recorded with source hashes. The plan retains 78 slices, 377 unchanged requirements and zero unowned operations/requirements/retained entries. Original operation ownership stays intact; the usage slice explicitly integrates the existing feature/approval authoring surfaces. The approved Word blob is unchanged.

Reproduce with compatible jsonschema/rpds Python dependencies:

```sh
python3 tools/contracts-eval/build_domain_contracts.py
cargo build --locked --manifest-path tools/contracts-eval/Cargo.toml --target-dir /tmp/dw21-probe
/tmp/dw21-probe/debug/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py --date 2026-11-05
python3 tools/core-codegen/generate.py
python3 tools/contracts-eval/build_domain_reference.py
python3 tools/planning/build_implementation_plan.py
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/verify_domain_contracts.py --binary /tmp/dw21-probe/debug/devworks-contract-probe --output docs/architecture/validation/usage-price-budget-contracts --date 2026-11-05
cargo test --locked --manifest-path tools/contracts-eval/Cargo.toml --test usage_projection --test allocation_projection --test approval_review --target-dir /tmp/dw21-probe
python3 -m unittest discover -s tools/core-codegen -p 'test_*.py'
cargo test --locked -p devworks-core --test registry --test schema_revisions --test recorded_fixtures
cargo fmt --all --check
```

All 20 native core contract tests passed on exact assembled snapshot `3077220039af3f1e75bb3197677256ac98f2020e`: seven recorded-fixture, five registry and eight schema-revision tests. The snapshot contains real owning constructors/services and was tested in a clean detached worktree without edits or stubs, using the existing isolated contract target. These tests admit all 546 registered revision/mode bindings and preserve all 56 legacy schema hashes. Full native accounting, pricing adapter and UI acceptance remains separate.

The native plan-editor delta has six bounded cases: revision-3 proposed-cap input, pending-plan reopening and authoritative stored digest before first approval, plus their earlier-revision rejections. One budget case replaces an automatic admission fixture, for a net five cases over the earlier 1,902-message checkpoint. `features.edit_plan` already registered revision 3, so the admission count remains 546. Plain PlanDraft remains budget-free; effective configuration and captured proposed configuration are separate. Clients read PendingFeaturePlan.planDigest instead of duplicating the composite digest algorithm. The prior exact-snapshot native proof remains historical until the owner constructor delta is assembled and rechecked.
