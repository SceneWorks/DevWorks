# Native PDF worker notices

DevWorks builds the PDF worker from the same locked Rust workspace as the desktop app. Keep
`devworks-pdf-worker` (or `devworks-pdf-worker.exe`) beside the desktop executable when packaging
it. On macOS both belong in the signed application's `Contents/MacOS` directory. There is no
downloaded PDFium library, operating-system PDF library lookup, or font-file lookup.

- [Lopdf 0.47.0](https://crates.io/crates/lopdf/0.47.0) extracts native page text. MIT.
- [Hayro 0.9.0](https://crates.io/crates/hayro/0.9.0) renders CPU raster evidence.
  Hayro and its font support are available under MIT or Apache 2.0.
- [Hayro Font 0.5.0](https://crates.io/crates/hayro-font/0.5.0) embeds the standard PDF fallback
  fonts shipped in that crate. Copyright 2014 PDFium Authors. Original code copyright
  2014 Foxit Software Inc. The accompanying Foxit notice applies to those fonts.

The package includes the exact Cargo lockfile as its dependency source and version manifest.
The qualification report records the installed worker path within the target distribution,
OS/architecture, binary digests, and compressed and installed lengths.
