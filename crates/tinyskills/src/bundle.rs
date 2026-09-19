//! Validation and materialization for skills compiled into a host binary.

use crate::model::{SKILL_MD, WORKFLOW_MD};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DIGEST_FILE: &str = ".digest";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One UTF-8 file in a compile-time skill bundle.
#[derive(Debug, Clone, Copy)]
pub struct BundledFile {
    /// Slash-separated path relative to the bundle directory.
    pub path: &'static str,
    /// File contents.
    pub contents: &'static str,
}

/// One skill compiled into an embedding host.
#[derive(Debug, Clone, Copy)]
pub struct BundledSkill {
    /// Safe on-disk directory identifier.
    pub dir_name: &'static str,
    /// Files to materialize.
    pub files: &'static [BundledFile],
}

impl BundledSkill {
    /// Compute a length-delimited digest over every file path and body.
    #[must_use]
    pub fn digest(self) -> String {
        let mut hasher = Sha256::new();
        for file in self.files {
            hasher.update((file.path.len() as u64).to_le_bytes());
            hasher.update(file.path.as_bytes());
            hasher.update((file.contents.len() as u64).to_le_bytes());
            hasher.update(file.contents.as_bytes());
        }
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest.as_slice() {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// Validate that materializing this bundle cannot escape its destination.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe names/paths, empty bundles, or a missing
    /// `SKILL.md`/`WORKFLOW.md` document.
    pub fn validate(self) -> Result<(), String> {
        if self.dir_name.is_empty()
            || self.dir_name.starts_with('.')
            || self.dir_name.contains(['/', '\\', ':'])
        {
            return Err(format!(
                "invalid bundled skill dir_name `{}`",
                self.dir_name
            ));
        }
        if self.files.is_empty() {
            return Err(format!("bundled skill `{}` has no files", self.dir_name));
        }
        if !self
            .files
            .iter()
            .any(|file| matches!(file.path, SKILL_MD | WORKFLOW_MD))
        {
            return Err(format!(
                "bundled skill `{}` has no {SKILL_MD} or {WORKFLOW_MD}",
                self.dir_name
            ));
        }
        let mut paths = HashSet::with_capacity(self.files.len());
        for file in self.files {
            validate_relative_path(self.dir_name, file.path)?;
            if !paths.insert(file.path) {
                return Err(format!(
                    "bundled skill `{}` contains duplicate file `{}`",
                    self.dir_name, file.path
                ));
            }
        }
        Ok(())
    }
}

/// Summary of a materialization pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InstallReport {
    /// Bundle ids written or replaced.
    pub written: Vec<String>,
    /// Bundle ids already matching their compiled contents.
    pub unchanged: Vec<String>,
    /// Bundle ids that could not be materialized and their errors.
    pub failed: Vec<(String, String)>,
}

/// Materialize bundles under `root`, replacing stale copies atomically enough
/// that an interrupted write is retried on the next pass.
#[must_use]
pub fn install(root: &Path, bundles: &[BundledSkill]) -> InstallReport {
    let mut report = InstallReport::default();
    for bundle in bundles {
        match install_one(root, *bundle) {
            Ok(true) => report.written.push(bundle.dir_name.to_owned()),
            Ok(false) => report.unchanged.push(bundle.dir_name.to_owned()),
            Err(error) => report.failed.push((bundle.dir_name.to_owned(), error)),
        }
    }
    report
}

/// Verify a materialized directory exactly matches a compiled bundle.
#[must_use]
pub fn is_current_materialization(dir: &Path, bundle: BundledSkill) -> bool {
    let Ok(dir_metadata) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if bundle.validate().is_err() || !dir_metadata.is_dir() || dir_metadata.file_type().is_symlink()
    {
        return false;
    }
    let expected: HashSet<_> = bundle.files.iter().map(|file| file.path).collect();
    for file in bundle.files {
        let path = dir.join(file.path);
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            return false;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return false;
        }
        if read_bounded(&path, file.contents.len() as u64 + 1)
            .ok()
            .as_deref()
            != Some(file.contents.as_bytes())
        {
            return false;
        }
    }
    materialized_tree_is_exact(dir, &expected)
}

fn materialized_tree_is_exact(dir: &Path, expected: &HashSet<&str>) -> bool {
    let mut stack = vec![PathBuf::from(dir)];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(current) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                return false;
            };
            let Ok(relative) = path.strip_prefix(dir) else {
                return false;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                return false;
            }
            if metadata.is_dir() {
                stack.push(path);
            } else if relative != DIGEST_FILE && !expected.contains(relative.as_str()) {
                return false;
            }
        }
    }
    true
}

