# DW-71 deterministic eval contract qualification

Confidence: high for the typed/schema/ownership evidence below. These checks execute no inference and do not establish native evaluator persistence, source attribution or authority acceptance; those belong to DW-71's service integration evidence.

The contract adds scoped `evals.grade` and `evals.results` operations, immutable supplied/draft measurements, per-step Exact/Mixed revision context, all eight explicit outcomes and coverage at instance/run/report levels. All three built-in deterministic predicates are explicitly Proxy; fixed-source `RecordFieldEquals` covers recorded Session, HistoryEvent, GitLink, PullRequestLink, ReviewLink and WorkState projections, with RFC 6901 pointer checks owned by the runtime and optional retained observed JSON for auditable comparisons after mutable records change. Pure validation checks denominator arithmetic, ungraded reason accounting, 64 KiB suite budget, check identity uniqueness and pagination continuation. The eval-specific fixtures include all outcomes and reject false enforced-rule declarations, misleading rates, arithmetic errors, an empty/oversized suite and forged activation.

Reproduction from the repository root (the authoring Python must match its installed jsonschema/rpds wheel ABI):

```sh
python3 tools/contracts-eval/build_domain_contracts.py
cargo build --locked --manifest-path tools/contracts-eval/Cargo.toml --target-dir /tmp/devworks-eval-contract-probe
/tmp/devworks-eval-contract-probe/debug/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py --date 2026-11-05
python3 tools/core-codegen/generate.py
python3 tools/contracts-eval/build_domain_reference.py
python3 tools/planning/build_implementation_plan.py
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/verify_domain_contracts.py --binary /tmp/devworks-eval-contract-probe/debug/devworks-contract-probe --output docs/architecture/validation/eval-contracts --date 2026-11-05
python3 -m unittest discover -s tools/core-codegen -p 'test_*.py'
cargo test --locked -p devworks-core --test registry --test schema_revisions
cargo fmt --all --check
```

The committed [schema/model report](schema-model-results.json) records exact source digests, typed/schema fixtures and each operation's selected input/result binding. [Native results](native-results.json) and [scenario results](scenario-results.json) preserve the probe output. Historical helper frames are replayed data; the verifier makes no claim of newly executing those helpers. The implementation plan generator reports 374 owned requirements, 356 owned operations in 49 families, zero unowned requirements and operations, and no unbound contract candidates. Existing revision-1 schema goldens remain unchanged after removing only newly registered operation branches and their private definitions, plus the previously documented hardening exceptions.
