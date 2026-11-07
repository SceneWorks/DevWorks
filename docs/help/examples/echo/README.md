# Native echo package example

`demo-echo` is the runnable tool. `build-bundle` uses the real native bundle writer and inspector to compute and validate every shipped schema/file/help reference. The generated development bundle is unsigned and still requires exact human approval before installation. It requests no broker permissions and returns the host-owned input artifact without inference or mutations.

`contributions.json` shows all eight SDK descriptor variants; `event-commit.json` shows the typed transaction shape. These are **contract examples**, not extra working implementations in the echo helper. Their illustrative schema/artifact hashes are not installable content. Replace references with actual shipped schema digests and host-issued invocation/subscription bindings, and implement/test the matching producer before declaring a contribution active. An event JSON file cannot manufacture a live token or subscription.

Build from the repository root:

```sh
cargo build --manifest-path docs/help/examples/echo/Cargo.toml --target-dir target/help-echo --bin demo-echo
cargo run --manifest-path docs/help/examples/echo/Cargo.toml --target-dir target/help-echo --bin build-bundle -- target/help-echo/debug/demo-echo demo-echo.dwpkg
```

On Windows use the `.exe` helper path. Use the actual executable for the current target triple. The app installs a package only through its normal artifact, inspection, trust/approval, permissions and lifecycle path; this example does not install itself.

Test offline after dependencies are available:

```sh
cargo test --manifest-path docs/help/examples/echo/Cargo.toml --offline
```
