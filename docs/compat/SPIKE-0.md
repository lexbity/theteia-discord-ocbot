# SPIKE-0 — Contract spike record (Phase 0)

**Date:** 2026-10-07 · **Server:** `opencode serve` **2.0.24** (background service on `127.0.0.1`, password auth)
**Client crate:** `opencode-v2-client` v0.1.0 (workspace dir `crates/opencode-client`)

This record converts §2's assumption table into **pinned facts**, with the
live transcript in `spike-transcript.txt`. Everything a later phase builds is
code first, then this file is the "why".

---

## 1. Live transcript summary

The spike CLI (`crates/opencode-client/examples/spike.rs`) ran against the live
pinned server and passed (`SPIKE-0 PASS`, exit 0):

1. `GET /api/info` → `{"version":"2.0.24", ...}` (basic auth, username `opencode`).
2. Root-session listing works; 50 roots returned (the operator's real server).
3. `POST /api/session` with `{"title": ..., "location": {"directory": ...}}` created a session bound to a directory (**A6 confirmed**).
4. `GET /api/experimental/session/{id}/log` returned exactly one SSE frame: the `log.synced` head marker (see S-2).
5. `POST /api/session/{id}/prompt` **admitted durably** (200, `{"data": {...}, "delivery": "steer"}`), returned in 0.14 s — not waiting for completion (**P9 "non-blocking admission" confirmed**).
6. Idle was detected by polling `GET /api/session/active` (session left the map → idle). The global `/api/event` SSE stream (volatile) delivered the full lifecycle: `session.inbox.enqueued → session.execution.started → session.text.started/delta/ended → session.step.* → session.usage.updated → session.execution.succeeded`.
7. `GET /api/session/{id}/message?type=assistant` returned the last assistant message incl. `finish`, `cost`, `tokens`, `error` (nullable), `content` (**A7 confirmed** for tokens/cost/error; timings available via `time.{created,streamed,completed}`).
8. `GET /api/permission/request` and `GET /api/form` returned `{"location": {...}, "data": []}` with no pending gates (**A5/A12 list shape confirmed**).

---

## 2. Assumption table → pinned facts

| ID | Status | Pinned fact (SOURCE) |
|---|---|---|
| A1 | **Confirmed** | OpenAPI 3.1 spec vendored at `spec/openapi-2.0.24.json`; 117 paths / 141 ops; title `opencode HttpApi`. |
| A2 | **Refined** | `session.log` exists and always emits a `log.synced` head marker carrying the current durable `seq` — **but neither replays items for `after=<seq>` nor streams deltas under `follow=true`** in 2.0.24 (tested exhaustively: `after` 0/1/11/12/18/19/20, with and without follow; only the sync marker returned while the head seq advanced 0→12→19→28). The volatile global `/api/event` stream carries `durable {aggregateID, seq, version}` on execution events. **Locked default (S-2):** the bot's watcher uses `/api/event` for live events + status/message polling as the durable signal; the per-session log cursor is an optimization only (P9) and its sync marker is used purely for seq awareness. |
| A3 | **Confirmed (fallback locked S-2)** | Idle derives from `session.active` membership + idle messages (`type=idle`, `outcome`). The experimental `session.wait` endpoint exists but is not needed. |
| A4 | **Confirmed** | `Session.Info.parentID` present; `GET /api/session?parentID=null` returns root sessions only. |
| A5 | **Confirmed** | `GET /api/permission/request` lists pending requests (fields: `id`, `sessionID`, `action`, `resources`, `save`, `metadata`, `message`, `source`). Reply is **per-session**: `POST /api/session/{sessionID}/permission/{requestID}/reply` with `{"decision": "once"\|"always"\|"reject"}` (**S-3 wire values pinned**); 204 on success; 404 on unknown request. |
| A6 | **Confirmed** | One server, many directories via `location.directory` at session create and the `directory=`/`location[directory]` list params. |
| A7 | **Confirmed** | Assistant messages expose `tokens` (input/output/reasoning/cache), `cost`, `finish`, `rawFinish`, `error` (nullable), `time`; title is on `Session.Info`. |
| A8 | **Confirmed** | `POST /api/session/{id}/interrupt` returns `{"interrupted": bool}` — `false` for the already-idle no-op (**S-8 pinned**: abort-on-idle is a clean no-op, no error). |
| A9 | **Confirmed** | HTTP Basic with username `opencode` and the server password; 401 with `{"_tag":"UnauthorizedError","message":...}` otherwise. Unauthenticated `/api/info` → 401. |
| A10 | **Confirmed** | `Session.Info.title` readable (nullable). |
| A11 | **Unresolved** | `Session.Info.permissions` (a `Permission.Ruleset` array) is accepted at create; **combine-vs-override semantics still require S-3 testing with a per-session ruleset against a global config**. `PER_SESSION_RULESET=false` stays (D-R6). |
| A12 | **Confirmed-ish** | `GET /api/form` + `GET /api/session/{id}/form` list pending forms (`Form.Info`: `id`, `sessionID`, `title`, `metadata`, `fields`); reply `POST /api/session/{id}/form/{formID}/reply` with `{"answer": {...}}`; field types string/number/integer/boolean/multiselect/external (**S-4 wire schema pinned**). No live form was raised in the spike; fixture is schema-derived and D-R5 surface-only degrade remains for anything unexpected. |
| A13 | **Not load-bearing** | `seq` on `session.log` sync markers is useful metadata; the cursor is a cache (P9). **No correctness guarantee depends on it.** |

**Architecture consequence (Rev B §2):** all Rev A forking assumptions A4/A5/A6
are confirmed; the only refinement this spike adds is that the live event bus
is the global `/api/event` stream — a watcher-shape change in `ocbot`'s
adapter, **not** a state-machine change.

---

## 3. S-pin table (locked defaults recorded, §12 of the spec)

| Spike | Outcome / locked default |
|---|---|
| S-1 (crate name availability) | `opencode-v2-client` published under that name; `ocbot-client` is the recorded fallback. |
| S-2 (log transport/schema; seq across restarts) | SSE confirmed. Replay/stream of the per-session log does not emit beyond the sync marker in 2.0.24 → **fallback engaged**: messages-poll (2 s) + `/api/event` live stream; `seq` treated as cache-only. |
| S-3 (gate reply values; combine semantics) | `once`/`always`/`reject` confirmed verbatim. Combine-vs-override: **unconfirmed** → FR-42 stays off. |
| S-4 (form payload schema) | Schema pinned (see §A12). |
| S-5 (tokens/cost/error/title) | Confirmed; footer will render real values. |
| S-6 (proof fixture) | Recorded: `fixtures/v2/2.0.24/` (18 files) + vendored spec. |
| S-7 (global discovery) | Root-session list poll is the durable discovery; `/api/event` is the live stream (volatile per its own summary). Default 5 s poll. |
| S-8 (interrupt-when-idle) | `{"interrupted": false}` — clean no-op. |
| S-9 (serenity stable vs next) | Deferred to Phase 2; recorded decision is stable 0.12.x. |

---

## 4. Review findings resolved in this phase

1. **Crate name/directory mismatch (D-R4 vs Phase 0 text):** Cargo requires the
   package name == published name. Resolved: package is `opencode-v2-client`,
   directory is `crates/opencode-client/` as D-R4 says. The NFR-11 tree gate
   greps for `opencode-v2-client`.
2. **`session.log` does not replay/stream in 2.0.24** (S-2 above). Watcher uses
   `/api/event`; the durable-log cursor is metadata only. No state-machine
   impact (P9 was designed for this).
3. **Spec `openapi.json` vs "filename carries version":** vendored as
   `spec/openapi-2.0.24.json` (both requirements honored: it is the spec, and
   the filename pins the version).
4. **jsonschema validation of `{sessionID}` pointers:** `{`/`}` are invalid in
   URI fragments; the contract test percent-encodes pointer fragments.
5. **clippy "teeth" vs test code:** `panic`/`unwrap`/`expect` bans are scoped
   out of integration-test crates with explicit reasons at the top of each
   test file (spec C6's `allow-*-in-tests` covers unit tests; production code
   remains strict, enforced by workspace lints + G3).