//! Authoring helpers: slugs, document rendering, and bundle scaffolding.
//!
//! The filesystem half of "create or edit a skill" lives here so every host
//! gets the same containment, body-preservation, and migration behavior. The
//! host still chooses the root, decides whether it is trusted, and writes any
//! product-specific sidecar files next to the returned document.

use crate::document::parse_skill;
use crate::flat::split_frontmatter;
use crate::model::{MAX_DESCRIPTION_LEN, MAX_NAME_LEN, RESOURCE_DIRS, SKILL_MD, WORKFLOW_MD};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

/// Errors returned while authoring a bundle.
#[derive(Debug, Error)]
pub enum AuthoringError {
    /// The display name is empty after trimming.
    #[error("name must not be empty")]
    EmptyName,
    /// The display name is longer than [`MAX_NAME_LEN`].
    #[error("name exceeds max {max} chars")]
    NameTooLong {
        /// Maximum accepted length.
        max: usize,
    },
    /// The description is empty after trimming.
    #[error("description must not be empty")]
    EmptyDescription,
    /// The description is longer than [`MAX_DESCRIPTION_LEN`].
    #[error("description exceeds max {max} chars")]
    DescriptionTooLong {
        /// Maximum accepted length.
        max: usize,
    },
    /// The name contains no ASCII alphanumeric character to build a slug from.
    #[error("name '{name}' has no alphanumeric characters; cannot derive slug")]
    NoSlug {
        /// The rejected name.
        name: String,
    },
    /// The derived slug is longer than [`MAX_NAME_LEN`].
    #[error("slug '{slug}' exceeds max {max} chars")]
    SlugTooLong {
        /// The rejected slug.
        slug: String,
        /// Maximum accepted length.
        max: usize,
    },
    /// The slug is not a single safe path component.
    #[error("slug '{slug}' is not a single safe directory name")]
    InvalidSlug {
        /// The rejected slug.
        slug: String,
    },
    /// The resolved bundle directory is not inside the canonical root.
    #[error("resolved skill dir {dir} escapes scope root {root}")]
    Escapes {
        /// The escaping directory.
        dir: String,
        /// The canonical root it should have stayed inside.
        root: String,
    },
    /// The bundle directory is a symbolic link.
    #[error("skill dir {dir} is a symlink — refusing to write through it")]
    SymlinkedDir {
        /// The symlinked directory.
        dir: String,
    },
    /// A create request found an existing bundle.
    #[error("skill '{slug}' already exists at {dir}")]
    AlreadyExists {
        /// Slug of the existing bundle.
        slug: String,
        /// Directory of the existing bundle.
        dir: String,
    },
    /// An edit request found no bundle to update.
    #[error("cannot update workflow '{slug}': it does not exist at {dir}")]
    NotFound {
        /// Slug that was looked up.
        slug: String,
        /// Directory that was expected.
        dir: String,
    },
    /// An edit request could not parse the existing document body safely.
    #[error(
        "cannot update workflow '{slug}': existing markdown could not be parsed safely (refusing to overwrite the body)"
    )]
    UnparseableBody {
        /// Slug being edited.
        slug: String,
    },
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
}

/// Which document filename a scaffolded bundle uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BundleDocument {
    /// `WORKFLOW.md`, which discovery prefers over `SKILL.md`.
    #[default]
    Workflow,
    /// The standard agentskills.io `SKILL.md`.
    Skill,
}

impl BundleDocument {
    /// Filename this variant writes.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Workflow => WORKFLOW_MD,
            Self::Skill => SKILL_MD,
        }
    }

    const fn other(self) -> &'static str {
        match self {
            Self::Workflow => SKILL_MD,
            Self::Skill => WORKFLOW_MD,
        }
    }
}

/// Frontmatter fields for a scaffolded bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleSpec {
    /// Directory name and frontmatter `name`; see [`slugify`].
    pub slug: String,
    /// One-line description.
    pub description: String,
    /// Optional SPDX license.
    pub license: Option<String>,
    /// Optional author, written under `metadata.author`.
    pub author: Option<String>,
    /// Optional tags, written under `metadata.tags`.
    pub tags: Vec<String>,
    /// Optional tool hints, written to `allowed-tools`.
    pub allowed_tools: Vec<String>,
}

