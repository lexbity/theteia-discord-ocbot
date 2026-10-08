#!/usr/bin/env bash
# NFR-6 measurement: start the release binary against a dead opencode with a
# throwaway DB, let it settle, then read its steady-state RSS.
#
# Usage: scripts/measure-rss.sh [path-to-ocbot-binary]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-$ROOT/target/release/ocbot}"
SETTLE_SECS="${OCBOT_RSS_SETTLE_SECS:-3}"

if [ ! -x "$BIN" ]; then
  echo "measure-rss: $BIN not found; run: cargo build --release -p ocbot" >&2
  exit 1
fi

WORK="$(mktemp -d)"
PID=""
cleanup() {
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

cat >"$WORK/config.toml" <<EOF
opencode_url = "http://127.0.0.1:9"
db_path = "$WORK/ocbot.db"
templates_dir = "$ROOT/templates"
ops_channel = 1
allowed_user_id = 42
metrics_bind = "127.0.0.1:19464"
permissions_file = "$ROOT/config/opencode.permissions.jsonc"
browse_roots = ["$WORK"]
inbox_dir = "$WORK/inbox"
EOF

DISCORD_TOKEN=dummy-token \
OPENCODE_PASSWORD=dummy-password \
OCBOT_CONFIG="$WORK/config.toml" \
  "$BIN" >"$WORK/stdout" 2>"$WORK/stderr" &
PID=$!

sleep "$SETTLE_SECS"

if ! kill -0 "$PID" 2>/dev/null; then
  echo "measure-rss: binary exited early" >&2
  cat "$WORK/stderr" >&2
  exit 1
fi

RSS_KB="$(awk '/^VmRSS:/ {print $2}' "/proc/$PID/status")"
echo "ocbot release RSS: ${RSS_KB} kB (VmRSS, ${SETTLE_SECS}s after start)"
