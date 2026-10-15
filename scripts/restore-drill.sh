#!/usr/bin/env bash
# Restore drill (NFR-12): prove the nightly backup is restorable and consistent.
# Builds a canary database, snapshots it with the real `backup.sh`, restores the
# snapshot to a fresh path, and verifies the canary row and SQLite integrity.
#
# The signal-cli identity archive is drilled the same way: a canary data dir is
# archived by the same script and restored, and the identity key inside it is
# checked (SPEC-004 PH-8).
#
# Run from a checkout: `scripts/restore-drill.sh` (documented in docs/RESTORE.md).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

DB="$WORK/ocbot.db"
BACKUPS="$WORK/backups"

# 1. A database shaped like the bot's diary (an audit canary is enough here).
sqlite3 "$DB" <<'SQL'
CREATE TABLE audit (ts INTEGER, user_id TEXT, verb TEXT, action_id INTEGER, result TEXT);
INSERT INTO audit VALUES (1, '42', 'restore-drill', NULL, 'canary');
SQL

# 2. Snapshot it with the production backup path.
OCBOT_DB="$DB" OCBOT_BACKUP_DIR="$BACKUPS" OCBOT_BACKUP_RETAIN_DAYS=14 \
  "$SCRIPT_DIR/backup.sh" 2>/dev/null

SNAP="$(find "$BACKUPS" -maxdepth 1 -name 'ocbot-*.db' | head -1)"
if [ -z "$SNAP" ]; then
  echo "restore drill FAILED: no snapshot was written" >&2
  exit 1
fi

# 3. Restore: a snapshot is just a database file; copy it into place.
RESTORED="$WORK/restored.db"
cp "$SNAP" "$RESTORED"

CANARY="$(sqlite3 "$RESTORED" "SELECT COUNT(*) FROM audit WHERE verb = 'restore-drill';")"
if [ "$CANARY" != "1" ]; then
  echo "restore drill FAILED: canary row missing ($CANARY)" >&2
  exit 1
fi
if [ "$(sqlite3 "$RESTORED" 'PRAGMA integrity_check;')" != "ok" ]; then
  echo "restore drill FAILED: integrity_check != ok" >&2
  exit 1
fi

echo "restore drill OK: $(basename "$SNAP") -> restored.db (canary intact, integrity ok)"

# --- the signal-cli identity archive (SPEC-004 PH-8) -----------------------
SIGNAL_SRC="$WORK/var-lib-signal-cli"
mkdir -p "$SIGNAL_SRC/data"
# A stand-in for the identity keys the daemon writes once at link time.
printf 'canary-identity-key' > "$SIGNAL_SRC/data/identity.key"

# A separate backup dir, because two runs in the same second would otherwise
# collide on the timestamped name (the guard that makes back-ups non-clobbering
# is doing its job — the drill works around it rather than weakening it).
SIGNAL_BACKUPS="$BACKUPS/signal"
OCBOT_DB="$DB" OCBOT_BACKUP_DIR="$SIGNAL_BACKUPS" OCBOT_BACKUP_RETAIN_DAYS=14 \
  OCBOT_SIGNAL_DATA_DIR="$SIGNAL_SRC" \
  "$SCRIPT_DIR/backup.sh" 2>/dev/null

SIGNAL_SNAP="$(find "$SIGNAL_BACKUPS" -maxdepth 2 -name 'ocbot-signal-*.tar.gz' | head -1)"
if [ -z "$SIGNAL_SNAP" ]; then
  echo "restore drill FAILED: no signal identity archive was written" >&2
  exit 1
fi

RESTORED_SIGNAL="$WORK/restored-signal"
mkdir -p "$RESTORED_SIGNAL"
tar -xzf "$SIGNAL_SNAP" -C "$RESTORED_SIGNAL"
KEY="$RESTORED_SIGNAL/$(basename "$SIGNAL_SRC")/data/identity.key"
if [ ! -f "$KEY" ] || [ "$(cat "$KEY")" != "canary-identity-key" ]; then
  echo "restore drill FAILED: the identity key did not survive the archive" >&2
  exit 1
fi

echo "signal identity drill OK: $(basename "$SIGNAL_SNAP") -> $(basename "$KEY") (canary key intact)"
