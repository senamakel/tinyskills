//! Versioned stream records and text chunk helpers for run logs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tinyskills::RunLogError;
use tinyskills::run_log;

use super::RECORD_CHUNK_BYTES;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum StreamRecord {
    Header {
        schema: u32,
        scope_id: String,
        run_id: String,
        workflow_id: String,
        generation: String,
    },
    Text {
        text: String,
    },
    Checkpoint {
        schema: u32,
        scope_id: String,
        run_id: String,
        generation: String,
        header_offset: u64,
        summary: run_log::RunLogSummary,
    },
    Tombstone {
        schema: u32,
        scope_id: String,
        run_id: String,
        generation: String,
    },
}

pub(super) fn record_value(record: &StreamRecord) -> Result<Value, RunLogError> {
    serde_json::to_value(record).map_err(|error| RunLogError::Backend(error.to_string()))
}

pub(super) fn text_records(text: &str) -> Result<Vec<Value>, RunLogError> {
    let mut records = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = start.saturating_add(RECORD_CHUNK_BYTES).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        records.push(record_value(&StreamRecord::Text {
            text: text[start..end].to_owned(),
        })?);
        start = end;
    }
    Ok(records)
}

pub(super) fn update_summary(summary: &mut run_log::RunLogSummary, text: &str) {
    for line in text.lines() {
        if line.starts_with("--- result ---") {
            summary.footer_started = true;
            continue;
        }
        if !summary.footer_started {
            continue;
        }
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match label.trim() {
            "status" => value.clone_into(&mut summary.run.status),
            "duration" => {
                summary.run.duration_ms = value.trim_end_matches(" ms").trim().parse::<u64>().ok();
            }
            "finished" => {
                summary.run.finished = Some(value.trim_end_matches(" UTC").trim().to_owned());
                summary.complete = !summary.run.status.is_empty();
            }
            _ => {}
        }
    }
}
