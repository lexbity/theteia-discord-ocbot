# Changelog

**Audience:** users and upgraders of ocbot and of the `opencode-v2-client` crate.
**Purpose:** record notable changes per release.
**Prerequisites:** none.

All notable changes are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows [SemVer](https://semver.org/).
Versions apply to the workspace; the publishable artifact today is the `opencode-v2-client` crate (see its [COMPAT](crates/opencode-client/COMPAT.md) table for the opencode server pins).

## [Unreleased]

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
