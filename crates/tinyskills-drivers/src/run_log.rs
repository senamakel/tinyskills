//! Durable skill-run log operations backed by the host's append-only streams.

use std::sync::Arc;

use serde_json::Value;
use tinyskills::run_log::{
    RunLogSlice, ScannedRun, format_footer, format_header, parse_runs, slice,
};
use tinystoragedrivers_api::StreamStore;
use tinystoragedrivers_types::StorageError;

const PREFIX: &str = "skills.runs.";
const READ_PAGE: usize = 256;
const APPEND_CHUNK_BYTES: usize = 64 * 1024;

/// An owner-bound run-log adapter that never chooses a backend or scope.
#[derive(Clone)]
pub struct StreamRunLogStore {
    streams: Arc<dyn StreamStore>,
}

impl std::fmt::Debug for StreamRunLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRunLogStore").finish_non_exhaustive()
    }
}

/// Errors returned by [`StreamRunLogStore`].
#[derive(Debug, thiserror::Error)]
pub enum RunLogStoreError {
    /// The stream port failed.
    #[error("run-log stream operation failed: {0}")]
    Storage(#[from] StorageError),
    /// A run identifier could not be represented as a stable stream name.
    #[error("run-log identifiers must be non-empty safe segments")]
    InvalidId,
    /// A stream contains values that are not UTF-8 text chunks.
    #[error("run-log stream contains a non-text entry")]
    Corrupt,
}

impl StreamRunLogStore {
    /// Bind to the scope-bound stream port selected by the host.
    #[must_use]
    pub fn new(streams: Arc<dyn StreamStore>) -> Self {
        Self { streams }
    }

    /// Start a run with its original human-readable header and return the
    /// opaque stream id used for subsequent appends.
    ///
    /// # Errors
    /// Returns [`RunLogStoreError::InvalidId`] for an empty run identifier or
    /// [`RunLogStoreError::Storage`] if the stream port rejects the append.
    pub async fn start(
        &self,
        workflow_id: &str,
        run_id: &str,
        timestamp: &str,
        started: &str,
        inputs: &Value,
        task_prompt: &str,
    ) -> Result<String, RunLogStoreError> {
        let stream = stream_id(workflow_id, timestamp, run_id)?;
        self.append_text(
            &stream,
            &format_header(workflow_id, run_id, started, inputs, task_prompt),
        )
        .await?;
        Ok(stream)
    }

    /// Append one human-readable step line, adding a trailing newline as the
    /// former file writer did.
    ///
    /// # Errors
    /// Returns [`RunLogStoreError::InvalidId`] for a stream outside this
    /// owner's namespace, or a storage/corruption error from the stream port.
    pub async fn append_line(&self, stream: &str, line: &str) -> Result<(), RunLogStoreError> {
        validate_stream_id(stream)?;
        if line.ends_with('\n') {
            self.append_text(stream, line).await
        } else {
            self.append_text(stream, &format!("{line}\n")).await
        }
    }

    /// Append the original terminal footer.
    ///
    /// # Errors
    /// Returns [`RunLogStoreError::InvalidId`] for a stream outside this
    /// owner's namespace, or a storage/corruption error from the stream port.
    pub async fn finish(
        &self,
        stream: &str,
        status: &str,
        elapsed_ms: u64,
        finished: &str,
        output: &str,
    ) -> Result<(), RunLogStoreError> {
        validate_stream_id(stream)?;
        self.append_text(stream, &format_footer(status, elapsed_ms, finished, output))
            .await
    }

    /// Scan stored runs, sorted newest first after filtering. A stream that
    /// does not decode as text is left out (and left in storage), so one
    /// foreign entry cannot hide every other run.
    ///
    /// # Errors
    /// Returns a storage error if a stream cannot be listed or read.
    pub async fn scan(
        &self,
        workflow_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ScannedRun>, RunLogStoreError> {
        let mut names = self.streams.streams(PREFIX).await?;
        names.sort();
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            match self.read_text(&name).await {
                Ok(text) => entries.push((name, text)),
                Err(RunLogStoreError::Corrupt) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(parse_runs(entries, workflow_id, limit))
    }

    /// Find a stored log by its full run id.
    ///
    /// # Errors
    /// Returns a storage error if a stored stream cannot be listed or read.
    pub async fn find(&self, run_id: &str) -> Result<Option<String>, RunLogStoreError> {
        if run_id.is_empty() {
            return Ok(None);
        }
        for run in self.scan(None, usize::MAX).await? {
            if run.run_id == run_id {
                return Ok(Some(run.stream_id));
            }
        }
        Ok(None)
    }

    /// Read a byte-offset slice with the same lossy UTF-8 behavior as the old
    /// filesystem reader.
    ///
    /// # Errors
    /// Returns [`RunLogStoreError::InvalidId`] for a stream outside this
    /// owner's namespace, or a storage/corruption error while reading it.
    pub async fn read_slice(
        &self,
        stream: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Result<RunLogSlice, RunLogStoreError> {
        validate_stream_id(stream)?;
        let text = self.read_text(stream).await?;
        Ok(slice(&text, offset, max_bytes))
    }

    /// Delete a run stream.
    ///
    /// # Errors
    /// Returns [`RunLogStoreError::InvalidId`] for a stream outside this
    /// owner's namespace, or the storage error returned by the stream port.
    pub async fn delete(&self, stream: &str) -> Result<bool, RunLogStoreError> {
        validate_stream_id(stream)?;
        Ok(self.streams.delete_stream(stream).await?)
    }

    async fn append_text(&self, stream: &str, text: &str) -> Result<(), RunLogStoreError> {
        validate_stream_id(stream)?;
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + APPEND_CHUNK_BYTES).min(text.len());
            // Each chunk boundary is adjusted to a UTF-8 boundary, preserving
            // the byte stream exactly while avoiding multi-megabyte entries.
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                return Err(RunLogStoreError::Corrupt);
            }
            self.streams
                .append(stream, Value::String(text[start..end].to_owned()))
                .await?;
            start = end;
        }
        Ok(())
    }

    async fn read_text(&self, stream: &str) -> Result<String, RunLogStoreError> {
        validate_stream_id(stream)?;
        let mut cursor = 0;
        let mut text = String::new();
        loop {
            let page = self.streams.read_window(stream, cursor, READ_PAGE).await?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map_or(cursor, |entry| entry.offset + 1);
            for entry in page {
                let chunk = entry.value.as_str().ok_or(RunLogStoreError::Corrupt)?;
                text.push_str(chunk);
            }
        }
        Ok(text)
    }
}

fn stream_id(workflow_id: &str, timestamp: &str, run_id: &str) -> Result<String, RunLogStoreError> {
    if workflow_id.is_empty() || timestamp.is_empty() || run_id.is_empty() {
        return Err(RunLogStoreError::InvalidId);
    }
    Ok(format!(
        "{PREFIX}{}.{}.{}",
        tinyskills::run_log::sanitize(workflow_id),
        tinyskills::run_log::sanitize(timestamp),
        tinyskills::run_log::sanitize(run_id)
    ))
}

fn validate_stream_id(stream: &str) -> Result<(), RunLogStoreError> {
    let Some(suffix) = stream.strip_prefix(PREFIX) else {
        return Err(RunLogStoreError::InvalidId);
    };
    if suffix.is_empty() {
        return Err(RunLogStoreError::InvalidId);
    }
    Ok(())
}

#[allow(dead_code)]
#[cfg(test)]
#[path = "run_log_tests.rs"]
mod tests;
