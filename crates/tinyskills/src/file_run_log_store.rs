//! The file-backed implementation of the run-log persistence contract.

use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    MAX_RUN_LOG_TAIL_BYTES, RunCursor, RunLogError, RunLogFuture, RunLogPage, RunLogStart,
    RunLogStore,
};
use crate::run_log::{self, ScannedRun};

static RUN_LIFECYCLE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
        let identity = self
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone());
        format!(
            "file:{}",
            hex(&Sha256::digest(identity.as_os_str().as_encoded_bytes()))
        )
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
            let summary_generation = generation.clone();
            let path_for_task = path.clone();
            let workspace = self.workspace.clone();
            let summary = run_log::RunLogSummary {
                run: ScannedRun {
                    run_id: run.run_id.to_owned(),
                    workflow_id: run.workflow_id.to_owned(),
                    started: format!("{} UTC", run.started),
                    status: "RUNNING".to_string(),
                    duration_ms: None,
                    finished: None,
                    log_path: path.to_string_lossy().into_owned(),
                },
                complete: false,
                footer_started: false,
            };
            tokio::task::spawn_blocking(move || {
                let parent = path_for_task.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "run log has no parent directory",
                    )
                })?;
                std::fs::create_dir_all(parent)?;
                let lifecycle_lock = run_lifecycle_lock(&path_for_task);
                let lifecycle_guard = lifecycle_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if exact_file_path(&workspace, &summary.run.run_id).is_some()
                    || path_for_task.exists()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "run already exists",
                    ));
                }
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path_for_task)?;
                let write_result = (|| {
                    file.write_all(header.as_bytes())?;
                    file.write_all(b"\n")?;
                    file.flush()
                })();
                if let Err(error) = write_result {
                    let _ = std::fs::remove_file(&path_for_task);
                    return Err(error);
                }
                drop(lifecycle_guard);
                let observed_len = u64::try_from(header.len())
                    .map_err(|_| io::Error::other("run log header length exceeds u64"))?
                    + 1;
                if let Err(error) =
                    write_file_summary(&path_for_task, &summary, &summary_generation, observed_len)
                {
                    cleanup_failed_start(&path_for_task, &summary_generation);
                    return Err(error);
                }
                Ok(())
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
                let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
                let pre_len = file.metadata()?.len();
                let mut written = u64::try_from(line.len())
                    .map_err(|_| io::Error::other("appended run-log line exceeds u64"))?;
                file.write_all(line.as_bytes())?;
                if !line.ends_with('\n') {
                    file.write_all(b"\n")?;
                    written = written
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("appended run-log line length overflow"))?;
                }
                file.flush()
                    .and_then(|()| update_file_summary(&path, pre_len, written, &line))
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
        tail_with_interleave(
            self,
            run_id,
            cursor,
            max_bytes,
            Box::pin(std::future::ready(())),
        )
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
                scan_file_runs(&workspace, workflow_id.as_deref(), limit)
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
                let path = file_store_path(&workspace, "", &run_id, "");
                let lifecycle_lock = run_lifecycle_lock(&path);
                let _lifecycle_guard = lifecycle_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let path = exact_file_path(&workspace, &run_id).or_else(|| {
                    let deterministic = file_store_path(&workspace, "", &run_id, "");
                    if !deterministic.exists() {
                        return None;
                    }
                    match run_log::read_run_log_header_identity(&deterministic) {
                        Ok(Some((_, stored_run_id))) if stored_run_id != run_id => None,
                        Ok(_) | Err(_) => Some(deterministic),
                    }
                });
                let Some(path) = path else {
                    return Ok::<bool, io::Error>(false);
                };
                std::fs::remove_file(&path)?;
                let _ = std::fs::remove_file(summary_path(&path));
                let _ = std::fs::remove_file(generation_path(&path));
                Ok(true)
            })
            .await
            .map_err(|error| RunLogError::Task(error.to_string()))??;
            Ok(existed)
        })
    }
}

