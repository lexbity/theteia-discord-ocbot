# ocbot — a deterministic Discord controller for a local opencode v2 server

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
