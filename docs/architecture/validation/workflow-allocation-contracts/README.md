# DW-13/DW-109 workflow and allocation contract qualification

Confidence: high for the typed/schema/projection, ownership and native integration evidence below. These checks execute no inference. Contract qualification and integrated production-service evidence are recorded separately.

Native integration at source snapshot `7de2415` passed 50 core gateway/registry/schema/fixture tests, 23 engine allocation/handoff tests, 70 execution-control tests, six workflow tests, 27 SQLite allocation/handoff tests, 46 SQLite work/metadata tests, 22 desktop work tests and five desktop workflow tests. Strict workspace/all-target Clippy, the native workspace build and formatting passed. The earlier full workspace baseline `8ca8138` passed 1,398 tests with three existing intentional ignores and one fixture-reader failure; unaffected suites are reused. The final reader now checks negotiated revisions before projection equality for standalone and nested results, and all three discriminating negative cases pass. Native fixture totals are 1,456 domain records and 532 operation/mode/revision admissions.

The focused independent review of allocation ownership, membership handoff, ABA recovery, coordinator bootstrap and inherited readiness found no remaining issues; its 12 closure regressions passed. Task type and labels preserve native membership kind, revision fences and history. Workflow removal uses backend occupancy/revision fences; allocation preserves exact ownership, replay identity, one running project root and independent holds. Membership changes preserve worker identity only at a quiescent generation-fenced boundary.

The Windows plan-editor regression uses the actual AccessKit control-action path, preserving its original hidden-data assertions and unchanged timeouts. Corrected diagnostic run [42138175964](https://github.com/SceneWorks/DevWorks/actions/runs/42138175964) passed the exact test and three complete 21-test work-suite repetitions (64 test executions). The initial intermittent pointer-interaction cause remains unconfirmed. The temporary diagnostic workflow is excluded from delivery source.

CI impact: the existing eight checks are unchanged (formatting, generated drift, three strict-lint jobs and three native-test jobs). No workflow, dependency or inference-compute expansion is introduced. Exact published-head CI and automated review remain required before merge and CodeGraph acceptance.

The retained ten operations include revision-3-only `next_work.inspect_queue`, which reads authoritative revisions even for empty project/feature task queues. Eight earlier workflow/claim operations gain revision 3, and metadata authoring adds 21 related revision bindings. The exact register has 357 operations, 362 operation/mode bindings and 532 operation/mode/revision bindings. All 56 existing revision-1/2 schema documents remain byte-identical to the [immutable baseline digests](../latest-model-contracts/legacy-schema-sha256.json); no compatibility exception was added.

`AllocationSelection` explicitly adapts successful/recovered outcomes to legacy ClaimRecord and preserves old receipt decoding. Revision3 distinguishes Claimed, Recovered and NoEligible, including domain reused on keyed no-work replay. Missing recovery observations remain unknown; actual native activity uses AllocationActivity with a null-safe bounded payload wrapper rather than an invented control event. Runtime receipt replay and secondary allocation idempotency remain distinct. Current work and source recovery evidence is bounded and authorized.

The gateway rejects an impossible projection before commit and rolls back handler writes. Three native probe integration tests pass: exact old receipt-value round-trip, successful/recovered legacy projection, and NoEligible rejection without a fake legacy claim. Two tests in the production gateway's existing transaction harness additionally exercise handler-write rollback/no receipt and exact projected legacy receipt replay. Both production-gateway tests passed on exact assembled snapshot `83e96f5ede52c626b44f31320a06577d9081ed46`, using a detached worktree and the existing transaction harness without constructor stubs or service edits. The earlier constructor-blocked attempt remains recorded in qualification.json. The same assembled snapshot also passed all nine generator tests, regeneration drift and all 56 legacy-schema byte checks.

The [schema/model report](schema-model-results.json), [strict native results](native-results.json) and [scenario results](scenario-results.json) retain source digests and observed fixture output. They cover status observations, full filters, reuse/recovery/no-work results, metadata authoring and clear semantics, rejected duplicates/blank labels, operation introduction, strict legacy/current result decoding and immutable receipt shapes. Nine generator tests pass, including operation introduction and metadata separation. Supporting DW-109 is tracked without taking primary ownership of existing operations; the regenerated plan has 78 slices, 377 unchanged requirements and 357 owned operations with zero unowned requirements/operations. The approved Word blob and its existing visual QA are unchanged.

Reproduce with an authoring Python using matching jsonschema/rpds dependencies:

```sh
python3 tools/contracts-eval/build_domain_contracts.py
cargo test --locked --manifest-path tools/contracts-eval/Cargo.toml --test allocation_projection --target-dir /tmp/devworks-work-contract-probe
/tmp/devworks-work-contract-probe/debug/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py --date 2026-11-05
python3 tools/core-codegen/generate.py
python3 tools/contracts-eval/build_domain_reference.py
python3 tools/planning/build_implementation_plan.py
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/verify_domain_contracts.py --binary /tmp/devworks-work-contract-probe/debug/devworks-contract-probe --output docs/architecture/validation/workflow-allocation-contracts --date 2026-11-05
python3 -m unittest discover -s tools/core-codegen -p 'test_*.py'
cargo test --locked -p devworks-core --test gateway allocation_
cargo test --locked -p devworks-core --test registry --test schema_revisions --test recorded_fixtures
cargo fmt --all --check
```

Optional standalone probe strict Clippy failed with 31 diagnostics of mixed provenance; its baseline was not compared. This optional check is not the native workspace lint gate. Historical probe code was not broadly rewritten. Required probe build/projection/fixtures and legacy drift checks pass; normal workspace strict lint remains an integrated native gate.
