# Reviewed Git manifest launch and gitlink qualification

Base: `7ad0532982db466b208e10ae30a50aaa579a9081`. The assembled native pass also includes the separately owned reviewed-Create fix `188d48dc`.

The fixes address PR 38 review findings [4490316270](https://github.com/SceneWorks/DevWorks/pull/38#discussion_r4490316270) and [4490316284](https://github.com/SceneWorks/DevWorks/pull/38#discussion_r4490316284). Confidence: high for the native macOS evidence below.

Manifest path enumeration, tracked patches and no-index untracked patches use the same qualified actual Git executable and helper-free launch profile as reviewed writes. Reviewed status observations, including recursive submodule observations, use that profile too. Ordinary legacy reads retain their existing behavior. Disabling observed filter names alone cannot constrain a new name configured after the observation.

Worktree manifests explicitly select `--ignore-submodules=dirty`; staged and committed object manifests select `--ignore-submodules=none`. All pin `--submodule=short`. This deliberately overrides a user's `diff.ignoreSubmodules=all`. Literal `none` for a worktree diff invokes child `git status --porcelain=2` inside an initialized submodule, which the launch policy correctly prohibits. `dirty` still includes every changed gitlink HEAD while excluding uncommitted inner contents that the outer `git add --all` cannot stage. The native case preserves those inner contents and verifies the outer committed gitlink equals the reviewed child HEAD.

Native counterexamples and controls:

- On the base, changing `.gitattributes` at either the tracked or untracked binary-patch spawn executed a clean-filter marker during Agent proposal preparation. Both regressions now refuse before a proposal or HEAD change, with no marker. Unconfined controls using the same Git execute the marker.
- A same-size tracked edit and a new filter configuration installed at the status spawn also executed a marker after the patch-only fix, because status still used the unrestricted profile. Reviewed status now refuses safely, including its recursive launch policy; its unconfined control still executes the marker.
- With user diff configuration ignoring all submodules, the base completed an original Human approval → Agent commit that contained a gitlink absent from the captured review manifest. The fixed native flow reviews that gitlink and completes with matching staged/committed manifests. Its uncommitted child content remains outside the outer commit and no `-dirty` content is fabricated in the manifest.

Verification on Darwin 27 arm64, uid 501, Git 2.54.0 (Apple Git-157):

- Full composed engine Git suite: 22 passed, including the five separately owned reviewed-Create cases and existing hook, signer, tree/message and recovery checks.
- Final focused tracked/untracked/gitlink cases: 3 passed; final status race: 1 passed, after a test-only enum cleanup.
- Core Git parsing, qualification and version/path cases: 8 passed.
- Strict core and engine Clippy, all targets, with `-D warnings`: passed.
- Workspace formatting and whitespace checks: passed.

Commands and source hashes are in [manifest-review-results.json](manifest-review-results.json). No inference or external network was used. This pass establishes the local macOS behavior; the portable tests preserve the Git 2.31 floor and Windows distribution selection, but do not establish native Linux or Windows execution. The process policy remains a child-process restriction, not a filesystem, network or in-place-exec sandbox.
