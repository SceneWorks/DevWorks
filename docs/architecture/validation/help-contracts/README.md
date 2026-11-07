# DW-52 offline Help contract qualification

Confidence: high for the typed contracts, legacy projection and frozen-source audit. These checks perform no inference, provider requests, Help publication, package activation or UI interaction. Native Help service/storage, artifact visibility, command lifecycle, scheduler and UI acceptance remain separate integrated evidence.

The [retained-source audit](retained-source-audit.json) reads exact CodeGraph objects at `afe73560215bbe7593fc59451de5cc3237e3eeca`. The Admin Settings endpoint has no caller arguments and waits for generation. The retained job normalizes empty arguments, propagates cancellation, and reports actual success/message/start/completion timestamps. Startup generation is best effort. Generation preserves authored text outside AUTO sections, avoids revisions for unchanged content, creates new generated pages and reconciles removed tools without deleting unrelated authored content. Native bundled content replaces the old server/wiki section, while the same maintenance operation remains in the existing qualified native job catalog.

The current catalog remains 359 operations in 49 families and 364 operation/mode bindings. Four Help queries gain revision 3; the existing generation command retains its sufficient original input/result. There are 550 operation/mode/revision admissions and 362 domain types. All 56 immutable revision-1/2 schema hashes remain exact, without exceptions or numbered generic-definition workarounds. The 377 requirements and 78 slices retain zero unowned operations, requirements or retained source entries. The approved Word document is unchanged.

Revision 3 returns compact topic pages and bounded direct passages through Help permission, without requiring artifact, add-on or history tools. Snapshots carry current app/Help/registry revisions, actual generation permission/refusal and the durable pending/latest generation operation. Exact app/package/version/manifest/content provenance and current input/result schema metadata remain visible. Missing, stale and unavailable sources are explicit. A screen query uses exactly one explicit revision or `current:true`; it cannot silently guess an initial revision. Related capability labels preserve dotted operation and qualified package names; legacy ID references remain real registered IDs only.

Basic legacy topic/page results use actual published artifact references. Generated adapters project available results only when those real references exist. They never manufacture artifacts for unavailable or inline-only views. The new tests prove old topic/page decoding and exact round-trip, faithful available projection, refusal of missing/stale/unavailable or reference-free projection, and exact 65,536-character Unicode passage retention with explicit continuation.

The [schema/model report](schema-model-results.json), [strict probe report](native-results.json) and [recorded scenarios](scenario-results.json) pass 1,938 messages: 1,609 domain cases, including 385 negatives, plus retained shared/provider/data/actual-frame messages. All 67 schema documents and 12 scenarios/26 steps pass. Twenty explicit bounded Help cases plus two generated query admissions cover text, provenance, status, paging, selector exclusivity and earlier-revision refusal. Qualification exposed and fixed an own pure-reader gap: JSON Schema rejected both screen selectors while typed Check originally accepted them. Both readers now reject the same case.

Production core tests pass: four new Help projection/boundary tests, seven recorded-fixture tests, five registry tests and eight schema-revision tests (24 total). Eleven generator tests and strict core Clippy on the four affected test targets pass. The probe's four Help tests and three existing allocation projection tests pass, preserving the other versioned adapter. An initially outdated registry expectation was updated to admit the four actual new query revisions; it remains an exact catalog assertion. No service constructors or storage stubs were introduced.

Reproduce from the repository root:

```sh
python3 tools/contracts-eval/build_domain_contracts.py
cargo build --locked --manifest-path tools/contracts-eval/Cargo.toml --target-dir /tmp/dw52-probe
/tmp/dw52-probe/debug/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py --date 2026-11-06
python3 tools/core-codegen/generate.py
python3 tools/contracts-eval/build_domain_reference.py
python3 tools/planning/build_implementation_plan.py
node tools/planning/build_architecture.cjs
# Use Python matching the installed jsonschema/rpds wheel ABI.
python3 tools/contracts-eval/verify_domain_contracts.py --binary /tmp/dw52-probe/debug/devworks-contract-probe --output docs/architecture/validation/help-contracts --date 2026-11-06
cargo test --locked --manifest-path tools/contracts-eval/Cargo.toml --test help_projection --test allocation_projection --target-dir /tmp/dw52-probe
python3 -m unittest discover -s tools/core-codegen -p 'test_*.py'
cargo test --locked -p devworks-core --test help_projection --test recorded_fixtures --test registry --test schema_revisions
cargo fmt --all --check
```


The final [strict-choice delta](strict-choice-delta-results.json) adds one source-derived counterexample: `current:true` together with an explicitly null `capabilityRevision`. The former optional reader erased null and accepted this schema-invalid request. Exactly-one members now use the existing non-null reader; ordinary nullable optional fields are unchanged. A fifth production Help test exercises the actual admission path. All 25 production core tests (5 Help, 7 recorded fixtures, 5 registry and 8 schema revisions), 11 generator tests and strict core Clippy pass after this change. The current set is 1,939 messages (1,610 domain, 386 negative); every typed regression passes. All 67 schema documents are byte-identical to the full 1,938-message checkpoint, so its schema/model and reader evidence is reused with the new case checked directly. All 56 legacy hashes remain exact.
