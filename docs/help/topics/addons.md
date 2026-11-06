# Install, configure, update, and roll back add-ons

Add-ons are managed executable packages. A package is identified by its package ID, version, publisher, and exact manifest digest; reusing a version string with different contents does not make it the same version. Inspect the actual bundle, target binary, schemas, permissions, dependencies, runtimes, and local Help before installing.

## Trust and approval

A signature verifies who signed the exact manifest and its file hashes. It does not grant permission or prove safe behavior. Agent-authored contents require exact human approval even when signed by a trusted publisher. Unsigned development bundles also need exact approval. Native executables run with ordinary user rights; the broker's policies are not an OS sandbox around arbitrary native code.

Publisher auto-update trust is a separate explicit choice. Automatic activation is limited to compatible signed updates within the approved key, package, stream, access envelope, dependencies, runtime and contract/state constraints. Expanded access, a new key, or an incompatible transition needs its appropriate approval. No setting silently trusts every future package version.

## Lifecycle

Enable, disable, update, and rollback use revisioned operations and quiescence. Pending activation does not replace the active version. Configuration is validated against the active version's exact schema and adopted through its recorded configuration revision. A restart or unresolved invocation can delay a transition. Rollback uses a previously available exact version; it does not fabricate state migration or erase old effects.

Package guides are bundled with the installed version and read offline. Missing/stale guides remain visible as such; a latest online guide is not a substitute. See [SDK quick start](help:sdk-start), [Manifest and bundles](help:sdk-manifest), [Protocol](help:sdk-protocol), and [Recovery](help:recovery).
