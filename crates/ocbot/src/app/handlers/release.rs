//! `/release` (P5 escape hatch): list held locks, confirm, then release with an
//! audit line. `custom_id`s carry a directory hash, never the path, and the
//! mapping is re-resolved from the current lock set at click time (D-6).

use std::fmt::Write as _;
use std::sync::Arc;

use ocbot_core::{ButtonKind, OutgoingMessage, component_button, dir_hash};

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::InteractionSource;

/// Execute `/release`: list every held lock with a Release button.
pub async fn list(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<(), AppError> {
    let locks = ctx.store.locked_projects().await?;
    let mut content = String::from("**Locked directories** (P5)\n");
    let mut components = Vec::new();
    if locks.is_empty() {
        content.push_str("(none held)\n");
    }
    for project in &locks {
        let _ = writeln!(
            content,
            "• `{}` — holder {} since {}",
            project.dir,
            project.lock_holder.as_deref().unwrap_or("?"),
            project.lock_since.unwrap_or_default()
        );
        components.push(component_button(
            format!("Release {}", short_dir(&project.dir)),
            format!("v1:releaseq:{}", dir_hash(&project.dir)),
            ButtonKind::Danger,
        ));
    }
    let message = OutgoingMessage {
        content,
        attachment: None,
        components,
        mention_allowlist: crate::app::handlers::support::mention_allowlist_for(&source.user_id),
    };
    ctx.chat
        .post(
            &ocbot_core::Destination::discord(source.channel_id),
            &message,
        )
        .await?;
    Ok(())
}

/// Execute `v1:releaseq:<hash>`: post the confirmation.
pub async fn confirm(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    hash: &str,
) -> Result<Option<String>, AppError> {
    let dir = resolve(ctx, hash).await?;
    let holder = ctx.store.lock_holder(&dir).await?.unwrap_or_default();
    let message = crate::app::handlers::support::with_button(
        format!("Release the lock on `{dir}` (holder {holder})?"),
        "Confirm release",
        format!("v1:release:{hash}"),
        ButtonKind::Danger,
        &source.user_id,
    );
    ctx.chat
        .post(
            &ocbot_core::Destination::discord(source.channel_id),
            &message,
        )
        .await?;
    Ok(Some("confirm to release".to_owned()))
}

/// Execute `v1:release:<hash>`: release and audit.
pub async fn release(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    hash: &str,
) -> Result<Option<String>, AppError> {
    let dir = resolve(ctx, hash).await?;
    let holder = ctx
        .store
        .lock_holder(&dir)
        .await?
        .ok_or(AppError::AlreadyHandled)?;
    let released = ctx.store.release_lock(&dir, &holder).await?;
    if !released {
        return Err(AppError::AlreadyHandled);
    }
    ctx.store
        .audit(&source.user_id, "release", None, &dir)
        .await?;
    Ok(Some(format!("released `{dir}` (was {holder})")))
}

/// Resolve a `custom_id` directory hash against the current lock set.
async fn resolve(ctx: &Arc<AppContext>, hash: &str) -> Result<String, AppError> {
    for project in ctx.store.locked_projects().await? {
        if dir_hash(&project.dir) == hash {
            return Ok(project.dir);
        }
    }
    Err(AppError::NotFound(
        "no held lock matches that button".to_owned(),
    ))
}

fn short_dir(dir: &str) -> String {
    dir.rsplit('/')
        .next()
        .unwrap_or(dir)
        .chars()
        .take(24)
        .collect()
}
