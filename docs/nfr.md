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
| NFR-8 coverage/mutation | core ≥ 90% line+branch; proptest; scoped mutants | `cargo llvm-cov -p ocbot-core --fail-under-lines 90`: **94.72% lines / 93.92% regions**. `plan.rs` mutation run: 52 mutants, **0 missed**; `paths.rs`: 95/100 (residuals documented in COMPAT/PHASE-7). | 94.72% |
| NFR-9 observability | JSON logs; metrics on `127.0.0.1:9464`; every transition logged | `metrics.rs` asserts the HTTP endpoint exposes every §6.8 family; `logging.rs` writes JSON with a secret-scrubbing writer (`redaction.rs`). | endpoint test green |
| NFR-10 security gates | zero secrets in logs; spawn/exit confined; no `unwrap` in core; `clippy -D warnings` | `scripts/gates.sh` G3–G19; clippy restriction lints; `logging` scrub layer. | gates green |
| NFR-11 layering | `ocbot-core` declares no I/O deps | `gates.sh` G2: `cargo tree -e normal -p ocbot-core` fails on serenity/tokio/reqwest/rusqlite/opencode-v2-client. | gap 2 green |
| NFR-12 backup | nightly `VACUUM INTO`, 14-day retention, restore drilled | `deploy/ocbot-backup.{service,timer}` + `scripts/backup.sh`; `scripts/restore-drill.sh` run recorded in `docs/RESTORE.md`. | drill green |

## Notes on NFR-3

The duplicate budget is a property of the crash window, not of steady-state
operation: two Discord retries of the same button hit the CAS and lose;
two watcher observations of the same session hit `report_msg_id` and lose. The
only duplication path is a process death between "Discord accepted the POST" and
"SQLite txn committed" — milliseconds, once per crash. Losing a report is the
failure that matters, so at-least-once is the deliberate side of the trade.
