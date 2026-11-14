#!/usr/bin/env bash
# Tests that read the source of a crate later in the chain core -> adapters -> engine -> desktop.
# The test scope (tools/ci/test-scope.sh) skips the foundation partition for a PR that changes
# only engine or desktop, but a change to the crate these tests read can break them, so the
# Ubuntu desktop job runs them whenever the foundation partition is skipped.
# tools/ci/test_test_scope.sh fails when a file reads a later crate without being listed here.
#
# reads: crates/devworks-adapters/src/storage/sqlite/tests/projects.rs
set -euo pipefail
cargo test -p devworks-adapters --lib --locked \
  storage::sqlite::tests::projects::purge_record_consumers_in_core_and_desktop_name_no_storage_table \
  -- --exact
