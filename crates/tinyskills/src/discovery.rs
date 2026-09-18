//! Deterministic discovery and cross-scope collision handling.

use crate::document::{load_document, load_legacy};
use crate::model::{SKILL_JSON, SKILL_MD, Skill, SkillScope, WORKFLOW_MD};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".github",
    ".hub",
    ".archive",
    ".venv",
    "venv",
    "node_modules",
    "site-packages",
    "__pycache__",
    ".tox",
    ".nox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
];

/// One directory tree to scan with its host-assigned scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryRoot {
    /// Root directory containing one or more skill bundles.
    pub path: PathBuf,
    /// Scope assigned to bundles below the root.
    pub scope: SkillScope,
}

impl DiscoveryRoot {
    /// Construct a discovery root.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, scope: SkillScope) -> Self {
        Self {
            path: path.into(),
            scope,
        }
    }
}

/// Discover skills under ordered roots and resolve collisions by scope.
///
/// Roots may be provided in any order; [`SkillScope::precedence`] decides
/// which bundle survives a name or directory-id collision. Results are sorted
/// by display name.
#[must_use]
pub fn discover(roots: impl IntoIterator<Item = DiscoveryRoot>) -> Vec<Skill> {
    let skills = roots
        .into_iter()
        .flat_map(|root| scan_root(&root.path, root.scope));
    resolve_collisions(skills)
}

/// Resolve name and directory-id collisions in an arbitrary skill sequence.
#[must_use]
pub fn resolve_collisions(skills: impl IntoIterator<Item = Skill>) -> Vec<Skill> {
    let mut by_name = HashMap::new();
    absorb(&mut by_name, skills);
    let mut skills: Vec<_> = by_name.into_values().collect();
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    skills
}

/// Recursively scan one root for safe skill manifests.
#[must_use]
pub fn scan_root(root: &Path, scope: SkillScope) -> Vec<Skill> {
    let mut skills = Vec::new();
    scan_root_inner(root, scope, &mut skills);
    skills.sort_by(|left, right| left.dir_name.cmp(&right.dir_name));
    skills
}

fn scan_root_inner(root: &Path, scope: SkillScope, skills: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || !file_type.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if dir_name.starts_with('.') || EXCLUDED_DIRS.contains(&dir_name.as_str()) {
            continue;
        }
        let path = entry.path();
        if let Some(skill) = load_skill_dir(&path, scope) {
            skills.push(skill);
        } else {
            scan_root_inner(&path, scope, skills);
        }
    }
}

/// Load a bundle rooted at `dir`, if it contains a safe recognized manifest.
#[must_use]
pub fn load_skill_dir(dir: &Path, scope: SkillScope) -> Option<Skill> {
    let dir_name = dir.file_name()?.to_string_lossy();
    let workflow = dir.join(WORKFLOW_MD);
    let skill = dir.join(SKILL_MD);
    let legacy = dir.join(SKILL_JSON);
    let is_safe_file = |path: &Path| {
        matches!(
            std::fs::symlink_metadata(path),
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink()
        )
    };
    if is_safe_file(&workflow) {
        Some(load_document(&workflow, dir, &dir_name, scope))
    } else if is_safe_file(&skill) {
        Some(load_document(&skill, dir, &dir_name, scope))
    } else if is_safe_file(&legacy) {
        Some(load_legacy(&legacy, dir, &dir_name, scope))
    } else {
        None
    }
}

fn absorb(by_name: &mut HashMap<String, Skill>, incoming: impl IntoIterator<Item = Skill>) {
    for mut skill in incoming {
        let collision_keys: Vec<_> = by_name
            .iter()
            .filter(|(name, existing)| **name == skill.name || existing.dir_name == skill.dir_name)
            .map(|(name, _)| name.clone())
            .collect();
        let highest = collision_keys
            .iter()
            .filter_map(|key| by_name.get(key))
            .max_by_key(|existing| existing.scope.precedence());
        if let Some(existing) = highest
            && skill.scope.precedence() < existing.scope.precedence()
        {
            if let Some(kept) = by_name.get_mut(&existing.name.clone()) {
                kept.warnings.push(format!(
                    "skill id '{}' or name '{}' also declared in {:?} scope at {} (ignored)",
                    skill.dir_name,
                    skill.name,
                    skill.scope,
                    display_location(&skill)
                ));
            }
            continue;
        }
        for key in collision_keys {
            if let Some(shadowed) = by_name.remove(&key) {
                skill.warnings.push(format!(
                    "shadowed {:?}-scope skill '{}' (skill id '{}') at {}",
                    shadowed.scope,
                    shadowed.name,
                    shadowed.dir_name,
                    display_location(&shadowed)
                ));
            }
        }
        by_name.insert(skill.name.clone(), skill);
    }
}

fn display_location(skill: &Skill) -> String {
    skill
        .location
        .as_deref()
        .map_or_else(|| "<unknown>".to_owned(), |path| path.display().to_string())
}
