# Native Conventions manager validation

This checkpoint uses the registered `ConventionService`, SQLite storage, immutable artifacts,
actual import/index consumers and the native desktop gateway. Root assembly and publication
remain separate. No successful convention records or status results are invented by a backend.

The native manager retains separate Workspace/Project state, captured revision/source identity,
complete source buffers and independent new/edit/move/resource drafts. A refresh does not adopt
another CAS anchor. An explicit source rebase does not resend unedited parent/order fields.
Only the Move control changes organization. Source editing is windowed for layout, while saving
and export retain the verified complete document.

| Surface | Actual native evidence |
|---|---|
| Create/import/list/read | Authorized file tickets → actual artifact save → portable import; literal empty slug; omitted initial order is server-owned, explicit zero remains zero |
| Edit/version/provenance | Markdown/raw NUL and CRLF preserved; inspect prior version and return; set/remove raw; concurrent edit refuses, retains full opening source and requires explicit rebase |
| Complete source bounds | Actual 16,777,215-byte Markdown import, full page collection/digest verification before editing, save and byte-equal full export |
| Applicability/lifecycle/delete | Literal comma-containing label; Active↔Retired; soft deletion and retained resource metadata |
| Move/hierarchy | Whole parent/child subtree moves to a real project; literal slash/NUL hierarchy components resolve through the shared read selector; subsequent source pages retain the served ID/revision; independent opening draft retained |
| Resources | Actual binary pages/non-UTF-8/NUL bytes, original 200-character descriptive media type, explicit empty media, metadata edit, chosen byte replacement/save, exact download, retired/deleted metadata |
| Virtual resources | Distinct catalog and exact-root document modes; dropped manifests stay in the Conventions owner |
| Export | Portable manifest and exact full Markdown/raw downloads through owning passage APIs |
| Index/status/cancel | Real persisted embedding setup, actual accounted recorded provider requests; index status/progress and cancellation; after-send Unknown retained; later explicit scope index and reopen preserve both operations |
| Search | Actual persisted qualified space → hybrid ranked chunk → authoritative section-path selector; lost query acknowledgment/reopen repeats exactly the original frame with no second provider request |
| Search cancellation | Actual cancellation token reaches in-flight native transport; original frame retained and confirmation reads actual unsettled preparation metadata, never guesses custody from reason text |
| Offline/context/setup | Local draft preservation, source window byte preservation, actual contextual Help and navigation-only embedding setup |

All native fixtures use `tests/common::config`: `SessionSecrets`, isolated temporary data roots,
and either `NoNetwork` or the recorded in-process embedding transport. Recorded vectors are
fixed observations, not computed from input. No learned inference, sockets, Git/auth probes,
real OS secret-store reads or Keychain operations are performed.

`tests/conventions/runtime.rs` contains the production-runtime fixtures. `QueryWire` only loses
a real query acknowledgment; all successful results remain actual runtime results. A controlled
recorded stream blocks body reads so cancellation is an observed interaction, not a timing race.
The independent close handle and cleanup guard release that fixture on failure.

The original same-query confirmation after clearing the selected embedding space was a
native RED (fresh lexical result instead of `CursorExpired`). With the owning custody validation
(`a9ebfde4`, `7ca4610b`) it is GREEN: the exact request is refused before new admission, the UI
retains its actual last-reported Unknown preparation, and only an explicit new search creates a
new intent. The earlier custody remains inspectable after reopening. The literal hierarchy UI
uses the qualified shared selector (`1ac4f14f`), never client splitting or normalization.

Final qualification on the combined source:

- `cargo test -p devworks-desktop --test conventions -- --nocapture`: **16 passed**, no ignored,
  skipped or filtered cases, default runner/concurrency, **42.65s**.
- `cargo clippy -p devworks-desktop --all-targets --no-deps -- -D warnings`: **passed, 35.09s**.
- Desktop formatting and `git diff --check`: passed.

The complete native suite includes the actual maximum legal source fixture, which independently
passed in 23.36s, not just an injected editor buffer. Recorded qualification/index/query requests
use the persisted DW119 setup; no host-space injection is substituted. After-send cancellation
retains Unknown rather than claiming cancellation proves absence of financial custody. No test
timeouts, assertions or concurrency settings were weakened. Local macOS headless native results
are not cross-platform CI or tracker acceptance claims; root delivery owns those gates.
