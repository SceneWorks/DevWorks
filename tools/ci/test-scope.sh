#!/usr/bin/env bash
# Choose which native test partitions a pull request needs from the files it changes.
#
# The workspace crates form one chain, core -> adapters -> engine -> desktop, and each
# crate's tests read only its own files, docs/ and tools/. So a PR that changes only
# files under crates/devworks-desktop/ needs only desktop tests, one that changes
# engine (and maybe desktop) needs engine and desktop, and one that changes adapters
# needs adapters, engine and desktop. Any other path, an empty or failed diff, and any
# event other than pull_request run everything.
#
# Inputs (environment): EVENT (github.event_name). CHANGED_FILES_FILE, when set, names
# a file listing the changed paths (for tests); otherwise the script diffs HEAD^1..HEAD,
# which on a pull_request checkout is the PR's merge commit against its base.
# Output (GITHUB_OUTPUT, stdout locally):
#   scope=all|adapters|engine|desktop
#   foundation=true|false   run the foundation partition (core, protocol, adapters, dev-host)
#   engine=true|false       run the engine partition
#   packages=...            cargo package arguments for a single-job run
set -uo pipefail

output=${GITHUB_OUTPUT:-/dev/stdout}

changed_files() {
  if [[ -n "${CHANGED_FILES_FILE:-}" ]]; then
    cat "$CHANGED_FILES_FILE"
  else
    git diff --name-only --no-renames HEAD^1 HEAD
  fi
}

scope=all
if [[ "${EVENT:-}" != pull_request ]]; then
  echo "Event ${EVENT:-unknown}: every partition runs."
elif ! files=$(changed_files); then
  echo "::warning::Could not list the changed files; every partition runs."
elif [[ -z "$files" ]]; then
  echo "No changed files found; every partition runs."
else
  # 3 = desktop only, 2 = engine and later, 1 = adapters and later, 0 = everything.
  level=3
  while IFS= read -r path; do
    case "$path" in
      crates/devworks-desktop/*) l=3 ;;
      crates/devworks-engine/*) l=2 ;;
      crates/devworks-adapters/*) l=1 ;;
      *) l=0 ;;
    esac
    if ((l < level)); then level=$l; fi
    if ((l == 0)); then echo "Runs everything because of: $path"; fi
  done <<<"$files"
  case $level in
    3) scope=desktop ;;
    2) scope=engine ;;
    1) scope=adapters ;;
    *) scope=all ;;
  esac
fi

case $scope in
  all) foundation=true engine=true packages="--workspace" ;;
  adapters) foundation=true engine=true packages="-p devworks-adapters -p devworks-engine -p devworks-desktop" ;;
  engine) foundation=false engine=true packages="-p devworks-engine -p devworks-desktop" ;;
  desktop) foundation=false engine=false packages="-p devworks-desktop" ;;
esac

echo "Test scope: $scope"
{
  echo "scope=$scope"
  echo "foundation=$foundation"
  echo "engine=$engine"
  echo "packages=$packages"
} >>"$output"
