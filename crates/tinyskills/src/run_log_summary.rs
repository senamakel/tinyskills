//! Bounded-memory metadata parsing for the human-readable run-log format.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;

#[cfg(test)]
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

use serde::{Deserialize, Serialize};

use super::ScannedRun;

const RESULT_MARKER: &str = "--- result ---";
const MAX_RUN_LOG_IDENTITY_BYTES: u64 = 16 * 1024;

#[cfg(test)]
static FULL_FILE_SUMMARY_PARSE_COUNTS: std::sync::LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bounded-memory summary parsed from a run log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunLogSummary {
    /// Parsed run metadata, when the identifying header is valid.
    pub run: ScannedRun,
    /// Whether the footer has both a status and a finished timestamp.
    pub complete: bool,
    /// Whether the result marker has appeared, even if its footer is partial.
    pub footer_started: bool,
}

/// Parse a run summary with fixed memory usage, regardless of log history size.
///
/// # Errors
///
/// Returns an error when the log cannot be opened or read.
pub fn read_run_log_summary(path: &Path) -> std::io::Result<Option<RunLogSummary>> {
    #[cfg(test)]
    {
        let mut counts = FULL_FILE_SUMMARY_PARSE_COUNTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *counts.entry(path.to_path_buf()).or_default() += 1;
    }
    const CHUNK_SIZE: usize = 8192;
    const MAX_LINE_SIZE: usize = 8192;
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut chunk = [0; CHUNK_SIZE];
    let mut line = Vec::with_capacity(256);
    let mut skipping_long_line = false;
    let mut fields = ParsedRunFields::default();
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            if !line.is_empty() && !skipping_long_line {
                fields.read_line(&line);
            }
            break;
        }
        for byte in &chunk[..count] {
            if *byte == b'\n' {
                if !skipping_long_line {
                    fields.read_line(&line);
                }
                line.clear();
                skipping_long_line = false;
            } else if !skipping_long_line {
                if line.len() < MAX_LINE_SIZE {
                    line.push(*byte);
                } else {
                    line.clear();
                    skipping_long_line = true;
                }
            }
        }
    }
    Ok(fields.finish(&path.to_string_lossy()))
}

#[cfg(test)]
pub(crate) fn full_file_summary_parse_count(path: &Path) -> usize {
    FULL_FILE_SUMMARY_PARSE_COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .copied()
        .unwrap_or_default()
}

/// Read the workflow and run ids from the bounded prefix containing the log header.
pub(crate) fn read_run_log_header_identity(
    path: &Path,
) -> std::io::Result<Option<(String, String)>> {
    read_run_log_header_identity_from_reader(std::fs::File::open(path)?)
}

pub(crate) fn read_run_log_header_identity_from_reader(
    reader: impl Read,
) -> std::io::Result<Option<(String, String)>> {
    let mut reader = BufReader::with_capacity(128, reader.take(MAX_RUN_LOG_IDENTITY_BYTES));
    let mut workflow_id = None;
    let mut run_id = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line.starts_with("==== workflow_run:") {
            workflow_id = Some(
                line.trim_start_matches("==== workflow_run:")
                    .trim()
                    .trim_end_matches('=')
                    .trim()
                    .to_owned(),
            );
        } else if let Some((label, value)) = line.split_once(':')
            && label.trim() == "run_id"
        {
            run_id = Some(value.trim().to_owned());
        }
        if let (Some(workflow_id), Some(run_id)) = (&workflow_id, &run_id) {
            return Ok(Some((workflow_id.clone(), run_id.clone())));
        }
    }
}

/// Parse a summary from an already available textual header or append.
#[must_use]
pub fn read_run_log_summary_from_text(text: &str, log_path: &str) -> Option<RunLogSummary> {
    let mut fields = ParsedRunFields::default();
    for line in text.lines() {
        fields.read_line(line.as_bytes());
    }
    fields.finish(log_path)
}

#[derive(Default)]
struct ParsedRunFields {
    workflow_id: String,
    run_id: String,
    started: String,
    status: String,
    duration_ms: Option<u64>,
    finished: Option<String>,
    seen_result: bool,
}

impl ParsedRunFields {
    fn read_line(&mut self, bytes: &[u8]) {
        let Ok(line) = std::str::from_utf8(bytes) else {
            return;
        };
        if line.starts_with("==== workflow_run:") {
            self.workflow_id = line
                .trim_start_matches("==== workflow_run:")
                .trim()
                .trim_end_matches('=')
                .trim()
                .to_string();
            return;
        }
        if line.starts_with(RESULT_MARKER) {
            self.seen_result = true;
            return;
        }
        let Some((label_raw, value_raw)) = line.split_once(':') else {
            return;
        };
        let label = label_raw.trim();
        let value = value_raw.trim();
        match (label, self.seen_result) {
            ("run_id", false) => self.run_id = value.to_string(),
            ("started", false) => self.started = value.to_string(),
            ("status", true) => self.status = value.to_string(),
            ("duration", true) => {
                if let Ok(duration) = value.trim_end_matches(" ms").trim().parse::<u64>() {
                    self.duration_ms = Some(duration);
                }
            }
            ("finished", true) => {
                self.finished = Some(value.trim_end_matches(" UTC").trim().to_string());
            }
            _ => {}
        }
    }

    fn finish(self, log_path: &str) -> Option<RunLogSummary> {
        if self.workflow_id.is_empty() || self.run_id.is_empty() {
            return None;
        }
        let complete = self.seen_result && !self.status.is_empty() && self.finished.is_some();
        Some(RunLogSummary {
            run: ScannedRun {
                run_id: self.run_id,
                workflow_id: self.workflow_id,
                started: self.started,
                status: if self.status.is_empty() {
                    "RUNNING".to_string()
                } else {
                    self.status
                },
                duration_ms: self.duration_ms,
                finished: self.finished,
                log_path: log_path.to_string(),
            },
            complete,
            footer_started: self.seen_result,
        })
    }
}
