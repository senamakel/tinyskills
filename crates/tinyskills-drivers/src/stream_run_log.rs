//! A [`tinyskills::RunLogStore`] backed by one append-only stream per run.

use std::sync::Arc;

use tinyskills::RunLogError;
use tinyskills::run_log;
use tinystoragedrivers_core::{StorageError, StreamStore};

#[path = "stream_run_log_records.rs"]
mod records;
use records::StreamRecord;
#[path = "stream_run_log_cursor.rs"]
mod cursor;
use cursor::{decode_hex, hex};

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
        header_offset: u64,
    ) -> Result<(String, String, String), RunLogError> {
        let record = self.record(stream, header_offset).await?;
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
    ) -> Result<(String, String, u64, run_log::RunLogSummary), RunLogError> {
        match record {
            StreamRecord::Checkpoint {
                schema: 1,
                scope_id,
                run_id: owner,
                generation,
                header_offset,
                summary,
            } if scope_id == self.scope_id && owner == run_id && summary.run.run_id == run_id => {
                Ok((scope_id, generation, header_offset, summary))
            }
            _ => Err(RunLogError::Backend(
                "run stream checkpoint is malformed".into(),
            )),
        }
    }

    fn validate_tombstone(&self, record: StreamRecord, run_id: &str) -> Result<(), RunLogError> {
        match record {
            StreamRecord::Tombstone {
                schema: 1,
                scope_id,
                run_id: owner,
                generation,
            } if scope_id == self.scope_id && owner == run_id && !generation.is_empty() => Ok(()),
            _ => Err(RunLogError::Backend(
                "run stream tombstone is malformed".into(),
            )),
        }
    }

    async fn latest_record(
        &self,
        stream: &str,
    ) -> Result<Option<(u64, StreamRecord)>, RunLogError> {
        let length = self
            .streams
            .len(stream)
            .await
            .map_err(|error| storage_error(&error))?;
        if length == 0 {
            return Ok(None);
        }
        let offset = length - 1;
        Ok(Some((offset, self.record(stream, offset).await?)))
    }

    async fn page_text(
        &self,
        stream: &str,
        header_offset: u64,
        checkpoint_offset: u64,
        mut offset: u64,
        mut within: usize,
        target: usize,
    ) -> Result<(String, u64, usize), RunLogError> {
        let mut content = String::with_capacity(target);
        while content.len() < target && offset <= checkpoint_offset {
            if offset == checkpoint_offset {
                offset += 1;
                break;
            }
            match self.record(stream, offset).await? {
                StreamRecord::Header { .. } if offset == header_offset => offset += 1,
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
                StreamRecord::Header { .. } | StreamRecord::Tombstone { .. } => {
                    return Err(RunLogError::Backend(
                        "run stream record is malformed".into(),
                    ));
                }
            }
        }
        Ok((content, offset, within))
    }
}

#[path = "stream_run_log_operations.rs"]
mod operations;

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