fn tail_with_interleave<'a>(
    store: &'a FileRunLogStore,
    run_id: &'a str,
    cursor: Option<&'a RunCursor>,
    max_bytes: usize,
    before_read: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
) -> RunLogFuture<'a, RunLogPage> {
    Box::pin(async move {
        FileRunLogStore::validate_id(run_id)?;
        let max_bytes = max_bytes.min(MAX_RUN_LOG_TAIL_BYTES);
        let backend = store.backend();
        let workspace = store.workspace.clone();
        let lifecycle_path = file_store_path(&workspace, "", run_id, "");
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
        before_read.await;
        let backend_for_task = backend.clone();
        let expected_generation = generation.clone();
        let slice = tokio::task::spawn_blocking(move || {
            let lifecycle_lock = run_lifecycle_lock(&lifecycle_path);
            let _lifecycle_guard = lifecycle_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let current_generation = file_generation(&path).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    io::Error::new(io::ErrorKind::InvalidInput, "run generation changed")
                } else {
                    error
                }
            })?;
            if current_generation != expected_generation {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "run generation changed",
                ));
            }
            if position > std::fs::metadata(&path)?.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cursor is past end of run log",
                ));
            }
            let mut slice = run_log::read_run_log_slice(&path, position, max_bytes)?;
            if let Some(summary) = read_file_summary(&path)? {
                slice.complete = summary.complete;
            }
            Ok::<_, io::Error>(slice)
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

fn exact_file_path(workspace: &std::path::Path, run_id: &str) -> Option<PathBuf> {
    let deterministic = file_store_path(workspace, "", run_id, "");
    if run_log::read_run_log_header_identity(&deterministic)
        .ok()
        .flatten()
        .is_some_and(|(_, stored_run_id)| stored_run_id == run_id)
    {
        return Some(deterministic);
    }
    let entries = std::fs::read_dir(run_log::runs_dir(workspace)).ok()?;
    let encoded_suffix = format!("_{}.log", hex(run_id.as_bytes()));
    let legacy_suffix = format!("_{}.log", run_log::sanitize(run_log::short(run_id)));
    let mut legacy = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if path.extension().and_then(|extension| extension.to_str()) != Some("log") {
            continue;
        }
        if name.ends_with(&encoded_suffix) || name.ends_with(&legacy_suffix) {
            legacy.push(path);
        }
    }
    legacy.into_iter().find(|path| {
        run_log::read_run_log_header_identity(path)
            .ok()
            .flatten()
            .is_some_and(|(_, stored_run_id)| stored_run_id == run_id)
    })
}

#[derive(Serialize, Deserialize)]
struct FileSummary {
    summary: run_log::RunLogSummary,
    generation: String,
    observed_len: u64,
}

fn summary_path(path: &Path) -> PathBuf {
    path.with_extension("summary.json")
}

