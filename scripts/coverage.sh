#!/usr/bin/env bash
# Coverage gates (NFR-8). Each crate is measured on its own test targets, so a
# crate's gate reflects its own tests (not coverage donated by another crate's
# integration suite).
#
#   ocbot-core          >= 90% line coverage
#   opencode-v2-client  >= 90% line coverage
#   ocbot (shell)       >= 70% line coverage
#
# The shell's floor is lower by design: bootstrap/process wiring (run.rs,
# main.rs, the serenity gateway loop, the child-process crash harness) cannot be
# exercised without a live environment and is where the residual misses sit.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

cargo llvm-cov -p ocbot-core --fail-under-lines 90 --summary-only
cargo llvm-cov -p opencode-v2-client --fail-under-lines 90 --summary-only
cargo llvm-cov -p ocbot --fail-under-lines 70 --summary-only

echo "coverage gates: OK"
