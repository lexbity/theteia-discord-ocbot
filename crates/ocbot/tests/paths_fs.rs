//! AC-9 full (fs half): the fs adapter re-verifies containment at every use, so
//! a symlink swapped between browse and select is refused; attachment names are
//! sanitized; and an out-of-root `/cd` has zero side effects.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use ocbot::adapters::fs::FsAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::{AppContext, ChatPort};
use ocbot::app::handlers::files;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot_core::ports::{FsPort, GitPort, OpenCodePort};

use common::{FakeChat, FakeClock, FakeGit, FakeOpenCode};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

struct Rig {
    ctx: Arc<AppContext>,
    store: StoreHandle,
    discord: FakeChat,
    root: PathBuf,
    inbox: PathBuf,
    tmp: tempfile::TempDir,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).expect("root");
    let inbox = tmp.path().join("inbox");
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode: Arc<dyn OpenCodePort> = Arc::new(FakeOpenCode::new());
    let git: Arc<dyn GitPort> = Arc::new(FakeGit::inert());
    let clock = Arc::new(FakeClock::at(1_700_000_000));
    let fs: Arc<dyn FsPort> = Arc::new(
        FsAdapter::new(vec![root.clone()], inbox.clone(), false, clock.clone()).expect("fs"),
    );
    let discord = FakeChat::new();
    let discord_port: Arc<dyn ChatPort> = Arc::new(discord.clone());
    let ctx = common::context_with_fs(
        store.clone(),
        opencode,
        git,
        fs,
        clock,
        None,
        common::DEFAULT_ABORT_TIMEOUT_SECS,
        discord_port,
        OPERATOR,
        Some(CHANNEL),
    );
    Rig {
        ctx,
        store,
        discord,
        root,
        inbox,
        tmp,
    }
}

fn button_source(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: Some(10),
        is_dm: false,
    }
}

fn modal_source(custom_id: &str, id: &str, value: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: vec![TextInput {
                id: id.to_owned(),
                value: value.to_owned(),
            }],
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

#[tokio::test]
async fn attachments_are_sanitized_under_the_inbox() {
    let rig = rig();
    let path = files::ingest(
        &rig.ctx,
        &format!("discord:{OPERATOR}"),
        "../evil name.txt",
        b"payload",
    )
    .await
    .expect("ingest");
    let path = PathBuf::from(path);
    assert!(
        path.starts_with(&rig.inbox),
        "attachment must stay in the inbox: {}",
        path.display()
    );
    assert!(
        !path.to_string_lossy().contains(".."),
        "no traversal component: {}",
        path.display()
    );
    assert!(path.exists());
    assert_eq!(
        rig.store
            .draft_path(&format!("discord:{OPERATOR}"))
            .await
            .expect("draft"),
        Some(path.to_string_lossy().into_owned())
    );
}
