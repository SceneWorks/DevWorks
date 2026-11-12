# Architecture baseline handoff

2 November 2026 · Baseline 1.1 · Architectural design complete; implementation acceptance pending.

The [final feasibility review](architecture-feasibility-review.html) closes the remaining design questions. Native Keychain/process/eframe experiments and existing authority/SDK contracts establish concrete implementation routes. The user selected optional direct simple-router Claude subscription access at their risk plus a separate Anthropic API-key option, and removed the external HelixDB reader. The user subsequently added incoming MCP access for other harnesses in AD76–78; the [server contracts](mcp-server-contracts.html) define the native transport, access and recovery route. No known product decision remains open. Confidence: high in design consistency and local evidence; medium in eventual production/target integration.

The canonical design is [devworks-architecture.md](devworks-architecture.md), with a [searchable reader](devworks-architecture.html). The [local implementation plan](implementation-plan.html) assigns 378 requirements, 357 operations, 494 retained source entries, 33 acceptance groups and 12 workloads to 78 planned slices. The 66-slice native baseline and 12-item EVAL plan retain their own terminal acceptance scopes. DW-038 is retired by user scope removal. Proposed requirements remain Proposed. Architecture baseline 1.1 does not freeze public SDK v1 or establish production acceptance.

## Architecture closure checklist

| Design question | Disposition |
| --- | --- |
| Native security, credentials and process ownership | Resolved design: native ports, host-created authority, verified reads, explicit ambient Full access, fail-closed restricted capability admission and honest process outcomes. Narrow macOS synthetic-Keychain/group experiments pass; actual confinement and target support remain implementation acceptance. |
| Subscription/provider authentication and host/SDK boundary | Resolved by direct-source audit and user decision: opt-in unsupported subscription path; separate API key/billing; native router and host authority; no Claude CLI. Complete DTO/contribution boundaries and native helper evidence exist. Actual transport/SDK conformance remains acceptance. |
| Native UI integration and accessibility | Matching eframe/egui 0.37/Glow compiles and runs a hidden native window; existing AccessKit evidence and semantic view rules establish the path. Full native panes and OS accessibility remain acceptance. |
| Incoming MCP server | User-confirmed code/work/Knowledge access; shared gateway, native stdio bridge and opt-in loopback HTTP, exact per-client grants and existing command recovery. DW-063/064 own both protocol profiles and two-client acceptance; no native server/auth result is claimed. |
| Conditional HelixDB schema reader | Removed by the user. No reader, schema-engine selector or active slice remains. Internal indexes use SQLite graph tables, FTS5 and sqlite-vec. Historical storage comparisons are evidence only. |

## Implementation acceptance ownership

| Gate | Work that must pass before the affected capability is exposed | Owning slices |
| --- | --- | --- |
| G01 Storage | Real repository/transaction conformance, claims/history, migrations/manual restore, checked publication, contention, linked pins and target durability | DW-002, DW-003, DW-007, DW-008, DW-009, DW-011, DW-013, DW-023, DW-032, DW-051, DW-055, DW-056, DW-057, DW-058 |
| G02 Native UI | Every retained pane, variable-height streaming, file/image/drop/paste/dialog behavior, focus/keyboard and OS assistive technology | DW-010, DW-016, DW-053, DW-060 |
| G03 Providers | Exact profile encoders/parsers, auth/refresh/revocation, independent Anthropic API/subscription credentials and billing, risk acceptance, SystemOne, tools/media/cache/error behavior | DW-008, DW-015, DW-017, DW-018, DW-019, DW-020, DW-021, DW-022, DW-032, DW-051 |
| G04 Enforcement | Actual host authority/revocation/publication, verified reads, destructive targets, credential/outbound isolation, process ownership and qualified per-target confinement | DW-005, DW-006, DW-009, DW-011, DW-014, DW-015, DW-018, DW-020, DW-034, DW-037, DW-042, DW-045, DW-046, DW-047, DW-048, DW-049, DW-052, DW-060, DW-063, DW-064 |
| G05 Packages | Real host/SDK round-trips for all eight contributions, framing/credits/cancel, trust/signatures, nested authority, replay, registry/state publication and rollback | DW-024, DW-025, DW-026, DW-027, DW-034, DW-044, DW-045, DW-046 |
| G06 Scope | Full rename/delete/uncommitted/cross-repository views, source/embedding identity, citations, visibility revocation and reference-aware GC | DW-007, DW-023, DW-024, DW-025, DW-026, DW-027, DW-028, DW-029, DW-030, DW-031, DW-033, DW-035, DW-054, DW-063 |
| G07 Git/recovery | Actual GitHub async/queue observations, head/base races, all merge methods, conflicts, partial delivery, cleanup and approval recovery | DW-012, DW-039, DW-040, DW-041, DW-042, DW-043 |
| G08 Documents/DB | Complete text/PDF/.docx extraction/coverage, bounded workers and source retention; selected relational DB metadata and separately permitted data tools | DW-035, DW-036, DW-037, DW-060 |
| G09 Automation | Complete owned browser surface/private transport, desktop capability/OK-Wait fencing and supported container lifecycle | DW-047, DW-048, DW-050, DW-060 |
| G10 Distribution/recovery | Every OS/CPU build, SecretStore/signing, clean installers, no mandatory runtime/server, manual backup/restore, path moves, signed updates and rollback | DW-006, DW-010, DW-014, DW-018, DW-041, DW-043, DW-044, DW-047, DW-049, DW-050, DW-053, DW-057, DW-058, DW-059, DW-060, DW-063, DW-064 |
| G11 Coverage | Actual domain service adoption and every retained source semantic/argument/result/authority/recovery obligation; exclusion/import/source-drift audit | DW-001, DW-003, DW-004, DW-011, DW-012, DW-013, DW-016, DW-029, DW-030, DW-032, DW-033, DW-053, DW-054, DW-055, DW-056, DW-062, DW-063, DW-064 |
| G12 Performance | Named target hardware, production process trees and all 12 workloads; calibrate provisional budgets with evidence | DW-021, DW-028, DW-029, DW-031, DW-035, DW-061 |

These gate labels identify acceptance obligations; none demands a production application before the design can close. The slice outcomes and exact bindings, rather than the ranges in this summary, are authoritative. Final DW-062 acceptance depends on every active slice. No real inference is allowed in normal unit/integration tests; authenticated compatibility and inference E2E retain their separate authorized entry point.

## Change and execution rules

Revalidate a slice and its source evidence before implementation. Preserve retained capability scope and all ordinary controls, approvals, authority, receipts and recovery rules. Do not infer success from schema validity, process exit, a diagram or source counting. A material demonstrated contradiction reopens the affected architectural decision; an ordinary implementation test failure remains accountable to its slice.

Subscription risk acceptance is authenticated human configuration bound to the current connection/account/profile and notice digest. It does not waive provider restrictions or guarantee availability. No agent/package/import/restore may invent that acceptance. API and subscription access never share credential or native continuation authority. Billing crossover requires explicit configured routing intent.

Incoming MCP registrations are human-owned. Context access does not grant mutations; explicit command grants retain all existing approvals, human-only holds, current scope/revocation and receipt rules. Closing/restore disables inbound authority as specified by the server contracts.

CodeGraph project DW is now the operational tracker. See `codegraph-work-map.json` for item keys and `codegraph-feature-split-proposal.json` for the approved, applied related-feature grouping; local slice IDs remain document identifiers. No Shortcut records are created. This review does not authorize broad application implementation, live inference, provider sign-in, installation or restart. The next step, when requested, is execution of the reviewed local slices with their acceptance evidence, not another open-ended architectural pass.
