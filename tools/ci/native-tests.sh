#!/usr/bin/env bash
# Complete, disjoint workspace partitions; default cargo test includes doctests.
set -euo pipefail
case "${1:-}" in
  foundation)
    cargo test --workspace --exclude devworks-desktop --exclude devworks-engine --locked
    ;;
  desktop)
    cargo test -p devworks-desktop --locked
    ;;
  engine)
    cargo test -p devworks-engine --locked
    ;;
  *)
    echo 'Expected native test partition: foundation, desktop or engine.' >&2
    exit 2
    ;;
esac
