//! Validation and materialization for skills compiled into a host binary.

use crate::model::{SKILL_MD, WORKFLOW_MD};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const DIGEST_FILE: &str = ".digest";

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
        format!("{:x}", hasher.finalize())
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
            || self.dir_name.contains(['/', '\\'])
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
        for file in self.files {
            validate_relative_path(self.dir_name, file.path)?;
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
        if std::fs::read(&path).ok().as_deref() != Some(file.contents.as_bytes()) {
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
    let dir = root.join(bundle.dir_name);
    let digest = bundle.digest();
    let digest_path = dir.join(DIGEST_FILE);
    if std::fs::read_to_string(&digest_path).is_ok_and(|found| found.trim() == digest)
        && is_current_materialization(&dir, bundle)
    {
        return Ok(false);
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|error| format!("failed to clear {}: {error}", dir.display()))?;
    }
    for file in bundle.files {
        let target = dir.join(file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        }
        std::fs::write(&target, file.contents)
            .map_err(|error| format!("failed to write {}: {error}", target.display()))?;
    }
    std::fs::write(&digest_path, digest)
        .map_err(|error| format!("failed to write {}: {error}", digest_path.display()))?;
    Ok(true)
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
