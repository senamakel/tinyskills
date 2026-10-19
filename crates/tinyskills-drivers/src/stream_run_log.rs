//! A [`RunLogStore`] backed by one append-only stream per run.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tinyskills::run_log::{self, ScannedRun};
use tinyskills::{RunCursor, RunLogError, RunLogFuture, RunLogPage, RunLogStart, RunLogStore};
use tinystoragedrivers_core::{StorageError, StreamStore};

/// Prefix for run streams, declared as a tenant-scoped item by [`crate::storage_schema`].
pub const RUN_LOG_PREFIX: &str = "skills/run_logs";

#[derive(Debug, Serialize, Deserialize)]
struct StreamCursorState {
    backend: String,
    run_id: String,
    generation: String,
    position: u64,
    within: usize,
}

/// Run logs stored as one string record per append under [`RUN_LOG_PREFIX`].
#[derive(Clone)]
pub struct StreamRunLogStore {
    streams: Arc<dyn StreamStore>,
    scope_id: String,
}

impl std::fmt::Debug for StreamRunLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRunLogStore")
            .field("prefix", &RUN_LOG_PREFIX)
            .finish_non_exhaustive()
    }
}

impl StreamRunLogStore {
    /// A store on `streams`, the tenant-scoped handle selected by the host,
    /// bound to the same stable, non-secret `scope_id` used for that handle.
    ///
    /// # Errors
    ///
    /// Returns [`RunLogError::InvalidId`] when `scope_id` is empty, too long,
    /// or contains control characters.
    pub fn new(
        streams: Arc<dyn StreamStore>,
        scope_id: impl Into<String>,
    ) -> Result<Self, RunLogError> {
        let scope_id = scope_id.into();
        if scope_id.is_empty() || scope_id.len() > 512 || scope_id.chars().any(char::is_control) {
            return Err(RunLogError::InvalidId(scope_id));
        }
        Ok(Self { streams, scope_id })
    }

    fn stream_name(run_id: &str) -> Result<String, RunLogError> {
        validate_id(run_id)?;
        let encoded = hex(run_id.as_bytes());
        let name = format!("{RUN_LOG_PREFIX}/{encoded}");
        if name.len() > 512 {
            return Err(RunLogError::InvalidId(run_id.to_owned()));
        }
        Ok(name)
    }

    fn decode_run_id(name: &str) -> Option<String> {
        let encoded = name.strip_prefix(&format!("{RUN_LOG_PREFIX}/"))?;
        let bytes = decode_hex(encoded)?;
        let run_id = String::from_utf8(bytes).ok()?;
        validate_id(&run_id).ok()?;
        Some(run_id)
    }

    fn backend(&self) -> String {
        format!("stream:{RUN_LOG_PREFIX}:{}", self.scope_id)
    }

    async fn read_all(&self, stream: &str) -> Result<String, RunLogError> {
        let length = self
            .streams
            .len(stream)
            .await
            .map_err(|error| storage_error(&error))?;
        let mut text = String::new();
        let mut from = 0;
        while from < length {
            let entries = self
                .streams
                .read_window(stream, from, 128)
                .await
                .map_err(|error| storage_error(&error))?;
            if entries.is_empty() {
                break;
            }
            for entry in entries {
                let Some(chunk) = entry.value.as_str() else {
                    return Err(RunLogError::Backend(
                        "run stream contains a non-text record".into(),
                    ));
                };
                text.push_str(chunk);
                from = entry.offset.saturating_add(1);
            }
        }
        Ok(text)
    }

    async fn generation(&self, stream: &str) -> Result<String, RunLogError> {
        let Some(entry) = self
            .streams
            .read_window(stream, 0, 1)
            .await
            .map_err(|error| storage_error(&error))?
            .into_iter()
            .next()
        else {
            return Err(RunLogError::Backend("run stream is empty".into()));
        };
        entry
            .value
            .as_str()
            .and_then(|header| {
                header
                    .lines()
                    .find_map(|line| line.strip_prefix("run_generation: "))
            })
            .map(str::to_owned)
            .ok_or_else(|| RunLogError::Backend("run generation is missing".into()))
    }

    async fn complete(&self, stream: &str) -> Result<bool, RunLogError> {
        Ok(self.read_all(stream).await?.contains("\n--- result ---\n"))
    }
}

impl RunLogStore for StreamRunLogStore {
    fn start<'a>(&'a self, run: RunLogStart<'a>) -> RunLogFuture<'a, ()> {
        Box::pin(async move {
            let stream = Self::stream_name(run.run_id)?;
            validate_id(run.workflow_id)?;
            validate_started(run.started)?;
            let mut header = run_log::format_header(
                run.workflow_id,
                run.run_id,
                run.started,
                run.inputs,
                run.task_prompt,
            );
            header = format!("run_generation: {}\n{header}", new_generation());
            header.push('\n');
            self.streams
                .append_batch_if_offset(&stream, 0, vec![Value::String(header)])
                .await
                .map_err(|error| storage_error(&error))?;
            Ok(())
        })
    }

