# PHASE-2 — Shell spine: config, store, guard, Discord, watcher, `/run`, reports

**Record date:** 2026-10-07 · **opencode pin:** 2.0.24 · **toolchain:** 1.98.1

Phase 2 delivers the running shell spine with the whole `ocbot-core` policy
layer it compiles against (the Phase-1 pure modules — state machine, outcome
classifier, render/truncation, strict template engine, port contracts — were
implemented fully because Phase 2's spine depends on them; nothing is stubbed).

## What is real now

| Slice | Delivered |
|---|---|
| Config | `config.toml` (`deny_unknown_fields`), **collect-all** validation, FR-4 loopback guard, env + 0600 secrets file, load-time template slot validation |
| Store | rusqlite actor on one thread (P18): WAL, CAS via `rows_changed`, atomic send+lock transaction (P5), exactly-once report delivery (`report_msg_id IS NULL` guard), full §6.3 schema (v1) + the recorded `action.channel_id` delta |
| Guard | single chokepoint; AC-1 proven: every handler × wrong-user/foreign-channel ⇒ zero port calls |
| Router | two-phase plan/execute (ack before slow work), `custom_id` grammar (`v1:send/runany/edit/editgo/newrun`) |
| `/run` | directory+label modal → template-rendered preview → Send/Edit&send → session create → prompt admit → FR-27 dirty warning with Run-anyway, P5 lock refusal naming the holder |
| Watcher/Reconcile | status-facts reconciliation (P9): busy ⇒ keep watching, idle & unreported ⇒ deliver once, premarked ⇒ never twice; `/api/event` deferred to Phase 7 (S-2 default: poll) |
| Discord adapter | serenity 0.12 (intents per FR-2), single `message_builder` that always sets `allowed_mentions{parse:[],users:[OPERATOR]}` (payload-serialization tested), loopback metrics endpoint |
| `ocbot-core` | transition table, `classify` (blocked/aborted/errored/completed/empty), fence-neutralized truncated reports with inline footer, strict slot templates, port traits |

## Spikes / findings pinned this phase

1. **`tokio::process::Command` + `kill_on_drop(true)` + `wait_with_output()` returns empty stdout** — the drop kills the child before the pipes drain. The git adapter now uses `Command::output()` under a timeout; on timeout the (rare) child self-terminates via SIGPIPE (documented in `adapters/git.rs`).
2. **crossbeam `select!` panics if a `SelectedOperation` is dropped uncompleted** — the store's shutdown branch must `recv` before `break`.
3. **`run.dir` FK → `project(dir)`** — every test seeds the project row (tests caught a real invariant).
4. **Atomicity of send+lock (P5)** — the lock refusal must roll back the staged→running CAS, or a loser becomes `running` with no lock (a correctness bug the store test caught).
5. **Serenity 0.12.5 forces `rustls 0.22 → webpki 0.102.8`** (tokio-tungstenite 0.21). Four webpki advisories have no 0.102-line fix; native-tls is banned (openssl). Recorded as a reasoned advisory ignore in `deny.toml`; revisited on any serenity bump (S-9).
6. **Exactly-once shape** — the DB guard lives in the completion path (`report_msg_id.is_some() ⇒ no post`, P9 step 3) plus the `deliver_report IS NULL` CAS. Both layers are proven: the mutation check flips the guard and `reconcile_boot` goes red; cargo-mutants caught 9/29 completion mutants.

## Exit criteria met

- e2e green: `run_to_report_end_to_end` (real git + wiremock opencode + fake Discord) proves busy⇒idle⇒exactly-one report with footer, FR-27, and P5 lock refusal; AC-14 case in `reconcile_boot`.
- Restart/mid-run exactly-once: `boot_reconcile_delivers_exactly_once` + `premarked_report_is_never_reposted`.
- `discord_payload.rs`: every mounted payload serializes `allowed_mentions` to `{parse:[],users:[OPERATOR]}`.
- `git_readonly.rs`: real temp repos + scripted-fake-git timeout (R-9).
- **Live manual run recorded** (`phase2-live-transcript.txt`): the real spine (real client + real git + real store) ran a full `/run` against opencode 2.0.24 and delivered the report exactly once with a tangible footer.
- All phase commands green (fmt/clippy/test/deny/machete/doc/gates), coverage ≥ 90% for `ocbot-core`, mutants run with 9/29 caught and the named dedupe-mutation provably flips `reconcile_boot`.

## Not in this slice (next phases)

Decision buttons (Phase 3), park/reset/accept git write verbs (3), gates lifecycle (4), abort/follow-up/inbox/multi-dir (5), files/plans (6), hardening/metrics completeness (7).