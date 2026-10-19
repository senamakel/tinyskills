//! Storage-neutral run-log persistence contract and file implementation.

use std::future::Future;
use std::io;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::run_log::{self, ScannedRun};

/// Maximum requested byte target for one [`RunLogStore::tail`] call.
///
/// A returned page may exceed its requested target by at most three bytes so
/// it can end on a UTF-8 character boundary.
pub const MAX_RUN_LOG_TAIL_BYTES: usize = 1024 * 1024;

/// A boxed future returned by [`RunLogStore`].
pub type RunLogFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, RunLogError>> + Send + 'a>>;

/// Failure from a run-log store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunLogError {
    /// The run or workflow identifier is invalid.
    #[error("invalid run-log identifier `{0}`")]
    InvalidId(String),
    /// A cursor is malformed or belongs to another store or run.
    #[error("invalid run-log cursor")]
    InvalidCursor,
    /// A file-backed operation failed.
    #[error("run-log file operation failed: {0}")]
    Io(#[from] io::Error),
    /// A storage backend operation failed.
    #[error("run-log backend operation failed: {0}")]
    Backend(String),
    /// The store's asynchronous file operation could not be joined.
    #[error("run-log file task failed: {0}")]
    Task(String),
}

/// Initial metadata written before a run's log lines.
#[derive(Clone)]
pub struct RunLogStart<'a> {
    /// Skill or workflow identifier.
    pub workflow_id: &'a str,
    /// Unique run identifier.
    pub run_id: &'a str,
    /// RFC 3339 start timestamp.
    pub started: &'a str,
    /// Inputs recorded in the header.
    pub inputs: &'a Value,
    /// Resolved task prompt recorded in the header.
    pub task_prompt: &'a str,
}

impl std::fmt::Debug for RunLogStart<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLogStart")
            .field("workflow_id", &self.workflow_id)
            .field("run_id", &self.run_id)
            .field("started", &self.started)
            .field("inputs", &"[redacted]")
            .field("task_prompt", &"[redacted]")
            .finish()
    }
}

/// An opaque continuation token returned by [`RunLogStore::tail`].
///
/// Callers must retain and return this value unchanged. Its representation is
/// private and is not a byte offset, even for the file implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunCursor(String);

impl RunCursor {
    /// Wrap an implementation-defined cursor token received from a store.
    #[must_use]
    pub fn from_opaque(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Borrow the implementation-defined token for serialization or transport.
    #[must_use]
    pub fn as_opaque(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CursorState {
    backend: String,
    run_id: String,
    generation: String,
    position: u64,
    within: usize,
}

impl RunCursor {
    pub(crate) fn new(
        backend: &str,
        run_id: &str,
        generation: &str,
        position: u64,
        within: usize,
    ) -> Result<Self, RunLogError> {
        let state = CursorState {
            backend: backend.to_owned(),
            run_id: run_id.to_owned(),
            generation: generation.to_owned(),
            position,
            within,
        };
        let bytes =
            serde_json::to_vec(&state).map_err(|error| RunLogError::Backend(error.to_string()))?;
        let mut token = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(token, "{byte:02x}");
        }
        Ok(Self(token))
    }

    pub(crate) fn state(
        &self,
        backend: &str,
        run_id: &str,
        generation: &str,
    ) -> Result<(u64, usize), RunLogError> {
        let state = self.decode_state()?;
        if state.backend != backend || state.run_id != run_id || state.generation != generation {
            return Err(RunLogError::InvalidCursor);
        }
        Ok((state.position, state.within))
    }

    pub(crate) fn validate_owner(&self, backend: &str, run_id: &str) -> Result<(), RunLogError> {
        let state = self.decode_state()?;
        if state.backend != backend || state.run_id != run_id {
            return Err(RunLogError::InvalidCursor);
        }
        Ok(())
    }

    fn decode_state(&self) -> Result<CursorState, RunLogError> {
        if self.0.is_empty() || self.0.len() > 8192 || !self.0.len().is_multiple_of(2) {
            return Err(RunLogError::InvalidCursor);
        }
        let bytes = self
            .0
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                let hex = std::str::from_utf8(pair).map_err(|_| RunLogError::InvalidCursor)?;
                u8::from_str_radix(hex, 16).map_err(|_| RunLogError::InvalidCursor)
            })
            .collect::<Result<Vec<_>, _>>()?;
        serde_json::from_slice(&bytes).map_err(|_| RunLogError::InvalidCursor)
    }
}

/// A bounded portion of a run log and the token for the next read.
///
/// A page may extend up to three bytes beyond the requested target to avoid
/// splitting a UTF-8 character.
#[derive(Clone, Serialize, Deserialize)]
pub struct RunLogPage {
    /// Text returned by this read, possibly empty when the run is current.
    pub content: String,
    /// Cursor to pass to the next tail request.
    pub cursor: RunCursor,
    /// Whether this page reached the current end of the stored log.
    pub eof: bool,
    /// Whether the run has a completed result footer.
    pub complete: bool,
}

impl std::fmt::Debug for RunLogPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunLogPage")
            .field("content", &"[redacted]")
            .field("cursor", &self.cursor)
            .field("eof", &self.eof)
            .field("complete", &self.complete)
            .finish()
    }
}

/// Persistent run-log operations independent of the backing store.
pub trait RunLogStore: Send + Sync + std::fmt::Debug {
    /// Create a run and write its workflow, inputs, prompt, and start time.
    fn start<'a>(&'a self, run: RunLogStart<'a>) -> RunLogFuture<'a, ()>;

    /// Append text to an existing run. A trailing newline is added when absent.
    fn append<'a>(&'a self, run_id: &'a str, line: &'a str) -> RunLogFuture<'a, ()>;

    /// Append the terminal status and output footer to an existing run.
    fn finish<'a>(
        &'a self,
        run_id: &'a str,
        status: &'a str,
        elapsed_ms: u64,
        finished: &'a str,
        output: &'a str,
    ) -> RunLogFuture<'a, ()> {
        let footer = run_log::format_footer(status, elapsed_ms, finished, output);
        Box::pin(async move { self.append(run_id, &footer).await })
    }

    /// Read toward `max_bytes` from a run, resuming from an optional cursor.
    ///
    /// The target is capped at [`MAX_RUN_LOG_TAIL_BYTES`]. A page may include
    /// up to three extra bytes to finish a UTF-8 character, and cursors always
    /// identify UTF-8 boundaries. A zero-byte request returns an empty page
    /// without advancing its cursor.
    fn tail<'a>(
        &'a self,
        run_id: &'a str,
        cursor: Option<&'a RunCursor>,
        max_bytes: usize,
    ) -> RunLogFuture<'a, RunLogPage>;

    /// List runs sorted by start time descending, optionally filtered by workflow.
    fn list<'a>(
        &'a self,
        workflow_id: Option<&'a str>,
        limit: usize,
    ) -> RunLogFuture<'a, Vec<ScannedRun>>;

    /// Delete one run, returning whether it existed.
    fn clear<'a>(&'a self, run_id: &'a str) -> RunLogFuture<'a, bool>;
}

#[cfg(test)]
#[path = "run_log_store_tests.rs"]
mod tests;
