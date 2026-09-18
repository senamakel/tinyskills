//! Safe lookup and reading of files bundled with a skill.

use crate::model::{MAX_RESOURCE_BYTES, Skill};
use std::path::Path;

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
pub fn read_resource(skill: &Skill, relative_path: &Path) -> Result<String, String> {
    if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
        return Err("resource path must be a non-empty relative path".to_owned());
    }
    if relative_path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("resource path must contain only normal relative components".to_owned());
    }
    let root = skill
        .location
        .as_deref()
        .and_then(Path::parent)
        .ok_or_else(|| format!("skill '{}' has no on-disk location", skill.name))?;
    let canonical_root = std::fs::canonicalize(root).map_err(|error| {
        format!(
            "failed to canonicalize skill root {}: {error}",
            root.display()
        )
    })?;
    let requested = canonical_root.join(relative_path);
    let metadata = std::fs::symlink_metadata(&requested)
        .map_err(|error| format!("failed to stat resource {}: {error}", requested.display()))?;
    if metadata.file_type().is_symlink() {
        return Err("resource path is a symlink".to_owned());
    }
    if !metadata.is_file() {
        return Err("resource path is not a regular file".to_owned());
    }
    if metadata.len() > MAX_RESOURCE_BYTES {
        return Err(format!(
            "resource file is {} bytes, exceeds limit of {MAX_RESOURCE_BYTES}",
            metadata.len()
        ));
    }
    let canonical_requested = std::fs::canonicalize(&requested).map_err(|error| {
        format!(
            "failed to canonicalize resource {}: {error}",
            requested.display()
        )
    })?;
    if !canonical_requested.starts_with(&canonical_root) {
        return Err(format!(
            "resource path escapes skill root: {}",
            canonical_requested.display()
        ));
    }
    let bytes = std::fs::read(&canonical_requested).map_err(|error| {
        format!(
            "failed to read resource {}: {error}",
            canonical_requested.display()
        )
    })?;
    std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|error| format!("resource is not valid UTF-8 text: {error}"))
}
