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
    write_file_summary(&log_path, &parsed_before_footer, &generation).unwrap();

    let cached = read_file_summary(&log_path).unwrap().unwrap();
    assert_eq!(cached.run.status, "DONE");
    assert!(cached.complete);
}
