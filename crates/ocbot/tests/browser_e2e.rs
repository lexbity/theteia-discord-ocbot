//! `/ls` browser e2e (FR-34): navigation, paging, selection, and the selected
//! path flowing into the `/run` modal's plan prefill. The browser is rooted and
//! drives a real (permissive) fs adapter over a temp tree.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::{ls, run};
use ocbot::app::interaction::{InteractionSource, SourceKind};
use ocbot_core::ports::OpenCodePort;

use common::{FakeChat, FakeGit, FakeOpenCode, context};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn slash(name: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: name.to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn button(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(10),
        is_dm: false,
    }
}

async fn entry_index(ctx: &Arc<ocbot::app::context::AppContext>, dir: &Path, name: &str) -> usize {
    let entries = ctx.fs.list(dir, false).await.expect("list");
    entries
        .iter()
        .position(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("{name} missing"))
}

#[tokio::test]
async fn browse_pages_descends_and_selects_into_the_run_prefill() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join("pkg")).expect("pkg");
    std::fs::write(root.join("note.md"), "# note\n").expect("note");
    for index in 0..25 {
        std::fs::write(root.join(format!("f{index:02}.txt")), "x").expect("file");
    }

    let store = StoreHandle::open_in_memory().expect("store");
    let opencode: Arc<dyn OpenCodePort> = Arc::new(FakeOpenCode::new());
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    store
        .set_ui_cwd(&format!("discord:{OPERATOR}"), &root.to_string_lossy())
        .await
        .expect("cwd");

    // `/ls` posts the first page with navigation buttons.
    ls::execute_ls(&ctx, &slash("ls")).await.expect("ls");
    let first = discord.last_post().expect("browser posted").message;
    let ids: Vec<&str> = first
        .components
        .iter()
        .map(|component| component.custom_id.as_str())
        .collect();
    assert!(ids.contains(&"v1:ls:up"));
    assert!(ids.contains(&"v1:ls:use"));
    assert!(ids.contains(&"v1:ls:p:1"), "second page offered");
    assert!(first.content.contains("page 1/2"), "{}", first.content);

    // Page forward: the edit replaces the message in place.
    ls::page(&ctx, &button("v1:ls:p:1"), 1).await.expect("page");
    let edited = discord.edits().pop().expect("edit").2;
    assert!(edited.content.contains("page 2/2"), "{}", edited.content);
    assert!(
        edited
            .components
            .iter()
            .any(|component| component.custom_id == "v1:ls:p:0"),
        "back to page 1"
    );

    // Select a file (still at the root): its path becomes the draft.
    let note_index = entry_index(&ctx, &root, "note.md").await;
    let ephemeral = ls::select(&ctx, &button(&format!("v1:lssel:{note_index}")), note_index)
        .await
        .expect("select")
        .expect("ephemeral");
    assert!(ephemeral.contains("note.md"), "{ephemeral}");
    let draft = store
        .draft_path(&format!("discord:{OPERATOR}"))
        .await
        .expect("draft")
        .unwrap();
    assert!(draft.ends_with("note.md"));

    // Descend into `pkg`: the cwd follows the open.
    let pkg_index = entry_index(&ctx, &root, "pkg").await;
    ls::open(&ctx, &button(&format!("v1:ls:open:{pkg_index}")), pkg_index)
        .await
        .expect("open");
    assert_eq!(
        store
            .ui_cwd(&format!("discord:{OPERATOR}"))
            .await
            .expect("cwd"),
        Some(root.join("pkg").to_string_lossy().into_owned())
    );

    // The `/run` modal prefills its plan field with the selected path.
    let modal = run::begin_modal(&ctx, &slash("run")).await.expect("modal");
    let plan_input = modal
        .inputs
        .iter()
        .find(|input| input.id == "plan")
        .expect("plan input");
    assert_eq!(plan_input.value.as_deref(), Some(draft.as_str()));
}
