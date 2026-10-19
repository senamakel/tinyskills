//! Per-run log layout for skill runs.
//!
//! A run writes a human-readable trace to
//! `<workspace>/skills/.runs/<skill>_<ts>_<run>.log`: a header (skill, inputs,
//! task prompt), one line per step appended by the host, then a footer (status,
//! duration, final output). `.runs` is a sibling of the skill definitions
//! (`<workspace>/skills/<id>/`) so run logs never collide with a skill-id
//! directory.
//!
//! This module owns the path layout, the header and footer formats, and the
//! readers that parse them back. It takes plain strings (timestamps included,
//! so it links no clock) and knows nothing of any agent event type: the host
//! formats its own step lines and hands them to [`append_line`].

#![allow(clippy::missing_errors_doc, clippy::cast_possible_truncation)]

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

const RESULT_MARKER: &str = "--- result ---";

#[path = "run_log_summary.rs"]
mod summary;
pub use summary::{RunLogSummary, read_run_log_summary, read_run_log_summary_from_text};

/// `<workspace>/skills/.runs`.
#[must_use]
pub fn runs_dir(workspace: &Path) -> PathBuf {
    workspace.join("skills").join(".runs")
}

/// Replace every character outside `[A-Za-z0-9_-]` with `-`.
#[must_use]
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The first 8 characters of an id (the whole id when shorter).
#[must_use]
pub fn short(s: &str) -> &str {
    s.get(..8).unwrap_or(s)
}

/// Collapse newlines to spaces and cap at `n` characters, marking a cut with `…`.
#[must_use]
pub fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}

/// `<runs_dir>/<skill>_<ts>_<short run id>.log`. `timestamp` is the host's
/// compact UTC stamp, e.g. `20260528T075113Z`.
#[must_use]
pub fn run_log_path(workspace: &Path, workflow_id: &str, run_id: &str, timestamp: &str) -> PathBuf {
    runs_dir(workspace).join(format!(
        "{}_{}_{}.log",
        sanitize(workflow_id),
        sanitize(timestamp),
        sanitize(short(run_id))
    ))
}

/// Append one line (newline-terminated) to `path`, creating its directory.
pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    if !line.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    f.flush()
}

/// Write the run header (skill, inputs, the resolved task prompt). `started`
/// is an RFC 3339 timestamp.
pub fn write_header(
    path: &Path,
    workflow_id: &str,
    run_id: &str,
    started: &str,
    inputs: &Value,
    task_prompt: &str,
) -> std::io::Result<()> {
    append_line(
        path,
        &format_header(workflow_id, run_id, started, inputs, task_prompt),
    )
}

/// Format a run header in the same layout written by [`write_header`].
#[must_use]
pub fn format_header(
    workflow_id: &str,
    run_id: &str,
    started: &str,
    inputs: &Value,
    task_prompt: &str,
) -> String {
    format!(
        "==== workflow_run: {workflow_id} ====\n\
         run_id : {run_id}\n\
         started: {started} UTC\n\
         inputs : {inputs}\n\n\
         --- task prompt ---\n{task_prompt}\n\n\
         --- steps ---",
        inputs = serde_json::to_string(inputs).unwrap_or_default(),
    )
}

/// Write the final footer: status, duration, and the final output text.
/// `finished` is an RFC 3339 timestamp.
pub fn write_footer(
    path: &Path,
    status: &str,
    elapsed_ms: u64,
    finished: &str,
    output: &str,
) -> std::io::Result<()> {
    append_line(path, &format_footer(status, elapsed_ms, finished, output))
}

pub(crate) fn format_footer(status: &str, elapsed_ms: u64, finished: &str, output: &str) -> String {
    format!(
        "\n{RESULT_MARKER}\n\
         status  : {status}\n\
         duration: {elapsed_ms} ms\n\
         finished: {finished} UTC\n\n{output}\n",
    )
}

/// Detect the degenerate "same paragraph repeated many times in one
/// generation" failure mode: trims each line, counts those of at least
/// `min_len` bytes, and returns the most-repeated line with its count when the
/// count reaches `min_count`.
#[must_use]
pub fn detect_repeated_line(
    text: &str,
    min_len: usize,
    min_count: usize,
) -> Option<(String, usize)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in text.lines() {
        let t = line.trim();
        if t.len() >= min_len {
            *counts.entry(t).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, c)| *c >= min_count)
        .max_by_key(|(_, c)| *c)
        .map(|(line, count)| (line.to_string(), count))
}

