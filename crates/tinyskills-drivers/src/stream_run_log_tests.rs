#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::cursor::{StreamCursorState, decode_hex, encode_cursor};
use super::records::StreamRecord;
use super::*;
use serde_json::Value;
use tinyskills::{RunCursor, RunLogStart, RunLogStore};
use tinystoragedrivers_core::{MemoryStorage, Scope, StorageBackend, StreamEntry, StreamStore};

#[path = "stream_run_log_race_tests.rs"]
mod race_tests;
#[path = "stream_run_log_tail_tests.rs"]
mod tail_tests;
#[path = "stream_run_log_validation_tests.rs"]
mod validation_tests;

#[derive(Debug, Default)]
struct ReadMetrics {
    records: std::sync::atomic::AtomicUsize,
    bytes: std::sync::atomic::AtomicUsize,
}

impl ReadMetrics {
    fn reset(&self) {
        self.records.store(0, std::sync::atomic::Ordering::Relaxed);
        self.bytes.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct CountingStreamStore {
    inner: Arc<dyn StreamStore>,
    metrics: Arc<ReadMetrics>,
}

#[tinystoragedrivers_core::async_trait]
impl StreamStore for CountingStreamStore {
    async fn append(&self, stream: &str, value: Value) -> tinystoragedrivers_core::Result<u64> {
        self.inner.append(stream, value).await
    }

    async fn append_batch(
        &self,
        stream: &str,
        values: Vec<Value>,
    ) -> tinystoragedrivers_core::Result<u64> {
        self.inner.append_batch(stream, values).await
    }

    async fn append_batch_if_offset(
        &self,
        stream: &str,
        expected_next_offset: u64,
        values: Vec<Value>,
    ) -> tinystoragedrivers_core::Result<u64> {
        self.inner
            .append_batch_if_offset(stream, expected_next_offset, values)
            .await
    }

    async fn read_window(
        &self,
        stream: &str,
        from: u64,
        limit: usize,
    ) -> tinystoragedrivers_core::Result<Vec<StreamEntry>> {
        let entries = self.inner.read_window(stream, from, limit).await?;
        self.metrics
            .records
            .fetch_add(entries.len(), std::sync::atomic::Ordering::Relaxed);
        let bytes = entries
            .iter()
            .map(|entry| {
                entry
                    .value
                    .as_str()
                    .map_or_else(|| entry.value.to_string().len(), str::len)
            })
            .sum::<usize>();
        self.metrics
            .bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        Ok(entries)
    }

    async fn len(&self, stream: &str) -> tinystoragedrivers_core::Result<u64> {
        self.inner.len(stream).await
    }

    async fn truncate_before(
        &self,
        stream: &str,
        offset: u64,
    ) -> tinystoragedrivers_core::Result<u64> {
        self.inner.truncate_before(stream, offset).await
    }

    async fn delete_stream(&self, stream: &str) -> tinystoragedrivers_core::Result<bool> {
        self.inner.delete_stream(stream).await
    }

    async fn streams(&self, prefix: &str) -> tinystoragedrivers_core::Result<Vec<String>> {
        self.inner.streams(prefix).await
    }
}

fn counted_store(scope: &str) -> (StreamRunLogStore, Arc<ReadMetrics>) {
    let storage = MemoryStorage::new();
    let scoped = storage.for_scope(&Scope::new(scope).unwrap()).unwrap();
    let metrics = Arc::new(ReadMetrics::default());
    let streams: Arc<dyn StreamStore> = Arc::new(CountingStreamStore {
        inner: Arc::clone(scoped.streams()),
        metrics: Arc::clone(&metrics),
    });
    (StreamRunLogStore::new(streams, scope).unwrap(), metrics)
}

fn store(scope: &str) -> StreamRunLogStore {
    let storage = MemoryStorage::new();
    let scoped = storage.for_scope(&Scope::new(scope).unwrap()).unwrap();
    StreamRunLogStore::new(Arc::clone(scoped.streams()), scope).unwrap()
}

fn run<'a>(
    workflow_id: &'a str,
    run_id: &'a str,
    started: &'a str,
    prompt: &'a str,
) -> RunLogStart<'a> {
    static INPUTS: std::sync::LazyLock<Value> =
        std::sync::LazyLock::new(|| serde_json::json!({"label": "café"}));
    RunLogStart {
        workflow_id,
        run_id,
        started,
        inputs: &INPUTS,
        task_prompt: prompt,
    }
}

