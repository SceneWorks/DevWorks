# DevWorks agent notes

## Pull requests

- **Feature work.** Each CodeGraph feature gets one pushed branch, `feature/<feature-key>` (for example `feature/dw-f16`). Each item of the feature is built on its own local branch and worktree, gets exactly one independent review, has its valid findings fixed, and is merged locally into the feature branch, which is then pushed. Items do not get their own PRs, and nobody waits for CI between items.
- **Feature PRs.** When every item of a feature is integrated and the feature's acceptance passes, open one PR per repository from the feature branch to `main`, naming every item it carries. It needs passing CI. Automated review runs for at most 3 rounds; after the third, file each remaining finding as a CodeGraph bug, link it in the thread reply, and merge on passing CI.
- **Other work** (a standalone item outside a feature, a hotfix): group related CodeGraph items into one PR. Items are related when a reviewer would read them together: the same subsystem or files, the same kind of change, or follow-ups to the same feature. Keep unrelated items in separate PRs, and name every item in the PR body. The same 3-round review limit applies.
- Do not merge one open PR's branch into another; if work depends on an unmerged PR, add it to that PR's branch while it is open, wait for it to merge, or put both on a feature branch.
- Keep each non-feature PR at or under 5000 hand-written changed lines. The `PR size` workflow enforces this (`tools/pr-size/check.py`); files marked `linguist-generated` in `.gitattributes` are not counted, and PRs from `feature/*` branches are not checked.
- Every item still needs its own tests and acceptance report. Land incomplete capabilities unwired (not reachable from the UI or agent tools) rather than holding a long-lived draft PR.

## Review guidelines

- Skip files marked `linguist-generated` in `.gitattributes` (contract schemas and fixtures under `docs/architecture/contracts/`, recorded output under `docs/architecture/validation/`, `@generated` Rust from `tools/core-codegen/generate.py`, `domain_generated.rs`, lockfiles). Review their sources instead: `tools/contracts-eval/domain-spec.json`, `docs/architecture/contracts/family-coverage.json` and the generators.
- Report only defects introduced by this PR's hand-written changes. Do not report pre-existing issues or hardening ideas outside the diff.

<!-- dev-host -->
## Host storage (dev-host)

- **Root and variable.** All machine-local disposable state lives under one root named by `DEV_HOST_ROOT`, on dedicated storage with a `.dev-host-root` marker. Never hardcode a machine path for that state; derive it from `DEV_HOST_ROOT`.
- **Layout under the root:** `locks/`, `cache/<project>`, `weights/<project>` (with its manifest), `build/<project>/<key>/slot-<n>` (cargo build slots), `t/<8-char id>` (temp folders), `campaigns/<project>/<id>/{work,evidence}`, `runner/<name>`, `docker/`.
- **The tool.** `dev-host` lives in this repo at `crates/dev-host` and builds with plain `cargo build -p dev-host`. Install a released version with `dev-host install --version X` (macOS arm64 and Windows x64). `dev-host check` verifies the root; `eval "$(dev-host env --format sh)"` points `CARGO_TARGET_DIR` at a build slot and `TMPDIR` at the root. The Claude Code SessionStart hook (`.claude/hooks/dev-host-env.sh`) runs `dev-host env` for every session; without dev-host or `DEV_HOST_ROOT` it prints one warning and the session keeps the defaults. Find build output with `cargo metadata --format-version 1 --no-deps` (`.target_directory`), never by assuming `<repo>/target`.
- **Guard.** CI runs `dev-host guard --diff <base>` on every PR. Run `dev-host guard --all` and `dev-host guard --docs` locally. Mark a deliberate path (a path inside a container, a rejection fixture, a citation) with `dev-host: allow-path <reason>` on the same line, and make the reason true.
- **Searching.** Never search from `/` or `$HOME` (no `find /`, `find ~` or `find $HOME`): it walks synced and network folders. Name the folders to search.
<!-- /dev-host -->
