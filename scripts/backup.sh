#!/usr/bin/env bash
# ocbot backup (NFR-12): a consistent `VACUUM INTO` snapshot of the diary, an
# archive of the signal-cli identity keys, then a retention prune over both.
# Runs from `ocbot-backup.service` on the nightly timer.
#
# The bot may be running: `VACUUM INTO` reads a consistent snapshot of the live
# WAL database without blocking the single writer for more than a moment.
#
# Losing the signal-cli data dir means re-linking the number and re-verifying
# the safety number, so it is backed up alongside the diary (SPEC-004 PH-8).
# Its absence (a Discord-only deployment) is not an error: the archive step is
# simply skipped.
set -euo pipefail

DB="${OCBOT_DB:-ocbot.db}"
DEST="${OCBOT_BACKUP_DIR:-backups}"
RETAIN_DAYS="${OCBOT_BACKUP_RETAIN_DAYS:-14}"
# The signal-cli data directory (identity keys); empty or absent = no signal.
SIGNAL_DATA="${OCBOT_SIGNAL_DATA_DIR:-}"

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

# The signal-cli identity keys: a compressed copy of the daemon's data dir.
# The daemon is not stopped (it must keep serving), so this is a live copy:
# the identity key files under `data/` are written once at link time and never
# change again, which is what makes the archive worth taking. The daemon's own
# account database is copied best-effort — a torn read there costs at most a
# re-link of one device, never the number itself. `tar` copies what it can and
# only fails on a real I/O error.
SIGNAL_OUT=""
if [ -n "$SIGNAL_DATA" ] && [ -d "$SIGNAL_DATA" ]; then
  SIGNAL_OUT="$DEST/ocbot-signal-$STAMP.tar.gz"
  tar -czf "$SIGNAL_OUT" -C "$(dirname "$SIGNAL_DATA")" "$(basename "$SIGNAL_DATA")"
fi

# Retention: delete snapshots older than RETAIN_DAYS (by mtime).
find "$DEST" -maxdepth 1 -type f -name 'ocbot-*.db' -mtime "+$RETAIN_DAYS" -delete
find "$DEST" -maxdepth 1 -type f -name 'ocbot-signal-*.tar.gz' -mtime "+$RETAIN_DAYS" -delete

printf 'backup written: %s\n' "$OUT" >&2
if [ -n "$SIGNAL_OUT" ]; then
  printf 'signal identity archive written: %s\n' "$SIGNAL_OUT" >&2
fi
