# ocbot — a deterministic Discord controller for a local opencode v2 server

**Status:** Phase 2 of SPEC-001 Rev B (`devdocs/opencode-discord-bot-spec-v2.md`)
implemented: the tooling bootstrap, the owned tolerant `opencode v2` client, the
pure policy core, and the running shell spine (config, store, guard/router,
Discord adapter with structurally-enforced `allowedMentions`, watcher, `/run`,
`/status`, reports).

```
[Discord] ──▶ ocbot ──loopback+password──▶ opencode serve v2 ──▶ [host: repos, shell]
                  │
             SQLite (diary)  ·  git (truth)
```

## Workspace

| Crate | Role | Status |
|---|---|---|
| `crates/ocbot-core` | pure policy core (state machine, outcome, render, templates, ports) | **implemented** (Phase 1 surface, required by Phase 2) |
| `crates/opencode-client` | published `opencode-v2-client` — ~14-endpoint tolerant v2 client | **implemented** (Phase 0) |
| `crates/ocbot` | shell spine: config, SQLite actor, guard/router, serenity adapter, watcher, `/run`+`/status` | **implemented** (Phase 2; decision verbs arrive Phase 3) |
| `fuzz/` | cargo-fuzz hostile-JSON targets (nightly) | authored |

## Phase progress

- **Phase 0 (client + tooling):** done — lint floor, contracts (vendored
  `openapi-2.0.24.json` + fixtures + drift test), live spike recorded.
- **Phase 2 (shell spine + the core it needs):** done — `docs/compat/PHASE-2.md`,
  live transcript `docs/compat/phase2-live-transcript.txt`.
- Next: Phase 3 (git truth: accept/park/reset + chaos), Phase 4 (gates),
  Phase 5 (sessions/inbox), Phase 6 (files/plans), Phase 7 (hardening),
  Phase 8 (publish).

## Command list (all green locally)

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
cargo machete
./scripts/gates.sh
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo llvm-cov -p ocbot-core --fail-under-lines 90 --summary-only   # ≥90% lines
cargo publish --dry-run -p opencode-v2-client
```

## Live proofs

- Client spike: `crates/opencode-client/examples/spike.rs` (recorded in
  `docs/compat/spike-transcript.txt`).
- Phase 2 spine (headless, real server): `crates/ocbot/tests/live_manual.rs`
  (`--ignored`), transcript in `docs/compat/phase2-live-transcript.txt`.

## Key documents

- Spec: `devdocs/opencode-discord-bot-spec-v2.md`
- Phase records: `docs/compat/SPIKE-0.md` (client pinning), `docs/compat/PHASE-2.md`
- Version matrix: `crates/opencode-client/COMPAT.md`
- Ops inventory: `docs/compat/inventory.md`