/// One run extracted from a `.runs/<skill>_<ts>_<run>.log` file. Status is
/// `RUNNING` until the footer lands.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ScannedRun {
    /// Full run id from the header.
    pub run_id: String,
    /// Skill id from the header banner.
    pub workflow_id: String,
    /// Header `started:` timestamp (RFC 3339); empty if the header was malformed.
    pub started: String,
    /// `DONE` / `STOPPED` / `DEGENERATE` / `FAILED` / `RUNNING` (no footer yet).
    pub status: String,
    /// Footer `duration: <ms> ms`; `None` while running.
    pub duration_ms: Option<u64>,
    /// Footer `finished:` timestamp; `None` while running.
    pub finished: Option<String>,
    /// Path to the streaming log file.
    pub log_path: String,
}

/// Scan `<workspace>/skills/.runs/`, parse each log's header and footer, and
/// return runs sorted by `started` descending. `workflow_id` filters by skill;
/// `limit` caps the result after filtering and sorting. Malformed files are
/// skipped.
#[must_use]
pub fn scan_runs(workspace: &Path, workflow_id: Option<&str>, limit: usize) -> Vec<ScannedRun> {
    let mut runs: Vec<ScannedRun> = Vec::new();
    let Ok(entries) = std::fs::read_dir(runs_dir(workspace)) else {
        return runs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let Ok(Some(summary)) = read_run_log_summary(&path) else {
            continue;
        };
        let run = summary.run;
        if workflow_id.is_none_or(|want| run.workflow_id == want) {
            runs.push(run);
        }
    }
    runs.sort_by(|a, b| b.started.cmp(&a.started));
    runs.truncate(limit);
    runs
}

/// Parse one complete or currently-writing run log using the existing header
/// and footer rules. `log_path` is retained for compatibility with the
/// [`ScannedRun`] wire shape; storage-backed adapters may supply a stable URI.
#[must_use]
pub fn parse_run_log_text(text: &str, log_path: &str) -> Option<ScannedRun> {
    let mut sid = String::new();
    let mut rid = String::new();
    let mut started = String::new();
    let mut status = String::from("RUNNING");
    let mut duration_ms: Option<u64> = None;
    let mut finished: Option<String> = None;
    let mut seen_result = false;
    for line in text.lines() {
        if line.starts_with("==== workflow_run:") {
            sid = line
                .trim_start_matches("==== workflow_run:")
                .trim()
                .trim_end_matches('=')
                .trim()
                .to_string();
            continue;
        }
        if line.starts_with(RESULT_MARKER) {
            seen_result = true;
            continue;
        }
        let Some((label_raw, value_raw)) = line.split_once(':') else {
            continue;
        };
        let label = label_raw.trim();
        let value = value_raw.trim();
        match (label, seen_result) {
            ("run_id", false) => rid = value.to_string(),
            ("started", false) => started = value.to_string(),
            ("status", true) => status = value.to_string(),
            ("duration", true) => {
                if let Ok(n) = value.trim_end_matches(" ms").trim().parse::<u64>() {
                    duration_ms = Some(n);
                }
            }
            ("finished", true) => {
                finished = Some(value.trim_end_matches(" UTC").trim().to_string());
            }
            _ => {}
        }
    }
    if sid.is_empty() || rid.is_empty() {
        return None;
    }
    Some(ScannedRun {
        run_id: rid,
        workflow_id: sid,
        started,
        status,
        duration_ms,
        finished,
        log_path: log_path.to_owned(),
    })
}

/// Resolve a run id to its log path by matching the `_<first 8 chars>.log`
/// file-name suffix, so the caller never supplies a path.
#[must_use]
pub fn find_run_log_path(workspace: &Path, run_id: &str) -> Option<PathBuf> {
    if run_id.is_empty() {
        return None;
    }
    let suffix = format!("_{}.log", short(run_id));
    for entry in std::fs::read_dir(runs_dir(workspace)).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) == Some("log") && name.contains(&suffix) {
            return Some(path);
        }
    }
    None
}

/// Terminal outcome of a finished run, parsed from the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// `DONE` / `STOPPED` / `DEGENERATE` / `FAILED` / `RUNNING` (no footer yet).
    pub status: String,
    /// Final output body after the footer.
    pub output: String,
}

