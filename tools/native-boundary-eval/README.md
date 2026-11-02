# Native boundary experiments

Disposable architecture probes, not production app scaffolding. Current host is macOS arm64. They contain fixture policy/credentials and do not implement a public SDK, router, authority service or application window.

Pinned Cargo dependencies and lockfile reproduce egui 0.37, PDFium 7881 binding, native localhost/CDP transport and macOS `openat` experiments. See [scope and evidence](../../docs/architecture/validation/native-boundaries/README.md). Python below only authors synthetic fixtures; no Python/Node runtime is needed by the Rust executable or proposed app.

Build from the repository root:

```sh
CARGO_TARGET_DIR="$PWD/.planning-build/native-boundaries/target" cargo build --locked --release --manifest-path tools/native-boundary-eval/Cargo.toml
```

Use a verified explicit PDFium 7881 macOS arm64 directory containing `lib/libpdfium.dylib`. The recorded asset source/digest/build flags and licensing limitations are in the evidence manifest. Keep its LICENSE/third-party notices. Do not silently load an arbitrary system library or mismatched latest ABI. Use an explicitly selected Chromium executable; the probe opens only its own headless process and empty temporary profile, then closes/reaps its direct child. It does not attach a user's browser. Full process-tree enforcement remains unqualified.

Use an authoring Python environment with reportlab, Pillow and pypdf, and a new output directory:

```sh
python3 tools/native-boundary-eval/run.py \
  --binary "$PWD/.planning-build/native-boundaries/target/release/devworks-native-boundary-probe" \
  --pdfium-dir "$PWD/.planning-build/native-boundaries/pdfium/expanded" \
  --chromium /absolute/path/to/chromium-executable \
  --output-dir "$PWD/.planning-build/native-boundaries/reproduction-new"
```

The wrapper creates owned PDF fixtures and short-lived empty read/browser roots. It saves five JSON reports plus browser/PDF images, fails on nonzero probe exits and imposes a per-process timeout. No dependency download, actual inference, live Jev/provider request or personal browser mutation occurs. The browser does start the selected native executable and a debugging transport for the owned local fixture. The current CDP experiment uses loopback discovery, not the proposed production private transport.

Individual commands are `ui`, `decision`, `guards OWNED_EMPTY_DIRECTORY`, `pdf PDFIUM_DIRECTORY FIXTURE_DIRECTORY OUTPUT_DIRECTORY` and `browser CHROMIUM_EXECUTABLE OWNED_EMPTY_DIRECTORY`. Do not pass user directories as fixture roots. All 39 checks are narrow experiment assertions; they do not establish production permission enforcement, full format/protocol parity, performance or another OS/CPU build.
