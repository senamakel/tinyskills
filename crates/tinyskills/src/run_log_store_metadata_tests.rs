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
