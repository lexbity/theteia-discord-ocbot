## Target
`Config::validate()` and `projects_root` deserialization in `crates/ocbot/src/config.rs` (SPEC-005 implementation-order item 6).

## Dependents (at least 9)
- `crates/ocbot/src/run.rs`: constructs filesystem project adapters from validated configuration.
- `crates/ocbot/src/app/context.rs`: stores the configured project root.
- `crates/ocbot/src/app/handlers/run.rs`: identifies project-centric runs using the configured root.
- `crates/ocbot/src/adapters/projects.rs`: project discovery consumes the root and defaults.
- `crates/ocbot/src/config.rs`: `Config::load()` and unit tests call validation.
- `crates/ocbot/tests/config_surface.rs`: integration tests exercise shared config validation.
- `crates/ocbot/tests/boot_*.rs`, `doctor_signal.rs`, `surface_policy.rs`, `surface_registry.rs`: parse or validate configuration during startup-path tests.
- `config/config.toml.example`: operator-facing config reference (updated in follow-up item 7).

## Affected Stories
- No `specs/release-plan.yaml` or epic capsules exist in this repository. Scope is SPEC-005 implementation-order item 6 only.

## Test Coverage
- `crates/ocbot/src/config.rs` unit tests now cover omitted `projects_root`, existing/nonexistent/non-directory roots, project defaults, and empty defaults.
- `crates/ocbot/tests/config_surface.rs` covers other shared validation contracts; it currently lacks SPEC-005-specific coverage.
- Focused run executes all four SPEC-005 config tests and records coverage for each changed validation branch; the full config unit-test group reports 81.32% line coverage for `config.rs` (`cargo llvm-cov test -p ocbot --lib config::tests --summary-only`).

## Risk: High
`Config::validate()` is a shared startup contract used across the application and many integration tests; inaccurate changes can prevent every bot startup or allow invalid project settings through.

## Recommended action
Implemented: added focused config tests, the empty serde default, and non-contradictory root validation. Workspace tests/check, strict Clippy, and formatting pass; item 7 adds a config-example parse test. The full mutation-gate timeout is recorded below.

## Adjacent SPEC-005 review findings (not changed in slice 6)
- `crates/ocbot-core/src/render/projects.rs`: the project list renderer emits no components, so the documented “click a project” flow has no selection control.
- `crates/ocbot-core/src/ports.rs` + `crates/ocbot/src/adapters/projects.rs`: the spec's per-project config is nested under `[ocbot]`, but the adapter deserializes the file directly as a flat `ProjectConfig` with unknown fields denied. The documented format will fail to load.
- `config/config.toml.example`: the missing `projects_root` setting was resolved in implementation-order item 7.

## Follow-up: SPEC-005 implementation-order item 7
- Added the required `projects_root` and explicit default plan/README lists to `config/config.toml.example`.
- Added a parser test that reads the example file and verifies the documented values.
- `cargo fmt`, strict workspace Clippy, workspace check/tests, doc tests/docs, structural gates, dependency checks, typos, and the configured coverage gates pass.
- The full 352-mutant CI run exceeded the 15-minute execution window twice. A diff-scoped mutant run found 4 candidates: 2 were caught and 2 were unviable; the full mutation gate is not claimed as passing.
- Updated locked dependencies with available security fixes. `RUSTSEC-2026-0382` remains explicitly ignored for `rustls-webpki 0.102.8`, which is pinned transitively by Serenity 0.12.x; the 0.103 line and other affected dependencies are updated.
