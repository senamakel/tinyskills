//! The file-backed implementation of the run-log persistence contract.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::{
    MAX_RUN_LOG_TAIL_BYTES, RunCursor, RunLogError, RunLogFuture, RunLogPage, RunLogStart,
    RunLogStore,
};
use crate::run_log::{self, ScannedRun};

/// The existing `<workspace>/skills/.runs/*.log` layout behind [`RunLogStore`].
#[derive(Debug, Clone)]
pub struct FileRunLogStore {
    workspace: PathBuf,
}

impl FileRunLogStore {
    /// A file run-log store rooted at `workspace`.
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }

    fn backend(&self) -> String {
        format!("file:{}", self.workspace.to_string_lossy())
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

    pub(crate) fn validate_started(value: &str) -> Result<(), RunLogError> {
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
}

impl RunLogStore for FileRunLogStore {
    fn start<'a>(&'a self, run: RunLogStart<'a>) -> RunLogFuture<'a, ()> {
        Box::pin(async move {
            Self::validate_id(run.workflow_id)?;
            Self::validate_id(run.run_id)?;
            Self::validate_started(run.started)?;
            let path = file_store_path(&self.workspace, run.workflow_id, run.run_id, run.started);
            if path.file_name().is_none_or(|name| name.len() > 255) {
                return Err(RunLogError::InvalidId(format!(
                    "{} / {}",
                    run.workflow_id, run.run_id
                )));
            }
            let header = run_log::format_header(
                run.workflow_id,
                run.run_id,
                run.started,
                run.inputs,
                run.task_prompt,
            );
            let generation = new_generation();
            let header = format!("run_generation: {generation}\n{header}");
            let path_for_task = path.clone();
            let workspace = self.workspace.clone();
            let run_id = run.run_id.to_owned();
            tokio::task::spawn_blocking(move || {
                let parent = path_for_task.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "run log has no parent directory",
                    )
                })?;
                std::fs::create_dir_all(parent)?;
                let marker = lock_path(&workspace, &run_id);
                let marker_file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&marker)?;
                if exact_file_path(&workspace, &run_id).is_some() || path_for_task.exists() {
                    drop(marker_file);
                    let _ = std::fs::remove_file(marker);
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "run already exists",
                    ));
                }
                let result = (|| {
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path_for_task)?;
                    file.write_all(header.as_bytes())?;
                    file.write_all(b"\n")?;
                    file.flush()
                })();
                drop(marker_file);
                if result.is_err() {
                    let _ = std::fs::remove_file(&path_for_task);
                }
                let _ = std::fs::remove_file(marker);
                result
            })
            .await
            .map_err(|error| RunLogError::Task(error.to_string()))??;
            Ok(())
        })
    }

    fn append<'a>(&'a self, run_id: &'a str, line: &'a str) -> RunLogFuture<'a, ()> {
        Box::pin(async move {
            Self::validate_id(run_id)?;
            let workspace = self.workspace.clone();
            let run_id = run_id.to_owned();
            let line = line.to_owned();
            tokio::task::spawn_blocking(move || {
                let path = exact_file_path(&workspace, &run_id)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "run log not found"))?;
                let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
                file.write_all(line.as_bytes())?;
                if !line.ends_with('\n') {
                    file.write_all(b"\n")?;
                }
                file.flush()
            })
            .await
            .map_err(|error| RunLogError::Task(error.to_string()))??;
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
            Self::validate_id(run_id)?;
            let max_bytes = max_bytes.min(MAX_RUN_LOG_TAIL_BYTES);
            let backend = self.backend();
            let workspace = self.workspace.clone();
            let run_id = run_id.to_owned();
            if let Some(cursor) = cursor {
                cursor.validate_owner(&backend, &run_id)?;
            }
            let run_id_for_lookup = run_id.clone();
            let (generation, path) = tokio::task::spawn_blocking(move || {
                let path = exact_file_path(&workspace, &run_id_for_lookup)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "run log not found"))?;
                let generation = file_generation(&path)?;
                Ok::<_, io::Error>((generation, path))
            })
            .await
            .map_err(|error| RunLogError::Task(error.to_string()))?
            .map_err(RunLogError::Io)?;
            let (position, within) = cursor.map_or(Ok((0, 0)), |cursor| {
                cursor.state(&backend, &run_id, &generation)
            })?;
            if within != 0 {
                return Err(RunLogError::InvalidCursor);
            }
            let backend_for_task = backend.clone();
            let slice = tokio::task::spawn_blocking(move || {
                if position > std::fs::metadata(&path)?.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "cursor is past end of run log",
                    ));
                }
                run_log::read_run_log_slice(&path, position, max_bytes)
            })
            .await
            .map_err(|error| RunLogError::Task(error.to_string()))
            .and_then(|result| {
                result.map_err(|error| {
                    if error.kind() == io::ErrorKind::InvalidInput {
                        RunLogError::InvalidCursor
                    } else {
                        RunLogError::Io(error)
                    }
                })
            })?;
            Ok(RunLogPage {
                content: slice.content,
                cursor: RunCursor::new(&backend_for_task, &run_id, &generation, slice.offset, 0)?,
                eof: slice.eof,
                complete: slice.complete,
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
                Self::validate_id(workflow_id)?;
            }
            let workspace = self.workspace.clone();
            let workflow_id = workflow_id.map(str::to_owned);
            tokio::task::spawn_blocking(move || {
                run_log::scan_runs(&workspace, workflow_id.as_deref(), limit)
            })
            .await
            .map_err(|error| RunLogError::Task(error.to_string()))
        })
    }

    fn clear<'a>(&'a self, run_id: &'a str) -> RunLogFuture<'a, bool> {
        Box::pin(async move {
            Self::validate_id(run_id)?;
            let workspace = self.workspace.clone();
            let run_id = run_id.to_owned();
            let existed = tokio::task::spawn_blocking(move || {
                let Some(path) = exact_file_path(&workspace, &run_id) else {
                    return Ok(false);
                };
                std::fs::remove_file(&path)?;
                let _ = std::fs::remove_file(generation_path(&path));
                let _ = std::fs::remove_file(lock_path(&workspace, &run_id));
                Ok(true)
            })
            .await
            .map_err(|error| RunLogError::Task(error.to_string()))??;
            Ok(existed)
        })
    }
}

