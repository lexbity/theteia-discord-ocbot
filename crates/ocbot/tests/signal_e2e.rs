//! SPEC-004 PH-4: the Signal surface end-to-end against the pinned fake daemon
//! — text decisions, the run flow, reactions, quote adoption, attachments, and
//! the trust boundary. Everything here drives the real inbound pump → guard →
//! router → handlers pipeline through a live JSON-RPC socket.
//!
//! The surface itself is the shared `common::signal_surface` harness (PH-7
//! extracted it), so this suite and `signal_flows.rs` build the same thing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::signal_fake::{Script, text_envelope};
use common::signal_surface::{
    OPERATOR, SENT_TS, STRANGER, SignalSurface, notice, repo_git, wait_for, wait_state,
};
use common::{FakeGit, FakeOpenCode};
use ocbot::app::context::ChatPort;
use ocbot::config::RateLimit;
use ocbot_core::{
    ActorId, ButtonKind, Destination, OutgoingMessage, State, Surface, component_button,
};

#[tokio::test]
async fn unknown_commands_get_the_help_table() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    surface.send(OPERATOR, "frobnicate");
    surface.wait_sent("ocbot commands:").await;
}

#[tokio::test]
async fn a_stranger_is_denied_without_a_reply() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    surface.send(STRANGER, "status");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(surface.sent().is_empty(), "a stranger gets no reply");
    assert!(
        surface
            .metrics
            .render_text()
            .contains("ocbot_surface_denied_total{surface=\"signal\"} 1"),
        "the denial is counted"
    );
}

#[tokio::test]
async fn a_redelivered_envelope_executes_once() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let action_id = surface
        .seed_reported("/tmp/sig-dedupe", "dedupe", None)
        .await;
    let ts = 1_700_000_200_000_u64;
    let id = surface.daemon.subscribe_id();
    let envelope = text_envelope(OPERATOR, ts, "accept dedupe");
    surface.daemon.stream_envelope(id, &envelope);
    surface.daemon.stream_envelope(id, &envelope); // the redelivery
    wait_state(&surface, action_id, State::Accepted).await;
    wait_for(
        || {
            surface
                .metrics
                .render_text()
                .contains("ocbot_signal_envelope_duplicates_total 1")
        },
        "the duplicate is counted",
    )
    .await;
}

#[tokio::test]
async fn the_rate_bucket_drops_excess_commands() {
    let surface = SignalSurface::new(
        Script::default(),
        Some(RateLimit {
            capacity: 1,
            refill_per_sec: 0.0,
        }),
        Arc::new(FakeGit::inert()),
    )
    .await;
    surface.send(OPERATOR, "status");
    surface.send(OPERATOR, "status");
    surface.wait_sent("ocbot").await;
    wait_for(
        || {
            surface
                .metrics
                .render_text()
                .contains("ocbot_surface_rate_dropped_total{surface=\"signal\"} 1")
        },
        "the second command is dropped",
    )
    .await;
}

#[tokio::test]
async fn bare_accept_marks_the_latest_reported_done() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let action_id = surface
        .seed_reported("/tmp/sig-accept", "my feature", None)
        .await;
    surface.send(OPERATOR, "accept");
    wait_state(&surface, action_id, State::Accepted).await;
    surface.wait_sent("marked").await;
}

#[tokio::test]
async fn a_reaction_resolves_a_mapped_component() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let action_id = surface
        .seed_reported("/tmp/sig-react", "react feature", None)
        .await;
    let message = OutgoingMessage {
        content: "report headline".to_owned(),
        attachment: None,
        components: vec![component_button(
            "Accept",
            format!("v1:accept:{action_id}"),
            ButtonKind::Success,
        )],
        mention_allowlist: Vec::new(),
    };
    let receipt = surface
        .adapter
        .post(&surface.destination(), &message)
        .await
        .expect("post");
    assert_eq!(
        surface
            .store
            .lookup_component_map(Surface::Signal.as_str(), &receipt.msg_id.0, "✅")
            .await
            .expect("lookup")
            .as_deref(),
        Some(format!("v1:accept:{action_id}").as_str())
    );
    let reaction = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_300_000_u64,
            "dataMessage": {
                "reaction": {
                    "emoji": "✅",
                    "targetSentTimestamp": SENT_TS,
                    "isRemove": false
                }
            }
        }
    });
    surface.stream_raw(&reaction);
    wait_state(&surface, action_id, State::Accepted).await;
}

