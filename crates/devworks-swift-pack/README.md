# DevWorks Swift semantic pack

The optional macOS arm64 add-on indexes captured Swift source through Apple's Swift frontend and IndexStore. This bundle qualifies the native Apple Swift 6.4.2 toolchain and macOS SDK 27.2 from an initialized Xcode installation. IndexStore uses format 5 and C API 16. Other toolchain combinations appear as unavailable.

The compiler reads ordinary literal SwiftPM library, executable and test targets, in-tree packages and loose Swift source. It uses installed Apple frameworks and built-in macro libraries from the selected Xcode installation. The pack recognizes literal source selections, excludes and in-tree dependency products. Compiler setup uses a private source copy and module cache. Xcode projects, computed manifests, unsupported build settings and unpublished dependencies retain baseline coverage. A target with a Swift compiler error leaves a coverage gap for itself and its dependents.

The graph stores compiler symbol identity, source ownership, token spans and exact relationship endpoints when they appear in the captured layer. Structural Swift indexing supplies its own coverage and source ranges. Compile-bound references to the installed SDK can appear without a local target.

## Build and qualify

Enter the DevWorks dev-host build environment and select the supported Xcode installation in the operating system. Then run the bundle builder.

```sh
cargo build --locked --release -p devworks-swift-pack --features bundle
swift_pack_target="$(cargo metadata --locked --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
"$swift_pack_target/release/devworks-swift-pack" --bundle   "${TMPDIR%/}/devworks-swift-semantic.dwpkg" <signing-key-file>
```

The signing key file contains the publisher's 32-byte Ed25519 seed. Omit the key for an unsigned development bundle. Install and approve the package through Add-ons and enable `devworks-swift-semantic` in indexing settings.

To run native acceptance, set `DEVWORKS_SWIFT_PACK_BUNDLE` to the signed bundle and run the engine fixture on macOS arm64 with the qualified toolchain.

```sh
cargo nextest run --locked -p devworks-engine --test packages --run-ignored only   -E 'test(native_swift_pack_)'
```

The C ABI used by the decoder comes from the Swift project's LLVM IndexStore header. The compatibility probe used header SHA-256 `50aba76c66459f8eefab21a60485c6cd2b32b35b2445e87de11bd5a44d21ff16`. [LLVM IndexStore C API](https://github.com/swiftlang/llvm-project/blob/next/clang/include/indexstore/indexstore.h)
