//! Skill bundle files on the host's `tinystoragedrivers` blob port.
//!
//! The blob store holds each bundle as a tree of blobs under
//! `skills/bundles/<bundle id>/<relative path>` ([`BUNDLE_PREFIX`]). Discovery needs real directories
//! (it walks them through capability handles), so the host does not point
//! discovery at the store: it calls [`BlobBundles::materialize`], which reads
//! the bundles through the blob *tree view* and rebuilds the cache directory
//! with [`materialize_tree`](tinyskills::materialize_tree).

use std::collections::BTreeSet;
use std::sync::Arc;

use cap_std::fs::Dir;
use tinystoragedrivers_core::{BlobStore, StorageError, TreeEntry, TreeFile};

use tinyskills::{
    MAX_MATERIALIZE_DEPTH, MAX_MATERIALIZE_FILE_BYTES, MaterializeEntry, MaterializeError,
    MaterializeFile, MaterializeReport, MaterializeSource, is_safe_segment, materialize_tree,
};

/// Why a blob-backed bundle operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BlobBundleError {
    /// The bundle id is not a single safe path segment.
    #[error("`{0}` cannot name a stored bundle")]
    InvalidId(String),
    /// A bundle file path is empty, absolute or has an unsafe segment.
    #[error("`{0}` is not a safe bundle file path")]
    InvalidPath(String),
    /// A bundle has no files to publish.
    #[error("a bundle needs at least one file")]
    Empty,
    /// The bundle does not exist.
    #[error("bundle `{0}` is not stored")]
    NotFound(String),
    /// A stored file is larger than [`MAX_MATERIALIZE_FILE_BYTES`].
    #[error("`{path}` is larger than {max} bytes")]
    TooLarge {
        /// The file, relative to the bundle.
        path: String,
        /// The limit.
        max: u64,
    },
    /// A stored bundle nests deeper than [`MAX_MATERIALIZE_DEPTH`].
    #[error("bundle `{0}` nests too deep")]
    TooDeep(String),
    /// The blob backend refused the operation.
    #[error("blob backend failed: {0}")]
    Backend(String),
    /// Writing the cache directory failed.
    #[error(transparent)]
    Materialize(#[from] MaterializeError),
}

impl From<StorageError> for BlobBundleError {
    fn from(error: StorageError) -> Self {
        Self::Backend(error.to_string())
    }
}

/// The blob prefix every bundle lives under, declared by
/// [`storage_schema`](crate::storage_schema).
pub const BUNDLE_PREFIX: &str = "skills/bundles";

/// Disjoint storage for immutable user-bundle generations.
pub const BUNDLE_GENERATION_PREFIX: &str = "skills/bundle-generations";

/// Bundle files in a blob store, one subtree per bundle under
/// [`BUNDLE_PREFIX`].
#[derive(Clone)]
pub struct BlobBundles {
    blobs: Arc<dyn BlobStore>,
}

impl std::fmt::Debug for BlobBundles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobBundles")
            .field("prefix", &BUNDLE_PREFIX)
            .finish_non_exhaustive()
    }
}

impl BlobBundles {
    /// Bundles in `blobs`, under [`BUNDLE_PREFIX`]. The host picks the
    /// backend and the tenant scope when it builds the handle.
    #[must_use]
    pub fn new(blobs: Arc<dyn BlobStore>) -> Self {
        Self { blobs }
    }

    fn dir(id: &str) -> Result<String, BlobBundleError> {
        if !is_safe_segment(id) {
            return Err(BlobBundleError::InvalidId(id.to_owned()));
        }
        Ok(format!("{BUNDLE_PREFIX}/{id}"))
    }

