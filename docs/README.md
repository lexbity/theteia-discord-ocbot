# Documentation index

**Audience:** everyone reading or writing ocbot documentation.
**Purpose:** find the right document and use the shared vocabulary.
**Prerequisites:** none.

Start with the [project README](../README.md).
Read [GETTING_STARTED](GETTING_STARTED.md) to run the bot; read [ARCHITECTURE](ARCHITECTURE.md) to change it.

## Index

| Document | Description | Audience |
|---|---|---|
| [README](../README.md) | Pitch, features, quick start | Everyone |
| [GETTING_STARTED](GETTING_STARTED.md) | Install, configure, run, verify; Discord command reference; troubleshooting | New operators and contributors |
| [ARCHITECTURE](ARCHITECTURE.md) | System diagram, crates, data model, decision records, trust model | Contributors |
| [CONTRIBUTING](CONTRIBUTING.md) | Dev setup, lint floor, commit and PR conventions, CI gates | Contributors |
| [TESTING](TESTING.md) | Test suites, coverage gates, chaos and fuzz testing | Contributors |
| [DEPLOYMENT](DEPLOYMENT.md) | systemd install, secrets, backups, upgrades | Operators |
| [OPERATIONS](OPERATIONS.md) | Metrics, logs, degraded mode, incident runbook | Operators |
| [RESTORE](RESTORE.md) | Restore the SQLite diary from a backup; the restore drill | Operators |
| [nfr](nfr.md) | Non-functional budgets and their measured values | Reviewers |
| [CHANGELOG](../CHANGELOG.md) | Version history | Everyone |
| [CODE_OF_CONDUCT](../CODE_OF_CONDUCT.md) | Conduct and enforcement contact | Everyone |
| [Client README](../crates/opencode-client/README.md) | Usage of the `opencode-v2-client` library | Client consumers |
| [Client COMPAT](../crates/opencode-client/COMPAT.md) | Pinned opencode versions and the drift policy | Upgraders |

## Glossary

One name per concept.
These terms are normative across every document in this set.

| Term | Meaning |
|---|---|
| **operator** | The single allowed Discord user (`allowed_user_id`). The only trust principal. |
| **opencode server** | The local `opencode serve` v2 process the bot controls. Loopback + password. |
| **directory** | A working directory (usually a git repo) the bot runs prompts in. |
| **session** | One opencode conversation in a directory. A **root session** reports; a **child session** never does. |
| **action** | One bot-tracked run: a prompt sent, carried through the state machine (`staged → running → reported → …`). |
| **decision** | The operator's choice on a report: **Accept**, **Follow up**, **Park & redo**, or **Discard**. |
| **report** | The message posted when a tracked session goes idle, with an outcome: Completed, Errored, Aborted, or Empty. |
| **gate** | A session blocked on a human: a **permission request** or a **form**. One lifecycle for both. |
| **directory lock** | The per-directory mutex. At most one active action per directory; `/release` is the escape hatch. |
| **inbox** | Where completions of untracked (terminal-run) root sessions post. Replying to an inbox report **adopts** that session. |
| **guard** | The single authorization chokepoint every interaction passes (user ID + channel scope) before any handler runs. |
| **CAS transition** | A state change as `UPDATE … WHERE state = :from`; zero rows changed means someone else won. The exactly-once mechanism. |
| **reconcile** | Recovery from facts (session status, pending gates, database intent rows) at boot and after every reconnect — never from a cursor. |
| **wake** | A hint from the opencode event stream ("something about X changed"). Wakes carry no authority; each triggers at most one **fact read**. |
| **fallback tick** | The slow polling loops (gates, activity, root sessions) that bound latency if wakes stop. |

## Secondary terms

These are defined at first use rather than in the glossary:
**work branch** (ARCHITECTURE), **degraded mode** (GETTING_STARTED, OPERATIONS), **stall watchdog** (OPERATIONS), **drift test** (TESTING).

## Ownership rules

Each topic lives in exactly one document; every other document links to it.
The index above is the map.
If you add a topic, give it a home here first, then link to it from everywhere else.

High-traffic examples: config keys and env variables live in GETTING_STARTED; metric names live in OPERATIONS; version pins live in client COMPAT; NFR budgets live in nfr.md.
