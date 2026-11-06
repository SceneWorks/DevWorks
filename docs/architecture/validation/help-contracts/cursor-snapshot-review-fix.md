# Passage cursor recovery on a changed Help snapshot

Review: [PR41 passage cursor finding](https://github.com/SceneWorks/DevWorks/pull/41#discussion_r4492295199). Base: `5e3033f9`.

Adopting a different Help revision, package-registry revision or app version now resets passage cursor history to the first passage. The selected topic and exact version stay selected. In-flight reads from the old snapshot are fenced by the read generation, so a delayed reply cannot restore the old snapshot or its later passage. Search input and conversation drafts are untouched.

The native regression uses the real runtime, signed package installation, actual Help publication and source reads. It reads a later passage of package version 1.0.0, installs an unrelated guide, and observes the new catalog snapshot. It also holds an actual later-passage reply across a second unrelated package update, proves that the fresh first-page read completes before that reply is released, and verifies the old reply is ignored. The exact inspected version and both unsubmitted Help search and chat draft are retained. No successful stand-in backend, inference or remote network is used.

Evidence on macOS:

- Before the fix, the native revision-change regression failed with `CursorExpired` in 18.38 seconds.
- After the fix, the expanded native regression passed in 23.15 seconds, including the delayed old reply.
- Existing installed-package Unicode passage and stale-version controls: 1/1 passed, 18.33 seconds.
- Existing stale-generation refusal, exact selection and unsaved search: 1/1 passed, 21.46 seconds.
- Strict desktop all-target Clippy, with dependencies excluded: passed, 30.17 seconds.
- Workspace formatting and `git diff --check`: passed.

Logs: `/tmp/devworks-pr41-help-cursor-red.log`, `/tmp/devworks-pr41-help-cursor-green.log`, `/tmp/devworks-pr41-help-package-regression.log`, `/tmp/devworks-pr41-help-stale-regression.log`, `/tmp/devworks-pr41-help-cursor-clippy.log`, `/tmp/devworks-pr41-help-cursor-fmt.log`.

Confidence: high for the bounded UI correction. Cross-platform CI and review are owned by the publishing branch.