    /// Store `files` as bundle `id`, replacing the previous content: files
    /// the new bundle no longer has are removed after the new ones land.
    ///
    /// The blob port is not atomic across files; a host that needs readers to
    /// see all-or-nothing publishes a manifest document after this returns.
    ///
    /// # Errors
    ///
    /// An invalid id or path, an empty bundle, an oversized file, or a
    /// backend failure.
    pub async fn publish(
        &self,
        id: &str,
        files: Vec<MaterializeFile>,
    ) -> Result<usize, BlobBundleError> {
        let dir = Self::dir(id)?;
        if files.is_empty() {
            return Err(BlobBundleError::Empty);
        }
        for file in &files {
            if file
                .path
                .split('/')
                .any(|segment| !is_safe_segment(segment))
            {
                return Err(BlobBundleError::InvalidPath(file.path.clone()));
            }
            if file.bytes.len() as u64 > MAX_MATERIALIZE_FILE_BYTES {
                return Err(BlobBundleError::TooLarge {
                    path: file.path.clone(),
                    max: MAX_MATERIALIZE_FILE_BYTES,
                });
            }
        }
        let keep: BTreeSet<String> = files
            .iter()
            .map(|file| format!("{dir}/{}", file.path))
            .collect();
        let count = files.len();
        let tree = files
            .into_iter()
            .map(|file| TreeFile::new(file.path, file.bytes))
            .collect();
        self.blobs.put_tree(&dir, tree).await?;
        for meta in self.blobs.list(&format!("{dir}/")).await? {
            if !keep.contains(&meta.key) {
                self.blobs.delete(&meta.key).await?;
            }
        }
        Ok(count)
    }

    /// Remove bundle `id`, returning whether it had any files.
    ///
    /// # Errors
    ///
    /// An invalid id or a backend failure.
    pub async fn delete(&self, id: &str) -> Result<bool, BlobBundleError> {
        let dir = Self::dir(id)?;
        Ok(self.blobs.delete_tree(&dir).await? > 0)
    }

    /// The ids of every stored bundle, sorted.
    ///
    /// # Errors
    ///
    /// A backend failure.
    pub async fn list(&self) -> Result<Vec<String>, BlobBundleError> {
        Ok(self
            .blobs
            .list_dir(BUNDLE_PREFIX)
            .await?
            .into_iter()
            .filter_map(|entry| match entry {
                TreeEntry::Dir { name, .. } => Some(name),
                TreeEntry::Blob { .. } => None,
            })
            .collect())
    }

    /// Read bundle `id` through the blob tree view, in path order.
    ///
    /// # Errors
    ///
    /// [`BlobBundleError::NotFound`] when it has no files, a limit error for
    /// an oversized or too deeply nested bundle, or a backend failure.
    pub async fn read(&self, id: &str) -> Result<Vec<MaterializeFile>, BlobBundleError> {
        let dir = Self::dir(id)?;
        let mut files = Vec::new();
        let mut pending = vec![(dir.clone(), 0usize)];
        while let Some((current, depth)) = pending.pop() {
            for entry in self.blobs.list_dir(&current).await? {
                match entry {
                    TreeEntry::Dir { prefix, .. } => {
                        if depth >= MAX_MATERIALIZE_DEPTH {
                            return Err(BlobBundleError::TooDeep(id.to_owned()));
                        }
                        pending.push((prefix, depth + 1));
                    }
                    TreeEntry::Blob { meta, .. } => {
                        let path = meta.key[dir.len() + 1..].to_owned();
                        if meta.len > MAX_MATERIALIZE_FILE_BYTES {
                            return Err(BlobBundleError::TooLarge {
                                path,
                                max: MAX_MATERIALIZE_FILE_BYTES,
                            });
                        }
                        // A blob deleted between list and get is skipped.
                        if let Some(blob) = self.blobs.get(&meta.key).await? {
                            files.push(MaterializeFile::new(path, blob.bytes));
                        }
                    }
                }
            }
        }
        if files.is_empty() {
            return Err(BlobBundleError::NotFound(id.to_owned()));
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Rebuild `root_name` inside `parent` with one directory per
    /// `(dir_name, bundle id)` pair, reading each bundle from the store.
    ///
    /// All bundles are read before the cache tree is touched, so a missing
    /// bundle or a backend failure leaves the previous tree in place. The
    /// directory write itself is [`materialize_tree`].
    ///
    /// # Errors
    ///
    /// Any [`BlobBundleError`] from reading, or a [`MaterializeError`] from
    /// writing.
    pub async fn materialize(
        &self,
        parent: &Dir,
        root_name: &str,
        bundles: &[(String, String)],
    ) -> Result<MaterializeReport, BlobBundleError> {
        let mut entries = Vec::with_capacity(bundles.len());
        for (dir_name, id) in bundles {
            entries.push(MaterializeEntry {
                dir_name: dir_name.clone(),
                source: MaterializeSource::Files(self.read(id).await?),
            });
        }
        Ok(materialize_tree(parent, root_name, &entries)?)
    }
}

#[cfg(test)]
#[path = "blob_bundles_tests.rs"]
mod tests;
