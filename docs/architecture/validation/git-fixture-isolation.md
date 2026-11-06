# Native Git fixture isolation

DW-123 covers the authentication boundary of native Git tests and diagnostic probes.

On 2026-11-06, a standalone diagnostic set a temporary HOME/XDG directory but inherited Apple Git's supplemental configuration. Git trace showed `git credential-osxkeychain store` for a fake credential named `fixture`. A completed Keychain write is unconfirmed. The probe was stopped and its executable body replaced with a refusal. No real Keychain inspection, cleanup, deletion or approval was performed.

The committed fixture now clears inherited process variables, disables system/supplemental configuration, explicitly selects its fixture global file, resets credential helper inheritance, and invokes a fixed recording fake helper. Read-only effective-config assertions run before private network execution and reject unexpected credential, HTTP, askpass, SSH or signing configuration. Setup Git, certificate generation and the loopback server use closed environments too. The production authentication implementation is unchanged.

Source checkpoint: `0a8a2873` → `4d3fdaf9` → `ed3faaf6` → `a25dd134`; integrated fixture bytes are identical on the delivery branch.

Validation on macOS:

- Engine Git integration target typecheck passed (25.03 s).
- Exact HTTPS, SSH and local security cases passed (12.92 s, 8.16 s, 10.14 s), including config checks and fake-helper recordings.
- The existing concurrent Git integration suite passed all 25 cases (78.16 s).
- Strict affected-target Clippy passed (26.61 s).
- No unexpected helper, config assertion or OS prompt occurred during these isolated runs.

Confidence is high in the inspected fixture isolation. The earlier macOS CI failure did not reproduce, so its original cause remains unknown. Sanitized helper/server diagnostics are retained to make a future failure actionable. Exact-head automated review and CI remain delivery gates.
