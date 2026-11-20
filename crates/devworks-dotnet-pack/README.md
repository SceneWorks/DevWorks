# DevWorks .NET semantic pack

This optional macOS arm64 add-on indexes C# and Visual Basic using Roslyn 5.3.0. It uses the highest installed stable SDK in the .NET 10.0.2xx band, an installed .NET 10 runtime, and installed reference packs for the project's target framework. It recognizes literal `Microsoft.NET.Sdk` and `Microsoft.NET.Sdk.Web` projects, in-tree project references and loose sources. It keeps the C# extractor's retained relationships and adds source-bound compiler results for both languages.

The pack reads an exact captured source layer. It requires a reference pack for each project target. Project build tasks, restore, source generators and custom imports stay outside indexing. Native framework and in-tree assembly references can bind. NuGet declarations stay visible and their unavailable assemblies appear in coverage. A project needing unmodeled MSBuild settings leaves structural and text coverage. Use Rebuild Index to retry a recorded failure after fixing its prerequisite.

## Build and install the bundle

Enter the normal DevWorks dev-host build environment. Install SDK 10.0.202 for the build. Stage the managed project outside the source checkout.

```sh
dw_net_build="$(mktemp -d "${TMPDIR%/}/dotnet-pack.XXXXXXXX")"
git archive HEAD crates/devworks-dotnet-pack | tar -x -C "$dw_net_build"
dw_net_source="$dw_net_build/crates/devworks-dotnet-pack"
(cd "$dw_net_source" && dotnet test tests/DevWorks.Dotnet.Tests.csproj --configuration Release)
(cd "$dw_net_source" && dotnet build compiler/DevWorks.Dotnet.csproj --configuration Release --output "$dw_net_build/compiler")
cargo build --locked --release -p devworks-dotnet-pack --features bundle
dw_net_target="$(cargo metadata --locked --format-version 1 --no-deps |
  python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
"$dw_net_target/release/devworks-dotnet-pack" --bundle \
  "$dw_net_build/devworks-dotnet-semantic.dwpkg" "$dw_net_build/compiler" <signing-key-file>
```

The signing key file holds the publisher's 32-byte Ed25519 seed. Omit the file for an unsigned development bundle. Install and approve the bundle in Add-ons and enable `devworks-dotnet-semantic` in indexing settings.

## Qualify the native package

Set `DEVWORKS_DOTNET_PACK_BUNDLE` to a signed bundle and run on macOS arm64 with a compatible SDK on PATH.

```sh
cargo nextest run --locked -p devworks-engine --test packages --run-ignored only \
  -E 'test(native_dotnet_pack_indexes_csharp_and_visual_basic)'
```