/// Options controlling [`scaffold_bundle_in_scratch`].
#[derive(Debug, Clone, Default)]
pub struct ScaffoldOptions {
    /// Edit an existing bundle instead of creating a new one. The existing
    /// document body is preserved and only the frontmatter is rewritten, and
    /// the other document filename is removed so discovery sees no duplicate.
    pub overwrite: bool,
    /// Read-only legacy sources searched for an existing bundle on edit when
    /// it is not under the scratch root (for example pre-rename locations).
    pub legacy_roots: Vec<PathBuf>,
    /// Document filename to write.
    pub document: BundleDocument,
}

/// Result of a successful [`scaffold_bundle_in_scratch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaffoldOutcome {
    /// Slug of the bundle.
    pub slug: String,
    /// Canonical bundle directory; hosts write sidecar files here.
    pub dir: PathBuf,
    /// Path of the written document.
    pub document: PathBuf,
}

/// Validate and trim a display name.
///
/// # Errors
///
/// Returns an error for empty names and names over [`MAX_NAME_LEN`] bytes.
pub fn validate_display_name(name: &str) -> Result<&str, AuthoringError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AuthoringError::EmptyName);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(AuthoringError::NameTooLong { max: MAX_NAME_LEN });
    }
    Ok(name)
}

/// Validate and trim a description.
///
/// # Errors
///
/// Returns an error for empty descriptions and those over
/// [`MAX_DESCRIPTION_LEN`] bytes.
pub fn validate_description(description: &str) -> Result<&str, AuthoringError> {
    let description = description.trim();
    if description.is_empty() {
        return Err(AuthoringError::EmptyDescription);
    }
    if description.len() > MAX_DESCRIPTION_LEN {
        return Err(AuthoringError::DescriptionTooLong {
            max: MAX_DESCRIPTION_LEN,
        });
    }
    Ok(description)
}

/// A description longer than a host's character budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error(
    "that description is {chars} characters; a description has to be {max} characters or fewer"
)]
pub struct DescriptionTooLong {
    /// The description's length, in characters.
    pub chars: usize,
    /// The budget it exceeded.
    pub max: usize,
}

/// Checks a description against a budget counted in characters, not bytes.
///
/// The description is measured as given; trim it first if the host does.
/// Unlike [`validate_description`], an empty description passes.
///
/// # Errors
///
/// [`DescriptionTooLong`] when it is longer than `max_chars` characters.
pub fn validate_description_chars(
    description: &str,
    max_chars: usize,
) -> Result<(), DescriptionTooLong> {
    let chars = description.chars().count();
    if chars > max_chars {
        return Err(DescriptionTooLong {
            chars,
            max: max_chars,
        });
    }
    Ok(())
}

/// A frontmatter block larger than a host's byte budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error(
    "that skill's frontmatter block is {bytes} bytes; a frontmatter block has to be {max} bytes or fewer"
)]
pub struct FrontmatterTooLarge {
    /// The block's size, in bytes, fences excluded.
    pub bytes: usize,
    /// The budget it exceeded.
    pub max: usize,
}

/// Checks the frontmatter block of `src`, as [`split_frontmatter`] finds it,
/// against a byte budget.
///
/// A document with no frontmatter block passes; refusing it is the parser's
/// job.
///
/// # Errors
///
/// [`FrontmatterTooLarge`] when the block is larger than `max_bytes`.
pub fn check_frontmatter_size(src: &str, max_bytes: usize) -> Result<(), FrontmatterTooLarge> {
    match split_frontmatter(src) {
        Some((frontmatter, _)) if frontmatter.len() > max_bytes => Err(FrontmatterTooLarge {
            bytes: frontmatter.len(),
            max: max_bytes,
        }),
        _ => Ok(()),
    }
}

/// Convert a human-readable name to a filesystem-safe slug.
///
/// ASCII alphanumerics are lowercased and kept; whitespace, `-`, and `_`
/// collapse to a single `-`; everything else is dropped; leading and trailing
/// `-` are trimmed.
///
/// # Errors
///
/// Returns an error when nothing alphanumeric remains or the slug exceeds
/// [`MAX_NAME_LEN`].
pub fn slugify(name: &str) -> Result<String, AuthoringError> {
    crate::slug::slugify_with(name, &crate::slug::SlugRules::default())
}

