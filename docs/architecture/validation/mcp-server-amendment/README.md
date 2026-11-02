# MCP server architecture amendment evidence

2 November 2026 · baseline 1.1 · AD76–78 · DW-063/064.

This amendment records the user-requested incoming MCP server alongside existing outgoing connections. It does not implement or run a native server, authenticate an external client, exercise real credentials, invoke inference or establish production authority/platform acceptance.

## Current checks

- 350 requirements (209 Confirmed, 141 Proposed), 351 input/result candidates in 48 families, 63 active slices; no unowned requirements, operations or retained source entries.
- 44 generated schemas and 1,049 combined native DTO/schema fixtures (727 domain messages, including 54 negatives). All 351 operation bindings pass. Twelve prior native stream/history scenarios with 26 steps pass again; no new native helper/server execution is claimed.
- Frozen-source audit: 111 files, 558 entries (494 retained and 64 intentional exclusions), verified against the original pinned source. New incoming MCP behavior is explicitly additional scope, not retroactive source parity.
- Publication candidates: every catalog operation/mode has a disposition; 95 scoped reads and 57 explicitly grantable commands. No new registration receives automatic grants. Actual MCP schema projection and client behavior remain unimplemented.
- Headless mock UI checks cover both directions, disabled-by-default incoming serving, secret-free setup, selected reads/no default writes, project scope, explicit command selection, revocation and 1600/1280 layouts. These are prototype checks only.
- Current HTML readers, local links and rendered requirements are checked separately. The reports retain exact totals and source digests. No visible browser tab was opened.

## Reproduce

Run from the repository root using the bundled document/Node runtime paths where required. Python schema validation needs the recorded jsonschema environment under `.planning-build/operation-contracts/python`.

```sh
python3 tools/contracts-eval/build_domain_contracts.py
CARGO_TARGET_DIR="$PWD/.planning-build/data-delivery-contracts/cargo-target" cargo build --locked --release -j4 --manifest-path tools/contracts-eval/Cargo.toml
.planning-build/data-delivery-contracts/cargo-target/release/devworks-contract-probe emit docs/architecture/contracts
python3 tools/contracts-eval/make_domain_fixtures.py
python3 tools/contracts-eval/build_register.py
python3 tools/planning/build_mcp_publication.py
python3 tools/planning/build_implementation_plan.py
python3 tools/contracts-eval/build_domain_reference.py
```

The verification entry points are `tools/contracts-eval/verify_domain_contracts.py`, `audit_domain_sources.py`, `tools/planning/verify_mcp_server_prototype.cjs`, `verify_domain_planning_readers.cjs`, `verify_document_links.cjs` and `verify_mcp_server_documents.py`. Native application acceptance is defined in [the server contracts](../../mcp-server-contracts.html), not satisfied by these checks.
