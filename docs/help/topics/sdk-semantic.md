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
