# Operations

**Audience:** the operator running a deployed bot.
**Purpose:** know what to watch, what each signal means, and what to do when something breaks.
**Prerequisites:** a running deployment ([DEPLOYMENT](DEPLOYMENT.md)); the [glossary](README.md#glossary).

## The three health surfaces

1. **Discord itself** — `/status` shows server reachability and version, active actions, held locks, and pending gates. Announcements (server down, stream down) arrive as posts, once each.
2. **The metrics endpoint** — `curl http://127.0.0.1:9464/metrics` (loopback only, gate G18).
3. **The logs** — one JSON line per event via `journalctl -u ocbot -o cat`.

## Metrics reference

Every family exposed on `127.0.0.1:9464` (names are load-bearing — see `crates/ocbot/src/app/metrics.rs`):

| Metric | Type | Meaning |
|---|---|---|
| `ocbot_actions_total{state}` | counter | Actions observed entering each state |
| `ocbot_transition_cas_conflicts_total` | counter | Lost CAS races — double taps, Discord retries. Each was absorbed; a steady rate is unusual |
| `ocbot_reports_delivered_total{surface}` | counter | Reports delivered, by the surface that carried them |
| `ocbot_denied_interactions_total` | counter | Interactions rejected by the guard — sustained growth means someone else is poking the bot |
| `ocbot_opencode_up` | gauge | `1` reachable, `0` not |
| `ocbot_gate_pending` | gauge | Gates unanswered right now |
| `ocbot_gate_requests_total{kind,tool}` | counter | Gate volume by kind and tool — your allow-listing signal |
| `ocbot_locks_held` | gauge | Directory locks currently held |
| `ocbot_lock_age_seconds` | gauge | Age of the oldest held lock |
| `ocbot_log_lag_seconds` | gauge | Derived log-position lag (fallback-era gauge) |
| `ocbot_stream_state` | gauge | `0` down, `1` connecting, `2` live |
| `ocbot_stream_reconnects_total{reason}` | counter | Reconnects; `reason="idle"` is the half-open deadline firing, others are transport errors |
| `ocbot_stream_live_seconds_total` | counter | Cumulative time the stream was live |
| `ocbot_frames_total{kind,mapped}` | counter | Frames received, by kind; `mapped` says whether the frame produced a wake key |
| `ocbot_frames_decode_failures_total` | counter | Frames that failed header parse — garbage is counted, never fatal |
| `ocbot_frames_dropped_total` | counter | Wakes shed by the dispatch budget — the fallback ticks inherit them |
| `ocbot_detection_total{source}` | counter | Completions detected via `event` wake vs `fallback` tick — the honest doorbell scorecard |
| `ocbot_probe_failures_total{loop}` | counter | Probe failures per fallback loop |
| `ocbot_reconcile_repairs_total{kind}` | counter | Interrupted git sequences rolled forward at boot — nonzero means a crash was survived |
| `ocbot_idle_to_report_seconds` | histogram | Idle→report latency (NFR-2: p95 < 5 s) |
| `ocbot_surface_up{surface}` | gauge | `1` = the surface last delivered; `0` = it crossed the failure threshold |
| `ocbot_surface_send_total{surface,op}` | counter | Send attempts per surface and op (`post`/`edit`/`react`/`broadcast`) |
| `ocbot_surface_send_failures_total{surface,op}` | counter | Failed sends per surface and op — compare against `send_total` |
| `ocbot_surface_denied_total{surface}` | counter | Non-allowlisted senders dropped by a surface |
| `ocbot_surface_rate_dropped_total{surface}` | counter | Inbound commands dropped by a surface's rate bucket |
| `ocbot_signal_envelope_duplicates_total` | counter | Signal redeliveries absorbed by the dedupe cache |
| `ocbot_pending_flows_open{flow}` | gauge | Live multi-step input flows (a stuck nonzero means an abandoned draft) |
| `ocbot_component_map_rows` | gauge | Reaction→verb map size (GC health) |

## Surface health

Each configured chat surface is wrapped in an instrumented port (SPEC-004 PH-2). Three consecutive failed sends declare the surface **down**; the first successful send after an outage declares it **recovered**. Both transitions are edge-triggered and announced exactly once — a downed surface posts one notice on every *other* configured surface (with one surface configured, the notice goes nowhere and only the gauge moves). Sustained `ocbot_surface_send_failures_total` growth with `ocbot_surface_up{surface}=0` means the transport (bot token, daemon, socket) is the problem, not the bot: restart the surface's daemon, and the recovery notice confirms the fix.

## Logs

Structured JSON from `tracing`, one line per event.
Spans carry `action_id`, `session_id`, and `dir`, so you can follow one action through the log.
The filter comes from `RUST_LOG` and defaults to `info`.

```sh
journalctl -u ocbot -o cat -n 100
RUST_LOG=debug systemctl restart ocbot   # temporary, not for production
```

Every line passes through the scrubbing writer: secret-shaped strings (tokens, passwords, API keys) are redacted before they hit disk.
The built-in patterns ship in `config.rs`; add more with `redact_patterns` in the config.

## Alerts worth adding

The bot ships metrics, not alert rules.
Against a stock Prometheus, these match the design's own thresholds:

| Alert | Condition | Why |
|---|---|---|
| Server unreachable | `ocbot_opencode_up == 0` for 5 m | The bot survives it (degraded mode), but you want to know |
| Stream dead | `ocbot_stream_state == 0` for 10 m | Doorbell gone; latency regresses to fallback ticks |
| Decode failures bursting | `increase(ocbot_frames_decode_failures_total[10m]) > 25` | The supervisor reconnects at this threshold — likely server drift |
| Lock stuck | `ocbot_lock_age_seconds > 86400` | A directory held for a day is probably forgotten |
| Latency regression | p95 bucket of `ocbot_idle_to_report_seconds` above 5 s | NFR-2 budget |

## Degraded behaviors (all by design)

- **opencode unreachable:** exactly one "server down" post, one recovery post (FR-38). Actions in flight resume via reconcile when the server returns.
- **Event stream down:** reconnect with backoff; after `announce_after_secs` (default 60), exactly one announcement and one recovery post. Fallback ticks keep correctness; worst-case gate latency regresses to `gate_poll_interval_ms`… or, if you tuned the ticks for doorbell-era latency, to that cadence.
- **Doorbell disabled** (`[events] enabled = false`): pure polling mode; `ocbot_detection_total{source="fallback"}` tells you the doorbell is off duty.
- **Stall watchdog:** an active action silent for `stall_minutes` (default 45) gets exactly one plain ping — it surfaces silence, it does not diagnose.

## Incident runbook

| Symptom | Diagnose | Fix |
|---|---|---|
| "server down" post | `systemctl status opencode`; `curl -su opencode:… http://127.0.0.1:8080/api/info` | Restart the server; the bot recovers on its own |
| Stream down posts flapping | `ocbot_stream_reconnects_total{reason}`; server restarts in `journalctl -u opencode` | `reason="idle"` on a quiet server: tune `events.idle_deadline_secs`; otherwise fix the server |
| Gate pings are noisy | `ocbot_gate_requests_total{kind,tool}` — the repeat offenders | Move the benign recurring permission to `allow` in the opencode config |
| Directory won't accept a run (lock held) | `/status` names the holder and age | It's you: finish in the terminal. Or `/release` with confirmation — audited in the `audit` table |
| Report never arrived | `ocbot_detection_total{source}`; `ocbot_stream_state`; `/status` | If detection happened but no post: check Discord-side limits. If not detected: the fallback tick will catch it; if the bot is degraded, fix the server |
| Two identical reports | Rare crash window (Discord accepted the POST, the transaction didn't commit) | Known at-least-once delivery (NFR-3); ignore the duplicate |
| `ocbot_reconcile_repairs_total` increased | A crash mid-Accept/Park/Reset | Nothing to do — reconcile rolled it forward; the report/git state is authoritative |
| Suspicious "manual" rows in `/history` | Commits made outside the bot, or a restored older database | Expected — git is the source of truth; see [RESTORE](RESTORE.md) |
| Database trouble | `PRAGMA integrity_check` on a copy | Follow [RESTORE](RESTORE.md); stop the bot first |
| RSS creeping up | `scripts/measure-rss.sh` | The unit enforces `MemoryMax=200M`; steady state is ~20 MB — sustained growth is a bug, report it |

## Database operations

The store actor owns the single SQLite connection (WAL mode) and applies schema migrations at boot, tracked in a one-row `schema_version` table.
You never need to touch the file while the bot runs.
For snapshots, use the backup script's `VACUUM INTO` — never copy the WAL sidecars by hand while the process is live.
Restore is [RESTORE](RESTORE.md)'s job.

## Capacity

Single operator, single host: that is the design envelope, not a limitation to grow out of — see [scaling notes](ARCHITECTURE.md#scaling-notes).
Memory steady state is ~20 MB against a 200 MB ceiling; the backup set is single-digit MB; request volume is bounded by one SSE stream and roughly one Discord message per event.