fn install_one(root: &Path, bundle: BundledSkill) -> Result<bool, String> {
    bundle.validate()?;
    std::fs::create_dir_all(root)
        .map_err(|error| format!("failed to create bundle root {}: {error}", root.display()))?;
    let root_metadata = std::fs::symlink_metadata(root)
        .map_err(|error| format!("failed to inspect bundle root {}: {error}", root.display()))?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(format!(
            "bundle root {} is not a real directory",
            root.display()
        ));
    }
    let dir = root.join(bundle.dir_name);
    let digest = bundle.digest();
    let digest_path = dir.join(DIGEST_FILE);
    let digest_matches = std::fs::symlink_metadata(&dir).is_ok_and(|metadata| {
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && std::fs::symlink_metadata(&digest_path).is_ok_and(|digest_metadata| {
                digest_metadata.is_file() && !digest_metadata.file_type().is_symlink()
            })
            && read_bounded(&digest_path, digest.len() as u64 + 1)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .is_some_and(|found| found.trim() == digest)
    });
    if digest_matches && is_current_materialization(&dir, bundle) {
        return Ok(false);
    }
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = root.join(format!(
        ".{}.tmp-{}-{}",
        bundle.dir_name,
        std::process::id(),
        nonce
    ));
    std::fs::create_dir(&temporary).map_err(|error| {
        format!(
            "failed to create staging directory {}: {error}",
            temporary.display()
        )
    })?;
    for file in bundle.files {
        let target = temporary.join(file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        }
        std::fs::write(&target, file.contents)
            .map_err(|error| format!("failed to write {}: {error}", target.display()))?;
    }
    std::fs::write(temporary.join(DIGEST_FILE), digest)
        .map_err(|error| format!("failed to write {}: {error}", digest_path.display()))?;

    let backup = root.join(format!(
        ".{}.backup-{}-{}",
        bundle.dir_name,
        std::process::id(),
        nonce
    ));
    let had_existing = match std::fs::symlink_metadata(&dir) {
        Ok(_) => {
            std::fs::rename(&dir, &backup).map_err(|error| {
                format!("failed to stage old bundle {}: {error}", dir.display())
            })?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("failed to inspect {}: {error}", dir.display())),
    };
    if let Err(error) = std::fs::rename(&temporary, &dir) {
        if had_existing {
            let _ = std::fs::rename(&backup, &dir);
        }
        let _ = std::fs::remove_dir_all(&temporary);
        return Err(format!("failed to publish {}: {error}", dir.display()));
    }
    if had_existing {
        remove_path(&backup).map_err(|error| {
            format!("failed to remove old bundle {}: {error}", backup.display())
        })?;
    }
    Ok(true)
}

fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    if bytes.len() as u64 >= limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file exceeds expected size",
        ));
    }
    Ok(bytes)
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)
    } else {
        std::fs::remove_dir_all(path)
    }
}

fn validate_relative_path(dir_name: &str, path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with(['/', '\\']) || path.contains([':', '\\']) {
        return Err(format!(
            "bundled skill `{dir_name}` file `{path}` is not relative"
        ));
    }
    if path.split('/').any(|component| {
        component.is_empty() || component == "." || component == ".." || component.starts_with('.')
    }) {
        return Err(format!(
            "bundled skill `{dir_name}` file `{path}` has an unsafe component"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    static FILES: &[BundledFile] = &[BundledFile {
        path: "SKILL.md",
        contents: "body",
    }];

    #[test]
    fn reports_staging_directory_conflicts() -> Result<(), Box<dyn std::error::Error>> {
        let _lock = TEST_LOCK.lock().map_err(|_| "test lock poisoned")?;
        let root = tempfile::tempdir()?;
        let nonce = 31_001;
        TEMP_COUNTER.store(nonce, Ordering::Relaxed);
        let staging = root
            .path()
            .join(format!(".demo.tmp-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&staging)?;
        assert_eq!(
            install(
                root.path(),
                &[BundledSkill {
                    dir_name: "demo",
                    files: FILES
                }]
            )
            .failed
            .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn reports_backup_directory_conflicts() -> Result<(), Box<dyn std::error::Error>> {
        let _lock = TEST_LOCK.lock().map_err(|_| "test lock poisoned")?;
        let root = tempfile::tempdir()?;
        let nonce = 31_002;
        TEMP_COUNTER.store(nonce, Ordering::Relaxed);
        std::fs::create_dir(root.path().join("demo"))?;
        let backup = root
            .path()
            .join(format!(".demo.backup-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&backup)?;
        std::fs::write(backup.join("existing"), "old")?;
        assert_eq!(
            install(
                root.path(),
                &[BundledSkill {
                    dir_name: "demo",
                    files: FILES
                }]
            )
            .failed
            .len(),
            1
        );
        Ok(())
    }
}
