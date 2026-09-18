//! Parsing and resource inventory for skill documents.

use crate::model::{
    LegacyManifest, MAX_DESCRIPTION_LEN, MAX_NAME_LEN, RESOURCE_DIRS, Skill, SkillFrontmatter,
    SkillScope,
};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Parse a skill document from disk.
#[must_use]
pub fn parse_skill(path: &Path) -> Option<(SkillFrontmatter, String, Vec<String>)> {
    let content = read_bounded_text(path, crate::model::MAX_DOCUMENT_BYTES).ok()?;
    parse_skill_str(&content)
}

/// Parse skill frontmatter and its Markdown body.
///
/// A document without frontmatter is accepted with empty metadata. An opened
/// but unterminated frontmatter block is rejected. Malformed YAML produces a
/// warning and empty metadata so callers can still surface the bundle.
#[must_use]
pub fn parse_skill_str(content: &str) -> Option<(SkillFrontmatter, String, Vec<String>)> {
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return Some((SkillFrontmatter::default(), content.to_owned(), Vec::new()));
    }

    let mut yaml = String::new();
    let mut body = String::new();
    let mut terminated = false;
    for line in lines {
        if !terminated && line.trim() == "---" {
            terminated = true;
        } else if terminated {
            body.push_str(line);
            body.push('\n');
        } else {
            yaml.push_str(line);
            yaml.push('\n');
        }
    }
    if !terminated {
        return None;
    }

    let mut warnings = Vec::new();
    let frontmatter = serde_yaml::from_str(&yaml).unwrap_or_else(|error| {
        warnings.push(format!("frontmatter parse error: {error}"));
        SkillFrontmatter::default()
    });
    Some((frontmatter, body, warnings))
}

/// Inventory regular, non-symlink files under conventional resource folders.
#[must_use]
pub fn inventory_resources(dir: &Path) -> Vec<PathBuf> {
    let mut resources = Vec::new();
    for name in RESOURCE_DIRS {
        let root = dir.join(name);
        let Ok(metadata) = std::fs::symlink_metadata(&root) else {
            continue;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            walk_files(&root, dir, &mut resources, 0);
        }
    }
    resources.sort();
    resources
}

fn walk_files(current: &Path, base: &Path, resources: &mut Vec<PathBuf>, depth: usize) {
    const MAX_RESOURCE_DEPTH: usize = 64;
    if depth > MAX_RESOURCE_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_files(&path, base, resources, depth + 1);
        } else if file_type.is_file()
            && let Ok(relative) = path.strip_prefix(base)
        {
            resources.push(relative.to_path_buf());
        }
    }
}

/// Load one Markdown document into normalized skill metadata.
#[must_use]
pub(crate) fn load_document(
    document: &Path,
    dir: &Path,
    dir_name: &str,
    scope: SkillScope,
) -> Skill {
    let mut warnings = Vec::new();
    let (frontmatter, body) =
        if let Some((frontmatter, body, parse_warnings)) = parse_skill(document) {
            warnings.extend(parse_warnings);
            (frontmatter, body)
        } else {
            warnings.push(format!(
                "could not parse {} — exposing directory as placeholder",
                document.display()
            ));
            (SkillFrontmatter::default(), String::new())
        };

    let name = if frontmatter.name.trim().is_empty() {
        warnings.push("frontmatter missing 'name'; using directory name".to_owned());
        dir_name.to_owned()
    } else {
        if frontmatter.name != dir_name {
            warnings.push(format!(
                "frontmatter name '{}' does not match directory '{dir_name}'",
                frontmatter.name
            ));
        }
        if frontmatter.name.len() > MAX_NAME_LEN {
            warnings.push(format!(
                "frontmatter name is {} chars (max recommended: {MAX_NAME_LEN})",
                frontmatter.name.len()
            ));
        }
        frontmatter.name.clone()
    };

    let description = if frontmatter.description.trim().is_empty() {
        warnings
            .push("frontmatter missing 'description'; falling back to first body line".to_owned());
        first_body_line(&body).unwrap_or_else(|| "No description provided".to_owned())
    } else {
        if frontmatter.description.len() > MAX_DESCRIPTION_LEN {
            warnings.push(format!(
                "description is {} chars (max recommended: {MAX_DESCRIPTION_LEN})",
                frontmatter.description.len()
            ));
        }
        frontmatter.description.clone()
    };

    let version = metadata_string(&frontmatter, "version").unwrap_or_else(|| {
        deprecated_string(&frontmatter, "version", &mut warnings).unwrap_or_default()
    });
    let author = metadata_string(&frontmatter, "author")
        .or_else(|| deprecated_string(&frontmatter, "author", &mut warnings));
    let mut tags = metadata_sequence(&frontmatter, "tags");
    if let Some(value) = frontmatter.extra.get("tags") {
        warnings.push("top-level 'tags' is deprecated; move under 'metadata.tags'".to_owned());
        tags.extend(string_sequence(value));
    }
    let hermes = frontmatter
        .metadata
        .get("hermes")
        .and_then(serde_yaml::Value::as_mapping);
    if let Some(value) = hermes.and_then(|map| map.get("tags")) {
        tags.extend(string_sequence(value));
    }
    tags.sort();
    tags.dedup();

    let mut related_skills = metadata_sequence(&frontmatter, "related_skills");
    if let Some(value) = hermes.and_then(|map| map.get("related_skills")) {
        related_skills.extend(string_sequence(value));
    }
    related_skills.sort();
    related_skills.dedup();

    let source_format = if hermes.is_some() || !frontmatter.platforms.is_empty() {
        "hermes"
    } else {
        "agentskills"
    };

    Skill {
        name,
        dir_name: dir_name.to_owned(),
        description,
        version,
        author,
        tags,
        platforms: frontmatter.platforms.clone(),
        related_skills,
        source_format: source_format.to_owned(),
        tools: frontmatter.allowed_tools.clone(),
        prompts: Vec::new(),
        location: Some(document.to_path_buf()),
        frontmatter,
        resources: inventory_resources(dir),
        scope,
        legacy: false,
        warnings,
        body: Some(body),
    }
}

