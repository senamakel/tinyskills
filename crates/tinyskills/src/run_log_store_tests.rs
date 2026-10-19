#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::FileRunLogStore;
use std::path::PathBuf;

static INPUTS: std::sync::LazyLock<serde_json::Value> =
    std::sync::LazyLock::new(|| serde_json::json!({"note": "café"}));

fn start<'a>(
    workflow_id: &'a str,
    run_id: &'a str,
    started: &'a str,
    prompt: &'a str,
) -> RunLogStart<'a> {
    RunLogStart {
        workflow_id,
        run_id,
        started,
        inputs: &INPUTS,
        task_prompt: prompt,
    }
}

#[tokio::test]
async fn file_store_starts_appends_lists_tails_and_clears_runs() {
    let temp = tempfile::TempDir::new().unwrap();
    let first = FileRunLogStore::new(temp.path());
    let second_handle = FileRunLogStore::new(temp.path());

    assert_eq!(first.list(None, 10).await.unwrap().len(), 0);
    first
        .start(start(
            "workflow-a",
            "run-12345678",
            "2026-01-01T00:00:00Z",
            "task line one\nline two café",
        ))
        .await
        .unwrap();
    first
        .append("run-12345678", "step one\nstep two")
        .await
        .unwrap();
    first
        .finish(
            "run-12345678",
            "DONE",
            17,
            "2026-01-01T00:00:01Z",
            "all done",
        )
        .await
        .unwrap();
    first
        .start(start(
            "workflow-b",
            "run-87654321",
            "2026-01-02T00:00:00Z",
            "another prompt",
        ))
        .await
        .unwrap();

    let runs = second_handle.list(None, 10).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].run_id, "run-87654321");
    assert_eq!(runs[1].workflow_id, "workflow-a");
    assert_eq!(runs[1].status, "DONE");
    assert_eq!(runs[1].duration_ms, Some(17));
    assert_eq!(
        second_handle
            .list(Some("workflow-a"), 10)
            .await
            .unwrap()
            .len(),
        1
    );

    let first_page = first.tail("run-12345678", None, 32).await.unwrap();
    assert!(first_page.content.len() <= 32);
    let mut cursor = first_page.cursor;
    let mut remainder = first_page.content;
    let mut eof = false;
    while !eof {
        let page = second_handle
            .tail("run-12345678", Some(&cursor), 32)
            .await
            .unwrap();
        remainder.push_str(&page.content);
        cursor = page.cursor;
        eof = page.eof;
    }
    assert!(remainder.contains("caf"));
    assert!(remainder.contains("step two"));
    assert!(remainder.contains("all done"));
    assert!(
        first
            .tail("run-12345678", None, 1024)
            .await
            .unwrap()
            .content
            .contains("café")
    );

    assert!(first.clear("run-12345678").await.unwrap());
    assert!(!second_handle.clear("run-12345678").await.unwrap());
    assert_eq!(first.list(None, 10).await.unwrap().len(), 1);
}

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
    assert!(empty.content.is_empty());
    assert!(!empty.eof);
    let repeated = store
        .tail("run-zero-tail", Some(&empty.cursor), 0)
        .await
        .unwrap();
    assert!(repeated.content.is_empty());
    assert_eq!(repeated.cursor, empty.cursor);

    let next = store
        .tail("run-zero-tail", Some(&empty.cursor), 64)
        .await
        .unwrap();
    assert!(next.content.contains("some text"));
}

#[tokio::test]
async fn file_store_rejects_bad_ids_missing_runs_and_stale_cursors() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    assert!(store.append("missing-run", "text").await.is_err());
    assert!(store.tail("missing-run", None, 8).await.is_err());
    assert!(!store.clear("missing-run").await.unwrap());
    assert!(matches!(
        store
            .start(start(
                "workflow\nunsafe",
                "run-1",
                "2026-01-01T00:00:00Z",
                "prompt"
            ))
            .await,
        Err(RunLogError::InvalidId(_))
    ));

    store
        .start(start(
            "workflow-a",
            "run-12345678",
            "2026-01-01T00:00:00Z",
            "first",
        ))
        .await
        .unwrap();
    let stale = store.tail("run-12345678", None, 16).await.unwrap().cursor;
    assert!(matches!(
        store
            .tail(
                "run-12345678",
                Some(&RunCursor::from_opaque("malformed")),
                16
            )
            .await,
        Err(RunLogError::InvalidCursor)
    ));
    assert!(store.clear("run-12345678").await.unwrap());
    store
        .start(start(
            "workflow-a",
            "run-12345678",
            "2026-01-02T00:00:00Z",
            "second",
        ))
        .await
        .unwrap();
    assert!(matches!(
        store.tail("run-12345678", Some(&stale), 16).await,
        Err(RunLogError::InvalidCursor)
    ));
    let other_store = FileRunLogStore::new(temp.path().join("other"));
    assert!(matches!(
        other_store.tail("run-12345678", Some(&stale), 16).await,
        Err(RunLogError::InvalidCursor)
    ));
}

#[tokio::test]
async fn file_store_rejects_ids_that_exceed_encoded_filename_limit() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    let workflow_id = "界".repeat(30);
    let run_id = "界".repeat(30);
    assert!(matches!(
        store
            .start(start(
                &workflow_id,
                &run_id,
                "2026-01-01T00:00:00Z",
                "prompt"
            ))
            .await,
        Err(RunLogError::InvalidId(_))
    ));
}

