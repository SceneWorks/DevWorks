# Native DOCX body extraction feasibility

1 November 2026. Narrow disposable Rust probe, not a production extractor, Word renderer or security certification. Run on macOS arm64 with Rust/Cargo 1.98.0. Candidate pins: zip 9.2.1, quick-xml 0.42.0; Cargo.lock retains the resolved graph. No Word/Office, Python, Node, network service or inference is used by the Rust executable. Python is only the fixture authoring harness.

The probe extracts main-body runs/table paragraphs and stable paragraph locators. It bounds archive size, declared expansion, parts, main-part bytes and XML depth; rejects selected malformed/DTD/path/duplicate inputs. Eleven checks passed, including the final requirements Word document. The inert external-reference fixture establishes that this probe ignores that relationship; it is not comprehensive external-entity or hostile-document qualification.

Known limits: local XML names are used without a full OOXML namespace/content-type/relationship validator; only the main part is parsed. No auxiliary stories, tracked-change interpretation, style/list semantics, nested-table fidelity, layout, encryption classification, full output/time policy or app publication/recovery adapter. The generated fixture packages are intentionally minimal; the requirements document is an additional real package. Full production acceptance remains G08.

Reproduce from the repository root:

```sh
CARGO_TARGET_DIR="$PWD/.planning-build/remaining-decisions/cargo-target" cargo build --locked --release --manifest-path tools/document-eval/Cargo.toml
python3 tools/document-eval/run_fixtures.py --binary .planning-build/remaining-decisions/cargo-target/release/devworks-docx-probe --output .planning-build/remaining-decisions/fixtures --document docs/planning/devworks-requirements.docx
```

[Results](results.json) record exact cases and the Word input digest. Fixture generation/source live in `tools/document-eval`; build output and generated ZIPs are ignored. An initial compile used older assumed library method signatures and failed. The recorded probe was corrected against the installed current APIs and rebuilt successfully; that initial failure is not a parser acceptance result. These versions are probe candidates, not a claim of final product dependency qualification.