/// The unbounded derivation behind [`slugify`] and
/// [`crate::slugify_with`]: lowercase ASCII alphanumerics, separators folded
/// to single `-`, everything else dropped.
pub(crate) fn slug_from_name(name: &str) -> Result<String, AuthoringError> {
    let mut out = String::new();
    let mut prev_hyphen = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_hyphen = false;
        } else if (ch == '-' || ch == '_' || ch.is_whitespace()) && !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        return Err(AuthoringError::NoSlug {
            name: name.to_owned(),
        });
    }
    Ok(out)
}

/// Best-effort YAML scalar encoder.
///
/// Plain-safe strings pass through; anything with structure, leading or
/// trailing whitespace, or control characters is double-quoted and escaped.
#[must_use]
pub fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars().any(|c| {
            matches!(
                c,
                ':' | '#'
                    | '\''
                    | '"'
                    | '\n'
                    | '\r'
                    | '\t'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | ','
                    | '&'
                    | '*'
                    | '!'
                    | '|'
                    | '>'
                    | '%'
                    | '@'
                    | '`'
            )
        })
        || s.starts_with(|c: char| c.is_ascii_whitespace() || c == '-' || c == '?')
        || s.ends_with(|c: char| c.is_ascii_whitespace());
    if !needs_quote {
        return s.to_owned();
    }
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

/// Render just the YAML frontmatter block (`---\n…\n---\n`).
#[must_use]
pub fn render_workflow_frontmatter(spec: &BundleSpec) -> String {
    let mut out = String::from("---\n");
    let _ = writeln!(out, "name: {}", spec.slug);
    let _ = writeln!(out, "description: {}", yaml_scalar(&spec.description));
    if let Some(license) = &spec.license {
        let _ = writeln!(out, "license: {}", yaml_scalar(license));
    }
    if spec.author.is_some() || !spec.tags.is_empty() {
        out.push_str("metadata:\n");
        if let Some(author) = &spec.author {
            let _ = writeln!(out, "  author: {}", yaml_scalar(author));
        }
        if !spec.tags.is_empty() {
            out.push_str("  tags:\n");
            for tag in &spec.tags {
                let _ = writeln!(out, "    - {}", yaml_scalar(tag));
            }
        }
    }
    if !spec.allowed_tools.is_empty() {
        out.push_str("allowed-tools:\n");
        for tool in &spec.allowed_tools {
            let _ = writeln!(out, "  - {}", yaml_scalar(tool));
        }
    }
    out.push_str("---\n");
    out
}

/// Render a minimal document (frontmatter plus template body) for a freshly
/// scaffolded bundle.
#[must_use]
pub fn render_workflow_md(spec: &BundleSpec) -> String {
    let mut out = render_workflow_frontmatter(spec);
    out.push('\n');
    let _ = write!(out, "# {}\n\n", spec.slug);
    out.push_str(&spec.description);
    if !spec.description.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n## Instructions\n\n");
    out.push_str("_Describe when and how this skill should be used._\n");
    out
}

