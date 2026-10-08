# Backup & restore runbook (NFR-12 / P15)

Git is the source of truth for accepted work; the SQLite file is the operator's
diary (labels, prompts, session mappings, decisions). Losing it loses **no
accepted work** (P2) — but restoring it is cheap, so do it.

## What runs automatically

`deploy/ocbot-backup.timer` fires `deploy/ocbot-backup.service` nightly at
03:30 (±5 min jitter), which runs `scripts/backup.sh`:

```sh
OCBOT_DB=/srv/ocbot/ocbot.db \
OCBOT_BACKUP_DIR=/srv/ocbot/backups \
OCBOT_BACKUP_RETAIN_DAYS=14 \
  scripts/backup.sh
```

- `VACUUM INTO` takes a transactionally consistent snapshot of the live WAL
  database without stopping the bot.
- Snapshots older than 14 days are pruned by mtime.
- A failure exits non-zero, so systemd records a failed unit (check
  `systemctl --failed`).

## Restoring

A snapshot is an ordinary SQLite file.

1. Stop the bot: `systemctl stop ocbot.service`.
2. Move the current database aside (keep it — it may be newer than the last
   nightly):
   `mv /srv/ocbot/ocbot.db /srv/ocbot/ocbot.db.broken`
   Also remove stale sidecars: `ocbot.db-wal`, `ocbot.db-shm` (they belong to
   the moved database; a mismatch would be refused by SQLite).
3. Copy the snapshot into place:
   `cp /srv/ocbot/backups/ocbot-<stamp>.db /srv/ocbot/ocbot.db`
4. Verify: `sqlite3 /srv/ocbot/ocbot.db 'PRAGMA integrity_check;'` ⇒ `ok`.
5. Start the bot: `systemctl start ocbot.service`.
6. Confirm with `/status` and `/history`; a DB that is older than git simply
   shows recent commits as **manual** rows (AC-13), never as lost work.

## Drill

`scripts/restore-drill.sh` exercises the whole path against a canary database
(build → `VACUUM INTO` → copy back → canary + `integrity_check`). It is safe to
run at any time and is expected in the release checklist.

Recorded drill run (2026-10-08, sqlite 3.53.4):

```
restore drill OK: ocbot-20261008T075750Z.db -> restored.db (canary intact, integrity ok)
```

## Retention & disk

14 nightly snapshots of a single-operator diary are small (single-digit MB).
Widen `OCBOT_BACKUP_RETAIN_DAYS` if the audit table grows; the prune uses
mtime, so a manual snapshot is protected only if its mtime is recent.
