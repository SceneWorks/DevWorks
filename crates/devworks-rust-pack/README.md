# DevWorks Rust semantic pack

The macOS arm64 pack uses rust-analyzer 1.98.0 to index a Cargo workspace from files the DevWorks indexing service captured. It works with the Add-ons installer, repository source configuration, and the same index and graph operations used by the desktop and agent tools. The package is separate from the desktop executable.

## Build a native bundle

Use Rust 1.98.0 with its Cargo, rust-analyzer, and rust-src components. In the DevWorks checkout, enter the normal dev-host build environment and run:

```sh
cargo build --locked --release -p devworks-rust-pack --features bundle
dw_pack_target="$(cargo metadata --locked --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
"$dw_pack_target/release/devworks-rust-pack" --bundle devworks-rust-semantic.dwpkg <signing-key-file>
```

The signing key file contains the publisher's 32-byte Ed25519 seed. Omit it to create an unsigned development bundle for inspection. The builder verifies the finished bundle with the native package inspector and prints its SHA-256. Install the prebuilt bundle through Add-ons and approve its exact manifest. Enable `devworks-rust-semantic` in the repository's source configuration. The Add-ons runtime check looks for rustup. The helper checks the pinned toolchain and its components when indexing, independent of the default Rust version. Indexing displays coverage and failure details. Use Rebuild Index after repairing a missing toolchain component or Cargo cache entry.

The pack reads eligible Rust files and Cargo manifests and locks from the recorded commit or worktree capture. It resolves dependencies offline. Crates.io archives and sparse index entries for locked dependencies must already exist in the local Cargo cache. Workspaces with missing or unsupported dependencies report partial coverage and keep their structural index. The pack omits repository toolchain overrides and Cargo configuration, and skips execution of build scripts and procedural macros.

The package processes one captured layer at a time. Its source limit is 4 MiB over 4,096 files and its SCIP output limit is 8 MiB. Compiler work has a 45-second deadline and a sampled 2 GiB memory budget for the owned process group. The host cancels the group with the index run. A failed compile leaves structural results and a visible coverage gap.

## Qualify the native pack

Use the release executable to create a bundle signed with a temporary test key. Set `DEVWORKS_RUST_PACK_BUNDLE` to that file and run the ignored engine package test on macOS arm64 with the pinned compiler components installed:

```sh
cargo nextest run --locked -p devworks-engine --test packages --run-ignored only \
  -E 'test(native_rust_pack_indexes_a_cargo_workspace)'
```