fn read_file_summary(path: &Path) -> io::Result<Option<run_log::RunLogSummary>> {
    let metadata = std::fs::metadata(path)?;
    let observed_len = metadata.len();
    let generation = file_generation(path)?;
    let cached = match std::fs::read(summary_path(path)) {
        Ok(bytes) => serde_json::from_slice::<FileSummary>(&bytes).ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(cached) = cached
        && cached.observed_len == observed_len
        && cached.generation == generation
    {
        return Ok(Some(cached.summary));
    }

    let Some(summary) = run_log::read_run_log_summary(path)? else {
        return Ok(None);
    };
    if std::fs::metadata(path)?.len() == observed_len && file_generation(path)? == generation {
        write_file_summary(path, &summary, &generation, observed_len)?;
    }
    Ok(Some(summary))
}

fn write_file_summary(
    path: &Path,
    summary: &run_log::RunLogSummary,
    generation: &str,
    observed_len: u64,
) -> io::Result<()> {
    let destination = summary_path(path);
    let temporary = path.with_extension(format!("summary-{}.tmp", new_generation()));
    let bytes = serde_json::to_vec(&FileSummary {
        summary: summary.clone(),
        generation: generation.to_owned(),
        observed_len,
    })
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    std::fs::write(&temporary, bytes)?;
    if let Err(error) = std::fs::rename(&temporary, &destination) {
        let _ = std::fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

fn update_file_summary(path: &Path, pre_len: u64, written: u64, appended: &str) -> io::Result<()> {
    let observed_len = std::fs::metadata(path)?.len();
    let generation = file_generation(path)?;
    let cached = match std::fs::read(summary_path(path)) {
        Ok(bytes) => serde_json::from_slice::<FileSummary>(&bytes).ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    }
    .filter(|cached| {
        cached.generation == generation
            && cached.observed_len == pre_len
            && pre_len.checked_add(written) == Some(observed_len)
    });
    let Some(mut summary) = (match cached {
        Some(cached) => Some(cached.summary),
        None => return read_file_summary(path).map(|_| ()),
    }) else {
        return Ok(());
    };
    for line in appended.lines() {
        if line.starts_with("--- result ---") {
            summary.footer_started = true;
            continue;
        }
        let Some((label, value)) = line.split_once(':') else {
            continue;
        };
        if !summary.footer_started {
            continue;
        }
        let value = value.trim();
        match label.trim() {
            "status" => summary.run.status = value.to_string(),
            "duration" => {
                summary.run.duration_ms = value.trim_end_matches(" ms").trim().parse::<u64>().ok();
            }
            "finished" => {
                summary.run.finished = Some(value.trim_end_matches(" UTC").trim().to_string());
                summary.complete =
                    !summary.run.status.is_empty() && summary.run.status != "RUNNING";
            }
            _ => {}
        }
    }
    if std::fs::metadata(path)?.len() != observed_len || file_generation(path)? != generation {
        return Ok(());
    }
    write_file_summary(path, &summary, &generation, observed_len)
}

fn scan_file_runs(workspace: &Path, workflow_id: Option<&str>, limit: usize) -> Vec<ScannedRun> {
    let mut runs = Vec::new();
    let Ok(entries) = std::fs::read_dir(run_log::runs_dir(workspace)) else {
        return runs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("log") {
            continue;
        }
        let summary = match read_file_summary(&path) {
            Ok(Some(summary)) => Some(summary),
            Ok(None) => run_log::read_run_log_summary(&path).ok().flatten(),
            Err(_) => None,
        };
        let Some(mut summary) = summary else {
            continue;
        };
        if workflow_id.is_none_or(|workflow| summary.run.workflow_id == workflow) {
            summary.run.log_path = path.to_string_lossy().into_owned();
            runs.push(summary.run);
        }
    }
    runs.sort_by(|left, right| right.started.cmp(&left.started));
    runs.truncate(limit);
    runs
}

fn file_store_path(
    workspace: &std::path::Path,
    _workflow_id: &str,
    run_id: &str,
    _started: &str,
) -> PathBuf {
    run_log::runs_dir(workspace).join(format!("run_{}.log", hex(run_id.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        encoded.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    encoded
}

pub(crate) fn file_generation(path: &Path) -> io::Result<String> {
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

fn cleanup_failed_start(path: &Path, generation: &str) {
    cleanup_failed_start_with(path, generation, || {});
}

fn cleanup_failed_start_with(path: &Path, generation: &str, before_remove: impl FnOnce()) {
    let lifecycle_lock = run_lifecycle_lock(path);
    let _lifecycle_guard = lifecycle_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if file_generation(path).ok().as_deref() != Some(generation) {
        return;
    }
    before_remove();
    let _ = std::fs::remove_file(summary_path(path));
    let _ = std::fs::remove_file(generation_path(path));
    let _ = std::fs::remove_file(path);
}

fn run_lifecycle_lock(path: &Path) -> Arc<Mutex<()>> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let key = match (parent.canonicalize(), path.file_name()) {
        (Ok(parent), Some(file_name)) => parent.join(file_name),
        _ => path.to_path_buf(),
    };
    let mut locks = RUN_LIFECYCLE_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

#[cfg(test)]
fn run_lifecycle_lock_is_held(path: &Path) -> bool {
    run_lifecycle_lock(path).try_lock().is_err()
}

pub(crate) fn new_generation() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{sequence:x}")
}

#[cfg(test)]
#[path = "file_run_log_store_tests.rs"]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests;