/// Read a run log and, once the footer has landed, return the status and the
/// final output body. `None` while the run is still running, the footer is
/// partially written (no closing `finished:` line), or the file is unreadable.
#[must_use]
pub fn read_terminal_outcome(path: &Path) -> Option<RunOutcome> {
    let text = std::fs::read_to_string(path).ok()?;
    let idx = text.find(RESULT_MARKER)?;
    let after = &text[idx + RESULT_MARKER.len()..];
    let mut status = String::new();
    let mut saw_finished = false;
    let mut lines = after.lines();
    for line in lines.by_ref() {
        let Some((label, value)) = line.split_once(':') else {
            if line.trim().is_empty() {
                continue;
            }
            break;
        };
        match label.trim() {
            "status" => status = value.trim().to_string(),
            "finished" => {
                saw_finished = true;
                break;
            }
            _ => {}
        }
    }
    if status.is_empty() || !saw_finished {
        return None;
    }
    let output = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    Some(RunOutcome { status, output })
}

/// One slice of a run log. `offset` is the new read cursor; `complete` is true
/// once the footer landed, so a viewer can stop polling.
#[derive(Debug, Clone, Serialize)]
pub struct RunLogSlice {
    /// New read cursor, the next call's `offset`.
    pub offset: u64,
    /// Bytes returned in this slice.
    pub bytes_read: u64,
    /// Slice text, ending at a UTF-8 character boundary.
    pub content: String,
    /// Whether the slice reached end of file.
    pub eof: bool,
    /// Whether the footer has landed.
    pub complete: bool,
}

/// Read at least one byte and up to `max_bytes` of the log from `offset`,
/// extending by at most three bytes when needed to finish a UTF-8 scalar.
///
/// Offsets must point to UTF-8 character boundaries. A cursor inside a scalar
/// is rejected instead of returning replacement characters.
pub fn read_run_log_slice(
    path: &Path,
    offset: u64,
    max_bytes: usize,
) -> std::io::Result<RunLogSlice> {
    let mut f = std::fs::File::open(path)?;
    let file_size = f.metadata()?.len();
    if offset > file_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cursor is past end of run log",
        ));
    }
    if offset >= file_size {
        return Ok(RunLogSlice {
            offset,
            bytes_read: 0,
            content: String::new(),
            eof: true,
            complete: file_has_footer(path)?,
        });
    }
    let mut first = [0; 1];
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(&mut first)?;
    if (0x80..=0xbf).contains(&first[0]) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cursor is inside a UTF-8 scalar",
        ));
    }
    if max_bytes == 0 {
        return Ok(RunLogSlice {
            offset,
            bytes_read: 0,
            content: String::new(),
            eof: false,
            complete: false,
        });
    }
    f.seek(SeekFrom::Start(offset))?;
    let target = max_bytes;
    let want = usize::try_from((file_size - offset).min(target as u64)).unwrap_or(target);
    let mut buf = vec![0u8; want];
    f.read_exact(&mut buf)?;
    while std::str::from_utf8(&buf).is_err_and(|error| error.error_len().is_none())
        && u64::try_from(buf.len()).unwrap_or(u64::MAX) < file_size - offset
        && buf.len() < target.saturating_add(3)
    {
        let mut next = [0; 1];
        f.read_exact(&mut next)?;
        buf.push(next[0]);
    }
    let bytes_read = buf.len() as u64;
    let content = String::from_utf8(buf).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "run log is not valid UTF-8",
        )
    })?;
    let new_offset = offset + bytes_read;
    let eof = new_offset >= file_size;
    let complete = eof && file_has_footer(path)?;
    Ok(RunLogSlice {
        offset: new_offset,
        bytes_read,
        content,
        eof,
        complete,
    })
}

fn file_has_footer(path: &Path) -> std::io::Result<bool> {
    let summary_path = path.with_extension("summary.json");
    if let Ok(bytes) = std::fs::read(summary_path)
        && let Ok(summary) = serde_json::from_slice::<serde_json::Value>(&bytes)
        && let Some(complete) = summary
            .get("summary")
            .and_then(|summary| summary.get("complete"))
            .and_then(serde_json::Value::as_bool)
    {
        return Ok(complete);
    }
    Ok(read_run_log_summary(path)?.is_some_and(|summary| summary.complete))
}

#[cfg(test)]
#[path = "run_log_tests.rs"]
mod tests;
