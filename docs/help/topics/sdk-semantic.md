# Semantic language packs and index producers

A Semantic contribution declares languages and Scip or Native format with exact input/output schemas and a local Help topic. It can add semantic support without changing the app's structural parser. A language name or valid manifest alone does not prove that a compiler/runtime or producer is present.

Example declaration shape:

```json
{"kind":"Semantic","meta":{"id":"demo-pack/index","inputSchema":{"schemaId":"demo-pack/in","revision":"1","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"outputSchema":{"schemaId":"demo-pack/out","revision":"1","sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},"helpTopic":"index","required":true,"cancellation":"Cooperative"},"languages":["csharp"],"format":"Scip"}
```

These example hashes illustrate typed references; ship real canonical schema documents and computed digests. Declare every required runtime/dependency and build a target binary for the host. Do not assume a Windows, macOS, Linux, or compiler variant from a different target's success.

## Produce source-bound results

Read only the admitted source snapshot/registered worktree. Emit the declared format and exact source/index artifacts. Preserve project/repository/path identity, source revision, language, ranges, symbols, relationships and diagnostics required by that format. Do not publish a partial failure as an empty successful index. The host validates/imports results into a coherent generation and filters scope/overlays before use.

Use small fixture repositories to test source edits, deletions, renamed files, invalid input, cancellation, missing prerequisites and stale source. Test one-shot producer I/O separately from the interactive package process protocol; their framing/lifetime need not be identical. Ordinary tests must not run model inference.

Install and configure through [Add-ons](help:addons), inspect actual [Indexing](help:indexing) runs, and consult [Manifest](help:sdk-manifest) and [Protocol](help:sdk-protocol) before advertising a pack as supported.

For configured semantic packages, the indexing worker sends an exact batch of eligible source text with its repository ID, commit, optional worktree capture digest, and each file's unit, language, and digest. Return one `SemanticIndex` event naming that invocation's source snapshot and an index artifact. A Native index contains a `units` list with each unit's definitions and relationships. Scip uses a binary SCIP index and a declared output schema for its `format`, `index`, and `sourceSnapshot` references. The worker checks paths and ranges against the captured source before publishing the generation. A failure appears in indexing coverage.

If a project disables a semantic package for a repository shared with other projects, the next index build omits that package for the shared repository. Earlier results carry a stale package warning. Structural indexing and other eligible packages can continue.

A repository-aware producer declares `sourceMode: "Repository"`. It receives the eligible language source and its admitted project context for one captured layer, within the repository-mode bounds. Rust uses Cargo manifests and locks. TypeScript uses its project and package configuration and the related framework files admitted by indexing. The host validates and publishes its results through the normal indexing service. Producers without this field continue to receive file batches.

The DevWorks Rust semantic pack uses the macOS arm64 build of rust-analyzer 1.98.0. It reports Cargo symbols and references for supported snapshots whose dependencies are available offline. Missing prerequisites or a compiler fault appears as a coverage gap.

The DevWorks TypeScript semantic pack uses Node 24 on macOS arm64. Its signed bundle includes the compiler and records JavaScript, TypeScript, local npm workspace, React, Angular, HTTP call, and Tauri relationships from the captured source. Indexing continues with structural results when the pack is disabled or its runtime is missing.

The DevWorks .NET semantic pack uses Roslyn for C# and Visual Basic on macOS arm64 with an installed .NET 10.0.2xx SDK and .NET 10 runtime. It binds source and in-tree project references using installed framework reference metadata. Indexed project builds and generators stay outside the runtime path. Coverage reports missing reference assemblies, unavailable prerequisites and unsupported project settings. Structural C# and text Visual Basic indexing remain available.

The DevWorks Swift semantic pack uses the installed Apple Swift 6.4.2 toolchain and macOS SDK 27.2 on macOS arm64. It adds compiler symbols and references for supported SwiftPM and loose Swift projects. A compiler fault affects the target and its dependents. The app keeps structural coverage and reports unavailable toolchains, unmodeled projects and unresolved references.
