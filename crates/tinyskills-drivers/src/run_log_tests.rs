#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use serde_json::Value;
use tinystoragedrivers_testkit::memory_storage;
use tinystoragedrivers_types::Scope;

#[tokio::test]
async fn stream_store_preserves_run_order_footer_lookup_and_byte_slices() {
    let storage = memory_storage(&Scope::new("alice").unwrap());
    let owner = crate::SkillsStorage::open(storage).await.unwrap();
    let store = owner.run_logs();
    let stream = store
        .start(
            "demo",
            "run-identifier-123",
            "20261021T090000Z",
            "2026-10-21T09:00:00+00:00",
            &serde_json::json!({"input": "value"}),
            "prompt",
        )
        .await
        .unwrap();
    store.append_line(&stream, "step α").await.unwrap();
    store
        .finish(&stream, "DONE", 12, "2026-10-21T09:00:01+00:00", "answer")
        .await
        .unwrap();

    let runs = store.scan(Some("demo"), 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, "run-identifier-123");
    assert_eq!(runs[0].status, "DONE");
    assert_eq!(
        store.find("run-identifier-123").await.unwrap(),
        Some(stream.clone())
    );
    assert_eq!(store.find("missing").await.unwrap(), None);

    let first = store.read_slice(&stream, 0, 1).await.unwrap();
    assert_eq!(first.offset, 1);
    assert_eq!(first.bytes_read, 1);
    let tail = store.read_slice(&stream, first.offset, 4096).await.unwrap();
    assert!(tail.content.contains("step α"));
    assert!(tail.content.contains("status  : DONE"));
    assert!(tail.complete);
    assert!(tail.eof);
    assert!(store.delete(&stream).await.unwrap());
    assert!(!store.delete(&stream).await.unwrap());
}

#[tokio::test]
async fn stream_names_sanitize_path_separators_and_reject_empty_identifiers() {
    let storage = memory_storage(&Scope::new("alice").unwrap());
    let owner = crate::SkillsStorage::open(storage).await.unwrap();
    assert!(matches!(
        owner
            .run_logs()
            .start("", "run", "stamp", "now", &serde_json::json!({}), "prompt")
            .await,
        Err(RunLogStoreError::InvalidId)
    ));
    let stream = owner
        .run_logs()
        .start(
            "owner/workflow",
            "run id",
            "stamp",
            "now",
            &serde_json::json!({}),
            "prompt",
        )
        .await
        .unwrap();
    assert!(!stream.contains('/'));
}

#[tokio::test]
async fn stream_run_logs_reject_streams_outside_the_owner_namespace() {
    let storage = memory_storage(&Scope::new("alice").unwrap());
    let owner = crate::SkillsStorage::open(storage.clone()).await.unwrap();
    storage
        .streams()
        .append("unrelated.audit", Value::String("keep me".into()))
        .await
        .unwrap();

    assert!(matches!(
        owner
            .run_logs()
            .append_line("unrelated.audit", "overwrite")
            .await,
        Err(RunLogStoreError::InvalidId)
    ));
    assert!(matches!(
        owner.run_logs().read_slice("unrelated.audit", 0, 10).await,
        Err(RunLogStoreError::InvalidId)
    ));
    assert!(matches!(
        owner.run_logs().delete("unrelated.audit").await,
        Err(RunLogStoreError::InvalidId)
    ));
    assert_eq!(
        storage
            .streams()
            .read_window("unrelated.audit", 0, 10)
            .await
            .unwrap()[0]
            .value,
        Value::String("keep me".into())
    );
}

#[tokio::test]
async fn stream_run_logs_page_large_runs_and_preserve_multibyte_byte_offsets_after_reopen() {
    let storage = memory_storage(&Scope::new("alice").unwrap());
    let first_open = crate::SkillsStorage::open(storage.clone()).await.unwrap();
    let stream = first_open
        .run_logs()
        .start(
            "many-steps",
            "run-300-lines",
            "20261021T091000Z",
            "started",
            &serde_json::json!({}),
            "prompt",
        )
        .await
        .unwrap();
    for line in 0..300 {
        first_open
            .run_logs()
            .append_line(&stream, &format!("line-{line:03}"))
            .await
            .unwrap();
    }
    let long = format!("{}終", "λ".repeat(40_000));
    first_open
        .run_logs()
        .append_line(&stream, &long)
        .await
        .unwrap();
    first_open
        .run_logs()
        .finish(&stream, "DONE", 2, "finished", "ok")
        .await
        .unwrap();

    // Reconstruct the owner adapter over the same durable port handles, as a
    // host does after restart. Read offsets count bytes even across UTF-8.
    let reopened = crate::SkillsStorage::open(storage.clone()).await.unwrap();
    let full = reopened
        .run_logs()
        .read_slice(&stream, 0, usize::MAX)
        .await
        .unwrap();
    assert!(full.content.contains("line-299\n"));
    assert!(full.content.contains(&format!("{long}\n")));
    assert!(full.content.ends_with(
        "--- result ---\nstatus  : DONE\nduration: 2 ms\nfinished: finished UTC\n\nok\n"
    ));
    assert!(full.eof);
    assert!(full.complete);
    assert!(full.bytes_read > 64 * 1024);

    let split = full.content.find("終").unwrap() as u64;
    let before = reopened
        .run_logs()
        .read_slice(&stream, split, 2)
        .await
        .unwrap();
    assert_eq!(before.offset, split + 2);
    assert_eq!(before.bytes_read, 2);
    let after = reopened
        .run_logs()
        .read_slice(&stream, before.offset, 8)
        .await
        .unwrap();
    assert!(after.content.contains('\u{fffd}') || after.content.contains('λ'));

    let isolated = crate::SkillsStorage::open(memory_storage(&Scope::new("bob").unwrap()))
        .await
        .unwrap();
    assert!(
        isolated
            .run_logs()
            .find("run-300-lines")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn one_non_text_run_stream_does_not_hide_every_other_run() {
    let storage = memory_storage(&Scope::new("alice").unwrap());
    let owner = crate::SkillsStorage::open(storage.clone()).await.unwrap();
    let store = owner.run_logs();
    let stream = store
        .start(
            "demo",
            "good-run",
            "20261023T000000Z",
            "2026-10-23T00:00:00+00:00",
            &serde_json::json!({}),
            "p",
        )
        .await
        .unwrap();
    let bad = "skills.runs.other.20261023T000001Z.bad";
    storage
        .streams()
        .append(bad, serde_json::json!(1))
        .await
        .unwrap();

    let runs = store.scan(None, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, "good-run");
    assert_eq!(store.find("good-run").await.unwrap(), Some(stream));
    // Left out of the listing, not removed.
    assert!(
        storage
            .streams()
            .streams("skills.runs.")
            .await
            .unwrap()
            .contains(&bad.to_owned())
    );
}