#[tokio::test]
async fn file_store_small_tail_pages_preserve_utf8_boundaries() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "run-utf8pages",
            "2026-01-01T00:00:00Z",
            "ascii-🙂-café-end",
        ))
        .await
        .unwrap();
    let expected = store
        .tail("run-utf8pages", None, 4096)
        .await
        .unwrap()
        .content;

    for max_bytes in 1..=3 {
        let mut cursor = None;
        let mut joined = String::new();
        let mut eof = false;
        for _ in 0..expected.len() + 4 {
            let page = store
                .tail("run-utf8pages", cursor.as_ref(), max_bytes)
                .await
                .unwrap();
            assert!(page.content.len() <= max_bytes + 3);
            assert!(std::str::from_utf8(page.content.as_bytes()).is_ok());
            joined.push_str(&page.content);
            eof = page.eof;
            if eof {
                break;
            }
            let next = page.cursor;
            assert_ne!(cursor.as_ref(), Some(&next), "tail cursor made no progress");
            cursor = Some(next);
        }
        assert!(eof, "small-page tail did not reach EOF");
        assert_eq!(joined, expected, "max_bytes={max_bytes}");
    }
}

#[tokio::test]
async fn file_store_rejects_a_cursor_inside_a_utf8_scalar() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow-a",
            "run-utf8cursor",
            "2026-01-01T00:00:00Z",
            "before 🙂 after",
        ))
        .await
        .unwrap();
    let page = store.tail("run-utf8cursor", None, 1).await.unwrap();
    let state = page.cursor.decode_state().unwrap();
    let run = store.list(None, 10).await.unwrap().remove(0);
    let bytes = std::fs::read(run.log_path).unwrap();
    let emoji_offset = bytes
        .windows("🙂".len())
        .position(|window| window == "🙂".as_bytes())
        .unwrap();
    let forged = RunCursor::new(
        &state.backend,
        &state.run_id,
        &state.generation,
        u64::try_from(emoji_offset + 1).unwrap(),
        0,
    )
    .unwrap();

    assert!(matches!(
        store.tail("run-utf8cursor", Some(&forged), 8).await,
        Err(RunLogError::InvalidCursor)
    ));
}

#[tokio::test]
async fn file_store_concurrent_starts_create_one_generation() {
    let temp = tempfile::TempDir::new().unwrap();
    let first = FileRunLogStore::new(temp.path());
    let second = FileRunLogStore::new(temp.path());
    let first_start = start(
        "workflow-a",
        "run-race1234",
        "2026-01-01T00:00:00Z",
        "first",
    );
    let second_start = start(
        "workflow-a",
        "run-race1234",
        "2026-01-02T00:00:00Z",
        "second",
    );

    let (left, right) = tokio::join!(first.start(first_start), second.start(second_start));
    assert_ne!(left.is_ok(), right.is_ok());
    let runs = first.list(None, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    let path = PathBuf::from(&runs[0].log_path);
    let text = std::fs::read_to_string(path).unwrap();
    assert_eq!(text.matches("run_generation:").count(), 1);
}

#[tokio::test]
async fn file_store_preserves_unicode_workflow_and_run_ids() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow ü",
            "run-café",
            "2026-01-01T00:00:00Z",
            "prompt line one\nline two café",
        ))
        .await
        .unwrap();
    let runs = store.list(Some("workflow ü"), 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, "run-café");
}

#[tokio::test]
async fn file_store_adopts_logs_written_by_legacy_helpers() {
    let temp = tempfile::TempDir::new().unwrap();
    let old_path = crate::run_log::run_log_path(
        temp.path(),
        "legacy-workflow",
        "legacy-run-12345678",
        "20260101T000000Z",
    );
    crate::run_log::write_header(
        &old_path,
        "legacy-workflow",
        "legacy-run-12345678",
        "2026-01-01T00:00:00Z",
        &serde_json::json!({"mode": "legacy"}),
        "legacy task",
    )
    .unwrap();
    crate::run_log::append_line(&old_path, "legacy step café").unwrap();
    crate::run_log::write_footer(
        &old_path,
        "DONE",
        23,
        "2026-01-01T00:00:01Z",
        "legacy result",
    )
    .unwrap();
    let unrelated_path = crate::run_log::run_log_path(
        temp.path(),
        "other-workflow",
        "other-run-12345678",
        "20260102T000000Z",
    );
    crate::run_log::write_header(
        &unrelated_path,
        "other-workflow",
        "other-run-12345678",
        "2026-01-02T00:00:00Z",
        &serde_json::json!({}),
        "other task",
    )
    .unwrap();

    let store = FileRunLogStore::new(temp.path());
    let runs = store.list(Some("legacy-workflow"), 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].started, "2026-01-01T00:00:00Z UTC");
    assert_eq!(runs[0].status, "DONE");
    assert_eq!(runs[0].duration_ms, Some(23));
    let page = store.tail("legacy-run-12345678", None, 4096).await.unwrap();
    assert!(page.content.contains("legacy step café"));
    assert!(page.content.contains("legacy result"));

    assert!(store.clear("legacy-run-12345678").await.unwrap());
    assert!(!old_path.exists());
    assert!(unrelated_path.exists());
    assert_eq!(store.list(None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn file_store_keeps_ids_that_sanitize_alike_distinct() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    store
        .start(start(
            "workflow/a",
            "run/a",
            "2026-01-01T00:00:00Z",
            "first",
        ))
        .await
        .unwrap();
    store
        .start(start(
            "workflow_a",
            "run_a",
            "2026-01-01T00:00:00Z",
            "second",
        ))
        .await
        .unwrap();

    let first = store.list(Some("workflow/a"), 10).await.unwrap();
    let second = store.list(Some("workflow_a"), 10).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(first[0].log_path, second[0].log_path);
    assert!(store.clear("run/a").await.unwrap());
    assert_eq!(store.list(Some("workflow_a"), 10).await.unwrap().len(), 1);
}