/// Load one legacy JSON manifest into normalized skill metadata.
#[must_use]
pub(crate) fn load_legacy(
    manifest_path: &Path,
    dir: &Path,
    dir_name: &str,
    scope: SkillScope,
) -> Skill {
    let mut warnings = vec!["skill uses legacy skill.json; migrate to SKILL.md frontmatter".into()];
    let manifest = read_bounded_text(manifest_path, crate::model::MAX_LEGACY_MANIFEST_BYTES)
        .ok()
        .and_then(|content| serde_json::from_str::<LegacyManifest>(&content).ok())
        .unwrap_or_else(|| {
            warnings.push(format!(
                "could not parse {} as JSON; using directory name",
                manifest_path.display()
            ));
            LegacyManifest {
                name: dir_name.to_owned(),
                description: String::new(),
                version: String::new(),
                author: None,
                tags: Vec::new(),
                tools: Vec::new(),
                prompts: Vec::new(),
            }
        });
    Skill {
        name: if manifest.name.trim().is_empty() {
            dir_name.to_owned()
        } else {
            manifest.name
        },
        dir_name: dir_name.to_owned(),
        description: if manifest.description.is_empty() {
            "No description provided".to_owned()
        } else {
            manifest.description
        },
        version: manifest.version,
        author: manifest.author,
        tags: manifest.tags,
        tools: manifest.tools,
        prompts: manifest.prompts,
        location: Some(manifest_path.to_path_buf()),
        resources: inventory_resources(dir),
        scope,
        legacy: true,
        source_format: "legacy".to_owned(),
        warnings,
        ..Skill::default()
    }
}

fn read_bounded_text(path: &Path, limit: u64) -> std::io::Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file is not a bounded regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "file grew beyond its limit while reading",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "file is not valid UTF-8")
    })
}

fn first_body_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

fn metadata_string(frontmatter: &SkillFrontmatter, key: &str) -> Option<String> {
    frontmatter
        .metadata
        .get(key)
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_owned)
}

fn deprecated_string(
    frontmatter: &SkillFrontmatter,
    key: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let value = frontmatter.extra.get(key)?.as_str()?;
    warnings.push(format!(
        "top-level '{key}' is deprecated; move under 'metadata.{key}'"
    ));
    Some(value.to_owned())
}

fn metadata_sequence(frontmatter: &SkillFrontmatter, key: &str) -> Vec<String> {
    frontmatter
        .metadata
        .get(key)
        .map(string_sequence)
        .unwrap_or_default()
}

fn string_sequence(value: &serde_yaml::Value) -> Vec<String> {
    value
        .as_sequence()
        .into_iter()
        .flatten()
        .filter_map(serde_yaml::Value::as_str)
        .map(str::to_owned)
        .collect()
}
