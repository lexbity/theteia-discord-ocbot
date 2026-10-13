# Getting started

**Audience:** a new operator or contributor who has never run ocbot.
**Purpose:** get from a clean machine to a verified running bot.
**Prerequisites:** Linux is the supported host (the production units are systemd); the bot is built for a **single operator** — you need a Discord account and a host you control.

## Prerequisites

| Requirement | Version | How you get it |
|---|---|---|
| rustup + Rust | 1.98.1, pinned via `rust-toolchain.toml` | [rustup](https://rustup.rs); the pinned toolchain is fetched automatically on first build |
| opencode server | v2, current pin 2.0.24 | install opencode; see [client COMPAT](../crates/opencode-client/COMPAT.md) for the version policy |
| git | any recent | your package manager; `git --version` |
| Discord account | — | yours, plus a bot application (step 1) |

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

You must set `opencode_url` and `allowed_user_id` (the latter can also come from the environment).
Everything else has a default.

| Key | Required | Default | Description |
|---|---|---|---|
| `opencode_url` | yes | — | opencode server base URL; must be loopback unless `ALLOW_NON_LOOPBACK=true` |
| `allowed_user_id` | yes (or env) | — | The operator's Discord user ID |
| `ops_channel` | no | DMs only | Ops channel ID; its threads are in scope too |
| `inbox_channel` | no | `ops_channel` | Where inbox completions post |
| `db_path` | no | `"ocbot.db"` | SQLite diary path (relative to the working directory) |
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
| `DISCORD_TOKEN` | yes | — | Bot token from the developer portal |
| `OPENCODE_PASSWORD` | yes | — | opencode server password |
| `ALLOWED_USER_ID` | no | config value | Overrides `allowed_user_id` |
| `ALLOW_NON_LOOPBACK` | no | `false` | Permit a non-loopback `opencode_url` (weakens the trust model) |
| `OCBOT_CONFIG` | no | `config/config.toml` | Config file path |

For development:

```sh
export DISCORD_TOKEN="<bot token>"
export OPENCODE_PASSWORD="<opencode password>"
export ALLOWED_USER_ID="<your Discord user id>"
```

In production these live in `/etc/ocbot/ocbot.env`, mode `0600` — see [DEPLOYMENT](DEPLOYMENT.md).

## Step 6 — Run and verify

Run from the repository root (config, templates, and the database resolve relative to it):

```sh
cargo run --release
```

The startup doctor fails fast — with a named reason — on a bad config, missing secrets, a non-v2 server, or a permissions baseline missing its `ask` patterns.
A healthy boot shows JSON log lines and a `connected to discord` line.

Then verify in Discord:

1. Send `/status` in a DM with the bot.
   You should see server reachability and version, no active actions, no held locks.
2. Check the metrics endpoint:

   ```sh
   curl -s http://127.0.0.1:9464/metrics | head
   ```
3. Start a real run in a scratch repository with `/run`, and make a decision on the report.

## Degraded mode

If the opencode server is unreachable at boot, the bot starts anyway in **degraded mode** and recovers on its own when the server returns.
You get exactly one "server down" post and one recovery post — no spam (FR-38).
With `[events] enabled = false`, the bot runs the pure polling architecture: every fallback tick uses its configured interval, and no SSE connection is made.

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