/// Create or update a bundle directory under caller-owned scratch space.
///
/// `root` is created if needed and canonicalized; the bundle directory
/// `root/<spec.slug>` must stay inside it and must not be a symlink. On
/// create the directory must not exist. On edit ([`ScaffoldOptions::overwrite`])
/// the bundle must exist under the scratch root or a read-only legacy source
/// is copied into it first. The existing body is kept, the other document
/// filename is removed, and conventional resource directories are created.
/// Persistent publication belongs to [`crate::scaffold_bundle_in_store`].
///
/// # Errors
///
/// Returns an error for an unsafe slug, an invalid description, root or
/// containment failures, create/edit precondition failures, an edit whose
/// existing body cannot be parsed, or filesystem failures.
pub fn scaffold_bundle_in_scratch(
    root: &Path,
    spec: &BundleSpec,
    options: &ScaffoldOptions,
) -> Result<ScaffoldOutcome, AuthoringError> {
    let description = validate_description(&spec.description)?;
    validate_slug(&spec.slug)?;

    std::fs::create_dir_all(root)
        .map_err(|error| io_error("failed to create skills root", root, error))?;
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|error| io_error("failed to canonicalize skills root", root, error))?;

    let mut dir = canonical_root.join(&spec.slug);
    if !dir.starts_with(&canonical_root) {
        return Err(AuthoringError::Escapes {
            dir: dir.display().to_string(),
            root: canonical_root.display().to_string(),
        });
    }
    if options.overwrite
        && !dir.exists()
        && let Some(legacy) = find_in_roots(&options.legacy_roots, &spec.slug)
    {
        let metadata = std::fs::symlink_metadata(&legacy)
            .map_err(|error| io_error("failed to inspect legacy bundle", &legacy, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AuthoringError::SymlinkedDir {
                dir: legacy.display().to_string(),
            });
        }
        copy_tree(&legacy, &dir)?;
    }
    if std::fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(AuthoringError::SymlinkedDir {
            dir: dir.display().to_string(),
        });
    }

    let exists = dir.exists();
    if exists && !options.overwrite {
        return Err(AuthoringError::AlreadyExists {
            slug: spec.slug.clone(),
            dir: dir.display().to_string(),
        });
    }
    if !exists && options.overwrite {
        return Err(AuthoringError::NotFound {
            slug: spec.slug.clone(),
            dir: dir.display().to_string(),
        });
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| io_error("failed to create skill dir", &dir, error))?;

    let document = dir.join(options.document.file_name());
    let other = dir.join(options.document.other());
    let content = if options.overwrite {
        let source = if document.exists() { &document } else { &other };
        let body = parse_skill(source)
            .map(|(_, body, _)| body)
            .ok_or_else(|| AuthoringError::UnparseableBody {
                slug: spec.slug.clone(),
            })?;
        let trimmed = BundleSpec {
            description: description.to_owned(),
            ..spec.clone()
        };
        let mut out = render_workflow_frontmatter(&trimmed);
        out.push('\n');
        out.push_str(body.trim_start_matches('\n'));
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out
    } else {
        render_workflow_md(&BundleSpec {
            description: description.to_owned(),
            ..spec.clone()
        })
    };
    std::fs::write(&document, content)
        .map_err(|error| io_error("failed to write", &document, error))?;
    if options.overwrite {
        // Best effort: a stale duplicate document must not shadow the edit.
        let _ = std::fs::remove_file(&other);
    }

    for sub in RESOURCE_DIRS {
        let path = dir.join(sub);
        std::fs::create_dir_all(&path)
            .map_err(|error| io_error("failed to create", &path, error))?;
    }

    Ok(ScaffoldOutcome {
        slug: spec.slug.clone(),
        dir,
        document,
    })
}

pub(crate) fn validate_slug(slug: &str) -> Result<(), AuthoringError> {
    let mut components = Path::new(slug).components();
    let single = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    );
    if !single || slug.len() > MAX_NAME_LEN || slug.contains(['/', '\\']) {
        return Err(AuthoringError::InvalidSlug {
            slug: slug.to_owned(),
        });
    }
    Ok(())
}

fn find_in_roots(roots: &[PathBuf], slug: &str) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        let canonical_root = std::fs::canonicalize(root).ok()?;
        let candidate = canonical_root.join(slug);
        (candidate.starts_with(&canonical_root) && candidate.exists()).then_some(candidate)
    })
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), AuthoringError> {
    std::fs::create_dir_all(destination)
        .map_err(|error| io_error("failed to create scratch bundle", destination, error))?;
    for entry in std::fs::read_dir(source)
        .map_err(|error| io_error("failed to read legacy bundle", source, error))?
    {
        let entry =
            entry.map_err(|error| io_error("failed to read legacy bundle", source, error))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| io_error("failed to inspect legacy entry", &path, error))?;
        let target = destination.join(entry.file_name());
        if metadata.file_type().is_symlink() {
            return Err(AuthoringError::SymlinkedDir {
                dir: path.display().to_string(),
            });
        }
        if metadata.is_dir() {
            copy_tree(&path, &target)?;
        } else if metadata.is_file() {
            std::fs::copy(&path, &target)
                .map_err(|error| io_error("failed to copy legacy entry", &path, error))?;
        } else {
            return Err(io_error(
                "unsupported legacy entry",
                &path,
                std::io::Error::other("unsupported file type"),
            ));
        }
    }
    Ok(())
}

fn io_error(context: &'static str, path: &Path, source: std::io::Error) -> AuthoringError {
    AuthoringError::Io {
        context,
        path: path.display().to_string(),
        source,
    }
}
