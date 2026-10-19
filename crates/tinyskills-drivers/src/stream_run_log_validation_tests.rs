use super::*;

#[tokio::test]
async fn stream_store_rejects_invalid_scope_ids_and_run_metadata() {
    let storage = MemoryStorage::new();
    let scoped = storage.for_scope(&Scope::new("tenant-a").unwrap()).unwrap();
    assert!(matches!(
        StreamRunLogStore::new(Arc::clone(scoped.streams()), ""),
        Err(RunLogError::InvalidId(_))
    ));
    assert!(matches!(
        StreamRunLogStore::new(Arc::clone(scoped.streams()), "tenant\nunsafe"),
        Err(RunLogError::InvalidId(_))
    ));
    assert!(matches!(
        StreamRunLogStore::new(Arc::clone(scoped.streams()), "x".repeat(513)),
        Err(RunLogError::InvalidId(_))
    ));

    let store = store("tenant-a");
    assert!(matches!(
        store
            .start(run(
                " workflow",
                "run-valid",
                "2026-01-01T00:00:00Z",
                "prompt"
            ))
            .await,
        Err(RunLogError::InvalidId(_))
    ));
    assert!(matches!(
        store
            .start(run("workflow", "run-valid", "", "prompt"))
            .await,
        Err(RunLogError::InvalidId(_))
    ));
    assert!(matches!(
        store
            .start(run(
                "workflow",
                &"r".repeat(256),
                "2026-01-01T00:00:00Z",
                "prompt"
            ))
            .await,
        Err(RunLogError::InvalidId(_))
    ));
}

#[tokio::test]
async fn starting_an_existing_stream_never_replaces_its_run() {
    let store = store("tenant-a");
    store
        .start(run(
            "workflow-a",
            "run-start-collision",
            "2026-01-01T00:00:00Z",
            "original prompt",
        ))
        .await
        .unwrap();
    assert!(
        store
            .start(run(
                "workflow-b",
                "run-start-collision",
                "2026-01-02T00:00:00Z",
                "replacement prompt",
            ))
            .await
            .is_err()
    );

    let listed = store.list(None, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].workflow_id, "workflow-a");
    assert!(
        store
            .tail("run-start-collision", None, 4096)
            .await
            .unwrap()
            .content
            .contains("original prompt")
    );
}

#[test]
fn stream_identifier_and_timestamp_validation_keep_unicode_ids_safe() {
    assert!(validate_id("workflow/雪").is_ok());
    let long_id = "x".repeat(513);
    for invalid in ["", " padded", "padded ", "control\nchar", long_id.as_str()] {
        assert!(matches!(
            validate_id(invalid),
            Err(RunLogError::InvalidId(_))
        ));
    }
    assert!(validate_started("2026-01-01T00:00:00Z").is_ok());
    let long_timestamp = "x".repeat(65);
    for invalid in ["", "2026-01-01T00:00:00Z!", long_timestamp.as_str()] {
        assert!(matches!(
            validate_started(invalid),
            Err(RunLogError::InvalidId(_))
        ));
    }
    assert_ne!(new_generation(), new_generation());
}

#[tokio::test]
async fn malformed_stream_records_fail_closed_in_every_read_operation() {
    let store = store("tenant-a");
    let stream = StreamRunLogStore::stream_name("run-corrupt-record").unwrap();
    store
        .streams
        .append(&stream, serde_json::json!({"kind": "unknown"}))
        .await
        .unwrap();

    assert!(matches!(
        store.tail("run-corrupt-record", None, 16).await,
        Err(RunLogError::Backend(_))
    ));
    assert!(matches!(
        store.append("run-corrupt-record", "text").await,
        Err(RunLogError::Backend(_))
    ));
    assert!(matches!(
        store.list(None, 10).await,
        Err(RunLogError::Backend(_))
    ));
    assert!(matches!(
        store.clear("run-corrupt-record").await,
        Err(RunLogError::Backend(_))
    ));
}

#[tokio::test]
async fn listing_rejects_a_valid_text_record_instead_of_a_checkpoint() {
    let store = store("tenant-a");
    let stream = StreamRunLogStore::stream_name("run-missing-checkpoint").unwrap();
    store
        .streams
        .append(
            &stream,
            serde_json::json!({"kind": "text", "text": "orphan"}),
        )
        .await
        .unwrap();

    assert!(matches!(
        store.list(None, 10).await,
        Err(RunLogError::Backend(_))
    ));
}
