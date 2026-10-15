# Theteia - the ocbot

[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.98.1_pinned-blue)](rust-toolchain.toml)
[![CI](https://img.shields.io/badge/CI-TODO-lightgrey)]()
[![crates.io](https://img.shields.io/badge/crates.io-TODO-lightgrey)]()

<!-- [TODO: repo URL] Point the CI badge at the hosted repository once it exists. -->
<!-- [TODO: crates.io] Point at the published opencode-v2-client crate. -->

**Audience:** anyone evaluating, running, or contributing to ocbot.
**Purpose:** explain what ocbot is and get you to a running bot.
**Prerequisites:** none — this page links to everything you need.

## What is ocbot?

Theteia - ocbot remote-controls a local opencode server (coding agent) from the chat app you already use — **Discord, Signal, or both**.
You send a prompt to one of your codebases from your phone.
When the agent finishes, you get one report and three choices: **Accept** (commit), **Follow up** (keep going), or **Park & redo** (branch the attempt, reset, and try again).
The bot itself contains no AI: it is a deterministic controller.
The mental model: **the bot is a TV remote, the opencode server is the TV, git is the truth, SQLite is a diary of what you pressed.**

## Features

- **Drive the agent from your chat app.** You run prompts, read reports, and make decisions from a DM (or the bot's Signal number); the guard rejects every other identity, on every surface.
- **One-tap decisions that are safe to double-tap.** Every state change is a compare-and-swap (CAS) transition: duplicate button presses lose, exactly one commit lands.
- **Crash-safe git operations.** Accept, Park & redo, and Reset survive a `SIGKILL` at any point; boot reconcile recovers from database facts, not memory.
- **Git is the source of truth.** `/history` renders `git log`; the SQLite diary stores only what git cannot know.
- **Gates reach your phone.** A permission request or form blocks the session until you answer; each becomes a message on your surface — with buttons on Discord, and a reaction legend on Signal.
- **Quiet by design.** Completions from sessions you started in a terminal go to the inbox; replying to one adopts the session.
- **Honest signals.** JSON logs with secret scrubbing, a loopback metrics endpoint (one gauge per surface), and a stall watchdog that pings once when nothing happens.
- **Two surfaces, one machine.** Reports broadcast to whichever surfaces you selected; a decision from either one races through the same compare-and-swap, and exactly one wins.

## Quick start

1. Install the prerequisites from the table below.
2. Clone the repository and build from its root (rustup fetches the pinned toolchain automatically):
   ```sh
   cargo build --release
   ```
3. Create your config from the example, pick your surface(s) with `channel`, and set the operator ids:
   ```sh
   cp config/config.toml.example config/config.toml
   $EDITOR config/config.toml
   ```
4. Export the secrets:
   ```sh
   export DISCORD_TOKEN="<bot token>"
   export OPENCODE_PASSWORD="<opencode password>"
   export ALLOWED_USER_ID="<your Discord user id>"
   ```
5. Start an opencode v2 server, then run the bot from the repository root and verify with `/status` in a DM:
   ```sh
   cargo run --release
   ```

Full setup, the Discord command reference, and troubleshooting live in [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md).
Production install with systemd lives in [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md).

## Prerequisites

| Requirement | Version | Notes |
|---|---|---|
| Rust | 1.98.1 (pinned) | rustup selects it from `rust-toolchain.toml` |
| opencode server | v2 (current pin: 2.0.24) | loopback + password; V1 is refused at startup — see [COMPAT](crates/opencode-client/COMPAT.md) |
| Discord bot account | — | Message Content intent enabled — see GETTING_STARTED (Discord surface only) |
| signal-cli | ≥ `MIN_SIGNAL_CLI_VERSION` | for the Signal surface; install and linking in [DEPLOYMENT](docs/DEPLOYMENT.md#the-signal-surface) |
| git | any recent | on `PATH`; the bot never pushes or force-anything |
| SQLite | — | bundled into the binary; nothing to install |
| Linux + systemd | optional | only for the production deployment units |

## Using it

`/run` (or `run <dir>` on Signal) walks you through it: pick a directory, optionally pick a step from a markdown plan, review the rendered prompt, send.
While the agent works, reactions on your message act as a status light.
When it finishes, you get a report: outcome headline, excerpt, git diff stat, and the decision buttons.
A blocked session pings you; answer a permission request with Once / Always / Reject.

[TODO: screenshot of a report message with the Accept / Follow up / Park & redo buttons]
[TODO: screenshot of a permission gate message with Once / Always / Reject]

The full command reference is in [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md).

## Documentation

| Doc | What it covers |
|---|---|
| [docs/README.md](docs/README.md) | Index of all docs + glossary |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | How it works: crates, data model, decisions |
| [docs/GETTING_STARTED.md](docs/GETTING_STARTED.md) | Setup, config, Discord + Signal commands, troubleshooting |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Production install, the signal-cli unit and the safety-number runbook |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | Metrics, logs, incidents |
| [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md) | Dev setup, PRs, CI gates |
| [docs/TESTING.md](docs/TESTING.md) | Test suites and coverage gates |
| [CHANGELOG.md](CHANGELOG.md) | Version history |

## Notes and license

- (this project is not associated with Opencode)
- Licensed under [Apache-2.0](LICENSE).
