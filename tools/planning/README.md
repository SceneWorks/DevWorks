# Planning authoring tools

These scripts generate planning artifacts; they are not application runtime dependencies.

- `requirements_content.py`: base requirement sections.
- `requirements_expansion.py`: interview additions and reconciled requirements.
- `build_requirements.py`: builds HTML, Word, structured JSON and the architecture figure.
- `build_gallery.py`: rebuilds the gallery and its screen coverage metadata from the existing UI previews.
- `build_architecture.cjs`: builds the architecture HTML reader, requirement traceability, capability catalog and structural validation record.

Install the authoring-only packages listed in `requirements.txt` in a development environment. Run from the repository root:

```sh
python3 tools/planning/build_requirements.py
python3 tools/planning/build_gallery.py
```

Generated files live in `docs/planning/`; temporary diagram input lives in ignored `.planning-build/`. The builder resolves paths relative to its own file rather than a specific checkout or working directory. Diagram generation needs Arial or DejaVu Sans fonts. Word rendering and visual inspection are still required after document edits; generation alone does not verify page layout.

The optional `--export-dir PATH` argument also mirrors the planning artifacts into another directory, preserving legacy `superharness-requirements.html` and `.docx` aliases there for earlier local links. Repository artifacts use the confirmed DevWorks name.

Keep confirmed requirements, proposed details, the interview record, UI limitations and source provenance aligned. CodeGraph feature requirements and delivery items are the operational source of truth; these versioned sources and generated readers publish the same approved decisions. No Shortcut records are created by these scripts.

## Architecture review artifacts

Edit `docs/architecture/devworks-architecture.md` and `architecture-contracts.json`. The latter maps requirement ID prefixes and explicit exceptions to design sections, owners and validation assignments. It also owns the proposed capability families. The builder consumes the generated requirements JSON without altering confirmed requirements or the Word edition.

`docs/architecture/devworks-storage-evaluation.md` is the editable storage comparison protocol; the same builder creates its offline HTML edition. Evidence in `helix-embedding-validation.json` preserves the historical registry/source probes and subsequent local native runtime qualification. Comparative results, correctness failures and remaining acceptance gates live in `storage-evaluation-status.json`, `storage-evaluation-results.json` and the evaluation reader. `validation/helix-direct-engine/` preserves the minimal probe inputs, lockfile and a path-redacted compilation log; it is not native application scaffolding or a benchmark implementation.

The architecture builder needs Node.js and the authoring-only `marked` package. Run with `marked` available in Node's module search path:

```sh
node tools/planning/build_architecture.cjs
node tools/planning/build_architecture.cjs --export-dir /path/to/review-copy
```

Sources and generated architecture files live in `docs/architecture/`. Diagram sources and rendered SVGs are committed together; reading the result requires no network. The current SVGs were authored with Mermaid 11.12.0, strict security, Arial fonts and flowchart HTML labels disabled. Render changed sources in an isolated authoring browser, serialize each SVG as valid XML with explicit dimensions, and check the standalone image as well as its inline document rendering. The ER diagram needs explicit dark text/light background styling for relationship labels. The document builder verifies asset existence, requirement uniqueness, complete design ownership and design anchors; it does not render diagrams or certify their visual quality.

After regeneration, check all diagram images, local links, section and requirement filters, document/diagram readability, and layout at representative widths. Record actual reader checks separately from the generated structural report. Dependency probes and native acceptance are separate evidence: a document mapping or valid SVG is not an application test result.

The native comparison harness and reproduction instructions live in `tools/storage-eval/`. Its release executables, fixtures and owned process-kill probes are authoring/evaluation tools, not native product implementation. Kuzu is excluded from the workspace and comparison. Retain raw failures and pair vector timings with measured recall; document rendering never certifies database correctness.

The current storage decision is plain SQLite plus sqlite-vec, with core-owned repository/query/transaction traits and adapter-local SQL. Detailed active contracts live in the architecture reader; HelixDB and GraphQLite comparison reports are historical evidence. Update editable diagram sources and requirement authoring sources together when this boundary changes. Use the current generated validation record for requirement/diagram counts. Runtime ownership and recovery semantics are adopted technical decisions; physical schemas and public serialized protocols remain gated. No design contract is a compiled product API.

Persistence refinement adds `docs/architecture/persistence-schema.sql` as an executable architecture fixture. `tools/storage-eval/persistence_contract_probe.py` checks its SQLite mechanics without inference; the retained native vector probe and exact reproduction steps are in `docs/architecture/validation/persistence-refinement/`. These probes do not implement the Rust repository ports or certify target durability, performance or application policy.

Router refinement uses `tools/planning/probe_router_contracts.py` for isolated architecture/reference fixtures and the evolved SQL schema. The source audit and result limitations are in `docs/architecture/validation/router-refinement/`. No fixture is a production provider adapter or a live inference/authentication/cache test.

Executable package refinement specifies the public semantic contracts and required G04/G05 cases in the architecture source. `validation/plugin-refinement-review.json` records documentation checks only. The package state/activation records extend the persistence contract but are not implemented by the initial SQL fixture. No native Rust SDK, signing subsystem or OS sandbox is claimed by this planning pass.

Security/platform refinement records confirmed SEC-03/04 updates and adopted AD37–40. `validation/security-refinement-review.json` distinguishes documentation/source checks from unimplemented enforcement and unqualified target builds. Keep actual OS/CPU/display/optional-capability results separate; a compile or a diagram is not a security acceptance result.

