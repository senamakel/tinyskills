//! Safe lookup and reading of files bundled with a skill.

use crate::model::{MAX_RESOURCE_BYTES, Skill};
use std::path::{Component, Path};
use thiserror::Error;

/// Errors returned while resolving or reading a skill resource.
#[derive(Debug, Error)]
pub enum ResourceError {
    /// The requested path is not a normal relative path.
    #[error("resource path must be a non-empty relative path containing only normal components")]
    InvalidPath,
    /// The skill does not point to a discovered on-disk bundle.
    #[error("skill `{0}` has no on-disk location")]
    NoLocation(String),
    /// A filesystem operation failed.
    #[error("{context} {path}: {source}")]
    Io {
        /// Operation that failed.
        context: &'static str,
        /// Path involved in the operation.
        path: String,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The requested path is a symbolic link.
    #[error("resource path is a symlink")]
    Symlink,
    /// The requested path is not a regular file.
    #[error("resource path is not a regular file")]
    NotRegular,
    /// The file exceeds the resource size limit.
    #[error("resource file is {size} bytes, exceeds limit of {limit}")]
    TooLarge {
        /// Actual file size observed.
        size: u64,
        /// Maximum permitted size.
        limit: u64,
    },
    /// Canonicalization escaped the skill bundle.
    #[error("resource path escapes skill root: {path}")]
    Escapes {
        /// Canonical path outside the skill root.
        path: String,
    },
    /// The resource is not UTF-8 text.
    #[error("resource is not valid UTF-8 text: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),
}

/// Resolve a skill by directory id or display name.
///
/// # Errors
///
/// Returns an error for missing or ambiguous identifiers.
pub fn resolve_skill(
    skills: impl IntoIterator<Item = Skill>,
    skill_id: &str,
) -> Result<Skill, String> {
    let mut dir_match = None;
    let mut name_match = None;
    for skill in skills {
        if skill.dir_name == skill_id {
            if dir_match.is_some() {
                return Err(format!(
                    "skill id '{skill_id}' is ambiguous across multiple directories"
                ));
            }
            dir_match = Some(skill);
        } else if skill.name == skill_id {
            if name_match.is_some() {
                return Err(format!(
                    "skill name '{skill_id}' is ambiguous; use the directory id"
                ));
            }
            name_match = Some(skill);
        }
    }
    match (dir_match, name_match) {
        (Some(by_dir), Some(by_name)) if by_dir.location == by_name.location => Ok(by_dir),
        (Some(_), Some(_)) => Err(format!(
            "skill id '{skill_id}' matches both a directory id and a different skill name"
        )),
        (Some(skill), None) | (None, Some(skill)) => Ok(skill),
        (None, None) => Err(format!("skill '{skill_id}' not found")),
    }
}

/// Read a UTF-8 resource without following symlinks or escaping its bundle.
///
/// # Errors
///
/// Returns an error for invalid relative paths, missing/non-regular files,
/// symlinks, traversal, oversized content, or invalid UTF-8.
pub fn read_resource(skill: &Skill, relative_path: &Path) -> Result<String, ResourceError> {
    if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
        return Err(ResourceError::InvalidPath);
    }
    if relative_path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ResourceError::InvalidPath);
    }
    let root = skill
        .location
        .as_deref()
        .and_then(Path::parent)
        .ok_or_else(|| ResourceError::NoLocation(skill.name.clone()))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| io_error("failed to canonicalize skill root", root, error))?;
    let requested = canonical_root.join(relative_path);
    let metadata = std::fs::symlink_metadata(&requested)
        .map_err(|error| io_error("failed to stat resource", &requested, error))?;
    if metadata.file_type().is_symlink() {
        return Err(ResourceError::Symlink);
    }
    if !metadata.is_file() {
        return Err(ResourceError::NotRegular);
    }
    if metadata.len() > MAX_RESOURCE_BYTES {
        return Err(ResourceError::TooLarge {
            size: metadata.len(),
            limit: MAX_RESOURCE_BYTES,
        });
    }
    let canonical_requested = std::fs::canonicalize(&requested)
        .map_err(|error| io_error("failed to canonicalize resource", &requested, error))?;
    if !canonical_requested.starts_with(&canonical_root) {
        return Err(ResourceError::Escapes {
            path: canonical_requested.display().to_string(),
        });
    }
    let file = open_resource(&canonical_root, relative_path)
        .map_err(|error| io_error("failed to open resource", &canonical_requested, error))?;
    let bytes = read_bounded_file(file, MAX_RESOURCE_BYTES)
        .map_err(|error| io_error("failed to read resource", &canonical_requested, error))?;
    std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(ResourceError::InvalidUtf8)
}

fn read_bounded_file(file: std::fs::File, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file grew beyond its limit while reading",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_resource(root: &Path, relative_path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
    let mut directory = std::fs::File::open(root)?;
    let mut components = relative_path.components().peekable();
    while let Some(Component::Normal(component)) = components.next() {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if components.peek().is_some() {
            let fd = openat(
                &directory,
                component,
                flags | OFlags::DIRECTORY,
                Mode::empty(),
            )?;
            directory = std::fs::File::from(fd);
        } else {
            let fd = openat(&directory, component, flags, Mode::empty())?;
            return Ok(std::fs::File::from(fd));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "resource path has no normal components",
    ))
}

#[cfg(not(unix))]
fn open_resource(root: &Path, relative_path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(root.join(relative_path))
}

fn io_error(context: &'static str, path: &Path, source: std::io::Error) -> ResourceError {
    ResourceError::Io {
        context,
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_rejects_growth() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("resource");
        std::fs::write(&path, "too large")?;
        let file = std::fs::File::open(path)?;
        assert!(read_bounded_file(file, 3).is_err());
        assert!(open_resource(temp.path(), Path::new("")).is_err());
        Ok(())
    }
}
