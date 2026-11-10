# DevWorks agent notes

## Pull requests

- One CodeGraph item per PR. Do not merge another item's branch into an item branch; if work depends on an unmerged PR, wait for it to merge or put both items on a feature integration branch.
- Keep each PR at or under 5000 hand-written changed lines. The `PR size` workflow enforces this (`tools/pr-size/check.py`); files marked `linguist-generated` in `.gitattributes` are not counted.
- Merge each slice as soon as its CI passes. Land incomplete capabilities unwired (not reachable from the UI or agent tools) rather than holding a long-lived draft PR.
- Automated review runs for at most 3 rounds. After the third round, file each remaining finding as a CodeGraph bug, link it in the thread reply, and merge on passing CI.

## Review guidelines

- Skip files marked `linguist-generated` in `.gitattributes` (contract schemas and fixtures under `docs/architecture/contracts/`, recorded output under `docs/architecture/validation/`, `@generated` Rust from `tools/core-codegen/generate.py`, `domain_generated.rs`, lockfiles). Review their sources instead: `tools/contracts-eval/domain-spec.json`, `docs/architecture/contracts/family-coverage.json` and the generators.
- Report only defects introduced by this PR's hand-written changes. Do not report pre-existing issues or hardening ideas outside the diff.