Capability review adds `inventory_codegraph.py` (read immutable source Git objects with authoring-only Tree-sitter 0.25.2/C# grammar 0.23.5), `map_codegraph_inventory.py` (reviewed native disposition rules) and `build_capability_review.cjs` (offline reader plus structural checks). Install the two parser packages into an isolated authoring directory and set PYTHONPATH for the scanner; they are not app dependencies. Run the scanner with explicit --repository, --revision and --output, pass that output to the mapper with --source/--output docs/architecture/codegraph-capability-inventory.json, then run `node tools/planning/build_capability_review.cjs` before the architecture builder. Acceptance and workload JSON are editable canonical design sources, not test results. Update their groups/bindings whenever the frozen source changes. The documented first scanner attempt with parser 0.26.0 crashed; the retained inventory uses the pinned successful parser.

The queue comparison in `docs/architecture/async-queue-options.md` records source observations for channel and broker alternatives, including Apalis SQLite setup/durability concerns. No dependency is adopted based on marketing benchmarks; no queue library/application runtime test ran in that comparison.

The shared-operation architecture candidate has a separate native DTO/schema and SQLite model harness in [tools/contracts-eval](../contracts-eval/README.md). After changing its DTO source, regenerate schemas/fixtures and verify them before rebuilding the architecture reader and family register. These remain authoring experiments, not shipped application code.

The provider/session/package reader is also generated by `build_architecture.cjs`. Current protocol/schema and native-helper reproduction lives in `docs/architecture/validation/provider-package-contracts/README.md`; update the selected-operation constraints when adding concrete public domain inputs.

The data/retrieval/artifact/delivery reader is generated by the same architecture builder. Current combined schema checks, the native trait/SQLite subset and real temporary Git effects are documented in `docs/architecture/validation/data-delivery-contracts/README.md`. Every probe owns its disposable inputs; none is application scaffolding or platform acceptance.

The native backend reader and local implementation draft are generated by `build_architecture.cjs`. Narrow native egui/PDF/CDP/decision/read reproduction lives in [tools/native-boundary-eval](../native-boundary-eval/README.md); it does not require or create application scaffolding. Retain candidate dependency/ABI pins and distinguish actual probe scope from target qualification.

Edit `docs/architecture/implementation-slices.json` for the local slice outcomes/dependencies. After regenerating the architecture catalog/traceability and contract family coverage, run `python3 tools/planning/build_implementation_plan.py`, then rerun the architecture builder for the detail reader. The plan builder checks unique operation ownership, all requirement/retained-source/CAP/workload assignments, dependency cycles and input source digests; it preserves Confirmed/Proposed and candidate/pending schema status. Its final acceptance slice depends on every other slice. Coverage counts are not native service acceptance or execution approval. No Shortcut records are created.

From the repository root, `node tools/planning/verify_native_planning_readers.cjs` uses an authoring Playwright installation to verify the main/native/implementation readers at 1440 and 800 pixels, local links/fragments, all images, decisions, requirement filters and slice anchors. It opens no user-facing tabs, writes ignored screenshots under `.planning-build/native-boundaries/` and records document checks separately from native experiment assertions. Inspect new diagram images and representative inline screenshots after rendering; structural checks alone do not establish visual readability.


The complete domain contract readers (`domain-operation-contracts` and `domain-operation-reference`) are also generated by the architecture builder. Source is `tools/contracts-eval/domain-spec.json`; `build_domain_reference.py` renders fields from that same source and the exact coverage register. Follow the contract evaluation README for regeneration and ABI-matched verification. Rebuild the local implementation register after schema coverage changes; keep historical probe counts/digests labeled as their original checkpoints. All 347 input/result candidates are now assigned, while actual production/authority/descriptor/SDK/provider/platform acceptance remains open.

`node tools/planning/verify_domain_planning_readers.cjs` checks the seven current/linked contract and implementation readers at 1440/800 pixels, all catalog operation rows, decisions/filters/slice anchors and local links/images. It opens no visible tabs; screenshots remain ignored and document QA stays separate from native/schema acceptance.

Baseline 1.0: `architecture-feasibility-review.md` and `architecture-handoff.md` are current design-closure sources and are rendered by the architecture builder. The domain reader verifier now also checks those readers, the native reader and historical storage readers; its current output is `validation/architecture-gates/reader-checks.json`. Current scope has 61 active slices; DW-038 is retired and is not renumbered. Keep historical probe evidence separate from current schema/reader results.

`node tools/planning/verify_baseline_prototype.cjs` checks the separate Anthropic forms, mock opt-in/out, selected storage text and absence of the removed Helix reader selector; it refreshes the two affected current gallery previews. `node tools/planning/verify_document_links.cjs` checks all top-level architecture/planning HTML local links/images and page errors, including runtime-created prototype anchors. Both use an isolated headless authoring browser and no real provider/database access.

After rendering/reviewing the Word requirements and running schema/source/reader/prototype checks, run `python3 tools/planning/verify_baseline_documents.py` with the bundled document authoring interpreter. It verifies all 345 Word/source requirement paragraphs, current ownership/input digests and scope decisions, parses document JSON and records a document inventory. This reports documentation consistency, not production acceptance.
