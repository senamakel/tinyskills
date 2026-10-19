use super::*;

static INPUTS: std::sync::LazyLock<serde_json::Value> =
    std::sync::LazyLock::new(|| serde_json::json!({}));

fn start() -> RunLogStart<'static> {
    RunLogStart {
        workflow_id: "workflow-a",
        run_id: "run-summary-failure",
        started: "2026-01-01T00:00:00Z",
        inputs: &INPUTS,
        task_prompt: "prompt",
    }
}

#[tokio::test]
async fn failed_start_removes_its_log_and_reservation() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    let run = start();
    let log_path = file_store_path(temp.path(), run.workflow_id, run.run_id, run.started);
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::create_dir(summary_path(&log_path)).unwrap();

    assert!(store.start(run.clone()).await.is_err());
    assert!(!log_path.exists());
    std::fs::remove_dir(summary_path(&log_path)).unwrap();
    store.start(run).await.unwrap();
}

#[tokio::test]
async fn partial_start_log_is_preserved_until_clear_then_can_be_recreated() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    let run = start();
    let path = file_store_path(temp.path(), run.workflow_id, run.run_id, run.started);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "run_generation: partial\n").unwrap();

    assert!(store.start(run.clone()).await.is_err());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "run_generation: partial\n"
    );
    assert!(store.clear(run.run_id).await.unwrap());
    assert!(!path.exists());
    store.start(run).await.unwrap();
}

#[test]
fn run_identity_lookup_stops_within_the_bounded_header_prefix() {
    let mut log = std::io::Cursor::new(format!(
        "==== workflow_run: workflow-a ====\nrun_id : run-bounded\n{}",
        "payload\n".repeat(1024 * 1024)
    ));

    let identity = run_log::read_run_log_header_identity_from_reader(&mut log).unwrap();
    assert_eq!(
        identity,
        Some(("workflow-a".to_owned(), "run-bounded".to_owned()))
    );
    assert!(log.position() <= 128);
}

#[tokio::test]
async fn clear_does_not_remove_a_deterministic_path_for_another_run() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    let path = file_store_path(temp.path(), "workflow-a", "requested-run", "");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    run_log::write_header(
        &path,
        "workflow-a",
        "different-run",
        "2026-01-01T00:00:00Z",
        &INPUTS,
        "prompt",
    )
    .unwrap();

    assert!(!store.clear("requested-run").await.unwrap());
    assert!(path.exists());
}

