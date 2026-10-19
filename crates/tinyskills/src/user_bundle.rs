//! Immutable user bundle payloads, independent of persistence and authority.

use crate::{BundleDocument, is_safe_segment};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;

/// One ordinary file supplied as bundle content, never an ambient path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleFile {
    /// Slash-separated relative name.
    pub path: String,
    /// Original bytes, including empty or binary files.
    pub bytes: Vec<u8>,
}

/// Content limits applied before a bundle becomes publishable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleLimits {
    /// Maximum file count including the primary document.
    pub max_files: usize,
    /// Maximum bytes in any one file.
    pub max_file_bytes: usize,
    /// Maximum bytes across all files.
    pub max_total_bytes: usize,
    /// Maximum bytes in one relative name.
    pub max_path_bytes: usize,
    /// Maximum number of path segments from the bundle root to a file.
    pub max_depth: usize,
}

impl Default for BundleLimits {
    fn default() -> Self {
        Self {
            max_files: 4096,
            max_file_bytes: 16 * 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_path_bytes: 1024,
            max_depth: 64,
        }
    }
}

/// Why content cannot form a portable, unambiguous bundle tree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BundleContentError {
    /// A file name is unsafe or not portable; no input bytes are reported.
    #[error("invalid relative bundle file name")]
    InvalidPath,
    /// Names collide, including case-insensitive aliases.
    #[error("bundle files have colliding names")]
    DuplicatePath,
    /// A file would also have to be a directory.
    #[error("bundle file conflicts with a parent directory")]
    PathConflict,
    /// A requested limit was exceeded.
    #[error("bundle exceeds its {0} limit")]
    Limit(&'static str),
}

/// One digest manifest entry, sorted by relative file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleManifestFile {
    /// Original relative file name.
    pub path: String,
    /// Exact byte count.
    pub bytes: u64,
    /// Lowercase hexadecimal SHA-256 of the file bytes.
    pub digest: String,
}

/// Validated immutable content for a user skill or workflow.
///
/// Owns original bytes, not filesystem handles or storage authority. No
/// deserializer can bypass construction. Empty files remain distinct from
/// missing files. Frontmatter policy remains separate from tree validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserBundle {
    primary: BundleDocument,
    files: Vec<BundleFile>,
}

impl UserBundle {
    /// Build a bundle preserving the primary document and every resource byte.
    ///
    /// # Errors
    /// Returns a content error for unsafe/colliding names, directory conflicts
    /// or exceeded limits. No filesystem or backend operation occurs.
    pub fn new(
        primary: BundleDocument,
        document: String,
        mut resources: Vec<BundleFile>,
        limits: BundleLimits,
    ) -> Result<Self, BundleContentError> {
        if resources
            .len()
            .checked_add(1)
            .is_none_or(|count| count > limits.max_files)
        {
            return Err(BundleContentError::Limit("file count"));
        }
        resources.push(BundleFile {
            path: primary.file_name().into(),
            bytes: document.into_bytes(),
        });
        let mut names = HashSet::with_capacity(resources.len());
        let mut total = 0usize;
        for file in &resources {
            if file.path.len() > limits.max_path_bytes {
                return Err(BundleContentError::Limit("path bytes"));
            }
            if file.path.split('/').count() > limits.max_depth {
                return Err(BundleContentError::Limit("depth"));
            }
            if !file.path.split('/').all(portable_segment) {
                return Err(BundleContentError::InvalidPath);
            }
            if !names.insert(file.path.to_ascii_lowercase()) {
                return Err(BundleContentError::DuplicatePath);
            }
            if file.bytes.len() > limits.max_file_bytes {
                return Err(BundleContentError::Limit("file bytes"));
            }
            total = total
                .checked_add(file.bytes.len())
                .ok_or(BundleContentError::Limit("total bytes"))?;
            if total > limits.max_total_bytes {
                return Err(BundleContentError::Limit("total bytes"));
            }
        }
        // A case-insensitive alias cannot become a directory on Windows even
        // when the source was authored on a case-sensitive filesystem.
        for name in &names {
            for (index, _) in name.match_indices('/') {
                if names.contains(&name[..index]) {
                    return Err(BundleContentError::PathConflict);
                }
            }
        }
        resources.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            primary,
            files: resources,
        })
    }

    /// Primary document selection, independent of tenant identity.
    #[must_use]
    pub const fn primary(&self) -> BundleDocument {
        self.primary
    }

    /// File content in canonical relative-name order.
    #[must_use]
    pub fn files(&self) -> &[BundleFile] {
        &self.files
    }

    /// Exact bytes at a validated relative name, distinguishing absent/empty.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.bytes.as_slice())
    }

    /// Digest metadata in canonical relative-name order.
    #[must_use]
    pub fn manifest(&self) -> Vec<BundleManifestFile> {
        self.files
            .iter()
            .map(|file| BundleManifestFile {
                path: file.path.clone(),
                bytes: file.bytes.len() as u64,
                digest: hex(&Sha256::digest(&file.bytes)),
            })
            .collect()
    }

    /// Versioned content digest; not a tenant identifier or access grant.
    ///
    /// Unlike compiled `BundledSkill` digests, input file order is irrelevant.
    /// The primary selection, relative names and original bytes are covered.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"tinyskills-user-bundle-v1\0");
        hash.update((self.primary.file_name().len() as u64).to_le_bytes());
        hash.update(self.primary.file_name().as_bytes());
        for file in &self.files {
            hash.update((file.path.len() as u64).to_le_bytes());
            hash.update(file.path.as_bytes());
            hash.update((file.bytes.len() as u64).to_le_bytes());
            hash.update(&file.bytes);
        }
        hex(&hash.finalize())
    }
}

fn portable_segment(segment: &str) -> bool {
    if !is_safe_segment(segment) || segment.ends_with('.') {
        return false;
    }
    let stem = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|number| {
                number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9')
            })
        })
}

fn hex(digest: &[u8]) -> String {
    let mut result = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

#[cfg(test)]
#[path = "user_bundle_tests.rs"]
mod tests;
