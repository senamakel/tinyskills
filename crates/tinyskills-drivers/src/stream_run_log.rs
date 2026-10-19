//! A [`RunLogStore`] backed by one append-only stream per run.

use std::sync::Arc;

use tinyskills::run_log::{self, ScannedRun};
use tinyskills::{RunCursor, RunLogError, RunLogFuture, RunLogPage, RunLogStart, RunLogStore};
use tinystoragedrivers_core::{StorageError, StreamStore};

#[path = "stream_run_log_records.rs"]
mod records;
use records::{StreamRecord, record_value, text_records, update_summary};
#[path = "stream_run_log_cursor.rs"]
mod cursor;
use cursor::{
    StreamCursorState, cursor_state, decode_hex, encode_cursor, hex, validate_cursor_owner,
};

/// Prefix for run streams, declared as a tenant-scoped item by [`crate::storage_schema`].
pub const RUN_LOG_PREFIX: &str = "skills.run_logs";

const RECORD_CHUNK_BYTES: usize = 1024;
const MAX_BATCH_RECORDS: usize = 256;
const APPEND_RETRIES: usize = 4;

/// Run logs stored as UTF-8 text chunks with versioned metadata checkpoints.
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

    async fn record(&self, stream: &str, offset: u64) -> Result<StreamRecord, RunLogError> {
        let entry = self
            .streams
            .read_window(stream, offset, 1)
            .await
            .map_err(|error| storage_error(&error))?
            .into_iter()
            .next()
            .ok_or_else(|| RunLogError::Backend("run stream record is missing".into()))?;
        if entry.offset != offset {
            return Err(RunLogError::Backend(
                "run stream record offset changed".into(),
            ));
        }
        serde_json::from_value(entry.value)
            .map_err(|_| RunLogError::Backend("run stream record is malformed".into()))
    }

    async fn checkpoint(
        &self,
        stream: &str,
        length: u64,
    ) -> Result<(u64, StreamRecord), RunLogError> {
        if length < 3 {
            return Err(RunLogError::Backend("run stream is incomplete".into()));
        }
        let offset = length - 1;
        let record = self.record(stream, offset).await?;
        Ok((offset, record))
    }

    async fn validate_header(
        &self,
        stream: &str,
        run_id: &str,
    ) -> Result<(String, String, String), RunLogError> {
        let record = self.record(stream, 0).await?;
        match record {
            StreamRecord::Header {
                schema: 1,
                scope_id,
                run_id: owner,
                workflow_id,
                generation,
            } if scope_id == self.scope_id
                && owner == run_id
                && !workflow_id.is_empty()
                && !generation.is_empty() =>
            {
                Ok((scope_id, generation, workflow_id))
            }
            _ => Err(RunLogError::Backend(
                "run stream header is malformed".into(),
            )),
        }
    }

    fn validate_checkpoint(
        &self,
        record: StreamRecord,
        run_id: &str,
        generation: &str,
    ) -> Result<run_log::RunLogSummary, RunLogError> {
        match record {
            StreamRecord::Checkpoint {
                schema: 1,
                scope_id,
                run_id: owner,
                generation: record_generation,
                summary,
            } if scope_id == self.scope_id
                && owner == run_id
                && record_generation == generation
                && summary.run.run_id == run_id =>
            {
                Ok(summary)
            }
            _ => Err(RunLogError::Backend(
                "run stream checkpoint is malformed".into(),
            )),
        }
    }
}

impl RunLogStore for StreamRunLogStore {
    fn start<'a>(&'a self, run: RunLogStart<'a>) -> RunLogFuture<'a, ()> {
        Box::pin(async move {
            let stream = Self::stream_name(run.run_id)?;
            validate_id(run.workflow_id)?;
            validate_started(run.started)?;
            let generation = new_generation();
            let mut text = format!(
                "run_generation: {generation}\n{}",
                run_log::format_header(
                    run.workflow_id,
                    run.run_id,
                    run.started,
                    run.inputs,
                    run.task_prompt,
                )
            );
            text.push('\n');
            let log_uri = format!("stream://{stream}");
            let summary = run_log::read_run_log_summary_from_text(&text, &log_uri)
                .ok_or_else(|| RunLogError::Backend("run header is malformed".into()))?;
            let mut values = vec![record_value(&StreamRecord::Header {
                schema: 1,
                scope_id: self.scope_id.clone(),
                run_id: run.run_id.to_owned(),
                workflow_id: run.workflow_id.to_owned(),
                generation: generation.clone(),
            })?];
            values.extend(text_records(&text)?);
            values.push(record_value(&StreamRecord::Checkpoint {
                schema: 1,
                scope_id: self.scope_id.clone(),
                run_id: run.run_id.to_owned(),
                generation,
                summary,
            })?);
            if values.len() > MAX_BATCH_RECORDS {
                return Err(RunLogError::Backend(
                    "run header exceeds the stream batch limit".into(),
                ));
            }
            self.streams
                .append_batch_if_offset(&stream, 0, values)
                .await
                .map_err(|error| storage_error(&error))?;
            Ok(())
        })
    }

