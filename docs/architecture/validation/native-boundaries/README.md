# Native boundary planning evidence

Recorded 1 November 2026 on macOS arm64. Architecture experiments only: no native DevWorks application, inference, real provider/Jev request, production credentials, personal browser profile or user-visible browser tab. Confidence: high in these narrow observations; medium in full native integration until qualification.

- [UI results](ui-results.json): 4 checks, six headless egui 0.37 frames; 100,000 uniform rows with at most 39 materialized, tessellation/AccessKit updates, shared fixture draft and drop interface.
- [PDF results](pdf-results.json): 8 checks, explicit native PDFium 7881 binding, text/scan raster, encryption/password, malformed input and absent explicit library.
- [Browser results](browser-results.json): 9 checks, actual Rust-owned headless Chrome 147.0.7727.15/CDP 1.3 navigation, input, upload, AX, console/network, screenshot/history and direct-child close/reap.
- [Decision results](decision-results.json): 12 checks, all four tier pairs/reused pins, custom membership, effort/empty-pool handling and typed SystemOne localhost HTTP subset. No durable production pin or live model claim.
- [Read guard results](guards-results.json): 6 checks, actual macOS root-relative no-follow regular-file open, substitution-resistant descriptor, fixture revision deny and byte bound. No production authority, deletion, mount/hard-link or confined-executable claim.
- [Evidence manifest](evidence.json): source/result hashes, host/tool versions and explicit PDFium asset provenance. Downloaded binary is unsigned experiment input; shipping audit/signing/SBOM/notices and all target builds remain open.
- [Reader verification](reader-checks.json): generated reader/layout/link/filter checks, separately identified from native assertions.
- [Reproduction source and instructions](https://github.com/SceneWorks/DevWorks/tree/f76506ec33c9563d19e03bc68a4b7e9e65178271/tools/native-boundary-eval/README.md) at commit `f76506ec`; the probe was removed from the main branch.
- [Native backend decisions and limits](../../native-backend-feasibility.md).
- [Complete implementation ownership draft](../../implementation-plan.json).

All 39 native assertions passed in the final wrapper run. The first source build needed egui 0.37 dropped-file trait/API updates and handling PDF `as_image()` as a Result. PDF native rasterization initially succeeded while PNG output failed because the encoder feature was absent; the explicit image PNG feature corrected it. The final reports supersede those attempts. These repairs are recorded to distinguish source/API qualification from a claim that defaults worked unchanged.

The Rust executable invokes native libraries and an explicitly selected Chromium binary. Python (reportlab/Pillow/pypdf), Node/Playwright and Mermaid only author fixtures/readers/diagrams. They are not proposed application runtime requirements. The browser experiment uses an ephemeral loopback debug endpoint; production private transport and full process-tree ownership remain unqualified. No Chromium `--no-sandbox` switch is used.

The current probe's PDF library path is macOS-specific. Its PDFium archive is the explicit 7881 macOS arm64 release, SHA-256 `52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40`; the matching wrapper feature is selected with defaults off. Archive/license files and all executable/build/fixture/image intermediates remain ignored under `.planning-build/`; the reproducible Rust source, Cargo lockfile and bounded reports are retained here. Never equate a verified download digest with approved shipping provenance.

The [local plan builder](../../../../tools/planning/build_implementation_plan.py) independently checks structural ownership: 62 acyclic slices, 345 requirements, 347 operations/47 families, 494 retained source entries, 64 intentional exclusions, 33 CAP groups and 12 workloads. It preserves 208 Confirmed/137 Proposed and 58 candidate inputs/289 pending inputs. These are coverage assertions, not behavioral acceptance. All production workloads remain unexecuted; all target support, actual host/SDK/authority/provider behavior and remaining full contracts stay gated.
