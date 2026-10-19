//! Run-log lifecycle and read operations for the stream adapter.

use super::cursor::{StreamCursorState, cursor_state, encode_cursor, validate_cursor_owner};
use super::records::{StreamRecord, record_value, text_records, update_summary};
use super::{
    APPEND_RETRIES, MAX_BATCH_RECORDS, RECORD_CHUNK_BYTES, RUN_LOG_PREFIX, StreamRunLogStore,
    new_generation, storage_error, validate_id, validate_started,
};
use tinyskills::run_log::{self, ScannedRun};
use tinyskills::{RunCursor, RunLogError, RunLogFuture, RunLogPage, RunLogStart, RunLogStore};

impl RunLogStore for StreamRunLogStore {
    fn start<'a>(&'a self, run: RunLogStart<'a>) -> RunLogFuture<'a, ()> {
        Box::pin(async move {
            let stream = Self::stream_name(run.run_id)?;
            validate_id(run.workflow_id)?;
            validate_started(run.started)?;
            let length = self
                .streams
                .len(&stream)
                .await
                .map_err(|error| storage_error(&error))?;
            let header_offset = if length == 0 {
                0
            } else {
                let latest = self.record(&stream, length - 1).await?;
                self.validate_tombstone(latest, run.run_id)?;
                length
            };
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
                header_offset,
                summary,
            })?);
            if values.len() > MAX_BATCH_RECORDS {
                return Err(RunLogError::Backend(
                    "run header exceeds the stream batch limit".into(),
                ));
            }
            self.streams
                .append_batch_if_offset(&stream, length, values)
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
            let mut initial_generation = None;
            for _ in 0..APPEND_RETRIES {
                let length = self
                    .streams
                    .len(&stream)
                    .await
                    .map_err(|error| storage_error(&error))?;
                if length == 0 {
                    return Err(RunLogError::Backend("run log not found".into()));
                }
                let (_, checkpoint) = self.checkpoint(&stream, length).await?;
                let (scope_id, generation, header_offset, mut summary) =
                    self.validate_checkpoint(checkpoint, run_id)?;
                if initial_generation
                    .as_ref()
                    .is_some_and(|expected| expected != &generation)
                {
                    return Err(RunLogError::Backend(
                        "run generation changed while appending".into(),
                    ));
                }
                initial_generation.get_or_insert_with(|| generation.clone());
                let (header_scope, header_generation, workflow_id) =
                    self.validate_header(&stream, run_id, header_offset).await?;
                if header_scope != scope_id || header_generation != generation {
                    return Err(RunLogError::Backend("run stream metadata mismatch".into()));
                }
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
                    header_offset,
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
            let (checkpoint_offset, checkpoint) = self
                .latest_record(&stream)
                .await?
                .ok_or_else(|| RunLogError::Backend("run log not found".into()))?;
            if matches!(&checkpoint, StreamRecord::Tombstone { .. }) {
                return Err(if cursor.is_some() {
                    RunLogError::InvalidCursor
                } else {
                    RunLogError::Backend("run log not found".into())
                });
            }
            let (scope_id, generation, header_offset, summary) =
                self.validate_checkpoint(checkpoint, run_id)?;
            let (header_scope, header_generation, workflow_id) =
                self.validate_header(&stream, run_id, header_offset).await?;
            if scope_id != self.scope_id
                || header_scope != scope_id
                || header_generation != generation
                || summary.run.workflow_id != workflow_id
            {
                return Err(RunLogError::Backend("run stream metadata mismatch".into()));
            }
            let (offset, within) = cursor.map_or(Ok((header_offset + 1, 0)), |cursor| {
                cursor_state(cursor, &backend, run_id, &generation)
            })?;
            if offset < header_offset + 1
                || offset > checkpoint_offset + 1
                || within > RECORD_CHUNK_BYTES
            {
                return Err(RunLogError::InvalidCursor);
            }
            if offset >= checkpoint_offset && within != 0 {
                return Err(RunLogError::InvalidCursor);
            }
            if max_bytes == 0 {
                return Ok(RunLogPage {
                    content: String::new(),
                    cursor: RunCursor::from_opaque(encode_cursor(&StreamCursorState {
                        backend,
                        run_id: run_id.to_owned(),
                        generation,
                        position: offset,
                        within,
                    })?),
                    eof: offset > checkpoint_offset && within == 0,
                    complete: summary.complete,
                });
            }
            let (content, offset, within) = self
                .page_text(
                    &stream,
                    header_offset,
                    checkpoint_offset,
                    offset,
                    within,
                    max_bytes,
                )
                .await?;
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
                let Some((_, checkpoint)) = self.latest_record(&stream).await? else {
                    continue;
                };
                let (scope_id, generation, header_offset, summary) = match checkpoint {
                    record @ StreamRecord::Checkpoint { .. } => {
                        self.validate_checkpoint(record, &run_id)?
                    }
                    StreamRecord::Tombstone { .. } => continue,
                    _ => {
                        return Err(RunLogError::Backend(
                            "run stream checkpoint is malformed".into(),
                        ));
                    }
                };
                let (header_scope, header_generation, header_workflow_id) = self
                    .validate_header(&stream, &run_id, header_offset)
                    .await?;
                if scope_id == header_scope
                    && generation == header_generation
                    && summary.run.run_id == run_id
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
            for _ in 0..APPEND_RETRIES {
                let Some((offset, latest)) = self.latest_record(&stream).await? else {
                    return Ok(false);
                };
                let generation = match latest {
                    tombstone @ StreamRecord::Tombstone { .. } => {
                        self.validate_tombstone(tombstone, run_id)?;
                        self.streams
                            .truncate_before(&stream, offset)
                            .await
                            .map_err(|error| storage_error(&error))?;
                        return Ok(false);
                    }
                    checkpoint @ StreamRecord::Checkpoint { .. } => {
                        let (scope_id, generation, header_offset, summary) =
                            self.validate_checkpoint(checkpoint, run_id)?;
                        let (header_scope, header_generation, _) =
                            self.validate_header(&stream, run_id, header_offset).await?;
                        if scope_id != header_scope || generation != header_generation {
                            return Err(RunLogError::Backend(
                                "run stream metadata mismatch".into(),
                            ));
                        }
                        if summary.run.workflow_id.is_empty() {
                            return Err(RunLogError::Backend(
                                "run stream metadata mismatch".into(),
                            ));
                        }
                        generation
                    }
                    _ => {
                        return Err(RunLogError::Backend(
                            "run stream checkpoint is malformed".into(),
                        ));
                    }
                };
                let tombstone = record_value(&StreamRecord::Tombstone {
                    schema: 1,
                    scope_id: self.scope_id.clone(),
                    run_id: run_id.to_owned(),
                    generation,
                })?;
                match self
                    .streams
                    .append_batch_if_offset(&stream, offset + 1, vec![tombstone])
                    .await
                {
                    Ok(tombstone_offset) => {
                        self.streams
                            .truncate_before(&stream, tombstone_offset)
                            .await
                            .map_err(|error| storage_error(&error))?;
                        return Ok(true);
                    }
                    Err(error) if error.kind() == tinystoragedrivers_core::ErrorKind::Conflict => {}
                    Err(error) => return Err(storage_error(&error)),
                }
            }
            Err(RunLogError::Backend(
                "run stream changed during every bounded clear retry".into(),
            ))
        })
    }
}
