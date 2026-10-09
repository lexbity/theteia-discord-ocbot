# opencode-v2-client — COMPAT / pinned-versions table

`opencode-v2-client` targets the opencode **v2** HTTP API, which is labeled
**experimental**. Upgrading opencode is therefore a deliberate act (spec P17):
bump the pinned version in this table, re-record fixtures, run the contract
suite.

**Current pin:**

| opencode server version | vendored spec | fixture set | client version |
|---|---|---|---|
| `2.0.24` | `spec/openapi-2.0.24.json` (117 paths / 141 ops, OpenAPI 3.1) | `fixtures/v2/2.0.24/` (19 files) | `0.1.0` |

**Toolchain pin:** `rust-toolchain.toml` → `1.98.1` (bumps only at phase
boundaries).

## What is covered

| Endpoint | Client method |
|---|---|
| `GET /api/info` | `Client::info`, `Client::probe` (version gate) |
| `GET /api/session` | `Client::list_sessions` |
| `GET /api/session/active` | `Client::active_sessions`, `Client::activity` |
| `GET /api/session/{id}` | `Client::get_session` |
| `POST /api/session` | `Client::create_session` |
| `POST /api/session/{id}/prompt` | `Client::send_prompt` (durable admission) |
| `POST /api/session/{id}/interrupt` | `Client::interrupt` |
| `GET /api/session/{id}/message` | `Client::messages`, `Client::last_assistant` |
| `GET /api/permission/request` | merged in `Client::pending_gates` |
| `POST /api/session/{id}/permission/{rid}/reply` | `Client::reply_permission` |
| `GET /api/form` | merged in `Client::pending_gates` |
| `POST /api/session/{id}/form/{fid}/reply` | `Client::reply_form` |
| `GET /api/experimental/session/{id}/log` | `Client::log_after` (SSE; sync-marker only in 2.0.24 — see SPIKE-0) |
| `GET /api/event` | `Client::subscribe_events` → typed tolerant `EventFrame` headers (`src/events.rs`); bodies never retained; undecodable frames counted and skipped (P-24); doorbell contract under SPEC-003, taxonomy in `devdocs/compat/EVENTS.md` |

The drift test (`tests/drift.rs`) fails CI if any of these paths disappears
from the vendored spec (AC-15).

## Upgrading the pin

1. Update the **Current pin** table (it is the pin record since W2; there is
   no separate version const).
2. Replace `spec/openapi-2.0.24.json` with the new version's spec; rename to
   `openapi-<version>.json` and update `tests/common/mod.rs` paths.
3. Re-record fixtures under `fixtures/v2/<version>/` with the spike.
4. Run the whole Phase 0 command list; update `SPIKE-0.md` with what changed.

## Tolerant-serde contract (normative)

- All API types use `#[serde(default)]`; unknown fields ignored.
- Every wire enum carries `#[serde(other)] Unknown`/`Other`.
- `deny_unknown_fields` (or any strict reject-unknown-fields mode) is banned in
  this crate's API types (gate G1).
- Hostile JSON must always produce `Result`, never panic — proven by proptest
  and the deterministic 10k-string battery (`tests/tolerant_serde.rs`) and by
  the nightly cargo-fuzz target (`fuzz/fuzz_targets/serde_fuzz.rs`).