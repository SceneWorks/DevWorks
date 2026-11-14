# DevWorks agent notes

## Pull requests

- One CodeGraph item per PR. Do not merge another item's branch into an item branch; if work depends on an unmerged PR, wait for it to merge or put both items on a feature integration branch.
- Keep each PR at or under 5000 hand-written changed lines. The `PR size` workflow enforces this (`tools/pr-size/check.py`); files marked `linguist-generated` in `.gitattributes` are not counted.
- Merge each slice as soon as its CI passes. Land incomplete capabilities unwired (not reachable from the UI or agent tools) rather than holding a long-lived draft PR.
- Automated review runs for at most 3 rounds. After the third round, file each remaining finding as a CodeGraph bug, link it in the thread reply, and merge on passing CI.

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
