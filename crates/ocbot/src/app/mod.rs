//! The app layer: guards, interaction vocabulary, router+handlers, watcher,
//! reconcile, completion, locks, metrics.

pub mod completion;
pub mod context;
pub mod crashpoint;
pub mod degraded;
pub mod doctor;
pub mod error;
pub mod gates_loop;
pub mod guard;
pub mod handlers;
pub mod inbox;
pub mod interaction;
pub mod locks;
pub mod logging;
pub mod metrics;
pub mod ops;
pub mod reconcile;
pub mod router;
pub mod stall;
pub mod supervisor;
pub mod wake;
pub mod watcher;
