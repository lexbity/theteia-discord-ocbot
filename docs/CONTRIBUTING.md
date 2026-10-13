# Contributing

**Audience:** anyone who wants to change ocbot's code or docs.
**Purpose:** set up a dev environment, meet the quality floor, and get a PR merged.
**Prerequisites:** [Getting started](GETTING_STARTED.md) through Step 3 (a building workspace); [Architecture](ARCHITECTURE.md) for the mental model.

## Dev setup

1. Install [rustup](https://rustup.rs).
   The pinned toolchain (1.98.1) is selected automatically from `rust-toolchain.toml` — do not override it.
2. Clone and build:

   ```sh
   git clone <TODO: repo URL>
   cd ocbot
   cargo build
   cargo test --workspace
   ```
3. Install the auxiliary checkers CI uses (optional locally, required to reproduce CI):

   | Tool | What it does | Install |
   |---|---|---|
   | cargo-nextest | Faster test runner | `cargo install cargo-nextest --locked` |
   | cargo-deny | Advisory/license/ban checks | `cargo install cargo-deny --locked` |
   | cargo-machete | Unused dependency check | `cargo install cargo-machete` |
   | cargo-llvm-cov | Coverage | `cargo install cargo-llvm-cov` |
   | cargo-mutants | Mutation testing | `cargo install cargo-mutants` |
   | typos-cli | Docs spelling | `cargo install typos-cli` |

## Where things live

| You are changing… | It lives in… |
|---|---|
| Decisions: state machine, classification, rendering, policies | `crates/ocbot-core/src/` — pure Rust, no I/O allowed |
| The opencode client | `crates/opencode-client/src/` — published as `opencode-v2-client` |
| Discord adapter, handlers, loops, store, bootstrap | `crates/ocbot/src/` |
| Structural security gates | `scripts/gates.sh` |
| Config keys | `crates/ocbot/src/config.rs`; the example in `config/config.toml.example` |

Docs have one home per topic — see [the index](README.md#ownership-rules).
The [glossary](README.md#glossary) is normative: use its names in code comments, docs, and error strings.

## The lint floor

The floor is defined in the workspace `Cargo.toml` and enforced with `-D warnings` in CI.

- `unsafe` is forbidden everywhere.
- In production code: `unwrap`, `expect`, `panic`, `todo!`, `unimplemented!`, `unreachable!`, `dbg!`, `print!`, and `as`-conversions are denied.
  Return `Result`; type-convert explicitly.
- `missing_docs` is a warning; public items need doc comments.
- clippy default groups are denied; `pedantic` and `cargo` groups warn.
- Every `#[allow]` needs a `reason`.
- Tests are exempt by design: `allow-unwrap-in-tests` is set in `clippy.toml`, and integration tests carry a crate-level allow with a reason — tests assert by unwrapping.

`clippy.toml` adds structural bans: `std::process::Command::new`, `tokio::process::Command::new`, and `std::process::exit` are disallowed methods — process spawning belongs to the git adapter, exit codes to `main`'s doctor path.

Formatting: `rustfmt` with edition 2024, `max_width = 100`, stable options only.
Run `cargo fmt --all` before committing.

## The gates

`./scripts/gates.sh` is the executable spec: structural checks that lint cannot express, G1–G20.
Every detector registers a probe — a snippet that must trigger it — so pattern rot turns CI red instead of silently passing.
Highlights:

| Gate | Enforces |
|---|---|
| G1 | No `deny_unknown_fields` in the client — unknown wire fields are ignored, not rejected |
| G2 | `ocbot-core` has no I/O dependency (`cargo tree` — the compiler-enforced layering) |
| G3 | No `unwrap`/`expect` in core production source (test blocks excluded) |
| G7/G11 | Process spawning is confined to the git adapter |
| G8/G15 | No shell interpreter is spawned anywhere in the workspace |
| G9 | The store is born with WAL mode |
| G10 | The bot never pushes and never `--force`s |
| G12 | The shipped permission baseline guards the dangerous families |
| G13/G14 | `per_session_ruleset` and `allow_freeform_paths` ship default-off |
| G17 | No secret value is committed |
| G18 | The metrics listener binds loopback only |
| G19 | The deployment units run unprivileged and memory-capped |
| G20 | Event frames are treated as untrusted input |

Run the whole local suite before pushing:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
./scripts/gates.sh
```

## Commits and branches

[ASSUMED] Trunk-based development: short-lived branches, pull requests into `main`; CI runs on both.
Commit style follows the existing history: an imperative summary line of at most 72 characters, a blank line, then an optional body explaining why.
Examples from the log:

```text
Supervisor: one connection, owned honestly
store actor cleanup
parse event endpoint into EventFrame headers with telemetry
```

No strict Conventional Commits prefix scheme — the summary sentence is the convention.

## CI and the PR checklist

CI runs on every push to `main` and every pull request.

| Job | Runs |
|---|---|
| `fmt` | `cargo fmt --all -- --check` |
| `lint` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `test` | `cargo nextest run --workspace` + doc tests |
| `deny` | `cargo deny check` (advisories, licenses, bans) |
| `machete` | Unused dependencies |
| `doc` | `cargo doc --no-deps` with warnings denied |
| `gates` | `./scripts/gates.sh` |
| `coverage` | Per-crate floors — see [TESTING](TESTING.md#coverage) |
| `mutants` | Mutation check scoped to core decision modules |
| `typos` | `typos docs README.md` |
| `client-publish-dry-run` | `cargo publish --dry-run -p opencode-v2-client` |

A nightly job (schedule or manual dispatch) runs fuzzing, the live contract suite against a real opencode server, the full core mutation run, and the real-SIGKILL chaos half.

Before you open a PR, check locally: fmt, clippy, nextest, gates, and coverage for the crates you touched.
Keep PRs small; one behavior per PR.
Testing expectations live in [TESTING](TESTING.md) — name the test you added for the behavior.

## Reporting bugs and proposing features

Open an issue at [TODO: repo URL]/issues with:

- what you sent (command, button) and what the bot replied,
- `/status` output,
- relevant log lines — logs are already secret-scrubbed, but double-check pastes,
- opencode server version (the pin matters — see [client COMPAT](../crates/opencode-client/COMPAT.md)).

Feature proposals: state the operator workflow you want, not the implementation.
Single-operator scope is a design constraint, not an oversight — see the non-goals in [ARCHITECTURE](ARCHITECTURE.md).

Security issues: do not open a public issue.
[TODO: private disclosure contact]

## Code of conduct

Everyone participating follows [CODE_OF_CONDUCT](../CODE_OF_CONDUCT.md).
