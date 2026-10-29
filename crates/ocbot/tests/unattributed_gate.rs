//! The unattributed-announcement fallback (SPEC-004 §5.4): a message about no
//! action — a gate for a session the bot did not start — lands on a surface
//! that is actually wired, not on a Discord channel the deployment does not
//! have. A signal-only box is the shape that used to lose these messages: the
//! fallback was `discord(ops_channel)` by definition, and the fan-out had no
//! Discord port to deliver through.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::signal_fake::Script;
use common::signal_surface::SignalSurface;
use common::{FakeGit, permission_gate};
use ocbot::app::gates_loop;

#[tokio::test]
async fn an_unattributed_gate_reaches_the_signal_surface() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    // A gate for a session the bot did not start: attribution finds no running
    // action, so delivery falls back to the context's announce destinations —
    // here, the Signal operator chat.
    surface.opencode.set_gates(vec![permission_gate(
        "per_external",
        Some("ses_external"),
        "bash",
        &["echo hi"],
    )]);

    gates_loop::sync(&surface.ctx).await.expect("sync");
    surface.wait_sent("permission needed").await;

    // The re-ping re-derives the destination through the same fallback (the
    // row still has no action to follow).
    surface.clock.advance(10_000);
    gates_loop::reping(&surface.ctx, 1).await.expect("re-ping");
    surface.wait_sent("still waiting").await;
}
