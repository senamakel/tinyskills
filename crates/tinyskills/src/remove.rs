//! Defensive removal of an installed skill bundle.

use crate::model::{MAX_NAME_LEN, SKILL_MD, WORKFLOW_MD};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors returned by [`remove_bundle`].
#[derive(Debug, Error)]
pub enum RemoveError {
    /// The slug is empty after trimming.
    #[error("skill name is required")]
    EmptyName,
    /// The slug could escape its root.
    #[error("skill name '{0}' must not contain path separators")]
    PathSeparators(String),
    /// The slug is longer than [`MAX_NAME_LEN`].
    #[error("skill name is {len} chars (max {max})")]
    NameTooLong {
        /// Observed length.
        len: usize,
        /// Maximum accepted length.
        max: usize,
    },
    /// No root holds the slug.
    #[error("skill '{0}' is not installed")]
    NotInstalled(String),
    /// The root holding the slug is a symbolic link.
    #[error("workflows root {0} is a symlink — refusing to resolve")]
    SymlinkedRoot(String),
    /// The bundle directory is a symbolic link.
    #[error("skill '{0}' is a symlinked alias — refusing to resolve")]
    SymlinkedAlias(String),
    /// The canonical bundle path is outside the canonical root.
    #[error("refused to remove {0} — path escapes skills root")]
    Escapes(String),
    /// The bundle path is not a real directory.
    #[error("{0} is not a directory — refusing to remove")]
    NotADirectory(String),
    /// The directory holds no recognized skill document.
    #[error("{0} does not look like a workflow (missing {WORKFLOW_MD})")]
    NotABundle(String),
    /// A filesystem operation failed.
    #[error("{action} {path} failed: {source}")]
    Io {
        /// Operation that failed (`stat`, `canonicalize`, or `remove`).
        action: &'static str,
        /// Path involved in the operation.
        path: String,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// Remove the bundle `slug` from the first of `roots` that holds it.
///
/// The slug must be a single path component. The root and the bundle
/// directory must not be symlinks, the canonical bundle path must stay inside
/// the canonical root and differ from it, and the directory must contain a
/// `WORKFLOW.md` or `SKILL.md`. Returns the canonical path that was removed.
///
/// Root order is the caller's search order. The host keeps root selection,
/// scope policy, cache invalidation, and notifications.
///
/// # Errors
///
/// Returns a [`RemoveError`] for an invalid slug, a missing bundle, a symlinked
/// root or alias, containment failures, a directory that is not a bundle, or
/// filesystem failures.
pub fn remove_bundle(roots: &[PathBuf], slug: &str) -> Result<PathBuf, RemoveError> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err(RemoveError::EmptyName);
    }
    if slug.contains(['/', '\\']) || slug == ".." || slug == "." {
        return Err(RemoveError::PathSeparators(slug.to_owned()));
    }
    if slug.len() > MAX_NAME_LEN {
        return Err(RemoveError::NameTooLong {
            len: slug.len(),
            max: MAX_NAME_LEN,
        });
    }

    // The first root with any entry named `slug` (a dangling symlink counts, so
    // it is reported as an alias rather than silently skipped).
    let (root, candidate, metadata) = roots
        .iter()
        .find_map(|root| {
            let candidate = root.join(slug);
            std::fs::symlink_metadata(&candidate)
                .ok()
                .map(|metadata| (root, candidate, metadata))
        })
        .ok_or_else(|| RemoveError::NotInstalled(slug.to_owned()))?;

    let root_meta =
        std::fs::symlink_metadata(root).map_err(|error| io_error("stat", root, error))?;
    if root_meta.file_type().is_symlink() {
        return Err(RemoveError::SymlinkedRoot(root.display().to_string()));
    }
    if metadata.file_type().is_symlink() {
        return Err(RemoveError::SymlinkedAlias(slug.to_owned()));
    }
    if !metadata.is_dir() {
        return Err(RemoveError::NotADirectory(candidate.display().to_string()));
    }

    let canonical_root =
        std::fs::canonicalize(root).map_err(|error| io_error("canonicalize", root, error))?;
    let canonical = std::fs::canonicalize(&candidate)
        .map_err(|error| io_error("canonicalize", &candidate, error))?;
    // Unreachable without a concurrent swap of the directory tree, kept as a
    // last guard before a recursive delete.
    if canonical == canonical_root || !canonical.starts_with(&canonical_root) {
        return Err(RemoveError::Escapes(canonical.display().to_string()));
    }
    if !canonical.join(WORKFLOW_MD).exists() && !canonical.join(SKILL_MD).exists() {
        return Err(RemoveError::NotABundle(canonical.display().to_string()));
    }

    std::fs::remove_dir_all(&canonical).map_err(|error| io_error("remove", &canonical, error))?;
    Ok(canonical)
}

fn io_error(action: &'static str, path: &Path, source: std::io::Error) -> RemoveError {
    RemoveError::Io {
        action,
        path: path.display().to_string(),
        source,
    }
}
