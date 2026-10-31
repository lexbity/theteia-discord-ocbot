//! SPEC-005 phase 14: the `project_state` store contract directly. The
//! handlers exercise these methods through the project flow; this pins the
//! semantics that contract rests on — plan-reset on project switch (the
//! spec's INSERT-or-REPLACE shape), the UPDATE-is-a-no-op ordering rule, the
//! FK requirement, and the spec-mandated clear.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use ocbot::adapters::store::StoreHandle;

const USER: &str = "discord:42";

#[tokio::test]
async fn selecting_a_new_project_resets_the_stale_plan() {
    let store = StoreHandle::open_in_memory().expect("store");
    // The handler contract: the ui_state parent row (FK target) is laid down
    // by set_ui_cwd before any selection is stored.
    store
        .set_ui_cwd(USER, "/projects/alpha")
        .await
        .expect("cwd");

    store
        .set_selected_project(USER, "root/alpha", "/projects/alpha")
        .await
        .expect("select project");
    let (name, dir, plan, path) = store
        .get_selected_project_and_plan(USER)
        .await
        .expect("read")
        .expect("selected");
    assert_eq!(
        (name.as_str(), dir.as_str()),
        ("root/alpha", "/projects/alpha")
    );
    assert_eq!(
        (plan, path),
        (None, None),
        "a fresh selection carries no plan"
    );

    store
        .set_selected_plan(
            USER,
            "spec-auth.md",
            "/projects/alpha/devdocs/plans/spec-auth.md",
        )
        .await
        .expect("select plan");
    let (_, _, plan, path) = store
        .get_selected_project_and_plan(USER)
        .await
        .expect("read")
        .expect("still selected");
    assert_eq!(plan.as_deref(), Some("spec-auth.md"));
    assert!(path.expect("plan path").ends_with("spec-auth.md"));

    // Switching projects must not leave the previous plan pointing at the
    // old directory: the INSERT-or-REPLACE carries NULL plan columns.
    store
        .set_selected_project(USER, "root/beta", "/projects/beta")
        .await
        .expect("switch");
    let (name, _, plan, path) = store
        .get_selected_project_and_plan(USER)
        .await
        .expect("read")
        .expect("beta");
    assert_eq!(name, "root/beta");
    assert_eq!(
        (plan, path),
        (None, None),
        "the stale plan was reset, not inherited"
    );
}

#[tokio::test]
async fn plan_selection_without_a_project_is_a_no_op_and_selection_requires_the_parent() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .set_ui_cwd(USER, "/projects/alpha")
        .await
        .expect("cwd");

    // UPDATE with no project_state row affects zero rows: Ok, and nothing
    // becomes readable. This is why the handlers store the project first
    // (and why the router cross-checks stored paths, not just tokens).
    store
        .set_selected_plan(USER, "orphan.md", "/projects/alpha/orphan.md")
        .await
        .expect("no-op accepted");
    assert!(
        store
            .get_selected_project_and_plan(USER)
            .await
            .expect("read")
            .is_none(),
        "no row was fabricated"
    );

    // The declared FK (project_state.user_id -> ui_state.user_id) is enforced
    // (PRAGMA foreign_keys=ON): selecting without the parent row is refused.
    let error = store
        .set_selected_project("discord:nobody", "root/x", "/projects/x")
        .await
        .expect_err("FK must refuse");
    assert!(
        error.to_string().to_uppercase().contains("FOREIGN KEY"),
        "{error}"
    );
}

#[tokio::test]
async fn clearing_the_selection_is_idempotent() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .set_ui_cwd(USER, "/projects/alpha")
        .await
        .expect("cwd");
    store
        .set_selected_project(USER, "root/alpha", "/projects/alpha")
        .await
        .expect("select");

    store
        .clear_selected_project(USER)
        .await
        .expect("first clear");
    assert!(
        store
            .get_selected_project_and_plan(USER)
            .await
            .expect("read")
            .is_none()
    );
    store
        .clear_selected_project(USER)
        .await
        .expect("clearing twice stays Ok");
}
