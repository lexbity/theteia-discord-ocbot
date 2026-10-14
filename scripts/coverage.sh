#!/usr/bin/env bash
# Coverage gates (NFR-8). Two layers:
#
#   1. a workspace floor (SPEC-004: the whole tree must stay above 85 %)
#   2. per-crate gates measured on the crate's own test targets, so a crate's
#      floor reflects its own tests (not coverage donated by another crate's
#      integration suite)
#
#   ocbot-core          >= 90% line coverage
#   opencode-v2-client  >= 90% line coverage
#   ocbot (shell)       >= 80% line coverage
#   workspace           >= 85% line coverage
#
# The shell's floor is lower by design: bootstrap/process wiring (run.rs,
# main.rs, the serenity gateway loop, the debug crash checkpoints) cannot be
# exercised without a live environment and is where the residual misses sit.
# The workspace floor is what keeps those dark corners from growing: any crate
# that regresses it fails the gate even if its own floor is untouched.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

cargo llvm-cov --workspace --fail-under-lines 85 --summary-only
cargo llvm-cov -p ocbot-core --fail-under-lines 90 --summary-only
cargo llvm-cov -p opencode-v2-client --fail-under-lines 90 --summary-only
cargo llvm-cov -p ocbot --fail-under-lines 80 --summary-only

echo "coverage gates: OK"
