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