#[tokio::test]
async fn streams_create_append_list_tail_and_clear_runs() {
    let storage = MemoryStorage::new();
    let scoped = storage.for_scope(&Scope::new("tenant-a").unwrap()).unwrap();
    let first = StreamRunLogStore::new(Arc::clone(scoped.streams()), "tenant-a").unwrap();
    let second_handle = StreamRunLogStore::new(Arc::clone(scoped.streams()), "tenant-a").unwrap();

    assert_eq!(first.list(None, 10).await.unwrap().len(), 0);
    first
        .start(run(
            "workflow-a",
            "run-12345678",
            "2026-01-01T00:00:00Z",
            "prompt café\nnext line",
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
            19,
            "2026-01-01T00:00:01Z",
            "final output",
        )
        .await
        .unwrap();
    first
        .start(run(
            "workflow-b",
            "run-87654321",
            "2026-01-02T00:00:00Z",
            "second prompt",
        ))
        .await
        .unwrap();

    let runs = second_handle.list(None, 10).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].run_id, "run-87654321");
    assert_eq!(runs[1].workflow_id, "workflow-a");
    assert_eq!(runs[1].status, "DONE");
    assert_eq!(runs[1].duration_ms, Some(19));
    assert!(runs[1].log_path.starts_with("stream://skills.run_logs/"));
    assert_eq!(
        second_handle
            .list(Some("workflow-a"), 10)
            .await
            .unwrap()
            .len(),
        1
    );

    let first_page = first.tail("run-12345678", None, 40).await.unwrap();
    assert!(first_page.content.len() <= 40);
    let mut cursor = first_page.cursor;
    let mut remainder = first_page.content;
    let mut eof = false;
    while !eof {
        let page = second_handle
            .tail("run-12345678", Some(&cursor), 40)
            .await
            .unwrap();
        remainder.push_str(&page.content);
        cursor = page.cursor;
        eof = page.eof;
    }
    assert!(remainder.contains("caf"));
    assert!(remainder.contains("step two"));
    assert!(remainder.contains("final output"));
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
async fn stream_zero_byte_tail_does_not_advance_its_cursor() {
    let store = store("tenant-a");
    store
        .start(run(
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
async fn stream_cursors_reject_other_runs_scopes_and_recreated_runs() {
    let storage = MemoryStorage::new();
    let scope_a = storage.for_scope(&Scope::new("tenant-a").unwrap()).unwrap();
    let scope_b = storage.for_scope(&Scope::new("tenant-b").unwrap()).unwrap();
    let first = StreamRunLogStore::new(Arc::clone(scope_a.streams()), "tenant-a").unwrap();
    let other_scope = StreamRunLogStore::new(Arc::clone(scope_b.streams()), "tenant-b").unwrap();
    assert!(first.append("missing-run", "text").await.is_err());
    assert!(!first.clear("missing-run").await.unwrap());

    first
        .start(run(
            "workflow-a",
            "run-12345678",
            "2026-01-01T00:00:00Z",
            "first prompt",
        ))
        .await
        .unwrap();
    other_scope
        .start(run(
            "workflow-a",
            "run-12345678",
            "2026-01-01T00:00:00Z",
            "first prompt",
        ))
        .await
        .unwrap();
    let stale = first.tail("run-12345678", None, 16).await.unwrap().cursor;
    assert!(matches!(
        first.tail("other-run", Some(&stale), 16).await,
        Err(RunLogError::InvalidCursor)
    ));
    assert!(matches!(
        other_scope.tail("run-12345678", Some(&stale), 16).await,
        Err(RunLogError::InvalidCursor)
    ));

    assert!(first.clear("run-12345678").await.unwrap());
    assert!(matches!(
        first.tail("run-12345678", None, 16).await,
        Err(RunLogError::Backend(_))
    ));
    assert!(matches!(
        first.tail("run-12345678", Some(&stale), 16).await,
        Err(RunLogError::InvalidCursor)
    ));
    first
        .start(run(
            "workflow-a",
            "run-12345678",
            "2026-01-02T00:00:00Z",
            "replacement prompt",
        ))
        .await
        .unwrap();
    assert!(matches!(
        first.tail("run-12345678", Some(&stale), 16).await,
        Err(RunLogError::InvalidCursor)
    ));
}

#[tokio::test]
async fn concurrent_stream_starts_use_conditional_append() {
    let storage = MemoryStorage::new();
    let scoped = storage.for_scope(&Scope::new("tenant-a").unwrap()).unwrap();
    let store = StreamRunLogStore::new(Arc::clone(scoped.streams()), "tenant-a").unwrap();
    let first = run(
        "workflow-a",
        "run-race1234",
        "2026-01-01T00:00:00Z",
        "first",
    );
    let second = run(
        "workflow-a",
        "run-race1234",
        "2026-01-02T00:00:00Z",
        "second",
    );

    let (left, right) = tokio::join!(store.start(first), store.start(second));
    assert_ne!(left.is_ok(), right.is_ok());
    let runs = store.list(None, 10).await.unwrap();
    assert_eq!(runs.len(), 1);
    let stream = StreamRunLogStore::stream_name("run-race1234").unwrap();
    let record = store.record(&stream, 1).await.unwrap();
    assert!(matches!(&record, StreamRecord::Text { .. }));
    let text = match record {
        StreamRecord::Text { text } => text,
        _ => String::new(),
    };
    assert_eq!(text.matches("run_generation:").count(), 1);
}

#[tokio::test]
async fn stream_names_reject_control_characters_and_accept_unicode() {
    let store = store("tenant-a");
    for run_id in ["", "line\nbreak"] {
        assert!(matches!(
            store
                .start(run("workflow-a", run_id, "2026-01-01T00:00:00Z", "prompt"))
                .await,
            Err(RunLogError::InvalidId(_))
        ));
    }
    store
        .start(run(
            "workflow ü",
            "café-run",
            "2026-01-01T00:00:00Z",
            "prompt",
        ))
        .await
        .unwrap();
    assert_eq!(store.list(Some("workflow ü"), 10).await.unwrap().len(), 1);

    let too_long = "r".repeat(249);
    assert!(matches!(
        store
            .start(run(
                "workflow-a",
                &too_long,
                "2026-01-01T00:00:00Z",
                "prompt"
            ))
            .await,
        Err(RunLogError::InvalidId(_))
    ));
}

#[tokio::test]
async fn stream_small_tail_reads_only_a_bounded_window_of_a_large_run() {
    let (store, metrics) = counted_store("tenant-a");
    store
        .start(run(
            "workflow-a",
            "run-large-tail",
            "2026-01-01T00:00:00Z",
            &"h".repeat(128 * 1024),
        ))
        .await
        .unwrap();
    for _ in 0..128 {
        store
            .append("run-large-tail", &"x".repeat(1024))
            .await
            .unwrap();
    }

    metrics.reset();
    let page = store.tail("run-large-tail", None, 32).await.unwrap();
    assert_ne!(page.content, "");
    assert!(metrics.records.load(std::sync::atomic::Ordering::Relaxed) <= 4);
    assert!(metrics.bytes.load(std::sync::atomic::Ordering::Relaxed) <= 4096);
}

#[tokio::test]
async fn stream_listing_reads_run_metadata_not_log_history() {
    let (store, metrics) = counted_store("tenant-a");
    for index in 0..4 {
        let run_id = format!("run-large-list-{index}");
        let workflow_id = format!("workflow-{index}");
        let prompt = "p".repeat(64 * 1024);
        store
            .start(run(&workflow_id, &run_id, "2026-01-01T00:00:00Z", &prompt))
            .await
            .unwrap();
        for _ in 0..32 {
            store.append(&run_id, &"x".repeat(1024)).await.unwrap();
        }
        store
            .finish(&run_id, "DONE", 12, "2026-01-01T00:00:01Z", "result")
            .await
            .unwrap();
    }

    metrics.reset();
    let runs = store.list(None, 10).await.unwrap();
    assert_eq!(runs.len(), 4);
    assert!(runs.iter().all(|run| run.status == "DONE"));
    assert!(metrics.records.load(std::sync::atomic::Ordering::Relaxed) <= 12);
    assert!(metrics.bytes.load(std::sync::atomic::Ordering::Relaxed) <= 4096);
}

#[tokio::test]
async fn streams_keep_ids_that_sanitize_alike_distinct() {
    let store = store("tenant-a");
    store
        .start(run("workflow/a", "run/a", "2026-01-01T00:00:00Z", "first"))
        .await
        .unwrap();
    store
        .start(run("workflow_a", "run_a", "2026-01-01T00:00:00Z", "second"))
        .await
        .unwrap();

    assert_eq!(store.list(Some("workflow/a"), 10).await.unwrap().len(), 1);
    assert_eq!(store.list(Some("workflow_a"), 10).await.unwrap().len(), 1);
    assert!(store.clear("run/a").await.unwrap());
    assert_eq!(store.list(Some("workflow_a"), 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn stream_batches_reject_headers_and_appends_over_the_batch_limit() {
    let store = store("tenant-a");
    let large_text = "x".repeat(RECORD_CHUNK_BYTES * (MAX_BATCH_RECORDS + 1));
    assert!(matches!(
        store
            .start(run(
                "workflow-a",
                "run-large-header",
                "2026-01-01T00:00:00Z",
                &large_text,
            ))
            .await,
        Err(RunLogError::Backend(_))
    ));

    store
        .start(run(
            "workflow-a",
            "run-large-append",
            "2026-01-01T00:00:00Z",
            "prompt",
        ))
        .await
        .unwrap();
    assert!(matches!(
        store.append("run-large-append", &large_text).await,
        Err(RunLogError::Backend(_))
    ));
    assert!(
        store
            .tail("run-large-append", None, 4096)
            .await
            .unwrap()
            .content
            .contains("prompt")
    );
}
