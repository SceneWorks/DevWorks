# Maintained local Help bundle

`catalog.json` is the app bundle index. The core Help owner bundles these files and supplies the **actual app package version**; content does not invent a parallel version. Every entry has stable `id`, `title`, relative `file`, `screenIds`, `settingsSections`, and `relatedCapabilityIds`. The last field contains exact operation names, served as current metadata labels rather than fabricated opaque IDs.

Topics are shared by native UI and agent Help reads. Use `help:<topic-id>` Markdown links. Each chapter stays within 65,536 Unicode characters and 256 KiB. The reader's own passage/response limits still apply. External links must not become a network search/read fallback.

Maintain working behavior, scope, defaults, actions, adoption, recovery, permissions and limitations alongside a capability change. Do not equate a generated schema or visible navigation tab with a qualified implementation. Keep tracker keys out of product prose. Installed package guides retain their own exact package version/manifest and are not copied into the app bundle as if current forever.

Run the source-aware checker:

```sh
python3 docs/help/check.py
```

When the owning Settings catalog changes, review its semantic change and regenerate its complete reference, then inspect the diff:

```sh
python3 docs/help/check.py --refresh-settings
python3 docs/help/check.py --refresh-example
```

The checker compares actual registered screen/topic bindings, Settings sections, all catalog defaults/scopes/adoption/policies, local links, bounds, exact operation spelling, and the full example source embedded in offline Help. It does not certify runtime availability. Runtime-generated capability references must be derived from the actual host registry/registered modes and current permissions; the manual is not a second capability authority.

The Rust echo package under `examples/echo` uses the actual SDK framing/types and native bundle writer/inspector. Its tests contain no learned inference:

```sh
cargo test --manifest-path docs/help/examples/echo/Cargo.toml --offline
cargo clippy --manifest-path docs/help/examples/echo/Cargo.toml --all-targets --offline -- -D warnings
```

See `coverage.json` for source owners and the retained documentation-regeneration disposition. This file is maintainer guidance; catalog topics are the product content.
