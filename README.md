# DevWorks

DevWorks is a planned local desktop application for autonomous software development, combining a chat-first coordinator, project management, code intelligence and model routing. The selected implementation direction is Rust with native egui and local embedded storage. Plain SQLite plus sqlite-vec is selected, with core-owned persistence traits. The earlier multi-engine evaluations remain historical evidence; Kuzu is excluded.

Architecture baseline 1.1 is complete, following requirements, UI design and targeted feasibility review. Native implementation is under way in the root Cargo workspace. The first services are listed below, and the paragraph after the list says what exists beyond them:

- `crates/devworks-core`: domain contracts, the operation registry, the shared admission gateway with receipts, the persistence and file ports, host authority (principal bindings, grants, access policy, destructive-action and repository-deletion seams) and the first application services: queue operations, settings, artifacts (with durable receipt replies), sessions and history, resources and owned processes, run root controls (`runs.*`: Start, Pause, Continue, Redirect, Stop, Inspect and Eligibility, with the dispatch-admission substrate and resume grants), and versioned skills and role instructions (`skills.*`, `roles.*`: shared `~/.agents/skills` read in place, harness overrides, approval-gated agent edits, history and restore).
- `crates/devworks-adapters`: the SQLite store (single write owner, migrations, operational and domain repositories) and native file adapters (managed artifact files, verified source reads).
- `crates/devworks-engine`: durable event delivery (fenced-claim dispatcher, sleep/wake recovery, the package event-handler commit path, the receipt reply consumer) run supervision and recovery of run roots (activations held for manual Continue after a restart, journaled stops, shutdown), and the one boot entry point that recovers, starts delivery and artifact collection, and admits commands.
- `crates/devworks-extension-protocol`: the versioned package wire protocol.
- `crates/devworks-desktop`: the native egui shell and the desktop binary. Its composition root opens the one store and boots only through the engine; the shell reaches the application only through the runtime's gateway (Home chat over sessions, Settings, workspace artifacts and resources) and shows designed unavailable states for screens later items fill. Run `cargo run -p devworks-desktop` (`--data-dir <path>` selects the data root).

Beyond those first services, handlers are registered for: routing and connections, usage and prices, jobs, the package host and its add-ons, indexing and graph queries, projects and repositories, work (features, issues, tasks, plans and readiness), agents, approvals and notifications, Knowledge and conventions, Git task operations, process tools, containers, desktop control, browser, outgoing MCP, web tools, watchers, schema metadata, evaluations, diagnostics and Help. The authoritative list is the operation registry (`crates/devworks-core/src/registry/catalog.rs`) read against the registrations in `crates/devworks-core/src/services/mod.rs`.

Not implemented yet: operations with no registered handler. Of the registry's 376 operations, 73 have none, and the gateway refuses them before any store access. They are the code analysis family (`analysis.*`, 13), database reads, writes and migrations (`database.*`, 5), document import, indexing and search (`documents.*`, 10), the Dreaming review queue (`dreaming.*`, 7), Git cleanup and integration (`git.cleanup`, `git.integrate`), the GitHub account, repository, check, review and issue-sync operations (`github.*`, 6), source imports (`imports.*`, 4), `issues.link` and `issues.publish`, the incoming DevWorks MCP server (`mcp_server.*`, 4), release preparation and deployment (`releases.*`, 4), `repository_docs.generate`, review requests and findings (`reviews.*`, 7), `router.decide` and `router.decision_models`, `ui.current_screen`, and workspace backup, restore, migration and update (`workspace.*`, 5). A few other operations register only when their service is composed, such as project purge and Knowledge summaries. The HTML prototype simulates agent and integration behavior with local sample data.

- [Planning index](docs/planning/README.md)
- [Architectural design baseline](docs/architecture/devworks-architecture.html)
- [Storage evaluation](docs/architecture/devworks-storage-evaluation.html)
- [Architecture sources, diagrams and validation](docs/architecture/README.md)
- [Visual architecture of the implemented code](docs/architecture/visual/README.md)
- [Requirements](docs/planning/devworks-requirements.html)
- [Editable requirements](docs/planning/devworks-requirements.docx)
- [Requirements interview decisions](docs/planning/requirements-interview.json)
- [Interactive UI prototype](docs/planning/harness-workbench.html)
- [Screen gallery](docs/planning/mockup-gallery.html)
- [Planning authoring tools](tools/planning/README.md)

CodeGraph project DW is the operational work tracker; DW-F1 holds the architecture baseline; related features DW-F2–DW-F12 hold native delivery. The repository retains the architecture, requirement sources and validation evidence. See the [CodeGraph key map](docs/architecture/codegraph-work-map.json) and [related-feature plan](docs/architecture/codegraph-feature-split-proposal.json). Confirmed and Proposed requirements remain distinguished; native implementation proceeds feature by feature, starting with the DW-F2 runtime and persistence foundation.

The [local implementation plan](docs/architecture/implementation-plan.html) maps all captured requirements and retained capabilities to 63 active slices (DW-038 retired by user scope removal). [Native backend feasibility](docs/architecture/native-backend-feasibility.html) records narrow Rust experiments. These remain planning artifacts with explicit production and platform qualification gates.

The [complete operation schemas](docs/architecture/domain-operation-contracts.html) and [field reference](docs/architecture/domain-operation-reference.html) cover every input/result candidate in the operation registry. Actual service, authority, SDK/provider and platform conformance remain explicit qualification gates.

[Final architecture feasibility review](docs/architecture/architecture-feasibility-review.html) records closure of the four design gates, the optional direct Claude subscription risk decision and separate Anthropic API-key support. Production acceptance remains assigned to the implementation slices.

Incoming MCP support lets other harnesses use indexed code, planned work and Knowledge through the shared application gateway. See the [server contracts](docs/architecture/mcp-server-contracts.html) for native stdio/HTTP, client grants and implementation acceptance.
