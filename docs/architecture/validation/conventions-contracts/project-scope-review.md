# Project source scope review closure

Base: `aa3558c90507091fd8b6315e26e2f8da1780c17d`.
Review findings:
[edit source capture](https://github.com/SceneWorks/DevWorks/pull/43#discussion_r4493309655),
[indexed breadcrumbs](https://github.com/SceneWorks/DevWorks/pull/43#discussion_r4493309673).

The actual desktop Host binds a Human context without project lineage. The initial
explicitly scoped convention lookup was correct, but preparation then resolved its
current Markdown/raw, resource and history associations using an implicit scope.
The same loss occurred when breadcrumb resolution reopened its current and captured
owner rows. Implicit scope on that desktop context resolves Workspace, excluding
the legitimate Project owner.

Both source helpers now derive the closed DataScope from the already-authorized
stored owner and pass it through their existing checks. They do not infer scope
from an arbitrary artifact, alter the invocation context, impersonate Host, widen
a generic artifact reader, or remove a current/historical row equality check.
The normal deciding unit, source/version/blob checks, current reach and final
fresh query snapshot remain unchanged.

The new `devworks-desktop --test conventions_scope` target boots the real desktop
Engine, uses its unmodified Backend and asserts that its actual user is Human with
`project_id() == None`. Actual project creation, project upload tickets,
`artifacts.save`, asynchronous import/index workers, commands and queries produce
all data. No direct storage insertion, successful handler stub, HTTP/provider
response, learned inference or OS secret facility is used.

Baseline execution reproduced three NotFound failures: metadata-only current
Markdown/raw edits, exact resource reuse alongside a new body upload, and project
search breadcrumb resolution. The historical-restore case already passed for this
Human through generic artifact visibility; it is recorded as preservation coverage,
not a claimed fourth failing baseline.

Final native execution passed all four cases in 9.81 seconds:

- Metadata-only edits preserve the current Markdown/raw bytes and revision CAS.
- An existing exact binary resource remains reusable and readable after a new
  body revision; its digest and exact downloaded bytes remain intact.
- An old immutable body reference restores through convention history, with all
  three historical revisions still present.
- A genuine project lexical search hit can be followed using its exact published
  generation/chunk SectionPath selector.

Additional assertions reject Workspace-scoped mutation and SectionPath access to
the Project owner, and preserve the owner revision after the refused mutation.
No timeout was raised and no context acquired synthetic project lineage.
The native linker reports its existing large `__eh_frame` warning; tests complete
successfully. Source/schema/DTO/storage formats are unchanged.

Strict Clippy passed for the core library and desktop scope-regression target
(`--no-deps -- -D warnings`, 34.03 seconds). Workspace `cargo fmt --all --check`
and `git diff --check` passed. Only the two reported core source files, the new
standalone native regression target and this evidence note are changed.
