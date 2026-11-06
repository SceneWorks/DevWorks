# Build and test a native add-on

The local extension SDK is the Rust `devworks-extension-protocol` crate shipped with this source checkout. It defines strict manifest, request/result, event, native UI, and provider codec types. Use the SDK version compatible with the installed host; a valid descriptor is not proof that every contribution has a qualified runtime.

## A small working example

Read the [complete bundled example source](help:sdk-echo-source), also maintained at `docs/help/examples/echo` in the source checkout. It builds a native helper that negotiates the actual framed protocol, activates the recorded registry/state generation, and returns an existing input artifact without executing a host mutation or model. Its bundle builder computes actual file/schema digests and writes the native package container. This is a development example, not an unsigned production trust bypass.

From a DevWorks source checkout:

```sh
cargo build --manifest-path docs/help/examples/echo/Cargo.toml --target-dir target/help-echo --bin demo-echo
cargo run --manifest-path docs/help/examples/echo/Cargo.toml --target-dir target/help-echo --bin build-bundle -- target/help-echo/debug/demo-echo demo-echo.dwpkg
```

Use the helper path printed by Cargo if you select a different target directory; Windows binaries use `.exe`. Build for the actual host target triple. Save the generated bundle as an artifact, inspect it in Add-ons, review its access and exact manifest digest, then use the supported human install path. Installing a native executable runs code with your ordinary user rights. No package signature or descriptor supplies OS sandboxing.

## Develop and qualify

Keep stdout exclusively for framed protocol messages; use bounded stderr diagnostics. Validate typed frames, match response IDs, retain invocation tokens, observe cancellation, and return truthful outcomes. Run the example's local tests, the extension-protocol contract tests, and relevant native package fixtures. Do not perform model inference in ordinary tests; use recorded protocol bytes and controlled services.

Read [Manifest and bundles](help:sdk-manifest), [Process protocol](help:sdk-protocol), [Tools](help:sdk-tools), and [Package lifecycle](help:addons). For language support see [Semantic packs](help:sdk-semantic). Build tools or dependencies can require setup even though reading this Help never requires a network.