#[tokio::test]
async fn a_reaction_on_an_unmapped_message_corrects_the_operator() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let reaction = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_600_000_u64,
            "dataMessage": {"reaction": {"emoji": "🎉", "targetSentTimestamp": 42, "isRemove": false}}
        }
    });
    surface.stream_raw(&reaction);
    surface.wait_sent("not mapped").await;
}

#[tokio::test]
async fn a_group_envelope_is_rejected() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let envelope = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_700_000_u64,
            "dataMessage": {"message": "hello group", "groupV2": {"groupId": "Zm9v"}}
        }
    });
    surface.stream_raw(&envelope);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(surface.sent().is_empty());
    assert!(
        surface
            .metrics
            .render_text()
            .contains("ocbot_surface_denied_total{surface=\"signal\"} 1")
    );
}

#[tokio::test]
async fn labels_qualify_a_decision() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let first = surface
        .seed_reported("/tmp/sig-label-a", "first", None)
        .await;
    let second = surface
        .seed_reported("/tmp/sig-label-b", "second", None)
        .await;
    surface.send(OPERATOR, "discard first");
    wait_state(&surface, first, State::Discarded).await;
    assert_eq!(surface.state(second).await, State::Reported);
}

#[tokio::test]
async fn quoting_a_report_adopts_the_session() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = "/tmp/sig-adopt";
    surface
        .store
        .upsert_seen("ses_ext_adopt", Some(dir))
        .await
        .expect("seen");
    surface
        .store
        .insert_msg_map(
            Surface::Signal.as_str(),
            &SENT_TS.to_string(),
            "ses_ext_adopt",
            None,
        )
        .await
        .expect("msg_map");
    let quote = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_400_000_u64,
            "dataMessage": {
                "message": "keep going please",
                "quote": {"id": SENT_TS, "author": OPERATOR, "text": "the report"}
            }
        }
    });
    surface.stream_raw(&quote);
    for _ in 0..600 {
        if !surface
            .store
            .running_actions()
            .await
            .expect("running")
            .is_empty()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the quote never adopted the session");
}

#[tokio::test]
async fn attachment_ingest_sets_the_draft() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let file = surface.tmp.path().join("upload.txt");
    std::fs::write(&file, b"an attachment").expect("write");
    let envelope = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_500_000_u64,
            "dataMessage": {
                "message": "see attached",
                "attachments": [{
                    "contentType": "text/plain",
                    "filename": "upload.txt",
                    "localPath": file.to_string_lossy()
                }]
            }
        }
    });
    surface.stream_raw(&envelope);
    let actor = ActorId::from_parts(Surface::Signal, OPERATOR).0;
    for _ in 0..300 {
        if surface
            .store
            .draft_path(&actor)
            .await
            .expect("draft")
            .is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no draft path was recorded");
}

#[tokio::test]
async fn the_run_flow_prompts_then_starts_then_reports_then_accepts() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_path_buf();
    surface.send(OPERATOR, &format!("run {}", dir.display()));
    surface.wait_sent("Send the prompt").await;

    surface.send(OPERATOR, "do the thing");
    surface.wait_sent("started").await;

    let running = surface
        .store
        .running_actions()
        .await
        .expect("running")
        .into_iter()
        .next()
        .expect("a running action");
    let session = running.session_id.clone().expect("session");
    surface
        .opencode
        .set_snapshot(&session, FakeOpenCode::completed_snapshot());
    ocbot::app::completion::finalize_action(
        &surface.ctx,
        &running,
        false,
        ocbot::app::completion::DetectedBy::Fallback,
    )
    .await
    .expect("finalize");
    surface.wait_sent("PONG").await;

    surface.send(OPERATOR, "accept");
    wait_state(&surface, running.id, State::Accepted).await;
}

