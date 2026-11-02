# Architecture baseline evidence

2 November 2026. These are scoped native feasibility experiments, source/document checks and explicit user decisions; no production acceptance is claimed.

- `secret.json`: native macOS Keychain fixture write/read, rotation, deletion and unavailable-after-delete; no existing credential access or prompt.
- `process.json`: new owned Unix group, descendant membership, cancellation, direct-child wait and descendant reaping. Cooperative fixtures only.
- `ui.json`: matching eframe/egui 0.37.0 and Glow; two invisible native frames. Earlier AccessKit-tree evidence remains in `../native-boundaries/`; this does not qualify OS assistive technology.
- `evidence.json`: exact host/tool/dependency/source/binary digests and source-inspection provenance, with remaining limits.
- `schema-model-results.json`: current complete candidate-schema/message verification; host human authority/risk admission is an implementation obligation.
- `reader-checks.json`: current document QA at 1440/800 pixels, local links/fragments/images, filters, decisions and active slice anchors; generated separately from native checks.
- `prototype-checks.json`: current mock Settings/storage/database interactions; no real sign-in, credentials or database calls.
- `all-document-links.json`: all top-level architecture/planning HTML local targets, images and page errors.
- `document-review.json`: current source/generated document inventory, baseline scope and structural reconciliation. Historical raw evidence remains scoped to its original checkpoint.

Reproduce native checks using [the probe README](../../../../tools/architecture-gates-eval/README.md). Normal verification never invokes a provider, sign-in, installed inference model or user-visible browser tab. This review used only `claude --help`/`--version` while evaluating the subsequently rejected CLI alternative; no Claude inference was run. The final design retains the direct simple-router subscription option and separate Anthropic API-key access.

A transient Helix native-reader experiment demonstrated bounded structural reduction before the user removed the reader. It has no current admission gate or production ownership. Its new probe source was removed; the older four-engine storage comparison remains historical evidence. DW-038 is retired. Neither code indexing nor app storage uses HelixDB.

The initial UI/credential/process build and all three native runs passed. The transient Helix experiment initially failed to compile because the probe used `is_read_only` instead of the pinned API's `is_reader_mode`, then required creating its owned disk directory before opening it; both were fixture mistakes, corrected before its passing run. They establish no product/engine defect. The experiment is not a current feature.

The architecture baseline closes design questions only. G01–G12, all real adapters/SDK contributions, risk/credential/recovery races, retained semantic parity, six target builds, signed installers and all twelve production workloads remain implementation acceptance. No cross-platform, live-provider or sandbox qualification follows from these files.
