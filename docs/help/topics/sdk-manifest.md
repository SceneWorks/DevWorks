# Manifest, schemas, signing, and packaging

A native bundle uses `devworks-package-bundle/1`, not an arbitrary ZIP or installer script. It contains the eight-byte magic `DWPKG\0\x01\0`, little-endian length-prefixed manifest/signature sections, then regular files with safe relative paths, exact SHA-256 digests and byte lengths. The native SDK bundle writer and the included example produce this format.

## Describe the package

The strict manifest supplies `packageId`, semantic `version`, `publisherId`, protocol min/max, minimum host version, exact dependencies/runtime requirements, schema assets, requested limits, target binaries, every file, state/configuration schema references, `helpFile`, requested permissions and contributions. IDs must follow their SDK scalar rules. Contributions and schema IDs use the package namespace. Schemas identify an exact revision and canonical document digest; listing a schema reference without shipping a required document does not make configuration valid.

The example builder generates a complete small manifest with real digests. Do not replace file/schema hashes with illustrative values. Target binaries must match the installed host architecture/OS; one target's existence does not qualify another. Paths cannot escape the bundle, alias by case, contain parent traversal, or name Windows device paths. Every listed file must be present and every bundled file listed.

## Trust and access

Signatures are Ed25519 over the domain-separated exact manifest bytes. A present invalid signature is refused, never downgraded to unsigned. Request only the access your contribution needs: worktree read/write, declared network endpoints or runtimes, inference, state writes, or UI contribution. The host validates current scope and authority for each actual broker action; a manifest is an access request, not a grant.

Unsigned development content and agent-authored contents require their exact approval. Publisher auto-update trust has a bounded access/compatibility envelope. Expanded access, signing-key changes, state/contract incompatibility, and new dependencies/runtimes can require a new decision. See [Add-ons](help:addons) for quiescence, configuration and rollback, and [Protocol](help:sdk-protocol) for runtime bindings.