#[tokio::test]
async fn a_git_accept_runs_the_text_form_and_commits() {
    let repo = tempfile::tempdir().expect("repo");
    common::init_repo(repo.path());
    common::write_file(repo.path(), "a.txt", "hello\n");
    common::commit_all(repo.path(), "initial");
    // Leave an uncommitted change for the accept to commit.
    common::append_file(repo.path(), "a.txt", "more\n");
    let surface = SignalSurface::new(Script::default(), None, repo_git(repo.path())).await;
    let dir = repo.path().to_string_lossy().into_owned();
    let action_id = surface
        .seed_reported(&dir, "git feature", Some("main"))
        .await;
    surface.send(OPERATOR, "accept");
    wait_for(
        || surface.sent().iter().any(|text| text.contains("text form")),
        "the git accept opens a text form",
    )
    .await;
    surface.send(OPERATOR, "ship it");
    wait_state(&surface, action_id, State::Accepted).await;
    let log = common::git_stdout(repo.path(), &["log", "--oneline"]);
    assert!(log.contains("ship it"), "the commit landed: {log}");
}

// --- broader verb coverage ------------------------------------------------

#[tokio::test]
async fn the_simple_text_verbs_reach_their_handlers() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    surface.set_cwd(&dir).await;
    for verb in [
        "status", "abort", "release", "mute", "reset", "ls", "history",
    ] {
        surface.send(OPERATOR, verb);
    }
    for _ in 0..600 {
        if surface.sent().len() >= 3 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the verbs produced too few replies: {:?}", surface.sent());
}

#[tokio::test]
async fn the_prompt_flow_asks_then_starts() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    surface.set_cwd(&dir).await;
    surface.send(OPERATOR, "prompt");
    surface.wait_sent("Send the prompt text").await;
    surface.send(OPERATOR, "do a prompt");
    surface.wait_sent("started").await;
}

#[tokio::test]
async fn prompt_with_inline_text_starts_directly() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    surface.set_cwd(&dir).await;
    surface.send(OPERATOR, "prompt do it now");
    surface.wait_sent("started").await;
}

#[tokio::test]
async fn send_targets_the_latest_staged_action() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    let action_id = surface.seed_staged(&dir, "staged one").await;
    surface.send(OPERATOR, "send");
    wait_state(&surface, action_id, State::Running).await;
}

#[tokio::test]
async fn edit_rewrites_then_sends() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    let action_id = surface.seed_staged(&dir, "edit me").await;
    surface.send(OPERATOR, "edit");
    surface.wait_sent("text form").await;
    surface.send(OPERATOR, "the new prompt");
    wait_state(&surface, action_id, State::Running).await;
}

#[tokio::test]
async fn history_paging_reaches_the_component() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    surface.set_cwd(&dir).await;
    surface.send(OPERATOR, "history 2");
    // The paging component plans a Defer and executes; a non-git dir reports so.
    surface.wait_sent("not a git repository").await;
}

#[tokio::test]
async fn cancel_closes_an_open_flow() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    surface.send(OPERATOR, "run");
    surface.wait_sent("Send the directory").await;
    surface.send(OPERATOR, "cancel");
    surface.wait_sent("flow cancelled").await;
}

#[tokio::test]
async fn inline_run_starts_immediately() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    surface.send(OPERATOR, &format!("run {dir} | do it inline"));
    surface.wait_sent("started").await;
}

#[tokio::test]
async fn pick_without_a_plan_corrects_the_operator() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    surface.send(OPERATOR, "pick 2");
    surface.wait_sent("no staged plan action").await;
}

#[tokio::test]
async fn quoting_an_unknown_message_is_ignored() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let quote = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_800_000_u64,
            "dataMessage": {
                "message": "hello?",
                "quote": {"id": 123_456_u64, "author": OPERATOR, "text": "unknown"}
            }
        }
    });
    surface.stream_raw(&quote);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(surface.sent().is_empty());
}

#[tokio::test]
async fn a_reaction_removal_is_ignored() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let reaction = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_900_000_u64,
            "dataMessage": {"reaction": {"emoji": "✅", "targetSentTimestamp": SENT_TS, "isRemove": true}}
        }
    });
    surface.stream_raw(&reaction);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(surface.sent().is_empty(), "removals are ignored");
}

#[tokio::test]
async fn a_bad_attachment_path_is_ignored() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let envelope = serde_json::json!({
        "envelope": {
            "source": OPERATOR,
            "timestamp": 1_700_000_950_000_u64,
            "dataMessage": {
                "attachments": [{"filename": "ghost.txt", "localPath": "/does/not/exist/ghost.txt"}]
            }
        }
    });
    surface.stream_raw(&envelope);
    let actor = ActorId::from_parts(Surface::Signal, OPERATOR).0;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        surface
            .store
            .draft_path(&actor)
            .await
            .expect("draft")
            .is_none(),
        "a missing attachment never becomes the draft"
    );
}

