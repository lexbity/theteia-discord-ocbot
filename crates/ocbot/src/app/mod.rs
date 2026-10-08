//! The app layer: guards, interaction vocabulary, router+handlers, watcher,
//! reconcile, completion, locks, metrics.

pub mod completion;
pub mod context;
pub mod error;
pub mod guard;
pub mod handlers;
pub mod interaction;
pub mod locks;
pub mod metrics;
pub mod reconcile;
pub mod router;
pub mod watcher;
