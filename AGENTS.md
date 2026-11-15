# DevWorks agent notes

## Pull requests and feature delivery

- For a CodeGraph feature, create a local `feature/<key>-<slug>` branch and push it before task work. Give every story, task and bug its own local branch and worktree. Run focused local checks, obtain one independent subagent review of the exact task diff, fix valid findings, and merge the task locally into the feature branch. Push the feature branch and begin the highest-priority dependency-ready task without waiting for CI. Do not open task or group PRs for feature work.
- When all required feature tasks are integrated and the full feature acceptance passes, open one PR from the feature branch to `main` per affected repository. Run CI and automated review on that PR. Fix CI failures and review findings for at most three automated review rounds. After round three, file each remaining actionable review finding as a CodeGraph bug and link it in the PR thread. Merge when required CI, branch protection and approvals are green.
- Keep the 5000 hand-written-line PR limit for standalone issues and other non-feature PRs. Final feature PRs are exempt because they collect the feature's locally reviewed tasks. Files marked `linguist-generated` in `.gitattributes` remain excluded from the count.

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