#[tokio::test]
async fn a_gate_answer_reaches_the_server() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    surface
        .store
        .upsert_project(&dir, None, None)
        .await
        .expect("project");
    let run_id = surface.store.insert_run(&dir, "free").await.expect("run");
    let action_id = surface
        .store
        .insert_action(run_id, Some(OPERATOR), "blocked", "prompt", 1)
        .await
        .expect("action");
    surface
        .store
        .set_action_destination(action_id, Surface::Signal.as_str(), OPERATOR)
        .await
        .expect("dest");
    surface
        .store
        .upsert_seen("ses_gate", Some(&dir))
        .await
        .expect("seen");
    assert!(
        surface
            .store
            .begin_send(action_id, &dir, "ses_gate", None, false)
            .await
            .expect("send"),
        "running"
    );
    surface
        .opencode
        .set_sessions(vec![ocbot_core::ports::PortSession {
            id: "ses_gate".to_owned(),
            parent_id: None,
            title: None,
            directory: Some(dir.clone()),
        }]);
    surface.opencode.set_gates(vec![common::permission_gate(
        "per_1",
        Some("ses_gate"),
        "bash",
        &["rm -rf /tmp/x"],
    )]);
    ocbot::app::gates_loop::sync(&surface.ctx)
        .await
        .expect("gate sync");
    surface.send(OPERATOR, "gate blocked once");
    for _ in 0..600 {
        if surface
            .opencode
            .inner()
            .calls
            .iter()
            .any(|call| matches!(call, common::Call::ReplyGate(_, _, _)))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the gate answer never reached the server");
}

#[tokio::test]
async fn a_gate_with_an_unknown_label_corrects() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    surface.send(OPERATOR, "gate nobody once");
    surface.wait_sent("no blocked action").await;
}

#[tokio::test]
async fn adapter_edit_and_broadcast_are_best_effort() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let receipt = ocbot_core::PostReceipt {
        surface: Surface::Signal,
        msg_id: ocbot_core::MsgId(SENT_TS.to_string()),
    };
    let replacement = OutgoingMessage {
        content: "new status".to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };
    surface.adapter.edit(&receipt, &replacement).await;
    surface.wait_sent("↻ update: new status").await;

    let outcome = surface.adapter.broadcast(&notice("an announcement")).await;
    assert_eq!(outcome.delivered.len(), 1);
    surface.wait_sent("an announcement").await;

    // A post to a foreign surface fails loud (D-02's adapter-boundary rule).
    let error = surface
        .adapter
        .post(&Destination::discord(7), &notice("wrong surface"))
        .await
        .expect_err("a discord destination is not signal");
    assert_eq!(error.surface, Surface::Signal);
}

#[tokio::test]
async fn an_outgoing_attachment_is_staged_and_sent() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    let message = OutgoingMessage {
        content: "the report".to_owned(),
        attachment: Some(ocbot_core::AttachmentSpec {
            filename: "report.md".to_owned(),
            bytes: b"# report\n".to_vec(),
        }),
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };
    surface
        .adapter
        .post(&surface.destination(), &message)
        .await
        .expect("post with attachment");
    let request = surface
        .daemon
        .requests()
        .into_iter()
        .find(|recorded| recorded.method == "send")
        .expect("a send");
    let attachments = request.params[0]["attachments"]
        .as_array()
        .expect("attachments present");
    assert_eq!(attachments.len(), 1);
    assert!(
        attachments[0].as_str().unwrap().ends_with("report.md"),
        "{attachments:?}"
    );
}

#[tokio::test]
async fn an_open_flow_expires_on_the_ops_tick() {
    let surface = SignalSurface::new(Script::default(), None, Arc::new(FakeGit::inert())).await;
    surface.send(OPERATOR, "run");
    surface.wait_sent("Send the directory").await;
    // Ten minutes pass: the flow is past its TTL.
    surface.clock.advance(700);
    ocbot::app::ops::sweep_pending(&surface.ctx)
        .await
        .expect("sweep");
    surface.wait_sent("flow expired").await;
    // The flow is gone, so the next text is a command, not a flow step.
    surface.send(OPERATOR, "frobnicate");
    surface.wait_sent("ocbot commands:").await;
}
