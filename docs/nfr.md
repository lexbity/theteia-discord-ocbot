# NFR measurements & enforcement

Measured on the development host (Linux, sqlite 3.53.4, toolchain 1.98.1) on
2026-10-08. Each row names the mechanism that *enforces* the budget and, where
it is a runtime number, the command that produced it.

| NFR | Budget | Mechanism / measurement | Recorded |
|---|---|---|---|
| NFR-1 interaction ack | p99 < 1 s to defer; never > 3 s | The adapter defers **before** any handler work (ack discipline, §6.6), and the pre-defer plan phase is one SQLite read with no port call (`guard.rs` AC-1). `e2e_report_latency::plan_phase_stays_under_the_ack_budget` measures it over 1,000 button interactions and holds p99 < 50 ms. | p99 < 50 ms |
| NFR-2 idle→report | p95 < 5 s (polling fallback ≤ interval + 2 s) | `cargo test -p ocbot --test e2e_report_latency`: 100 cycles through the real client + adapter against wiremock; the test asserts p95 < 5 s. Suite wall time 0.24 s for 100 cycles ≈ **2.4 ms/cycle** (≈3 orders under budget). | 0.24 s / 100 cycles |
| NFR-3 report delivery | at-least-once; < 1 duplicate / 200 | P9: the `report_msg_id IS NULL` CAS plus `notified` dedupe keys make detection exactly-once; only the Discord POST↔txn crash gap can duplicate (once per crash, never steady-state). `deliver_report` is covered by `store_cas::deliver_report_is_exactly_once` and `reconcile_boot`. | design + tests |
| NFR-4 decisions exactly-once | 0 duplicate commits / 1,000 double-taps | `cargo test -p ocbot --test double_tap`: 1,000 randomized interleavings, exactly one commit (AC-5). | 1,000 trials green |
| NFR-5 crash recovery | SIGKILL anywhere ⇒ consistent < 10 s, no lost work | `chaos_full` (11 labelled CP points, snapshot half) + `chaos_sigkill` (real SIGKILL at accept/park/reset). Boot reconcile is synchronous; the suites finish in < 0.1 s. | 14 chaos tests green |
| NFR-6 memory | < 200 MB RSS steady | `scripts/measure-rss.sh` releases the binary against a dead opencode and reads `VmRSS`. **19,988 kB** at 3 s. `deploy/ocbot.service` enforces `MemoryMax=200M` as the ceiling. | ~20 MB |
| NFR-7 availability | single process, systemd `Restart=on-failure`, 99%/month | `deploy/ocbot.service`, `deploy/opencode.service`; no HA (self-hosted, single operator). | units committed |
| NFR-8 coverage/mutation | core ≥ 90% line+branch; proptest; scoped mutants | `scripts/coverage.sh` gates **core ≥ 90, client ≥ 90, shell ≥ 70** (per-crate line coverage). Core is 94.72% by its own tests; mutation: `plan.rs` 52 mutants **0 missed**, `paths.rs` 95/100, `stall.rs` 10/10 (residuals in COMPAT/PHASE-7). | gates green |
| NFR-9 observability | JSON logs; metrics on `127.0.0.1:9464`; every transition logged | `metrics.rs` asserts the HTTP endpoint exposes every §6.8 family; `logging.rs` writes JSON with a secret-scrubbing writer (`redaction.rs`). | endpoint test green |
| NFR-10 security gates | zero secrets in logs; spawn/exit confined; no `unwrap` in core; `clippy -D warnings` | `scripts/gates.sh` G3–G19; clippy restriction lints; `logging` scrub layer. | gates green |
| NFR-11 layering | `ocbot-core` declares no I/O deps | `gates.sh` G2: `cargo tree -e normal -p ocbot-core` fails on serenity/tokio/reqwest/rusqlite/opencode-v2-client. | gap 2 green |
| NFR-12 backup | nightly `VACUUM INTO`, 14-day retention, restore drilled | `deploy/ocbot-backup.{service,timer}` + `scripts/backup.sh`; `scripts/restore-drill.sh` run recorded in `docs/RESTORE.md`. | drill green |

## Coverage by crate

Measured per crate (each runner's own tests) and gated by `scripts/coverage.sh`
(2026-10-08). The gate floor is per crate: a crate's gate reflects its own tests,
not coverage donated by another crate's integration suite.

| Crate | Line coverage (own tests) | Gate | Merged |
|---|---:|---:|---:|
| `ocbot-core` | **94.7%** | ≥ 90 | 96.7% |
| `opencode-v2-client` | **93.7%** | ≥ 90 | 95.2% |
| `ocbot` (shell) | **77.8%** | ≥ 70 | 76.9% |
| workspace total | — | — | **82.1%** |

The "merged" column is `cargo llvm-cov --workspace`, where shell integration
tests also cover core rendering/state.

The shell's remaining misses are dominated by code that cannot be exercised in
a unit/integration process without a live environment:

- `run.rs` (bootstrap), `main.rs` — process wiring; only meaningful against real
  secrets, a real opencode and a real Discord gateway.
- `adapters/discord.rs` — the serenity gateway event loop; its pure payload
  builder (`message_builder`, `operator_allowed_mentions`) *is* covered by
  `discord_payload.rs`.
- `harness.rs` / `bin/ocbot-crash-harness.rs` — executed in a separate process by
  the real-SIGKILL chaos tests.
- `crashpoint.rs` — the env-gated checkpoint hook, likewise a child process.

`tests/boot_matrix.rs` spawns the real binary for the first-boot failure matrix
(config / secrets / V1 refusal / bad baseline), so part of `run.rs` is covered
through the child's inherited profile.

Everything else (router, store, all handlers, gates, watcher, ops, degraded,
stall, redaction, metrics) has direct tests. The coverage pass added
`opencode_adapter.rs` (every `OpenCodePort` method through the real adapter),
`router_grammar.rs` (the full interaction grammar), `shell_handlers.rs`
(`/status`, `/mute`, Discard, `/release`, ops tick, watcher), and
`opencode-client/tests/streams.rs` (all SSE paths).

## Notes on NFR-3

The duplicate budget is a property of the crash window, not of steady-state
operation: two Discord retries of the same button hit the CAS and lose;
two watcher observations of the same session hit `report_msg_id` and lose. The
only duplication path is a process death between "Discord accepted the POST" and
"SQLite txn committed" — milliseconds, once per crash. Losing a report is the
failure that matters, so at-least-once is the deliberate side of the trade.
