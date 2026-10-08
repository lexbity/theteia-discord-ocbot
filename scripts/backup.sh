#!/usr/bin/env bash
# ocbot SQLite backup (NFR-12): a consistent `VACUUM INTO` snapshot, then a
# retention prune. Runs from `ocbot-backup.service` on the nightly timer.
#
# The bot may be running: `VACUUM INTO` reads a consistent snapshot of the live
# WAL database without blocking the single writer for more than a moment.
set -euo pipefail

DB="${OCBOT_DB:-ocbot.db}"
DEST="${OCBOT_BACKUP_DIR:-backups}"
RETAIN_DAYS="${OCBOT_BACKUP_RETAIN_DAYS:-14}"

if [ ! -f "$DB" ]; then
  echo "backup: database $DB does not exist" >&2
  exit 1
fi
mkdir -p "$DEST"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="$DEST/ocbot-$STAMP.db"

if [ -e "$OUT" ]; then
  echo "backup: $OUT already exists" >&2
  exit 1
fi

# VACUUM INTO writes a transactionally consistent copy. The path is inside our
# own quoting; there is no operator interpolation here.
sqlite3 "$DB" "VACUUM INTO '$OUT'"

# Retention: delete snapshots older than RETAIN_DAYS (by mtime).
find "$DEST" -maxdepth 1 -type f -name 'ocbot-*.db' -mtime "+$RETAIN_DAYS" -delete

printf 'backup written: %s\n' "$OUT" >&2
