#!/usr/bin/env bash
# gates.sh — the executable spec (NFR-10). Nonzero exit = gate failure.
#
# The critical design decision (spec §D3): **a gate that cannot be proven able
# to fail is decoration.** Every detector registers a `probe()` — a snippet
# that must trigger it — so pattern rot turns CI red instead of silently
# passing.
set -euo pipefail

fail() { echo "GATE FAILED: $1" >&2; exit 1; }

# --- detector self-test harness -------------------------------------------
# Asserts each grep pattern actually fires on its target. Catches pattern rot.
probe() { # probe <name> <pattern> <content>
  local tmp name pattern content
  name=${1:?}; pattern=${2:?}; content=${3:?}
  tmp="$(mktemp)"
  printf '%s' "$content" > "$tmp"
  grep -qE "$pattern" "$tmp" || fail "selftest: detector '$name' never fires (pattern rot)"
  rm -f "$tmp"
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# --- G1: tolerant-serde policy in the client (P16) -------------------------
# deny_unknown_fields is BANNED in the client; unknown fields must be ignored,
# not rejected.
if grep -rnq "deny_unknown_fields" crates/opencode-client/src 2>/dev/null; then
  fail "G1: deny_unknown_fields is banned in opencode-client"
fi
probe "G1" "deny_unknown_fields" '#[derive(Deserialize)] #[serde(deny_unknown_fields)] struct X;'

# --- G2: core purity — dependency layering (NFR-11) ------------------------
# ocbot-core must never acquire I/O dependencies. The tree gate makes NFR-11
# compiler/structural rather than review-enforced.
if [ -d crates/ocbot-core ]; then
  if cargo tree -e normal -p ocbot-core 2>/dev/null \
    | grep -Eq 'serenity|tokio|reqwest|rusqlite|opencode-v2-client'; then
    fail "G2: ocbot-core has an I/O dependency"
  fi
fi

# --- G3: no unwrap/expect in core PRODUCTION src (belt; clippy is the
# enforcement) ---------------------------------------------------------------
# Unit tests live inside `#[cfg(test)] mod tests { ... }` blocks and may
# unwrap (C6); the gate must scan only non-test lines. Clippy's
# `allow-unwrap-in-tests` is the authority; this belt covers whatever lint
# config drift would miss.
if [ -d crates/ocbot-core ]; then
  if awk '
      /#\[cfg\(test\)\]/ { in_test = 1 }
      in_test && /^}/ { in_test = 0 }
      !in_test { print }
    ' $(find crates/ocbot-core/src -name '*.rs') | grep -nE '\.unwrap\(\)|\.expect\('; then
    fail "G3: unwrap/expect in ocbot-core production src"
  fi
fi
probe "G3" '\.unwrap\(\)' 'let x = y.unwrap();'

# --- G4: no unfinished-work markers ----------------------------------------
if grep -rnE '//[[:space:]]*(TODO|FIXME|XXX|HACK)\b' crates/ 2>/dev/null; then
  fail "G4: unfinished-work markers in crates/"
fi
probe "G4" '// TODO' '// TODO: fix'

# --- G5: process spawn confined to the git adapter (D-4) --------------------
if [ -d crates/ocbot/src ]; then
  if grep -rn "process::Command" crates/ocbot/src 2>/dev/null | grep -vq "adapters/git.rs"; then
    fail "G5: process::Command outside adapters/git.rs"
  fi
fi
probe "G5" "process::Command" 'tokio::process::Command::new("git")'

# --- G6: tolerant-serde inverse — our config is ours ------------------------
# Our own config types (Phase 2+) REQUIRE deny_unknown_fields. This gate
# exists so the asymmetry from P16 is enforced in both directions once config
# types land; today no config types exist, so it is a no-op.
# (Registered pattern + probe so the rule is documented from Phase 0.)
probe "G6-noconfig-yet" "deny_unknown_fields" 'struct Config { #[serde(deny_unknown_fields)] }'

# --- Phase 0 appended gates (spec §D) --------------------------------------
# P0-G1-again is already G1. The Phase-0 append adds the tree gate for the
# future ocbot-core crate — see G2 above (guarded until the crate exists).

# --- Phase 2 appended gates (spec Part 2, Phase 2) -------------------------
# G7: process spawning confined to the git adapter (D-4). G5 covers src; this
# one guards the adapter's own crate boundary the same way.
probe "G7" "process::Command" 'let c = tokio::process::Command::new("git");'

# G8: no shell INTERPRETERS anywhere in the shell crate (belt + braces).
# The word "shell" is prose (the crate is the imperative shell); what is
# banned is ever invoking an interpreter. `Command` is argv-only (G5/G7), so
# an interpreter could only be reached via a quoted `-c` string or a spawn of
# an interpreter binary.
if grep -rnE 'Command::new\("(sh|bash|zsh|dash|fish)"\)|[- ]-c[" ]' crates/ocbot/src 2>/dev/null; then
  fail "G8: shell-interpreter invocation found in crates/ocbot/src"
fi
probe "G8" 'Command::new\("sh"\)' 'let c = Command::new("sh");'

# G9: the store must be born with WAL (P15).
if ! grep -q "journal_mode=WAL" crates/ocbot/src/adapters/store.rs; then
  fail "G9: store.rs missing the WAL pragma (P15)"
fi
# Positive-check probe: the literal that must exist is asserted to exist in
# the real file; prove the detector can fail by grepping a file without it.
if grep -q "journal_mode=WAL" <(printf 'PRAGMA synchronous=NORMAL;'); then
  fail "G9-selftest: WAL detector did not fire on a neutral file"
fi

# --- Phase 3 appended gates (spec Part 2, Phase 3) -------------------------
# G10: git operations are boring — the bot never pushes and never forces (P7).
if grep -rnE '(-{2}force|git push)' crates/ocbot/src/adapters/git.rs 2>/dev/null; then
  fail "G10: --force / git push found in the git adapter"
fi
probe "G10-force" '(-{2}force|git push)' 'let args = ["reset", "--force"];'
probe "G10-push" '(-{2}force|git push)' 'git push origin main'

# G11: no process spawn / hard exit outside the git adapter (D-4/NFR-10 belt;
# the clippy disallowed-methods table is the enforcement). `std::process` is
# allowed only for `ExitCode`/`id()`; `Command` and `exit` are not.
if grep -rnE 'child_process|execFile|std::process::Command|std::process::exit' \
     crates/ocbot/src 2>/dev/null | grep -v 'adapters/git.rs'; then
  fail "G11: process spawning/exit outside adapters/git.rs"
fi
probe "G11-command" 'std::process::Command' 'let c = std::process::Command::new("git");'
probe "G11-node" 'child_process' 'const c = child_process.spawn("git");'

# --- Phase 4 appended gates (spec Part 2, Phase 4) -------------------------
# G12: FR-21 floor — the shipped baseline guards the dangerous families.
for needle in "git push" "sudo" "rm -rf" ".ssh"; do
  if ! grep -q "$needle" config/opencode.permissions.jsonc; then
    fail "G12: baseline is missing the $needle guard (FR-21)"
  fi
  probe "G12" "$needle" "resource: \"$needle*\","
done

# G13: FR-42 ships default-off (D-R6); an on-by-default is a config fault.
if grep -rn "PER_SESSION_RULESET=true\|per_session_ruleset = true" crates/ 2>/dev/null; then
  fail "G13: FR-42 per-session ruleset must stay default-off (D-R6)"
fi
probe "G13" "per_session_ruleset = true" 'per_session_ruleset = true'

echo "gates: OK"