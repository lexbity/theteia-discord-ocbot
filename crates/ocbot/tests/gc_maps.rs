//! PH-5: gc_maps sweep test (SPEC-004 FR-113).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping (C6)"
)]

mod common;

use ocbot::adapters::store::StoreHandle;
use tempfile::TempDir;

#[tokio::test]
async fn gc_maps_runs_without_error() {
    let tmp = TempDir::new().expect("tmp");
    let db_path = tmp.path().join("test.db");

    let store = StoreHandle::open(&db_path).expect("open");

    // Sweep should not error even on empty DB
    store.gc_maps(2000000000000, 7).await.expect("gc");

    let cm = store.component_map_count().await.expect("count");
    let mm = store.msg_map_count().await.expect("count");
    assert_eq!(cm, 0);
    assert_eq!(mm, 0);
}

#[tokio::test]
async fn gc_maps_deletes_component_map_rows() {
    let tmp = TempDir::new().expect("tmp");
    let db_path = tmp.path().join("test.db");

    let store = StoreHandle::open(&db_path).expect("open");

    // Insert component_map rows
    for i in 0..5 {
        let emoji = format!("emoji{}", i);
        let custom_id = format!("v1:accept:{}{}", i, i);
        let msg_id = format!("{}", i);
        store
            .insert_component_map("discord", &msg_id, &[(emoji.clone(), custom_id.clone())])
            .await
            .expect("insert");
    }

    // Verify initial count
    let cm_before = store.component_map_count().await.expect("count");
    assert_eq!(cm_before, 5);

    // Run GC - should delete component_map rows
    store.gc_maps(2000000000000, 7).await.expect("gc");

    // Verify count after sweep
    let cm_after = store.component_map_count().await.expect("count");
    assert_eq!(cm_after, 0);
}

#[tokio::test]
async fn gc_maps_deletes_msg_map_rows() {
    let tmp = TempDir::new().expect("tmp");
    let db_path = tmp.path().join("test.db");

    let store = StoreHandle::open(&db_path).expect("open");

    // Insert msg_map rows
    for i in 0..3 {
        let msg_id = format!("msg_{}", i);
        store
            .insert_msg_map("discord", &msg_id, "session", Some(1))
            .await
            .expect("insert");
    }

    // Verify initial count
    let mm_before = store.msg_map_count().await.expect("count");
    assert_eq!(mm_before, 3);

    // Run GC - should delete msg_map rows
    store.gc_maps(2000000000000, 7).await.expect("gc");

    // Verify count after sweep
    let mm_after = store.msg_map_count().await.expect("count");
    assert_eq!(mm_after, 0);
}
