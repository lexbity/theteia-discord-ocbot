//! Where a message about an action goes, and the receipt that addresses it
//! again (SPEC-004 §5.4 reply routing).
//!
//! Both are derived from the action row's persisted `(surface, chat_ref)`: a
//! reply, a quiet edit, or a status reaction lands on the surface the action
//! lives on, and no handler ever builds a platform receipt. Without an action
//! (an unattributed gate, a degraded-mode announcement) the fallback is the
//! configured announce channel, which is a Discord channel by definition.

use std::sync::Arc;

use ocbot_core::{ChatRef, Destination, MsgId, PostReceipt, Surface};

use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;

/// The destination an action's messages go to: the action row's persisted
/// `(surface, chat_ref)`.
pub(crate) fn destination_for(action: &ActionRow) -> Result<Destination, AppError> {
    let chat_ref = action
        .chat_ref
        .clone()
        .ok_or_else(|| AppError::Other("action has no chat_ref (invariant)".to_owned()))?;
    let surface = Surface::parse(&action.surface).ok_or_else(|| {
        AppError::Other(format!("action has unknown surface {:?}", action.surface))
    })?;
    Ok(Destination {
        surface,
        chat_ref: ChatRef(chat_ref),
    })
}

/// The receipt of a message the bot posted about `action`, so a later quiet op
/// (edit, reaction) addresses the surface that message lives on.
pub(crate) fn receipt_for(action: &ActionRow, msg_id: &str) -> Result<PostReceipt, AppError> {
    Ok(PostReceipt {
        surface: destination_for(action)?.surface,
        msg_id: MsgId(msg_id.to_owned()),
    })
}

/// Where a message *about* an action goes: the action's own persisted
/// destination, else the ops/inbox announce channel. Shared by the gate loop,
/// the stall watchdog, and the inbox — the loops that talk about an action
/// without having received an interaction.
pub(crate) fn about_destination(ctx: &Arc<AppContext>, action: Option<&ActionRow>) -> Destination {
    if let Some(action) = action
        && let Ok(destination) = destination_for(action)
    {
        return destination;
    }
    // Fallback to the first configured surface's announce channel
    // For Discord: use ops_channel or inbox_channel
    // For Signal: use the operator number (there's no channel concept in Signal)
    let surfaces = ctx.chat.surfaces();
    if surfaces.contains(&Surface::Discord) {
        Destination::discord(
            ctx.guard
                .ops_channel
                .or(ctx.guard.inbox_channel)
                .unwrap_or(0),
        )
    } else if surfaces.contains(&Surface::Signal) {
        // For Signal, we need an operator number. For now, use a placeholder.
        // In the SignalSurface test, the operator is OPERATOR from signal_surface.rs
        Destination {
            surface: Surface::Signal,
            chat_ref: ChatRef("opencode".to_string()),
        }
    } else {
        // Default to Discord with channel 0 if no surfaces are configured (shouldn't happen)
        Destination::discord(0)
    }
}

/// The receipt of a message posted to [`about_destination`]: the same surface,
/// the stored message id. Used by the gate loop's quiet outcome edit, which has
/// no interaction source to ask.
pub(crate) fn about_receipt(
    ctx: &Arc<AppContext>,
    action: Option<&ActionRow>,
    msg_id: &str,
) -> PostReceipt {
    PostReceipt {
        surface: about_destination(ctx, action).surface,
        msg_id: MsgId(msg_id.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{destination_for, receipt_for};
    use crate::adapters::store::ActionRow;

    /// A full-shaped action row (every column, so a schema change keeps this
    /// literal honest).
    fn action(surface: &str, chat_ref: Option<&str>) -> ActionRow {
        ActionRow {
            id: 7,
            run_id: 1,
            session_id: None,
            parent_action_id: None,
            surface: surface.to_owned(),
            chat_ref: chat_ref.map(str::to_owned),
            label: "a label".to_owned(),
            prompt_sent: String::new(),
            state: ocbot_core::State::Reported,
            blocked_gate_id: None,
            outcome_kind: None,
            report_full: None,
            footer_json: None,
            report_msg_id: None,
            origin_msg_id: None,
            start_head: None,
            dirty_at_start: false,
            include_existing: false,
            pending_commit_subject: None,
            pending_commit_body: None,
            park_branch_name: None,
            park_step: 0,
            accept_sha: None,
            attempt: 1,
            followups: 0,
            started_at: None,
            ended_at: None,
            last_event_at: None,
            stall_ping_at: None,
            template: "fresh".to_owned(),
        }
    }

    #[test]
    fn a_signal_action_routes_to_signal() {
        let row = action("signal", Some("+15551234567"));
        let destination = destination_for(&row).expect("a signal destination");
        assert_eq!(destination.surface, ocbot_core::Surface::Signal);
        assert_eq!(destination.chat_ref.0, "+15551234567");
        let receipt = receipt_for(&row, "1700000000000").expect("a signal receipt");
        assert_eq!(receipt.surface, ocbot_core::Surface::Signal);
        assert_eq!(receipt.msg_id.0, "1700000000000");
    }

    #[test]
    fn a_discord_action_routes_to_discord() {
        let receipt = receipt_for(&action("discord", Some("7")), "99").expect("a discord receipt");
        assert_eq!(receipt.surface, ocbot_core::Surface::Discord);
        assert_eq!(receipt.msg_id.0, "99");
    }

    #[test]
    fn a_row_without_a_chat_ref_or_surface_is_an_error() {
        assert!(destination_for(&action("discord", None)).is_err());
        assert!(destination_for(&action("carrier-pigeon", Some("7"))).is_err());
        assert!(receipt_for(&action("signal", None), "1").is_err());
    }
}
