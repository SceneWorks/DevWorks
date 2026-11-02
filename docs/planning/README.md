# DevWorks planning

This directory is the canonical local home for the requirements, interview record and UI design. It was transferred from the Codex planning workspace on 1 November 2026. The source repository is [SceneWorks/DevWorks](https://github.com/SceneWorks/DevWorks).

The [architectural design baseline](../architecture/devworks-architecture.html) and its [sources, diagrams and validation records](../architecture/README.md) live alongside this directory. Requirements retain their existing confirmation status; architecture baseline 1.1 is adopted and production acceptance remains pending.

- `devworks-requirements.html`: searchable review document with stable requirement IDs.
- `devworks-requirements.docx`: editable Word edition of the same requirements.
- `requirements-interview.json`: confirmed decisions, remaining questions and source findings.
- `requirements-content.json`: generated structured requirement records for later migration into CodeGraph.
- `harness-workbench.html`, `workbench.js`, `workbench.css`: interactive design reference.
- `mockup-gallery.html`, `ui-07/`: gallery and 43 current screen previews. `ui-05/` and `ui-06/` retain prior studies.
- `design-coverage.json`: source-to-screen coverage notes and prototype limitations.
- `requirements-assets/` and `revised-*.png`: document figures; older reviewed figures are retained as design history.

Edit the requirement sources in `tools/planning/` and the interview record here, then regenerate both document editions. Do not edit generated HTML, Word or requirement JSON independently. Manual corrections to the Word edition need to be reconciled into the authoring source before rebuilding. See the authoring guide for commands.

To serve the prototype from the repository root:

```sh
python3 -m http.server 8766 --bind 127.0.0.1 --directory docs/planning
```

Open `http://127.0.0.1:8766/harness-workbench.html?page=chat&project=sw` or `http://127.0.0.1:8766/mockup-gallery.html`. This optional Python command serves the planning prototype only; the native product must not require Python or an application server.

The prototype uses local browser sample state. Agents, inference, GitHub operations, parsing/indexing, database operations and desktop automation are simulated. It does not establish native implementation or acceptance. Revision 06 adds managed document RAG, language packs and harness extensions, local Help, update status, access/safety, project paths and lifecycle, container preferences and revised delivery policies. Settings has 21 searchable topics. The corrected test policy is visible: unit/integration tests use no real inference; a separate live end-to-end suite will be set up later. Architecture draft 0.1 is now available for review; native implementation remains pending.

Revision 07 adds a toolbar-accessible workspace coordinator sidebar on every screen. It shares the current Home session and preserves drafts, attachments, the model pin and transcript. Navigation appends deduplicated host-authored screen context, linked to local Help and relevant effective tools. It stays open until explicitly closed or Home is opened. Feature and issue conversations remain independent. At narrow widths it overlays the right edge; at wider widths it docks beside content.

Router selection refinement adds direct Max/High/Mid/Low tiers above the named models, independent effort, editable/custom tier membership and a separate SystemOne/Jev configuration section. The simulation preserves pins and shared Home/sidebar preferences. Native capabilities and decision adapters still require qualification. ROUTE-18–20 and revised UI-10 are captured in the requirements.

2 November 2026: ROUTE-09/10 distinguish direct Claude subscription access (unsupported, disabled until explicit risk opt-in) from Anthropic API-key access with separate billing. The routing Settings prototype reflects both; no provider requests or real credentials are used.

The external HelixDB schema reader was removed on 2 November 2026. Current connection selectors and schema contracts include only the selected relational targets. Historical UI revisions and storage evaluation evidence are not current product scope.
