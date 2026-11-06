# DW-91: native Settings credential entry

Settings now offers masked entry, replacement and removal for every editable
`secretRef` setting. Submission clears the input and its undo history. Raw input
is transient, redacted in Debug, excluded from SettingsMemory, and moved into the
existing worker-side `stage_setting_secret` route. Only the resulting opaque
reference reaches the normal `settings.save_revision` command.

The selected setting is saved against the revision captured before native intake.
Other drafts remain local. Refusal or an uncertain outcome retains the reference
and the exact prepared command for the existing durable client recovery path;
retry does not require storing the raw secret again. A failed native store call
shows a visible limitation and leaves the effective setting unchanged.

Removal clears the current setting. Retained settings revisions may still own
the prior handle for rollback; DW-88 separately owns reference-aware collection.

## Native qualification, 2026-11-06

- `cargo test --locked -p devworks-desktop --test settings_secrets`: **4/4 passed,
  12.85 s** with the shared two-case runtime budget. Real Settings gestures enter,
  replace and remove the Brave credential;
  the actual Brave adapter resolves it through the native gateway/SecretStore and
  sends the expected header into a recorded checked-address transport. Removal
  prevents another request. The password editor stays empty after submission and
  persisted desktop state contains neither submitted value.
- A denied in-memory SecretStore leaves the setting empty and displays the
  refusal. A controlled store intake permits a real concurrent settings commit:
  the captured revision conflicts, both drafts survive, and explicit retry saves
  them with exactly one native secret-store write.
- Review regression: replacement intake previously left Reset enabled and could
  overwrite that later draft. Its real controlled-intake test reproduced RED in
  6.11 s, then passed in 6.32 s. The selected key's reset/discard and global discard
  wait only during native intake; other edits remain available. After intake,
  normal editing resumes and explicit reset/save removes the reference.
- The existing headless categorized/searchable Settings save, validation, draft
  and conflict regression passed unchanged: **1/1, 6.52 s**.
- Desktop all-target Clippy with `--no-deps -- -D warnings`: passed after the
  affected target was warm.

The initial conflict fixture focused a stale input position after injecting a
draft banner. Rendering the changed layout before focusing the password field
corrected the driver; an enabled-save assertion now establishes that premise.
The original operation bounds remain. Test cleanup releases the controlled store
even during unwinding.

Tests use SessionSecrets or controlled in-memory substitutes, temporary native
SQLite/runtime composition, and recorded HTTP data. They do not contact a provider,
run a learned model, or access the operating-system secret facility. The native
build succeeded with an Apple linker compact-unwind size warning; no build or lint
gate was disabled. Hosted CI, automated review, merge and acceptance remain required.
