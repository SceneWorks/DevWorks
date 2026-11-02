# Shared-operation contract evidence

Historical checkpoint; current combined provider/session/package evidence is in [the later report](../provider-package-contracts/README.md). Counts and digests below describe this earlier checkpoint.

1 November 2026. Architecture candidate `1.0-draft.1`; no production application, gateway, provider, authority adapter or SDK was implemented. No inference or external effects ran. The fixture databases are created under owned temporary directories and removed by the harness.

- [Schema/DTO results](schema-dto-results.json): five generated JSON Schema 2020-12 documents, 60 positive/negative message fixtures, six normalized fingerprint checks and an extra oversized-frame rejection before typed parsing. Native Rust decoding and generated schemas agree within the stated structural/semantic layers. Decimal signed-64 overflow and cross-field inconsistencies intentionally pass lexical schema checks but fail typed validation.
- [Native decoding detail](native-results.json): exact candidate messages, normalized successful forms and rejected cases. Includes untrusted actor/policy/grant injection, unsupported child-control variants, invalid/duplicate fields, revision spellings, duplicate hold IDs, independent blocker responses, stale-owner observation shape, scope/revision mismatch, cursor/page bounds and query watermark consistency.
- [Receipt model results](receipt-model-results.json): 23 deterministic checks over actual SQLite with the initial schema, including two concurrent connections, atomic decision/history/outbox changes, lost commit acknowledgment, changed payload under a retry key, receipt replay after a new pause, visibility denial, explicit work/control/blocker revision conflicts, exact grant consumption/rollback, independent child blockers and restore-epoch refusal.

The receipt model uses scripted host contexts and only the root-control subset. It demonstrates the proposed transaction/race mechanics, not real human authentication, command admission, all readiness policies, dispatcher fencing, process cancellation, target effects or platform durability. It does not run a supervisor. The Rust experiment implements candidate DTO serialization/range/consistency checks, not those services. Native JCS, complete parser/framing limits, all domain/event payloads, cursor signing/validation, migration/upcasters and actual event consumers remain qualification work.

Candidate build host: macOS arm64, Rust/Cargo 1.98.0. Probe dependencies are pinned in `tools/contracts-eval/Cargo.lock`: Serde 1.0.229, serde_json 1.0.154, Schemars 1.2.3. Authoring-only schema/hash libraries are jsonschema 4.26.0 and rfc8785 0.1.4, with resolved versions in the evidence. These tools do not add Python, JSON Schema runtime validation or Schemars to the future shipped application dependency graph by implication.

Reproduce from the repository root:

```sh
CARGO_TARGET_DIR="$PWD/.planning-build/operation-contracts/cargo-target" cargo build --locked --release --manifest-path tools/contracts-eval/Cargo.toml
.planning-build/operation-contracts/cargo-target/release/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_fixtures.py
python3 -m pip install --target .planning-build/operation-contracts/python jsonschema==4.26.0 rfc8785==0.1.4
PYTHONPATH=.planning-build/operation-contracts/python python3 tools/contracts-eval/verify_contracts.py --binary .planning-build/operation-contracts/cargo-target/release/devworks-contract-probe --output docs/architecture/validation/operation-contracts
PYTHONPATH=.planning-build/operation-contracts/python python3 tools/contracts-eval/probe_receipts.py
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/build_register.py
```

The Node authoring command uses the established bundled `NODE_PATH`/dependencies described in `tools/planning/README.md`. No authoring server or visible browser tab is required. [The schema register](../../contracts/family-coverage.json) accounts for all 47 families and 345 catalog operations: eight concrete input candidates, 337 domain inputs still pending. These catalog operations are not a count of source features, SDK methods, implementation stories or already supported capabilities.
