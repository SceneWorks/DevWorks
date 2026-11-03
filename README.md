# DevWorks

DevWorks is a planned local desktop application for autonomous software development, combining a chat-first coordinator, project management, code intelligence and model routing. The selected implementation direction is Rust with native egui and local embedded storage. Plain SQLite plus sqlite-vec is selected, with core-owned persistence traits. The earlier multi-engine evaluations remain historical evidence; Kuzu is excluded.

Architecture baseline 1.1 is complete, following requirements, UI design and targeted feasibility review. Native implementation has begun: the root Cargo workspace holds `crates/devworks-core` (domain contracts, operation registry and the shared admission gateway) and `crates/devworks-extension-protocol`; application services, storage and the egui shell are not implemented yet. The HTML prototype simulates agent and integration behavior with local sample data.

- [Planning index](docs/planning/README.md)
- [Architectural design baseline](docs/architecture/devworks-architecture.html)
- [Storage evaluation](docs/architecture/devworks-storage-evaluation.html)
- [Architecture sources, diagrams and validation](docs/architecture/README.md)
- [Requirements](docs/planning/devworks-requirements.html)
- [Editable requirements](docs/planning/devworks-requirements.docx)
- [Requirements interview decisions](docs/planning/requirements-interview.json)
- [Interactive UI prototype](docs/planning/harness-workbench.html)
- [Screen gallery](docs/planning/mockup-gallery.html)
- [Planning authoring tools](tools/planning/README.md)

CodeGraph project DW is the operational work tracker; DW-F1 holds the architecture baseline; related features DW-F2–DW-F12 hold native delivery. The repository retains the architecture, requirement sources and validation evidence. See the [CodeGraph key map](docs/architecture/codegraph-work-map.json) and [related-feature plan](docs/architecture/codegraph-feature-split-proposal.json). Confirmed and Proposed requirements remain distinguished; native implementation has only begun with that contract workspace.

The [local implementation plan](docs/architecture/implementation-plan.html) maps all captured requirements and retained capabilities to 63 active slices (DW-038 retired by user scope removal). [Native backend feasibility](docs/architecture/native-backend-feasibility.html) records narrow Rust experiments. These remain planning artifacts with explicit production and platform qualification gates.

The [complete operation schemas](docs/architecture/domain-operation-contracts.html) and [field reference](docs/architecture/domain-operation-reference.html) now cover all 353 input/result candidates. Actual service, authority, SDK/provider and platform conformance remain explicit qualification gates.

[Final architecture feasibility review](docs/architecture/architecture-feasibility-review.html) records closure of the four design gates, the optional direct Claude subscription risk decision and separate Anthropic API-key support. Production acceptance remains assigned to the implementation slices.

Incoming MCP support lets other harnesses use indexed code, planned work and Knowledge through the shared application gateway. See the [server contracts](docs/architecture/mcp-server-contracts.html) for native stdio/HTTP, client grants and implementation acceptance.
