//! P13 + SPEC-005 G-01: the rendered prompt is stored verbatim and reaches the
//! operator only through the Edit modal — the staged preview is opaque chrome
//! (label, project name, template) and never carries the prompt or a path.
//! Park & redo re-stages the *stored* prompt, never a re-render.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::{park, run};
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot_core::paths::project_display_name;
use ocbot_core::ports::OpenCodePort;

use common::{FakeChat, FakeGit, FakeOpenCode, append_file, context, init_repo, write_file};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn modal(custom_id: &str, values: &[(&str, &str)]) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: values
                .iter()
                .map(|(id, value)| TextInput {
                    id: (*id).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn make_repo(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    common::commit_all(&repo, "initial");
    repo
}

#[tokio::test]
async fn preview_hides_the_prompt_and_the_edit_modal_keeps_it_verbatim() {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = make_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode: Arc<dyn OpenCodePort> = Arc::new(FakeOpenCode::new());
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    run::execute_newrun(
        &ctx,
        &modal(
            run::NEW_RUN_MODAL,
            &[
                ("directory", &repo.to_string_lossy()),
                ("label", "verbatim"),
                ("plan", ""),
            ],
        ),
    )
    .await
    .expect("stage");
    let action = store
        .staged_actions()
        .await
        .expect("staged")
        .pop()
        .expect("action");
    let preview = discord.last_post().expect("preview").message;

    // The whole preview is opaque chrome: label, display name, template —
    // exactly, byte-for-byte, and nothing of the rendered prompt.
    let project = project_display_name(&repo);
    assert_eq!(
        preview.content,
        format!("**Ready to run: verbatim**\n_Project: {project}_\n_Template: fresh_"),
        "the preview is the opaque trio and nothing else"
    );
    assert!(
        !action.prompt_sent.is_empty(),
        "a rendered prompt exists to hide"
    );
    assert!(
        !preview.content.contains(&action.prompt_sent),
        "the prompt never rides the preview"
    );

    // P13's verbatim contract now lives where the operator actually needs it:
    // the Edit modal prefills the stored prompt exactly.
    let spec = run::edit_modal(&ctx, action.id)
        .await
        .expect("edit modal opens");
    assert_eq!(
        spec.inputs.first().and_then(|input| input.value.as_deref()),
        Some(action.prompt_sent.as_str()),
        "the Edit modal keeps the full stored prompt byte-for-byte"
    );
}

#[tokio::test]
async fn park_restages_the_stored_prompt_not_a_rerender() {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = make_repo(&tmp);
    let base = common::git_stdout(&repo, &["rev-parse", "HEAD"]);
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo.to_string_lossy(),
        Some("main"),
        Some(&base),
        CHANNEL,
        "work",
        "STORED-PROMPT-8f3",
        "ses_1",
    )
    .await;
    append_file(&repo, "README.md", "edit\n");

    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    park::begin_park(&ctx, seeded.action_id, &format!("discord:{OPERATOR}"))
        .await
        .expect("park");

    let staged = store.staged_actions().await.expect("staged");
    assert_eq!(staged.len(), 1);
    assert_eq!(
        staged[0].prompt_sent, "STORED-PROMPT-8f3",
        "park reuses the stored prompt"
    );
    assert_eq!(staged[0].template, "fresh", "the template travels with it");

    // The retry preview shows the same opaque chrome as any staging — the
    // stored prompt is verbatim in the row, never in the message.
    let preview = discord.last_post().expect("retry preview").message;
    assert!(
        !preview.content.contains("STORED-PROMPT-8f3"),
        "{}",
        preview.content
    );
    assert!(
        !preview
            .content
            .contains(&repo.to_string_lossy().into_owned()),
        "{}",
        preview.content
    );
    let project = project_display_name(&repo);
    assert!(
        preview.content.contains(&format!("_Project: {project}_")),
        "{}",
        preview.content
    );
    assert!(preview.content.contains("_Template: fresh_"));
}
