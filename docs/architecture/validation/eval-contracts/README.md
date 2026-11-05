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

## Native application coverage

The native implementation registers both operations through the shared gateway and persists insert-only reports in SQLite migration 24. The evaluator observes transcript/tool records, session records, full root history, recorded Git/PR/review links and resulting work state through read-only ports. A report binds its suite and source content digests, session revision and last durable transcript sequence; all source fences are rechecked before commit. Compared JSON fields remain in the immutable report, including a distinct retained JSON null. Missing historical attribution is explicitly Mixed and is never filled from current configuration.

The native regressions in `crates/devworks-adapters/src/storage/sqlite/tests/sessions/evals.rs` cover durable reopen, all six non-transcript sources, exact and mixed step attribution, eight outcomes, authority-fenced receipt replay, project hiding, cursor rebinding, coverage across 513 candidate steps, no mutation of observed work/repository/Knowledge records, and independently committed source changes between preparation and report commit. The eight-test group takes 6.34 seconds on the development Mac, excluding compilation. The six storage tests cover migration compatibility, immutable rows, source identity and bounded paging. Core tests cover manifest integrity, pointer semantics and outcome arithmetic.

The seven engine regressions in `crates/devworks-engine/tests/evals.rs` cover completed/stopped coordinator and worker conversations, prior-boot recovery, idempotent catch-up, 130-session keyset traversal, command wakeups and interruption followed by retry. Automatic suites are host-supplied authorized snapshots and default to empty; suite management owns activation. Shutdown fences and drains the one local evaluator. Native report reads return at most eight reports per page (plus one internal lookahead); each produced report is bounded to 512 KiB. Overflow remains explicit budget-exhausted coverage.

Reproduce native application checks with:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
```

These regressions extend the existing macOS/Linux/Windows workspace test jobs; no job, matrix, dependency or inference execution is added. Test execution is fixture-only and local. Incremental native compilation/build costs vary with the cache; a separate attributable CI compilation cost has not been measured. Existing job timeouts remain unchanged. Contract-probe qualification above and native application tests are separate evidence.
