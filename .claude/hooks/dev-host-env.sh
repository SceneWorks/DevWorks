#!/bin/sh
# Claude Code SessionStart hook: put this session's cargo builds and temp files under
# DEV_HOST_ROOT by appending `dev-host env --format sh` to the file Claude Code names in
# CLAUDE_ENV_FILE (its export lines apply to every later Bash command of the session).
# It never fails the session: without dev-host, DEV_HOST_ROOT or CLAUDE_ENV_FILE it prints
# one warning and exits 0, and the session keeps the default build and temp folders.

warn() {
    echo "dev-host: $1; this session keeps the default build and temp folders"
    exit 0
}

command -v dev-host >/dev/null 2>&1 ||
    warn "dev-host is not installed (dev-host install --version X, or cargo build -p dev-host)"
[ -n "$DEV_HOST_ROOT" ] || warn "DEV_HOST_ROOT is not set"
[ -n "$CLAUDE_ENV_FILE" ] || warn "CLAUDE_ENV_FILE is not set, so there is nowhere to export to"

if [ -n "$CLAUDE_PROJECT_DIR" ]; then
    set -- --worktree "$CLAUDE_PROJECT_DIR"
fi
if ! exports=$(dev-host env --format sh "$@" 2>&1); then
    warn "dev-host env failed: $(printf '%s\n' "$exports" | head -n 1)"
fi
printf '%s\n' "$exports" >>"$CLAUDE_ENV_FILE" || warn "cannot write $CLAUDE_ENV_FILE"
echo "dev-host: builds and temp files for this session go under $DEV_HOST_ROOT"