    fn append<'a>(&'a self, run_id: &'a str, line: &'a str) -> RunLogFuture<'a, ()> {
        Box::pin(async move {
            let stream = Self::stream_name(run_id)?;
            let mut text = line.to_owned();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            for _ in 0..APPEND_RETRIES {
                let length = self
                    .streams
                    .len(&stream)
                    .await
                    .map_err(|error| storage_error(&error))?;
                if length == 0 {
                    return Err(RunLogError::Backend("run log not found".into()));
                }
                let (scope_id, generation, workflow_id) =
                    self.validate_header(&stream, run_id).await?;
                let (_, checkpoint) = self.checkpoint(&stream, length).await?;
                let mut summary = self.validate_checkpoint(checkpoint, run_id, &generation)?;
                if summary.run.workflow_id != workflow_id {
                    return Err(RunLogError::Backend("run stream metadata mismatch".into()));
                }
                update_summary(&mut summary, &text);
                let mut values = text_records(&text)?;
                values.push(record_value(&StreamRecord::Checkpoint {
                    schema: 1,
                    scope_id,
                    run_id: run_id.to_owned(),
                    generation,
                    summary,
                })?);
                if values.len() > MAX_BATCH_RECORDS {
                    return Err(RunLogError::Backend(
                        "run append exceeds the stream batch limit".into(),
                    ));
                }
                match self
                    .streams
                    .append_batch_if_offset(&stream, length, values)
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(error) if error.kind() == tinystoragedrivers_core::ErrorKind::Conflict => {}
                    Err(error) => return Err(storage_error(&error)),
                }
            }
            Err(RunLogError::Backend(
                "run stream changed during every bounded append retry".into(),
            ))
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
            let (scope_id, generation, workflow_id) = self.validate_header(&stream, run_id).await?;
            let length = self
                .streams
                .len(&stream)
                .await
                .map_err(|error| storage_error(&error))?;
            let (checkpoint_offset, checkpoint) = self.checkpoint(&stream, length).await?;
            let summary = self.validate_checkpoint(checkpoint, run_id, &generation)?;
            if scope_id != self.scope_id || summary.run.workflow_id != workflow_id {
                return Err(RunLogError::Backend("run stream metadata mismatch".into()));
            }
            let (mut offset, mut within) = cursor.map_or(Ok((0, 0)), |cursor| {
                cursor_state(cursor, &backend, run_id, &generation)
            })?;
            if offset > length
                || (offset > checkpoint_offset && offset != length)
                || within > RECORD_CHUNK_BYTES
            {
                return Err(RunLogError::InvalidCursor);
            }
            if offset == 0 {
                if within != 0 {
                    return Err(RunLogError::InvalidCursor);
                }
                offset = 1;
            }
            if offset >= checkpoint_offset && within != 0 {
                return Err(RunLogError::InvalidCursor);
            }
            let target = max_bytes.max(1);
            let mut content = String::with_capacity(target);
            while content.len() < target && offset <= checkpoint_offset {
                if offset == checkpoint_offset {
                    offset += 1;
                    break;
                }
                match self.record(&stream, offset).await? {
                    StreamRecord::Header { .. } if offset == 0 => offset += 1,
                    StreamRecord::Text { text } => {
                        if within > text.len() || !text.is_char_boundary(within) {
                            return Err(RunLogError::InvalidCursor);
                        }
                        let target_end = within
                            .saturating_add(target - content.len())
                            .min(text.len());
                        let mut end = target_end;
                        while end < text.len() && !text.is_char_boundary(end) {
                            end += 1;
                        }
                        content.push_str(&text[within..end]);
                        if end == text.len() {
                            offset += 1;
                            within = 0;
                        } else {
                            within = end;
                        }
                    }
                    StreamRecord::Checkpoint { .. } => offset += 1,
                    StreamRecord::Header { .. } => {
                        return Err(RunLogError::Backend(
                            "run stream record is malformed".into(),
                        ));
                    }
                }
            }
            let eof = offset > checkpoint_offset && within == 0;
            Ok(RunLogPage {
                content,
                cursor: RunCursor::from_opaque(encode_cursor(&StreamCursorState {
                    backend,
                    run_id: run_id.to_owned(),
                    generation,
                    position: offset,
                    within,
                })?),
                eof,
                complete: summary.complete,
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
                let length = self
                    .streams
                    .len(&stream)
                    .await
                    .map_err(|error| storage_error(&error))?;
                let (_, generation, header_workflow_id) =
                    self.validate_header(&stream, &run_id).await?;
                let (_, checkpoint) = self.checkpoint(&stream, length).await?;
                let summary = self.validate_checkpoint(checkpoint, &run_id, &generation)?;
                if summary.run.run_id == run_id
                    && summary.run.workflow_id == header_workflow_id
                    && workflow_id.is_none_or(|want| summary.run.workflow_id == want)
                {
                    runs.push(summary.run);
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

fn new_generation() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{nanos:x}-{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
#[path = "stream_run_log_tests.rs"]
mod tests;