#[tokio::test]
async fn concurrent_same_id_starts_have_one_winner() {
    let temp = tempfile::TempDir::new().unwrap();
    let first = FileRunLogStore::new(temp.path());
    let second = FileRunLogStore::new(temp.path());
    let first_run = start();
    let second_run = RunLogStart {
        started: "2026-01-01T00:00:01Z",
        ..first_run.clone()
    };

    let (first_result, second_result) =
        tokio::join!(first.start(first_run), second.start(second_run));
    assert_ne!(first_result.is_ok(), second_result.is_ok());
    assert_eq!(first.list(None, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn summary_write_keeps_the_length_that_was_parsed() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    let run = RunLogStart {
        workflow_id: "workflow-a",
        run_id: "run-summary-race",
        started: "2026-01-01T00:00:00Z",
        inputs: &INPUTS,
        task_prompt: "prompt",
    };
    store.start(run).await.unwrap();
    let log_path = exact_file_path(temp.path(), "run-summary-race").unwrap();

    let observed_len = std::fs::metadata(&log_path).unwrap().len();
    let parsed_before_footer = run_log::read_run_log_summary(&log_path).unwrap().unwrap();
    let generation = file_generation(&log_path).unwrap();
    run_log::write_footer(
        &log_path,
        "DONE",
        23,
        "2026-01-01T00:00:01Z",
        "legacy result",
    )
    .unwrap();
    write_file_summary(&log_path, &parsed_before_footer, &generation, observed_len).unwrap();

    let cached = read_file_summary(&log_path).unwrap().unwrap();
    assert_eq!(cached.run.status, "DONE");
    assert!(cached.complete);
}

#[tokio::test]
async fn append_uses_the_cached_summary_without_reparsing_the_log() {
    let temp = tempfile::TempDir::new().unwrap();
    let store = FileRunLogStore::new(temp.path());
    let run = RunLogStart {
        workflow_id: "workflow-a",
        run_id: "run-incremental-summary",
        started: "2026-01-01T00:00:00Z",
        inputs: &INPUTS,
        task_prompt: "prompt",
    };
    store.start(run).await.unwrap();
    let log_path = exact_file_path(temp.path(), "run-incremental-summary").unwrap();
    let parses_before_append = run_log::full_file_summary_parse_count(&log_path);

    store
        .append("run-incremental-summary", "step one")
        .await
        .unwrap();

    assert_eq!(
        run_log::full_file_summary_parse_count(&log_path),
        parses_before_append,
        "appending to a log with a current summary cache should not parse its full history"
    );
}

#[tokio::test]
async fn failed_start_cleanup_preserves_a_replacement_run() {
    let temp = tempfile::TempDir::new().unwrap();
    let first_handle = FileRunLogStore::new(temp.path());
    let second_handle = FileRunLogStore::new(temp.path());
    let original = start();
    let path = file_store_path(temp.path(), original.workflow_id, original.run_id, "");
    first_handle.start(original.clone()).await.unwrap();
    let failed_generation = file_generation(&path).unwrap();

    assert!(first_handle.clear(original.run_id).await.unwrap());
    second_handle
        .start(RunLogStart {
            started: "2026-01-01T00:00:01Z",
            ..original
        })
        .await
        .unwrap();

    cleanup_failed_start(&path, &failed_generation);

    assert!(path.exists(), "cleanup must keep the replacement log");
    assert_ne!(file_generation(&path).unwrap(), failed_generation);
    assert!(read_file_summary(&path).unwrap().is_some());
}

#[tokio::test]
async fn tail_never_reads_replacement_bytes_with_the_previous_generation_cursor() {
    let temp = tempfile::TempDir::new().unwrap();
    let first_handle = std::sync::Arc::new(FileRunLogStore::new(temp.path()));
    let second_handle = FileRunLogStore::new(temp.path());
    let run = RunLogStart {
        workflow_id: "workflow-a",
        run_id: "run-tail-generation-race",
        started: "2026-01-01T00:00:00Z",
        inputs: &INPUTS,
        task_prompt: "original payload",
    };
    first_handle.start(run.clone()).await.unwrap();
    let stale_cursor = first_handle.tail(run.run_id, None, 1).await.unwrap().cursor;
    let path = file_store_path(temp.path(), run.workflow_id, run.run_id, "");
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let tail_store = std::sync::Arc::clone(&first_handle);
    let run_id = run.run_id.to_owned();
    let tail = tokio::spawn(async move {
        let before_read = Box::pin(async move {
            ready_tx.send(()).unwrap();
            resume_rx.await.unwrap();
        });
        tail_with_interleave(
            &tail_store,
            &run_id,
            Some(&stale_cursor),
            16 * 1024,
            before_read,
        )
        .await
    });

    ready_rx.await.unwrap();
    let lifecycle_is_excluded = run_lifecycle_lock_is_held(&path);
    if !lifecycle_is_excluded {
        assert!(second_handle.clear(run.run_id).await.unwrap());
        second_handle
            .start(RunLogStart {
                started: "2026-01-01T00:00:01Z",
                task_prompt: "replacement payload",
                ..run.clone()
            })
            .await
            .unwrap();
        second_handle
            .append(run.run_id, "replacement-only-tail-marker")
            .await
            .unwrap();
    }
    resume_tx.send(()).unwrap();

    match tail.await.unwrap() {
        Ok(page) => assert!(
            !page.content.contains("replacement-only-tail-marker"),
            "tail must not return replacement bytes under the previous generation cursor"
        ),
        Err(RunLogError::InvalidCursor) => {}
        Err(error) => panic!("unexpected tail error: {error}"),
    }
}
