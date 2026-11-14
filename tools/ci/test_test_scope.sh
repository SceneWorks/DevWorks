#!/usr/bin/env bash
# Tests for tools/ci/test-scope.sh: each class of changed path maps to the right scope.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/test-scope.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

# expect <event> <expected scope> <expected foundation> <expected engine> [changed paths...]
expect() {
  local event=$1 want_scope=$2 want_foundation=$3 want_engine=$4
  shift 4
  printf '%s\n' "$@" | sed '/^$/d' >"$work/files"
  : >"$work/out"
  EVENT=$event CHANGED_FILES_FILE="$work/files" GITHUB_OUTPUT="$work/out" bash "$script" >/dev/null
  local got
  got="$(grep -E '^(scope|foundation|engine)=' "$work/out" | tr '\n' ' ')"
  local want="scope=$want_scope foundation=$want_foundation engine=$want_engine "
  if [[ "$got" != "$want" ]]; then
    echo "FAIL ($event: $*): got '$got', want '$want'"
    failures=$((failures + 1))
  fi
}

expect pull_request desktop false false crates/devworks-desktop/src/lib.rs crates/devworks-desktop/tests/help.rs
expect pull_request engine false true crates/devworks-engine/src/runtime.rs
expect pull_request engine false true crates/devworks-engine/src/runtime.rs crates/devworks-desktop/src/lib.rs
expect pull_request adapters true true crates/devworks-adapters/src/lib.rs crates/devworks-desktop/src/lib.rs
expect pull_request all true true crates/devworks-core/src/lib.rs crates/devworks-desktop/src/lib.rs
expect pull_request all true true crates/devworks-extension-protocol/src/lib.rs
expect pull_request all true true crates/dev-host/src/main.rs
expect pull_request all true true docs/help/topics/containers.md
expect pull_request all true true tools/ci/native-tests.sh
expect pull_request all true true Cargo.lock
expect pull_request all true true .github/workflows/ci.yml
expect pull_request all true true crates/devworks-desktopish/src/lib.rs
expect pull_request all true true # empty diff
expect push all true true crates/devworks-desktop/src/lib.rs
expect workflow_dispatch all true true crates/devworks-desktop/src/lib.rs

# A diff that fails runs everything.
: >"$work/out"
(cd "$work" && git init -q && EVENT=pull_request GITHUB_OUTPUT="$work/out" bash "$script" >/dev/null 2>&1)
if ! grep -qx 'scope=all' "$work/out"; then
  echo "FAIL (failed diff): got '$(tr '\n' ' ' <"$work/out")', want scope=all"
  failures=$((failures + 1))
fi

# The package arguments match the scope.
printf 'crates/devworks-engine/src/a.rs\n' >"$work/files"
: >"$work/out"
EVENT=pull_request CHANGED_FILES_FILE="$work/files" GITHUB_OUTPUT="$work/out" bash "$script" >/dev/null
if ! grep -qx 'packages=-p devworks-engine -p devworks-desktop' "$work/out"; then
  echo "FAIL (engine packages): $(grep packages "$work/out")"
  failures=$((failures + 1))
fi

if ((failures)); then
  echo "$failures test-scope case(s) failed"
  exit 1
fi
echo "test-scope: all cases passed"
