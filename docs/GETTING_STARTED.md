# Getting started

**Audience:** a new operator or contributor who has never run ocbot.
**Purpose:** get from a clean machine to a verified running bot.
**Prerequisites:** Linux is the supported host (the production units are systemd); the bot is built for a **single operator** — you need a chat surface to talk on (Discord, Signal, or both) and a host you control.

## Prerequisites

| Requirement | Version | How you get it |
|---|---|---|
| rustup + Rust | 1.98.1, pinned via `rust-toolchain.toml` | [rustup](https://rustup.rs); the pinned toolchain is fetched automatically on first build |
| opencode server | v2, current pin 2.0.24 | install opencode; see [client COMPAT](../crates/opencode-client/COMPAT.md) for the version policy |
| git | any recent | your package manager; `git --version` |
| Discord account | — | yours, plus a bot application (step 1) — only if `channel` selects `discord` |
| A phone number | — | for `channel = "signal"`: the bot's own number (registered or linked on the daemon host) plus yours as the allowlisted sender — see [DEPLOYMENT](DEPLOYMENT.md#the-signal-surface) |

## Step 1 — Create the Discord bot

1. Create an application in the [Discord developer portal](https://discord.com/developers/applications).
2. Open the **Bot** tab, reset the token, and copy it — this is `DISCORD_TOKEN`.
3. Enable the **Message Content** privileged intent.
   The bot needs it to map replies to inbox reports back to sessions.
4. Under **OAuth2 → URL generator**, select the scopes `bot` and `applications.commands`, and at minimum the permissions Send Messages, Attach Files, and Add Reactions.
   Open the generated URL to invite the bot to a private server you own.
5. In Discord, enable Developer Mode (user settings → Appearance), right-click your own name, and Copy User ID — this is `ALLOWED_USER_ID`.

## Step 2 — Start the opencode server

ocbot expects the server on loopback with a password.
The minimal manual start:

```sh
OPENCODE_PASSWORD="<choose a password>" opencode serve --hostname 127.0.0.1 --port 8080
```

Check it answers:

```sh
curl -su "opencode:$OPENCODE_PASSWORD" http://127.0.0.1:8080/api/info
```

For production, use the provided unit instead: [DEPLOYMENT](DEPLOYMENT.md).

## Step 3 — Build

From the repository root ([TODO: repo URL] — clone it first):

```sh
cargo build --release
```

The binary lands at `target/release/ocbot`.

## Step 4 — Configure

ocbot reads `config/config.toml` relative to the directory you run it from, or the path in `OCBOT_CONFIG`.
Unknown keys are a load error — start from the example:

```sh
cp config/config.toml.example config/config.toml
$EDITOR config/config.toml
```

You must set `opencode_url`, and every key of every surface you select.
Everything else has a default.

### Surfaces

`channel` selects which chat surfaces run — one name or a list:

```toml
channel = ["discord", "signal"]
```

| Value | What it needs | Notes |
|---|---|---|
| `"discord"` (default) | `[discord] allowed_user_id` + `DISCORD_TOKEN` | today's behaviour, unchanged |
| `"signal"` | `[signal] socket` + `[signal] operator_number` | requires a running signal-cli daemon; the boot doctor gates it |
| a list | both sets | both surfaces run at once; reports broadcast, decisions from either race through the same CAS |

A `[signal]` section you are not using yet is fine — it is a **warning**, not an error, so you can stage the config before the daemon exists.

| Key | Required | Default | Description |
|---|---|---|---|
| `channel` | no | `["discord"]` | The chat surfaces to run: `"discord"`, `"signal"`, or a list |
| `opencode_url` | yes | — | opencode server base URL; must be loopback unless `ALLOW_NON_LOOPBACK=true` |
| `db_path` | no | `"ocbot.db"` | SQLite diary path (relative to the working directory) |
| `signal.socket` | if signal | — | The signal-cli daemon's unix socket (must be `0600`, owned by the service user) |
| `signal.operator_number` | if signal | — | Your E.164 number — the only sender the bot serves |
| `signal.report_excerpt` | no | `"full"` | How much of a report reaches Signal |
| `signal.report_attachments` | no | `true` | Whether report attachments leave the host |
| `signal.rate_limit` | no | `{ capacity = 10, refill_per_sec = 1.0 }` | Inbound command token bucket |
| `discord.ops_channel` | no | DMs only | Ops channel ID; its threads are in scope too |
| `discord.inbox_channel` | no | `ops_channel` | Where inbox completions post |
| `discord.allowed_user_id` | if discord | — | The operator's Discord user ID (env `ALLOWED_USER_ID` overrides) |
| `discord.report_excerpt` | no | `"excerpt"` | How much of a report reaches Discord |
| `discord.report_attachments` | no | `false` | Whether report attachments leave the host (they did before SPEC-004 D-12) |
| `templates_dir` | no | `"templates"` | Where `fresh.md` / `resume.md` live |
| `poll_interval_ms` | no | `2000` | Activity fallback tick |
| `gate_poll_interval_ms` | no | `2000` | Pending-gate fallback tick |
| `inbox_poll_interval_ms` | no | `5000` | Root-session fallback tick |
| `ops_poll_interval_ms` | no | `30000` | Health monitor, gauges, stall scan |
| `metrics_bind` | no | `"127.0.0.1:9464"` | Loopback metrics endpoint |
| `git_timeout_secs` | no | `10` | Per-command git timeout |
| `perm_reping_minutes` | no | `10` | Re-ping cadence for unanswered gates |
| `abort_timeout_secs` | no | `30` | How long `/abort` waits for idle confirmation |
| `stall_minutes` | no | `45` | Silence before the stall watchdog pings once |
| `browse_roots` | no | `["/home"]` | Directories `/ls` and typed paths are confined to |
| `inbox_dir` | no | `"inbox"` | Where uploaded attachments are saved |
| `allow_freeform_paths` | no | `false` | Allow typed paths outside `browse_roots` |
| `plan_heading_min` / `plan_heading_max` | no | `2` / `3` | Plan picker heading depth range |
| `ls_page_size` | no | `18` | Entries per `/ls` page |
| `per_session_ruleset` | no | `false` | Experimental; leave off |
| `permissions_file` | no | `"config/opencode.permissions.jsonc"` | Baseline ruleset verified at startup |
| `redact_patterns` | no | built-ins | Regexes scrubbed from inline reports and logs |
| `events.enabled` | no | `true` | The SSE doorbell; `false` = pure polling mode |
| `events.idle_deadline_secs` | no | `25` | No bytes for this long ⇒ reconnect |
| `events.backoff_min_ms` / `events.backoff_max_ms` | no | `1000` / `30000` | Reconnect backoff range |
| `events.announce_after_secs` | no | `60` | Stream down this long ⇒ one announcement post |
| `events.reconcile_min_interval_secs` | no | `5` | Coalescing floor for reconnect reconciles |

Prompt templates live in `templates/` (`fresh.md` for a new run, `resume.md` for Park & redo).
They support exactly four slots: `{label}`, `{plan_path}`, `{git_log}`, `{git_status}`; an unknown slot is a load-time error.

## Step 5 — Set the secrets

Secrets come from the environment, never the config file.

| Variable | Required | Default | Description |
|---|---|---|---|
| `DISCORD_TOKEN` | if discord | — | Bot token from the developer portal |
| `OPENCODE_PASSWORD` | yes | — | opencode server password |
| `ALLOWED_USER_ID` | no | config value | Overrides `[discord] allowed_user_id` |
| `SIGNAL_OPERATOR` | no | config value | Overrides `[signal] operator_number` |
| `SIGNAL_CLI_ACCOUNT` | for the daemon | — | The signal-cli **daemon's** own number (the unit's `-a`), distinct from yours |
| `ALLOW_NON_LOOPBACK` | no | `false` | Permit a non-loopback `opencode_url` (weakens the trust model) |
| `OCBOT_CONFIG` | no | `config/config.toml` | Config file path |

For development:

```sh
export DISCORD_TOKEN="<bot token>"
export OPENCODE_PASSWORD="<opencode password>"
export ALLOWED_USER_ID="<your Discord user id>"
# and, on the Signal surface:
export SIGNAL_OPERATOR="+15559876543"
```

In production these live in `/etc/ocbot/ocbot.env`, mode `0600` — see [DEPLOYMENT](DEPLOYMENT.md).

## Step 6 — Run and verify

Run from the repository root (config, templates, and the database resolve relative to it):

```sh
cargo run --release
```

The startup doctor fails fast — with a named reason — on a bad config, missing secrets, a non-v2 server, an unhealthy signal-cli daemon (Signal surface only), or a permissions baseline missing its `ask` patterns.
A healthy boot shows JSON log lines and a `connected to discord` line — plus `signal-cli daemon healthy` when the Signal surface runs.

Then verify on each surface you selected:

1. **Discord:** send `/status` in a DM with the bot.
   You should see server reachability and version, no active actions, no held locks.
2. **Signal:** send `status` from your phone; the same answer arrives as a plain message.
3. Check the metrics endpoint:

   ```sh
   curl -s http://127.0.0.1:9464/metrics | grep ocbot_surface_up
   ```
   `1` for every surface you selected.
4. Start a real run in a scratch repository with `/run` (Discord) or `run <dir>` (Signal), and make a decision on the report.

## Degraded mode

If the opencode server is unreachable at boot, the bot starts anyway in **degraded mode** and recovers on its own when the server returns.
You get exactly one "server down" post and one recovery post — no spam (FR-38).
With `[events] enabled = false`, the bot runs the pure polling architecture: every fallback tick uses its configured interval, and no SSE connection is made.

## Signal commands

Send plain text to the bot's number; it answers with plain messages.

| Command | What it does |
|---|---|
| `run [dir]` | Start a run: the bot asks for the prompt, then sends it |
| `run <dir> \| <prompt>` | The same, with the prompt inline |
| `prompt [text]` | Prompt the current directory's session (opens a text form without it) |
| `status` / `abort` / `release` / `mute` / `reset` | The slash verbs' equivalents |
| `ls [dir] [page]` / `cd <dir>` | Browse directories |
| `history [page]` | Git history |
| `send [label]` / `edit [label]` | The latest staged action: send it, or rewrite its prompt |
| `pick <n>` | Choose plan step `n` |
| `accept \| park \| follow \| discard [label]` | Decide the latest report (or the one you name) |
| `gate <label> <once\|always\|reject>` | Answer a permission gate |
| `help` | The command table and the reaction legend |
| `cancel` | Close an open flow |

Messages that carry decision buttons also render a **legend** (`✅ accept · 🔁 follow up · 🅿️ park · 🗑️ discard`, keycaps for gate options): react with the emoji to decide. Ten minutes of silence closes an open flow; `cancel` closes it now.

## Discord commands

| Command | What it does |
|---|---|
| `/run` | Open a run: directory → optional plan step → review the rendered prompt → send |
| `/prompt` | Send a free-form prompt to a directory's active session (starts one if none) |
| `/status` | Server reachability and version, active actions, held locks, pending gates |
| `/history` | Paged action history for the current directory — git log merged with bot actions |
| `/reset` | Reset the directory to a live commit (a `superseded/<ts>` branch is created first) |
| `/release` | Release a held directory lock (requires confirmation) |
| `/abort` | Interrupt the running action; confirmed only after the server reports idle |
| `/mute` | Toggle inbox reports for the current directory |
| `/ls` | Browse directories under `browse_roots` |
| `/cd` | Open a directory in the browser |

Beyond slash commands: report messages carry decision buttons (**Accept**, **Follow up**, **Park & redo**, or **Discard**, depending on the outcome); gates carry **Once / Always / Reject** (or form buttons and a modal); replying to an inbox report adopts that session; attaching a file in the DM saves it under `inbox_dir` and offers its path to the agent.
Reactions on the origin message are the quiet status light.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `config error: unknown field …` | A stale or misspelled key | Unknown keys are a load error; remove or rename it |
| `secrets error: missing DISCORD_TOKEN` | Environment not set | Export it, or use an `EnvironmentFile` in production |
| Exits naming a version at boot | The server is opencode v1 | Upgrade the server to v2 — see [client COMPAT](../crates/opencode-client/COMPAT.md) |
| Exits `3` with `no chat surface survived the boot doctor` | The signal-cli daemon failed its boot checks | See [the signal-cli checks](OPERATIONS.md#the-signal-cli-surface-spec-004-ph-3-contract-layer); with Discord also selected, Discord alone never causes this |
| `refusing non-loopback OPENCODE_URL` | Server URL is not loopback | Bind the server to `127.0.0.1`, or set `ALLOW_NON_LOOPBACK=true` knowingly |
| Startup names missing `ask` patterns | The permissions baseline drifted | Restore the required entries in `config/opencode.permissions.jsonc` |
| Template load error at boot | An unknown slot in a template | Use only `{label}`, `{plan_path}`, `{git_log}`, `{git_status}` |
| Bot shows offline in Discord | Wrong token or gateway problem | Recheck `DISCORD_TOKEN`; check the logs for the connect line |
| Every interaction answers "not authorized" | Wrong user or out-of-scope channel | Use the operator account in the DM or ops channel — this is the guard working |
| Replies to inbox reports are ignored | Message Content intent disabled | Enable it in the developer portal and restart |
| No report after the agent finished | Stream down plus a slow fallback tick | Check `/status` and the `ocbot_stream_state` metric ([OPERATIONS](OPERATIONS.md)); ticks bound the latency |
| Gate pings are noisy | A benign recurring permission | Move it to `allow` in your opencode config |
| Directory locked by `external:…` | A session you started in a terminal | Finish it there, or `/release` with confirmation |
| After a restore, recent commits show as "manual" | The restored database is older than git | Expected — git is the source of truth; see [RESTORE](RESTORE.md) |
