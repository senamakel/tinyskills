//! Pure formatting and parsing for skill-run logs.
//!
//! This module has no filesystem or storage behavior. Hosts format run headers,
//! step lines, and footers here, then persist the resulting UTF-8 bytes through
//! their configured run-log store. The storage adapter owns discovery and
//! stream reads; this module only interprets supplied content.

#![allow(clippy::missing_errors_doc, clippy::cast_possible_truncation)]

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

const RESULT_MARKER: &str = "--- result ---";

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

/// Format the original human-readable run header as UTF-8 text. Prompt lines
/// are indented two spaces so no prompt line can pass for a header field or
/// the result marker.
#[must_use]
pub fn format_header(
    workflow_id: &str,
    run_id: &str,
    started: &str,
    inputs: &Value,
    task_prompt: &str,
) -> String {
    let task_prompt = task_prompt
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "==== workflow_run: {workflow_id} ====\n\
         run_id : {run_id}\n\
         started: {started} UTC\n\
         inputs : {inputs}\n\n\
         --- task prompt ---\n{task_prompt}\n\n\
         --- steps ---\n",
        inputs = serde_json::to_string(inputs).unwrap_or_default(),
    )
}

/// Format the original terminal footer as UTF-8 text.
#[must_use]
pub fn format_footer(status: &str, elapsed_ms: u64, finished: &str, output: &str) -> String {
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

/// One run parsed from a stored stream. Status is `RUNNING` until a footer
/// lands; `stream_id` is an opaque store name, not a filesystem path.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ScannedRun {
    /// Full run id from the header.
    pub run_id: String,
    /// Skill id from the header banner.
    pub workflow_id: String,
    /// Header `started:` timestamp; empty if the header was malformed.
    pub started: String,
    /// `DONE` / `STOPPED` / `DEGENERATE` / `FAILED` / `RUNNING`.
    pub status: String,
    /// Footer duration in milliseconds; `None` while running.
    pub duration_ms: Option<u64>,
    /// Footer `finished:` timestamp; `None` while running.
    pub finished: Option<String>,
    /// Opaque stream identifier supplied by the storage adapter.
    pub stream_id: String,
}

/// Parse stored run contents and return matching runs sorted newest first.
#[must_use]
pub fn parse_runs(
    entries: impl IntoIterator<Item = (String, String)>,
    workflow_id: Option<&str>,
    limit: usize,
) -> Vec<ScannedRun> {
    let mut runs = entries
        .into_iter()
        .filter_map(|(stream_id, text)| parse_run(&stream_id, &text))
        .filter(|run| workflow_id.is_none_or(|want| run.workflow_id == want))
        .collect::<Vec<_>>();
    runs.sort_by(|a, b| b.started.cmp(&a.started));
    runs.truncate(limit);
    runs
}

fn parse_run(stream_id: &str, text: &str) -> Option<ScannedRun> {
    let mut workflow_id = String::new();
    let mut run_id = String::new();
    let mut started = String::new();
    let mut status = String::from("RUNNING");
    let mut duration_ms = None;
    let mut finished = None;
    let mut seen_result = false;
    for line in text.lines() {
        if line.starts_with("==== workflow_run:") {
            line.trim_start_matches("==== workflow_run:")
                .trim()
                .trim_end_matches('=')
                .trim()
                .clone_into(&mut workflow_id);
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
            ("run_id", false) if run_id.is_empty() => value.clone_into(&mut run_id),
            ("started", false) if started.is_empty() => value.clone_into(&mut started),
            ("status", true) => value.clone_into(&mut status),
            ("duration", true) => {
                if let Ok(value) = value.trim_end_matches(" ms").trim().parse::<u64>() {
                    duration_ms = Some(value);
                }
            }
            ("finished", true) => {
                finished = Some(value.trim_end_matches(" UTC").trim().to_owned());
            }
            _ => {}
        }
    }
    if workflow_id.is_empty() || run_id.is_empty() {
        return None;
    }
    Some(ScannedRun {
        run_id,
        workflow_id,
        started,
        status,
        duration_ms,
        finished,
        stream_id: stream_id.to_owned(),
    })
}

/// Terminal outcome of a finished run, parsed from the footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// `DONE` / `STOPPED` / `DEGENERATE` / `FAILED` / `RUNNING`.
    pub status: String,
    /// Final output body after the footer.
    pub output: String,
}

/// Parse the terminal outcome from run contents. `None` means it is still
/// running, the footer is partial, or the supplied data is malformed.
#[must_use]
pub fn parse_terminal_outcome(text: &str) -> Option<RunOutcome> {
    let marker = format!("\n{RESULT_MARKER}\n");
    let after = match text.strip_prefix(&marker[1..]) {
        Some(after) => after,
        None => &text[text.find(&marker)? + marker.len()..],
    };
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
            "status" => value.trim().clone_into(&mut status),
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
    let output = lines.collect::<Vec<_>>().join("\n").trim().to_owned();
    Some(RunOutcome { status, output })
}

/// One slice of a stored log. `offset` is the byte cursor for the next call.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RunLogSlice {
    /// New read cursor, the next call's `offset`.
    pub offset: u64,
    /// Bytes returned in this slice.
    pub bytes_read: u64,
    /// Slice text (lossy UTF-8 when a byte range splits a code point).
    pub content: String,
    /// Whether the slice reached the current end of the log.
    pub eof: bool,
    /// Whether a result footer exists anywhere in the stored log.
    pub complete: bool,
}

/// Read up to `max_bytes` from an in-memory copy of a stored log.
#[must_use]
pub fn slice(text: &str, offset: u64, max_bytes: usize) -> RunLogSlice {
    let bytes = text.as_bytes();
    let len = bytes.len() as u64;
    let complete = text.contains("\n--- result ---\n") || text.starts_with("--- result ---\n");
    if offset >= len {
        return RunLogSlice {
            offset,
            bytes_read: 0,
            content: String::new(),
            eof: true,
            complete,
        };
    }
    let start = usize::try_from(offset).unwrap_or(bytes.len());
    let want = usize::try_from((len - offset).min(max_bytes as u64)).unwrap_or(max_bytes);
    let end = start.saturating_add(want).min(bytes.len());
    let chunk = &bytes[start..end];
    let bytes_read = chunk.len() as u64;
    RunLogSlice {
        offset: offset + bytes_read,
        bytes_read,
        content: String::from_utf8_lossy(chunk).into_owned(),
        eof: end >= bytes.len(),
        complete,
    }
}

#[cfg(test)]
#[path = "run_log_tests.rs"]
mod tests;
