use super::*;
use std::path::PathBuf;

#[tokio::test]
async fn file_store_zero_byte_tail_does_not_advance_its_cursor() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "run-zero-tail",
            "2026-01-01T00:00:00Z",
            "some text",
        ))
        .await
        .unwrap();

    let empty = store.tail("run-zero-tail", None, 0).await.unwrap();
    assert_eq!(empty.content, "");
    assert!(!empty.eof);
    let repeated = store
        .tail("run-zero-tail", Some(&empty.cursor), 0)
        .await
        .unwrap();
    assert_eq!(repeated.content, "");
    assert_eq!(repeated.cursor, empty.cursor);

    let next = store
        .tail("run-zero-tail", Some(&empty.cursor), 4096)
        .await
        .unwrap();
    assert!(next.content.contains("some text"));
}

#[tokio::test]
async fn file_store_repairs_stale_summary_after_legacy_append() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "run-sidecar123",
            "2026-01-01T00:00:00Z",
            "prompt",
        ))
        .await
        .unwrap();
    let path = PathBuf::from(&store.list(None, 10).await.unwrap().remove(0).log_path);

    crate::run_log::write_footer(&path, "DONE", 23, "2026-01-01T00:00:01Z", "result").unwrap();

    let runs = store.list(None, 10).await.unwrap();
    assert_eq!(runs[0].status, "DONE");
    assert_eq!(runs[0].duration_ms, Some(23));
    let page = store.tail("run-sidecar123", None, 4096).await.unwrap();
    assert!(page.complete);
    assert!(page.content.contains("result"));
}

#[tokio::test]
async fn file_store_finished_timestamp_without_status_is_incomplete() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "run-no-footer-status",
            "2026-01-01T00:00:00Z",
            "prompt",
        ))
        .await
        .unwrap();
    store
        .append(
            "run-no-footer-status",
            "--- result ---\nfinished: 2026-01-01T00:00:01Z UTC\n",
        )
        .await
        .unwrap();

    let listed = store.list(None, 10).await.unwrap();
    assert_eq!(listed[0].finished.as_deref(), Some("2026-01-01T00:00:01Z"));
    assert_eq!(listed[0].status, "RUNNING");
    let page = store
        .tail("run-no-footer-status", None, 4096)
        .await
        .unwrap();
    assert!(!page.complete);
}

#[tokio::test]
async fn file_cursor_does_not_encode_the_workspace_path() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "run-private-cursor",
            "2026-01-01T00:00:00Z",
            "prompt",
        ))
        .await
        .unwrap();

    let cursor = store
        .tail("run-private-cursor", None, 16)
        .await
        .unwrap()
        .cursor;
    let state = cursor.decode_state().unwrap();
    let workspace = temp.path().to_string_lossy();
    assert!(!state.backend.contains(workspace.as_ref()));
}

#[tokio::test]
async fn file_store_does_not_leave_a_reservation_after_summary_failure() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    let run = start(
        "workflow-a",
        "run-summary-failure",
        "2026-01-01T00:00:00Z",
        "prompt",
    );
    let log_path = file_store_path(
        temp.path(),
        run.workflow_id,
        run.run_id,
        run.started,
    );
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::create_dir(summary_path(&log_path)).unwrap();

    assert!(store.start(run.clone()).await.is_err());
    assert!(!log_path.exists());
    std::fs::remove_dir(summary_path(&log_path)).unwrap();
    store.start(run).await.unwrap();
}

#[tokio::test]
async fn encoded_suffix_collision_does_not_alias_a_legacy_run() {
    let temp = tempfile::TempDir::new().unwrap();
    let legacy_id = "61626364-legacy-run";
    let legacy_path = crate::run_log::run_log_path(
        temp.path(),
        "workflow-a",
        legacy_id,
        "20260101T000000Z",
    );
    crate::run_log::write_header(
        &legacy_path,
        "workflow-a",
        legacy_id,
        "2026-01-01T00:00:00Z",
        &INPUTS,
        "legacy prompt",
    )
    .unwrap();
    crate::run_log::append_line(&legacy_path, "legacy content").unwrap();

    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "abcd",
            "2026-01-02T00:00:00Z",
            "new prompt",
        ))
        .await
        .unwrap();
    store.append("abcd", "new content").await.unwrap();

    let new_run = store.tail("abcd", None, 4096).await.unwrap();
    assert!(new_run.content.contains("new content"));
    assert!(!new_run.content.contains("legacy content"));
    assert!(legacy_path.exists());
    assert!(store.clear("abcd").await.unwrap());
    assert!(legacy_path.exists());
}

#[tokio::test]
async fn file_summary_cache_never_claims_to_include_a_later_legacy_footer() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "run-summary-race",
            "2026-01-01T00:00:00Z",
            "prompt",
        ))
        .await
        .unwrap();
    let log_path = PathBuf::from(&store.list(None, 10).await.unwrap().remove(0).log_path);

    let parsed_before_footer = crate::run_log::read_run_log_summary(&log_path)
        .unwrap()
        .unwrap();
    let generation = file_generation(&log_path).unwrap();
    crate::run_log::write_footer(
        &log_path,
        "DONE",
        23,
        "2026-01-01T00:00:01Z",
        "legacy result",
    )
    .unwrap();
    write_file_summary(&log_path, &parsed_before_footer, &generation).unwrap();

    let repaired = read_file_summary(&log_path).unwrap().unwrap();
    assert_eq!(repaired.run.status, "DONE");
    assert!(repaired.complete);
}
