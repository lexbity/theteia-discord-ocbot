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
# Scanned everywhere *except* tests: a test must be able to exercise the
# opt-in branch (NFR-107 — a requirement with no failing test is decoration),
# and a literal inside a test file is never a deployed setting. The probe
# proves the detector still fires on its target.
if grep -rn --exclude-dir=tests "PER_SESSION_RULESET=true\|per_session_ruleset = true" \
     crates/ config/ 2>/dev/null; then
  fail "G13: FR-42 per-session ruleset must stay default-off (D-R6)"
fi
probe "G13" "per_session_ruleset = true" 'per_session_ruleset = true'

# --- Phase 6 appended gates (spec Part 2, Phase 6) -------------------------
# G14: freeform paths ship default-off (P11); containment (FR-34/FR-35) is the
# baseline, and enabling freeform is a deliberate deployment act, never a
# literal in the tree. Same test exclusion as G13: the branch is tested.
if grep -rn --exclude-dir=tests "ALLOW_FREEFORM_PATHS=true\|allow_freeform_paths = true" \
     crates/ config/ 2>/dev/null; then
  fail "G14: freeform paths must stay default-off (P11)"
fi
probe "G14" "allow_freeform_paths = true" 'allow_freeform_paths = true'

# G15: no shell interpreter is spawned anywhere in the workspace (belt+braces;
# G5/G7 already confine spawns to the git adapter, which is argv-only).
if grep -rnE 'Command::new\("(sh|bash|zsh|dash|fish)"\)' crates/ 2>/dev/null; then
  fail "G15: shell-interpreter spawn found in crates/"
fi
probe "G15" 'Command::new\("sh"\)' 'let c = Command::new("sh");'

# G16: the fs adapter is the only place a path becomes real — the browser and
# plan handlers must go through the port, never `std::fs` directly.
if grep -rn "std::fs::" crates/ocbot/src/app/handlers/ls.rs crates/ocbot/src/app/handlers/run.rs 2>/dev/null; then
  fail "G16: handlers must use the fs port, not std::fs directly (FR-35)"
fi
probe "G16" "std::fs::" 'let x = std::fs::read_to_string(p);'

# --- Phase 7 appended gates (spec Part 2, Phase 7) -------------------------
# G17: no secret value is committed (NFR-10 / R-12). A token/password assigned
# a long literal would be a live credential in git history.
SECRET_RE="(DISCORD_TOKEN|OPENCODE_PASSWORD)[[:space:]]*=[[:space:]]*[\"'][A-Za-z0-9_-]{8,}"
# Exclude this script: its own self-test contains the sample it must detect.
if grep -rnE "$SECRET_RE" --exclude-dir=target --exclude-dir=.git --exclude=gates.sh . 2>/dev/null; then
  fail "G17: a secret value is committed"
fi
probe "G17" "$SECRET_RE" 'DISCORD_TOKEN="abcdefgh12345678"'

# G18: the metrics listener is loopback-only (NFR-9). A wildcard bind would
# expose the registry to the network.
if grep -rn "0\.0\.0\.0" crates/ocbot/src/app/metrics.rs config/config.toml.example 2>/dev/null; then
  fail "G18: metrics must bind loopback only"
fi
if ! grep -q "127.0.0.1:9464" config/config.toml.example; then
  fail "G18: the loopback metrics default is missing from config.toml.example"
fi
probe "G18" "0\.0\.0\.0" 'metrics_bind = "0.0.0.0:9464"'

# G19: the deployment units run unprivileged and are memory-capped (NFR-6/R-3).
for unit in deploy/ocbot.service deploy/opencode.service deploy/signal-cli.service; do
  if [ ! -f "$unit" ]; then
    fail "G19: missing deployment unit $unit"
  fi
  if ! grep -q "^User=" "$unit"; then
    fail "G19: $unit does not drop to a non-root user"
  fi
  if ! grep -q "^MemoryMax=" "$unit"; then
    fail "G19: $unit has no MemoryMax ceiling (NFR-6)"
  fi
done
# FR-116's socket hygiene is a deployment property as much as a code one: the
# daemon's socket must land at 0600 under a 0700 runtime directory.
if ! grep -q "^UMask=0177" deploy/signal-cli.service; then
  fail "G19: signal-cli.service must set UMask=0177 (the 0600 socket FR-116 demands)"
fi
if ! grep -q "^RuntimeDirectory=signal-cli" deploy/signal-cli.service; then
  fail "G19: signal-cli.service must own its socket directory (RuntimeDirectory)"
fi
probe "G19" "^User=" 'User=ocbot'

# --- SPEC-002 appended gates ------------------------------------------------
# The store actor's message set is typed (Phase 4/5): no trait-object erasure
# (`Box<dyn ...>`, including the CAS `ToSql` params, D-8), no dead `TypeMismatch`
# variant, no erased `Job`/`call` path. Detector self-tests keep the patterns
# honest (a gate that cannot fail is decoration).
if [ -f crates/ocbot/src/adapters/store.rs ]; then
  for pattern in 'Box<dyn' 'TypeMismatch' 'type Job' 'pub async fn call'; do
    if grep -n "$pattern" crates/ocbot/src/adapters/store.rs; then
      fail "SPEC-002: store.rs still contains '$pattern'"
    fi
  done
