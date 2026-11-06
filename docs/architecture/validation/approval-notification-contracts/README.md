# DW-14 approval and notification contract qualification

Confidence: high for the schema, typed DTO and ownership checks recorded here. These checks run no inference. Human-event ingress, transaction/restart behavior, owner preflight, notification producers and native UI acceptance require separate integrated DW-14 evidence.

The register has 359 operations, 364 operation/mode bindings and 541 operation/mode/revision admissions. Four existing approval operations and notification list/read/acknowledge gain revision 3. `approvals.review_queue` and `notifications.mark_read` begin at revision 3; notification settings retains its original hybrid contract. All 56 existing revision-1/2 schema documents remain byte-identical to the [immutable baseline](../latest-model-contracts/legacy-schema-sha256.json), without exceptions.

Approval-only `ApprovalViewRecord` metadata preserves the settings/work-owned `ProposalRecord`. `ApprovalContent` selects immutable settings, an existing feature-plan reference, a finding/assessment revision, or a saved registered operation's schema and closed argument body. Reviewed snapshots and admission-derived previews remain distinct from private host human witnesses, original actor context and internal resume grants. Forged public authority fields are rejected. Registered admission and owner preflight remain required service obligations; schema qualification cannot authorize disabled/read-only capabilities or waive prerequisites and verification.

The informational Knowledge review query retains source-derived ranking, explicit remaining challenge counts, a maximum page of 50 and three visible challenges. Adopted KNOW-03/05 excludes autonomous acknowledgment and rule-approval authority. A dedicated `KnowledgeReviewPage` preserves existing generic definition names. Notification detail stays pure, explicit read/unread is distinct from existing durable individual card acknowledgment, and neither mutation resolves decisions or resumes work. Completion records expose source identity and typed links for persistent deduplicated newest-first cards.

The [schema/model report](schema-model-results.json) and [strict typed results](native-results.json) cover all 1,841 fixture messages, including 1,512 domain cases (345 negative). The [scenario report](scenario-results.json) reruns 12 existing scenarios/26 steps as data; it claims no new process or provider execution. Source digests are retained in the reports. Ten generator tests pass. The plan retains 78 slices and 377 unchanged requirements, with zero unowned operations, requirements or retained entries. The approved Word blob is unchanged; existing document QA is reused, without a new native-acceptance claim.

Reproduce from the repository root:

```sh
python3 tools/contracts-eval/build_domain_contracts.py
cargo build --locked --manifest-path tools/contracts-eval/Cargo.toml --target-dir /tmp/dw14-probe
/tmp/dw14-probe/debug/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py --date 2026-11-05
python3 tools/core-codegen/generate.py
python3 tools/contracts-eval/build_domain_reference.py
python3 tools/planning/build_implementation_plan.py
node tools/planning/build_architecture.cjs
python3 tools/contracts-eval/verify_domain_contracts.py --binary /tmp/dw14-probe/debug/devworks-contract-probe --output docs/architecture/validation/approval-notification-contracts --date 2026-11-05
python3 -m unittest discover -s tools/core-codegen -p 'test_*.py'
cargo test --locked -p devworks-core --test registry --test schema_revisions --test recorded_fixtures
cargo fmt --all --check
```

The verifier's Python must have compatible jsonschema/rpds wheels. All 20 native contract tests passed on exact contract snapshot `0bb7c6d`: seven recorded-fixture, five registry and eight schema-revision tests. They prove actual native admission of all 541 registered mode/revision bindings and legacy byte preservation. Regeneration and formatting checks pass. This contract-only snapshot does not establish the separately integrated DW-14 owner service or UI acceptance.

Two additional native probe regressions exercise legal typed plan and aggregate settings detail above 512 KiB. The old DynamicJson metadata wrapper refuses those aggregate sizes, while the dedicated `ApprovalReviewedContent` values round-trip every byte. Inspect-only `reviewedSource` returns full typed source; lists omit it, and bounded `reviewedContent` remains separate. Source-owned native query replies are distinct from the request-frame limit; no hidden truncation or replacement digest is used.
