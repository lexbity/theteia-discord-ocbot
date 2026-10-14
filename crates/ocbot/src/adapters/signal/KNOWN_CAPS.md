# Known signal-cli capabilities (pinned contract notes)

What the PH-3 contract layer pins, and what a live capture would supersede.

## Version

`MIN_SIGNAL_CLI_VERSION = "0.15.0"` (adapters/signal/mod.rs). **Pinned
without a live daemon** — no signal-cli was installed on the capture host, so
the minimum is the documented JSON-RPC feature floor, not a measured smoke
test. Run the live smoke (`tests/live_manual.rs` pattern) against the real
daemon and, if it proves a *higher* honest floor (or the shapes below drift),
raise the constant and update `tests/common/signal_fake.rs`'s scripts — the
fake is the pinned contract.

## Protocol subset (D-10)

Newline-delimited JSON-RPC 2.0 over the daemon's unix socket:

| method | params | result |
|---|---|---|
| `version` | `[]` | `"0.15.0"` (string) |
| `listAccounts` | `[]` | `["+15551234567"]` (array of E.164 strings) |
| `subscribe` | `[]` | `null` ack; then one response line per envelope, `id` = the subscribe request's id, `result` = `{envelope, account}` |
| `send` | `[{"recipient", "message"}]` | timestamp string |
| `sendReaction` | `[{"recipient", "targetTimestamp", "reaction", "remove"}]` | timestamp string |

Assumptions a live capture must confirm (each is load-bearing in the fake):

- **Subscribe ack is `result: null`.** Envelope lines then carry the same
  `id`. A daemon that streams envelopes with a different id scheme needs the
  routing in `rpc.rs::route_line` retargeted.
- **`listAccounts` returns strings**, not account objects.
- **Reactions arrive as data messages** with `reaction: {emoji,
  targetSentTimestamp, isRemove}` (signal-cli ≥ 0.13 behavior).
- **Attachment local paths** arrive as `localPath` (daemon-side download).

Unknown fields are ignored everywhere (tolerant-serde discipline); a daemon
upgrade adding fields must not decode-fail. Undecodable *envelopes* count
toward the drift burst rule (`DECODE_BURST_RECONNECT` within
`DECODE_BURST_WINDOW_MS`, borrowed from SPEC-003).

## Native edit (feeds D-13)

**Unknown / unproven.** PH-3 ships no native edit; D-14's fallback post
(`↻ update:`) remains the tested path in PH-4+. If a live capture on the
pinned minimum proves a stable edit verb, record it here and let the adapter
prefer it.

## Socket hygiene (FR-116)

The doctor requires: the path is a unix socket, mode exactly `0600`, owned by
the ocbot service uid (compared against `/proc/self/status`'s effective uid).
systemd's `RuntimeDirectory=signal-cli` is the deployment answer (PH-8).
