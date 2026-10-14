# Changelog

**Audience:** users and upgraders of ocbot and of the `opencode-v2-client` crate.
**Purpose:** record notable changes per release.
**Prerequisites:** none.

All notable changes are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows [SemVer](https://semver.org/).
Versions apply to the workspace; the publishable artifact today is the `opencode-v2-client` crate (see its [COMPAT](crates/opencode-client/COMPAT.md) table for the opencode server pins).

## [Unreleased]

### Changed (breaking — config)

- **SPEC-004 PH-1: multi-surface chat transport, vocabulary unification.** The core now speaks `Surface`/`Destination`/`ActorId`/`PostReceipt` through one `ChatPort` instead of `DiscordPort` + `u64` snowflakes. Discord behavior is unchanged byte-for-byte.
- **Breaking config change:** the flat top-level `ops_channel`, `inbox_channel`, and `allowed_user_id` keys moved into a `[discord]` section, and a new `channel` key (single name or list, default `["discord"]`) selects the surfaces to run. **Mechanical fix:** indent those three keys under `[discord]`. A `[signal]` section now parses (socket, operator_number, per-surface report policy, rate limit) but selecting `signal` is refused until the Signal surface ships; `SIGNAL_OPERATOR` is the new env override for the operator number.
- Database migration v5 (the de-Discord reshape, transactional under the schema-version guard): `action.channel_id` → `action.surface` + `action.chat_ref`, `msg_map` keyed by `(surface, msg_id)`, `notified.discord_msg_id` renamed to the informational `receipts` column, and the new `component_map`/`pending_input` tables created for the later Signal slices. Old databases migrate in place at boot; unclaim semantics are preserved exactly.
- `OutgoingMessage.mentioned_users` was renamed `mention_allowlist` (Discord-rendered FR-3 allowlist; other surfaces ignore it).
- `SendError` lost its `"discord: "` text prefix and gained a typed `surface` field.

### Added

- Deterministic Discord controller for a local opencode v2 server: `/run`, `/prompt`, `/abort`, `/history`, `/reset`, `/release`, `/mute`, `/ls`, `/cd`, `/status`.
- Decisions — Accept, Follow up, Park & redo, Discard — exactly-once via CAS transitions with write-ahead intent rows and boot reconcile.
- Gates: one lifecycle for permission requests and forms, with re-ping policy and surface-only degradation for non-renderable forms.
- Git-truth history: `/history` rendered from `git log`, merged with actions; Park creates `rejected/*` branches, Reset creates `superseded/*`.
- Files and plans: rooted `/ls` browser, attachment inbox, markdown plan picker.
- External sessions: inbox reports for terminal-run sessions; replying adopts the session.
- Ops hardening: degraded mode, stall watchdog, secret-scrubbing JSON logs, loopback Prometheus metrics, redaction, nightly backups with a drilled restore path.
- Event-stream doorbell: a supervised SSE connection whose frames trigger wake-guarded fact reads, with polling fallback ticks.
- `opencode-v2-client`: thin, tolerant HTTP + SSE client for the opencode v2 API, with a startup version gate and a CI drift test.

[TODO: cut this list into dated release entries at the first release]
