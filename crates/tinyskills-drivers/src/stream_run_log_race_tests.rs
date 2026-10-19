#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Debug)]
struct PauseNextAppend {
    inner: Arc<dyn StreamStore>,
    armed: AtomicBool,
    entered: Notify,
    resume: Notify,
}

impl PauseNextAppend {
    fn new(inner: Arc<dyn StreamStore>) -> Self {
        Self {
            inner,
            armed: AtomicBool::new(false),
            entered: Notify::new(),
            resume: Notify::new(),
        }
    }
}

#[tinystoragedrivers_core::async_trait]
impl StreamStore for PauseNextAppend {
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
        if self.armed.swap(false, Ordering::AcqRel) {
            self.entered.notify_one();
            self.resume.notified().await;
        }
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
        self.inner.read_window(stream, from, limit).await
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

#[tokio::test]
async fn in_flight_append_cannot_write_into_a_recreated_run_generation() {
    let storage = MemoryStorage::new();
    let scoped = storage.for_scope(&Scope::new("tenant-a").unwrap()).unwrap();
    let backing = Arc::clone(scoped.streams());
    let pause = Arc::new(PauseNextAppend::new(Arc::clone(&backing)));
    let racing = StreamRunLogStore::new(pause.clone(), "tenant-a").unwrap();
    let lifecycle = StreamRunLogStore::new(backing, "tenant-a").unwrap();

    racing
        .start(run(
            "workflow-a",
            "run-aba12345",
            "2026-01-01T00:00:00Z",
            "original prompt",
        ))
        .await
        .unwrap();
    pause.armed.store(true, Ordering::Release);

    let append = tokio::spawn(async move { racing.append("run-aba12345", "stale writer").await });
    pause.entered.notified().await;

    assert!(lifecycle.clear("run-aba12345").await.unwrap());
    lifecycle
        .start(run(
            "workflow-a",
            "run-aba12345",
            "2026-01-02T00:00:00Z",
            "replacement prompt",
        ))
        .await
        .unwrap();
    pause.resume.notify_one();

    let _ = append.await.unwrap();
    let page = lifecycle.tail("run-aba12345", None, 4096).await.unwrap();
    assert!(page.content.contains("replacement prompt"));
    assert!(!page.content.contains("stale writer"));
    assert_eq!(lifecycle.list(None, 10).await.unwrap().len(), 1);
}
