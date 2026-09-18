//! Skill metadata and shared format constants.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Current workflow document filename.
pub(crate) const WORKFLOW_MD: &str = "WORKFLOW.md";
/// Standard agentskills.io document filename.
pub(crate) const SKILL_MD: &str = "SKILL.md";
/// Legacy JSON manifest filename.
pub(crate) const SKILL_JSON: &str = "skill.json";
/// Recommended upper bound for a skill name.
pub(crate) const MAX_NAME_LEN: usize = 64;
/// Recommended upper bound for a skill description.
pub(crate) const MAX_DESCRIPTION_LEN: usize = 1024;
/// Conventional resource directories discovered inside a skill bundle.
pub(crate) const RESOURCE_DIRS: &[&str] = &[
    "scripts",
    "references",
    "assets",
    "templates",
    "examples",
    "prompts",
];
/// Maximum text resource size accepted by [`crate::read_resource`].
pub(crate) const MAX_RESOURCE_BYTES: u64 = 128 * 1024;
/// Maximum Markdown document size accepted by the parser.
pub(crate) const MAX_DOCUMENT_BYTES: u64 = 1024 * 1024;
/// Maximum legacy JSON manifest size accepted by the parser.
pub(crate) const MAX_LEGACY_MANIFEST_BYTES: u64 = 256 * 1024;

/// Origin of a discovered skill, ordered from lowest to highest precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SkillScope {
    /// A host-provided, compiled-in bundle.
    Builtin,
    /// A legacy workspace bundle.
    Legacy,
    /// A user-scoped bundle.
    #[default]
    User,
    /// A project-scoped bundle.
    Project,
    /// A bundle private to an active profile.
    Profile,
    /// A host-provided automation represented outside the filesystem.
    Flow,
}

impl SkillScope {
    /// Return the collision precedence for this scope.
    #[must_use]
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Builtin => 0,
            Self::Legacy => 1,
            Self::User => 2,
            Self::Project => 3,
            Self::Profile => 4,
            Self::Flow => 5,
        }
    }
}

/// Parsed YAML frontmatter for a skill document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    /// Stable or display name.
    #[serde(default)]
    pub name: String,
    /// Short explanation of when and why to use the skill.
    #[serde(default)]
    pub description: String,
    /// Optional SPDX license expression.
    #[serde(default)]
    pub license: Option<String>,
    /// Optional compatibility note.
    #[serde(default)]
    pub compatibility: Option<String>,
    /// Platform compatibility hints.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Extension metadata, including version, author, and tags.
    #[serde(default)]
    pub metadata: HashMap<String, serde_yaml::Value>,
    /// Non-binding tool requirements declared by the author.
    #[serde(
        default,
        rename = "allowed-tools",
        alias = "allowed_tools",
        alias = "tools",
        deserialize_with = "deserialize_string_or_sequence"
    )]
    pub allowed_tools: Vec<String>,
    /// Host event patterns that may activate the skill.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// Forward-compatible top-level values.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_yaml::Value>,
}

fn deserialize_string_or_sequence<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Sequence(Vec<String>),
        String(String),
    }

    Ok(match Value::deserialize(deserializer)? {
        Value::Sequence(values) => values,
        Value::String(value) => value
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

/// A discovered skill bundle.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Skill {
    /// Display name from frontmatter, falling back to the directory name.
    pub name: String,
    /// On-disk directory identifier.
    #[serde(default)]
    pub dir_name: String,
    /// Short catalog description.
    pub description: String,
    /// Declared version, if any.
    pub version: String,
    /// Declared author, if any.
    pub author: Option<String>,
    /// Search tags.
    pub tags: Vec<String>,
    /// Compatible platform hints.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Related skill names.
    #[serde(default)]
    pub related_skills: Vec<String>,
    /// Source ecosystem hint such as `agentskills`, `hermes`, or `legacy`.
    #[serde(default)]
    pub source_format: String,
    /// Declared tool hints.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Prompt paths from a legacy JSON manifest.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Path to the document or legacy manifest.
    pub location: Option<PathBuf>,
    /// Parsed document frontmatter.
    #[serde(default)]
    pub frontmatter: SkillFrontmatter,
    /// Resource paths relative to the bundle root.
    #[serde(default)]
    pub resources: Vec<PathBuf>,
    /// Discovery scope.
    #[serde(default)]
    pub scope: SkillScope,
    /// Whether this came from a legacy JSON manifest or root.
    #[serde(default)]
    pub legacy: bool,
    /// Non-fatal diagnostics produced while loading.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Cached body populated only for documents loaded by discovery.
    #[serde(skip)]
    pub body: Option<String>,
}

impl Skill {
    /// Re-read the Markdown body for this skill.
    #[must_use]
    pub fn read_body(&self) -> Option<String> {
        if self.legacy {
            return None;
        }
        self.body.clone()
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct LegacyManifest {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
}