fn exact_file_path(workspace: &std::path::Path, run_id: &str) -> Option<PathBuf> {
    std::fs::read_dir(run_log::runs_dir(workspace))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            if path.extension().and_then(|extension| extension.to_str()) != Some("log") {
                return false;
            }
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| run_log::parse_run_log_text(&text, "").map(|run| run.run_id))
                .is_some_and(|candidate| candidate == run_id)
        })
}

fn lock_path(workspace: &std::path::Path, run_id: &str) -> PathBuf {
    let encoded = hex(&Sha256::digest(run_id.as_bytes()));
    run_log::runs_dir(workspace).join(format!(".run-{encoded}.lock"))
}

fn file_store_path(
    workspace: &std::path::Path,
    workflow_id: &str,
    run_id: &str,
    started: &str,
) -> PathBuf {
    let timestamp = run_log::sanitize(started);
    run_log::runs_dir(workspace).join(format!(
        "{}_{}_{}.log",
        hex(workflow_id.as_bytes()),
        timestamp,
        hex(run_id.as_bytes())
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn file_generation(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut header = vec![0; 16 * 1024];
    let read = file.read(&mut header)?;
    let header = String::from_utf8_lossy(&header[..read]);
    if let Some(generation) = header
        .lines()
        .find_map(|line| line.strip_prefix("run_generation: "))
    {
        return Ok(generation.to_owned());
    }

    // Older logs predate generation headers. A sidecar gives them stable
    // cursor identity without rewriting a file that may still be appended by
    // a host using the legacy free functions.
    let sidecar = generation_path(path);
    let generation = new_generation();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sidecar)
    {
        Ok(mut file) => {
            file.write_all(generation.as_bytes())?;
            file.flush()?;
            Ok(generation)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let generation = std::fs::read_to_string(sidecar)?;
            if generation.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "legacy run generation is being initialized",
                ));
            }
            Ok(generation)
        }
        Err(error) => Err(error),
    }
}

fn generation_path(path: &Path) -> PathBuf {
    path.with_extension("generation")
}

pub(crate) fn new_generation() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{sequence:x}")
}