    fn append<'a>(&'a self, run_id: &'a str, line: &'a str) -> RunLogFuture<'a, ()> {
        Box::pin(async move {
            let stream = Self::stream_name(run_id)?;
            if self
                .streams
                .len(&stream)
                .await
                .map_err(|error| storage_error(&error))?
                == 0
            {
                return Err(RunLogError::Backend("run log not found".into()));
            }
            let mut line = line.to_owned();
            if !line.ends_with('\n') {
                line.push('\n');
            }
            self.streams
                .append(&stream, Value::String(line))
                .await
                .map_err(|error| storage_error(&error))?;
            Ok(())
        })
    }

    fn tail<'a>(
        &'a self,
        run_id: &'a str,
        cursor: Option<&'a RunCursor>,
        max_bytes: usize,
    ) -> RunLogFuture<'a, RunLogPage> {
        Box::pin(async move {
            let stream = Self::stream_name(run_id)?;
            let max_bytes = max_bytes.min(tinyskills::MAX_RUN_LOG_TAIL_BYTES);
            let backend = self.backend();
            if let Some(cursor) = cursor {
                validate_cursor_owner(cursor, &backend, run_id)?;
            }
            let generation = self.generation(&stream).await?;
            let (mut offset, mut within) = cursor.map_or(Ok((0, 0)), |cursor| {
                cursor_state(cursor, &backend, run_id, &generation)
            })?;
            let mut content = Vec::with_capacity(max_bytes);
            let length = self
                .streams
                .len(&stream)
                .await
                .map_err(|error| storage_error(&error))?;
            if offset > length {
                return Err(RunLogError::InvalidCursor);
            }
            if offset == length && within != 0 {
                return Err(RunLogError::InvalidCursor);
            }
            while content.len() < max_bytes && offset < length {
                let Some(entry) = self
                    .streams
                    .read_window(&stream, offset, 1)
                    .await
                    .map_err(|error| storage_error(&error))?
                    .into_iter()
                    .next()
                else {
                    break;
                };
                let Some(chunk) = entry.value.as_str() else {
                    return Err(RunLogError::Backend(
                        "run stream contains a non-text record".into(),
                    ));
                };
                let bytes = chunk.as_bytes();
                if within > bytes.len() {
                    return Err(RunLogError::InvalidCursor);
                }
                let count = (max_bytes - content.len()).min(bytes.len() - within);
                content.extend_from_slice(&bytes[within..within + count]);
                if within + count == bytes.len() {
                    offset = entry.offset.saturating_add(1);
                    within = 0;
                } else {
                    within += count;
                }
            }
            let complete = self.complete(&stream).await?;
            let eof = offset >= length && within == 0;
            Ok(RunLogPage {
                content: String::from_utf8_lossy(&content).into_owned(),
                cursor: RunCursor::from_opaque(encode_cursor(&StreamCursorState {
                    backend,
                    run_id: run_id.to_owned(),
                    generation,
                    position: offset,
                    within,
                })?),
                eof,
                complete,
            })
        })
    }

    fn list<'a>(
        &'a self,
        workflow_id: Option<&'a str>,
        limit: usize,
    ) -> RunLogFuture<'a, Vec<ScannedRun>> {
        Box::pin(async move {
            if let Some(workflow_id) = workflow_id {
                validate_id(workflow_id)?;
            }
            let names = self
                .streams
                .streams(&format!("{RUN_LOG_PREFIX}/"))
                .await
                .map_err(|error| storage_error(&error))?;
            let mut runs = Vec::new();
            for stream in names {
                let Some(run_id) = Self::decode_run_id(&stream) else {
                    continue;
                };
                let text = self.read_all(&stream).await?;
                let log_uri = format!("stream://{stream}");
                if let Some(run) = run_log::parse_run_log_text(&text, &log_uri)
                    && run.run_id == run_id
                    && workflow_id.is_none_or(|want| run.workflow_id == want)
                {
                    runs.push(run);
                }
            }
            runs.sort_by(|left, right| right.started.cmp(&left.started));
            runs.truncate(limit);
            Ok(runs)
        })
    }

    fn clear<'a>(&'a self, run_id: &'a str) -> RunLogFuture<'a, bool> {
        Box::pin(async move {
            let stream = Self::stream_name(run_id)?;
            self.streams
                .delete_stream(&stream)
                .await
                .map_err(|error| storage_error(&error))
        })
    }
}

fn storage_error(error: &StorageError) -> RunLogError {
    RunLogError::Backend(error.to_string())
}

fn validate_id(value: &str) -> Result<(), RunLogError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > 512
        || value.chars().any(char::is_control)
    {
        return Err(RunLogError::InvalidId(value.to_owned()));
    }
    Ok(())
}

fn validate_started(value: &str) -> Result<(), RunLogError> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'+' | b'.')
        })
    {
        return Err(RunLogError::InvalidId(value.to_owned()));
    }
    Ok(())
}

fn encode_cursor(state: &StreamCursorState) -> Result<String, RunLogError> {
    let bytes =
        serde_json::to_vec(state).map_err(|error| RunLogError::Backend(error.to_string()))?;
    Ok(hex(&bytes))
}

fn cursor_state(
    cursor: &RunCursor,
    backend: &str,
    run_id: &str,
    generation: &str,
) -> Result<(u64, usize), RunLogError> {
    validate_cursor_owner(cursor, backend, run_id)?;
    let bytes = decode_hex(cursor.as_opaque()).ok_or(RunLogError::InvalidCursor)?;
    let state: StreamCursorState =
        serde_json::from_slice(&bytes).map_err(|_| RunLogError::InvalidCursor)?;
    if state.backend != backend || state.run_id != run_id || state.generation != generation {
        return Err(RunLogError::InvalidCursor);
    }
    Ok((state.position, state.within))
}

fn validate_cursor_owner(
    cursor: &RunCursor,
    backend: &str,
    run_id: &str,
) -> Result<(), RunLogError> {
    let bytes = decode_hex(cursor.as_opaque()).ok_or(RunLogError::InvalidCursor)?;
    let state: StreamCursorState =
        serde_json::from_slice(&bytes).map_err(|_| RunLogError::InvalidCursor)?;
    if state.backend != backend || state.run_id != run_id {
        return Err(RunLogError::InvalidCursor);
    }
    Ok(())
}

fn new_generation() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{nanos:x}-{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        encoded.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    encoded
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() > 8192 || !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

#[cfg(test)]
#[path = "stream_run_log_tests.rs"]
mod tests;
