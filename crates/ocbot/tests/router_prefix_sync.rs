//! Prefix-list sync guard for the router (SPEC-002 §4.4, FR-14/FR-15/FR-16).
//!
//! Before this guard, the deferred `custom_id` prefixes were written out twice:
//! once in `is_deferred_action` (the plan fallback) and once as the `execute`
//! dispatch arms. Nothing mechanically linked the two, so a prefix added to one
//! but not the other would either leave the interaction un-acked (timeout) or
//! acked and then left unanswered (`UnknownCustomId` after the deferral).
//!
//! `DEFERRED_PREFIXES` is now the single source of truth. This test reads the
//! router source with `include_str!` so a reintroduced literal list or a missing
//! `execute` route turns this test red, and it drives every listed prefix
//! through the real `guarded_plan` seam so the constant cannot drift from
//! behaviour.
//!
//! Interpretation of FR-16: the state-dependent button families
//! (`v1:send:`, `v1:runany:`, `v1:accept:`, `v1:park:`, `v1:discard:`,
//! `v1:gate:`) are *not* plain deferrals — `plan` routes them through a
//! per-action preflight that may return a modal or an ephemeral re-ack. They
//! therefore live in their own branches, and the invariant this test enforces is
//! that the *plain* deferral set is expressed exactly once, in
//! `DEFERRED_PREFIXES`, and is wired to both phases.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::AppContext;
use ocbot::app::interaction::{AckPlan, InteractionSource, SourceKind};
use ocbot::app::router::{self, PlanOutcome};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

/// The one deferred prefix that lives outside the constant: the history pager
/// shares the `v1:page:` namespace, so `is_deferred_action` special-cases it
/// exactly as the spec (§4.4.1) prescribes.
const PAGE_HISTORY: &str = "v1:page:history:";

/// The router source, embedded at compile time so this test can inspect it.
fn router_source() -> &'static str {
    include_str!("../src/app/router.rs")
}

/// Extract the string literals from a slice of Rust source.
fn literals(source: &str) -> Vec<String> {
    source
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// Return the body (from the opening `{` through its matching `}`) of the first
/// function whose signature begins with `signature`.
///
/// The scan skips string literals so braces inside `format!` templates do not
/// unbalance the count.
fn function_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("router source is missing `{signature}`"));
    let rest = &source[start..];
    let open = rest.find('{').expect("function opens a brace");
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in rest[open..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in `{signature}`");
}

/// The prefix literals declared by the `DEFERRED_PREFIXES` constant.
fn constant_prefixes() -> Vec<String> {
    let source = router_source();
    let start = source
        .find("const DEFERRED_PREFIXES")
        .expect("DEFERRED_PREFIXES is defined");
    let rest = &source[start..];
    let open = rest.find("= &[").expect("constant is an array") + 4;
    let close = rest[open..].find(']').expect("array is closed") + open;
    literals(&rest[open..close])
}

fn source(kind: SourceKind) -> InteractionSource {
    InteractionSource {
        kind,
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(1),
        is_dm: false,
    }
}

fn component(custom_id: &str) -> InteractionSource {
    source(SourceKind::Component {
        custom_id: custom_id.to_owned(),
    })
}

fn base_ctx(store: &StoreHandle) -> Arc<AppContext> {
    context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(FakeChat::new()),
        OPERATOR,
        Some(CHANNEL),
    )
}

/// `DEFERRED_PREFIXES` is the single source of truth for the plain deferral
/// set, and every entry is routed by the plan and execute phases.
#[tokio::test]
async fn prefix_sync() {
    let constant = constant_prefixes();
    assert!(!constant.is_empty(), "DEFERRED_PREFIXES must not be empty");

    let mut unique = constant.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        constant.len(),
        "DEFERRED_PREFIXES contains a duplicate entry"
    );

    // `is_deferred_action` must derive from the constant and may only mention
    // the history-pager special case; any other literal would be a
    // reintroduced hardcoded list (FR-15).
    let router = router_source();
    let predicate = function_body(router, "fn is_deferred_action(");
    assert!(
        predicate.contains("DEFERRED_PREFIXES"),
        "is_deferred_action must use DEFERRED_PREFIXES, not an inline list"
    );
    assert_eq!(
        literals(predicate),
        vec![PAGE_HISTORY.to_owned()],
        "is_deferred_action may only special-case the history pager"
    );

    // `plan` must decide deferrals through the shared predicate.
    let plan = function_body(router, "async fn plan(");
    assert!(
        plan.contains("is_deferred_action(custom_id)"),
        "plan must decide deferrals through is_deferred_action"
    );

    let execute = function_body(router, "fn execute(");

    let mut all = constant.clone();
    all.push(PAGE_HISTORY.to_owned());

    // Every deferred prefix must have an execute route; otherwise the ack is
    // sent and the follow-up dies as UnknownCustomId (a silent timeout bug).
    for prefix in &all {
        let route = format!("strip_prefix(\"{prefix}\")");
        assert!(
            execute.contains(&route),
            "{prefix:?} is deferred by plan but has no execute route (missing {route})"
        );
    }

    // Behavioural sync: every listed prefix really plans a `DeferUpdate`.
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = base_ctx(&store);
    for prefix in &all {
        let source = component(&format!("{prefix}sync"));
        assert!(
            matches!(
                router::guarded_plan(&ctx, &source).await,
                PlanOutcome::Plan(AckPlan::DeferUpdate)
            ),
            "{prefix} must plan a DeferUpdate through the real router"
        );
    }

    // And an unlisted prefix is still rejected loudly, not silently deferred.
    assert!(matches!(
        router::guarded_plan(&ctx, &component("v1:not-deferred:sync")).await,
        PlanOutcome::Failed(_)
    ));
}
