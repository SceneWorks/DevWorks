# DevWorks TypeScript semantic pack

The macOS arm64 add-on indexes TypeScript and JavaScript from committed or captured worktree source. It uses the frozen CodeGraph compiler passes for React, Angular, local workspace packages, service routes and Tauri. Node 24 is an optional runtime. The package includes the compiler and its locked libraries.

## Build and install the bundle

Enter the normal DevWorks dev-host build environment. The Node build uses a scratch directory.

```sh
dw_ts_build="$(mktemp -d "${TMPDIR%/}/typescript-pack.XXXXXXXX")"
git archive HEAD crates/devworks-typescript-pack/extractor | tar -x -C "$dw_ts_build"
dw_ts_source="$dw_ts_build/crates/devworks-typescript-pack/extractor"
npm --prefix "$dw_ts_source" ci --ignore-scripts
npm --prefix "$dw_ts_source" test
node "$dw_ts_source/stage.cjs" "$dw_ts_build/compiler"
cargo build --locked --release -p devworks-typescript-pack --features bundle
dw_ts_target="$(cargo metadata --locked --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
"$dw_ts_target/release/devworks-typescript-pack" --bundle \
  "$dw_ts_build/devworks-typescript-semantic.dwpkg" "$dw_ts_build/compiler" <signing-key-file>
```

The builder writes a content-addressed bundle and checks it with the native inspector. The signing key file contains the publisher's 32-byte Ed25519 seed. Omit the key for an unsigned development bundle. Install the bundle through Add-ons, approve its manifest, and enable `devworks-typescript-semantic` in the repository's source configuration.

The helper sends only files from the exact index snapshot to Node. Project scripts and their installed modules stay outside that snapshot. The package records partial coverage when its compiler refuses the input or exceeds a resource limit. Rebuild Index retries a recorded failure.

## Qualify the native package

Set `DEVWORKS_TYPESCRIPT_PACK_BUNDLE` to a signed native bundle and run the ignored engine package test on macOS arm64 with Node 24 on PATH.

```sh
cargo nextest run --locked -p devworks-engine --test packages --run-ignored only \
  -E 'test(native_typescript_pack_indexes_framework_relationships)'
```
