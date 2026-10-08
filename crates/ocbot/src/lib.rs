//! ocbot — the imperative shell (spec §6.1).
//!
//! Layering: `ocbot-core` (pure policy) ← traits ← this crate's app layer,
//! with adapters (opencode client, git, SQLite, serenity) wired through port
//! contracts. Nothing in this crate touches the core's decisions; it owns the
//! wiring, the store, the Discord UX, and the watcher.

pub mod adapters;
pub mod app;
pub mod config;
pub mod run;

/// The current bot identity, used in user-agent strings and logs.
pub const BOT_NAME: &str = "ocbot";
