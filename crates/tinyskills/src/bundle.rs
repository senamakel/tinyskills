//! Validation and materialization for skills compiled into a host binary.

use crate::model::{SKILL_MD, WORKFLOW_MD};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write;

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

