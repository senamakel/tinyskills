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
