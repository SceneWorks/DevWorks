# Storage evaluation evidence

Four active candidates: OverGraph, Grafeo, HelixDB and SQLite. Kuzu is excluded.

The measured manifest is `measured/20261101T174520146593Z/manifest.json`. It records 36 completed native helper cases, nine with structured correctness failures (all OverGraph). An exit code of zero does not override `case_errors`. Pilots and their errors are preserved separately. Recovery has eight owned-process SIGKILL probes; interrupted-import reopen is not generation-resume acceptance.

The full review export supplies `devworks-storage-evidence.zip`, containing these records, native evaluator source/locks/README and the exact fixture as `fixture/source.json.gz`. Build caches, generated databases and executable binaries are excluded. The fixture SHA-256 before compression is `84588880e378beb11ed839c91ecbce5d7e33b9002b60aec6fd4c600447a56034`. It is a 76,550,608-byte local numerical/source-metadata fixture, not model output. It is kept in the review export rather than added as a large binary to this source tree. Reproduction commands are in `tools/storage-eval/README.md` at the repository root. Original config/command paths record the execution host; use the runner arguments to create fresh paths on another machine.

Raw reports retain query samples, per-run summaries, recall, scope checks, errors, import/readiness, mutations, lifecycle times, point-in-time RSS and logical storage sizes. Runner manifests retain executable hashes/sizes, maximum whole-process RSS, input/lockfile digests and compiler/profile. `logs/environment-final.json` records host, OS linkage and native source digests; later README/report additions do not change compiled Rust inputs. `logs/build-004.stderr` records the measured build, and `logs/test-final.*` records the three shared-oracle tests.

These artifacts demonstrate only the tested local operations. They do not certify complete CodeGraph functionality, real inference, app packaging, larger-scale operation, migrations/backups or power-loss recovery.
