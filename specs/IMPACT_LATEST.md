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
- `config/config.toml.example`: operator-facing config reference (adjacent, but not changed in this slice).

## Affected Stories
- No `specs/release-plan.yaml` or epic capsules exist in this repository. Scope is SPEC-005 implementation-order item 6 only.

## Test Coverage
- `crates/ocbot/src/config.rs` unit tests now cover omitted `projects_root`, existing/nonexistent/non-directory roots, project defaults, and empty defaults.
- `crates/ocbot/tests/config_surface.rs` covers other shared validation contracts; it currently lacks SPEC-005-specific coverage.
- Focused run executes all four SPEC-005 config tests and records coverage for each changed validation branch; the full config unit-test group reports 81.32% line coverage for `config.rs` (`cargo llvm-cov test -p ocbot --lib config::tests --summary-only`).

## Risk: High
`Config::validate()` is a shared startup contract used across the application and many integration tests; inaccurate changes can prevent every bot startup or allow invalid project settings through.

## Recommended action
Implemented: added focused config tests, the empty serde default, and non-contradictory root validation. `cargo test --workspace`, `cargo check --workspace`, and `git diff --check` pass. Strict Clippy and workspace formatting remain blocked by unrelated existing findings.

## Adjacent SPEC-005 review findings (not changed in slice 6)
- `crates/ocbot-core/src/render/projects.rs`: the project list renderer emits no components, so the documented “click a project” flow has no selection control.
- `crates/ocbot-core/src/ports.rs` + `crates/ocbot/src/adapters/projects.rs`: the spec's per-project config is nested under `[ocbot]`, but the adapter deserializes the file directly as a flat `ProjectConfig` with unknown fields denied. The documented format will fail to load.
- `config/config.toml.example`: it does not yet include the now-required `projects_root` setting (implementation-order item 7 remains pending).
