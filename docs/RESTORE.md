# Backup & restore runbook (NFR-12 / P15)

Git is the source of truth for accepted work; the SQLite file is the operator's
diary (labels, prompts, session mappings, decisions). Losing it loses **no
accepted work** (P2) — but restoring it is cheap, so do it.

The nightly backup also archives the **signal-cli identity keys**, because
those are the one thing no git history can rebuild: losing them means
re-linking the number and re-verifying the safety number.

## What runs automatically

`deploy/ocbot-backup.timer` fires `deploy/ocbot-backup.service` nightly at
03:30 (±5 min jitter), which runs `scripts/backup.sh` — a `VACUUM INTO`
snapshot of the diary, plus a compressed copy of the signal-cli data dir
(`OCBOT_SIGNAL_DATA_DIR`, default absent on a Discord-only host):

```sh
OCBOT_DB=/srv/ocbot/ocbot.db \
OCBOT_BACKUP_DIR=/srv/ocbot/backups \
OCBOT_BACKUP_RETAIN_DAYS=14 \
  scripts/backup.sh
```

- `VACUUM INTO` takes a transactionally consistent snapshot of the live WAL
  database without stopping the bot.
- The signal archive is a *live* copy: the identity key files are written once
  at link time and never change, while the daemon's account database is copied
  best-effort. Neither the daemon nor the bot is stopped, so a torn read of the
  account database is possible — the keys (the irreplaceable part) are files.
- Snapshots older than 14 days are pruned by mtime, diary and identity alike.
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

## Restoring the Signal identity

The bot's number is the account; losing its keys means Signal forgets the
device, and a re-link mints a new safety number (which you must re-verify).

1. Stop both units: `systemctl stop ocbot.service signal-cli.service`.
2. Move the live data dir aside and untar the newest archive:

   ```sh
   mv /var/lib/signal-cli /var/lib/signal-cli.broken
   tar -xzf /srv/ocbot/backups/ocbot-signal-<stamp>.tar.gz -C /var/lib
   chown -R ocbot:ocbot /var/lib/signal-cli
   ```
3. Start the daemon: `systemctl start signal-cli.service`.
4. Confirm the account and note its fingerprint:

   ```sh
   sudo -u ocbot signal-cli -a +15551234567 listAccounts
   sudo -u ocbot signal-cli -a +15551234567 listIdentities
   ```
   If the fingerprint does **not** match your phone's safety number for the
   bot, the keys did not survive: re-link the device
   ([DEPLOYMENT](DEPLOYMENT.md#the-signal-surface)) and verify the new safety
   number.
5. Start the bot: `systemctl start ocbot.service`.

A re-link is also the recovery when a *linked device* is revoked rather than
lost: remove the device in the phone's Linked devices list and link a new one
with the same command as the initial setup.

## Drill

`scripts/restore-drill.sh` exercises the whole path against canary files — a
canary database (build → `VACUUM INTO` → copy back → canary row +
`integrity_check`) **and** a canary signal-cli data dir (identity key → archive
→ untar → key intact). It is safe to run at any time and is expected in the
release checklist.

Recorded drill run (2026-10-15, sqlite 3.53.4):

```
restore drill OK: ocbot-20261015T212238Z.db -> restored.db (canary intact, integrity ok)
signal identity drill OK: ocbot-signal-20261015T212238Z.tar.gz -> identity.key (canary key intact)
```

## Retention & disk

14 nightly snapshots of a single-operator diary are small (single-digit MB).
Widen `OCBOT_BACKUP_RETAIN_DAYS` if the audit table grows; the prune uses
mtime, so a manual snapshot is protected only if its mtime is recent.