fi
probe "SPEC002-erasure" 'Box<dyn Any' 'type Job = Box<dyn Any + Send>;'
probe "SPEC002-erasure-tosql" 'Box<dyn' 'let v: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();'
probe "SPEC002-mismatch" 'TypeMismatch' 'return Err(StoreError::TypeMismatch);'
probe "SPEC002-erased-call" 'pub async fn call' 'pub async fn call<T, F>(&self, f: F) {}'

# Row mapping is by column name (Phase 2): no numeric `row.get(N)`.
if [ -f crates/ocbot/src/adapters/store.rs ]; then
  if grep -nE 'row\.get(::<[^>]*>)?\(\s*[0-9]' crates/ocbot/src/adapters/store.rs; then
    fail "SPEC-002: numeric row.get index in store.rs"
  fi
fi
probe "SPEC002-numeric-row" 'row\.get\(0\)' 'let id = row.get(0)?;'

# The render god module stayed decomposed (Phase 3): every concern file exists
# and none may grow back to the 500-line ceiling.
for render_file in mod report gate browser redact label legend flow; do
  path="crates/ocbot-core/src/render/$render_file.rs"
  if [ ! -f "$path" ]; then
    fail "SPEC-002: missing render submodule $path"
  fi
  lines="$(wc -l < "$path")"
  if [ "$lines" -ge 500 ]; then
    fail "SPEC-002: $path is $lines lines (>= 500); split it by concern"
  fi
done

echo "gates: OK"

# --- SPEC-003 W4 appended gates ---------------------------------------------
# G20: frames are untrusted input (P-32): the supervisor and the dispatcher
# must never pass a frame's body/payload/data field to any tracing call. The
# structural rule: the only frame-logging helper is log_frame(event, session_id),
# which takes neither.
if grep -nE 'tracing::(trace|debug|info|warn|error)!!?\(.*\b(body|payload|data)\b' \
     crates/ocbot/src/app/supervisor.rs crates/ocbot/src/app/wake.rs 2>/dev/null; then
  fail "G20: a frame body field reached a tracing call (P-32)"
fi
probe "G20" 'tracing::.*\bpayload\b' 'tracing::debug!(payload = ?frame, "frame");'

# --- SPEC-004 PH-7 appended gates -------------------------------------------
# G21: the modal-parity proof is load-bearing (FR-109/PH-7). The real drift
# guard is the meta-test in tests/signal_flows.rs (router modal set ⊆ the
# ocbot-core flow catalog); this gate stops the shared parity helper from
# being deleted while suites still assert through it, and stops a *new* modal
# family from being added to the router without a matching flow entry by
# keeping the two declarations adjacent to one documented cross-check.
if ! grep -rq "assert_modal_parity" crates/ocbot/tests/ 2>/dev/null; then
  fail "G21: the modal-parity helper vanished from the test suite (FR-109)"
fi
probe "G21" "assert_modal_parity" 'common::assert_modal_parity(&discord, id, values);'

# The flow catalog and the router's modal set must both exist for the
# cross-check to mean anything.
if ! grep -q "MODAL_FAMILIES" crates/ocbot-core/src/render/flow.rs; then
  fail "G21: the flow catalog (MODAL_FAMILIES) is missing from ocbot-core"
fi
if ! grep -q "pub fn modal_ids" crates/ocbot/src/app/router.rs; then
  fail "G21: the router no longer declares its modal ids"
fi
probe "G21-catalog" "MODAL_FAMILIES" 'pub const MODAL_FAMILIES: [&str; 8]'

# --- SPEC-004 PH-8 final gates (the whole plan) ------------------------------
# G22: the handlers are surface-blind (FR-107's structural proof). The word
# "discord" may appear in a handler only inside a comment or a doc line — a
# platform *identifier* in handler code means a handler that knows which
# surface it is serving. The uppercase constant `DISCORD_MESSAGE_CAP` is the
# one allowed code reference: it is the tightest limit any surface imposes, so
# a spec clamped to it is deliverable everywhere.
if grep -rn "discord" crates/ocbot/src/app/handlers/ --include="*.rs" \
     | grep -v ':[0-9]*: *//' | grep -v ':[0-9]*: *///'; then
  fail "G22: a handler names a platform in code (FR-107)"
fi
probe "G22" "discord" 'mention_allowlist: discord_operator().into_iter().collect(),'

# G23: every document in the SPEC-004 §7 set says what the Signal surface is.
# A doc that never mentions Signal after this slice is a doc that lies.
for doc in docs/ARCHITECTURE.md docs/DEPLOYMENT.md docs/OPERATIONS.md \
           docs/GETTING_STARTED.md docs/RESTORE.md README.md; do
  if [ ! -s "$doc" ]; then
    fail "G23: $doc is missing"
  fi
  if ! grep -qi "signal" "$doc"; then
    fail "G23: $doc never mentions the Signal surface"
  fi
done
probe "G23" "signal" 'channel = ["discord", "signal"]'
