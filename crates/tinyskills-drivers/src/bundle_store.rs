//! User bundles as immutable blob generations selected by CAS-protected heads.

use sha2::{Digest, Sha256};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tinyskills::{
    BundleDocument, BundleFile, BundleFuture, BundleLimits, BundleRevision, BundleStore,
    BundleStoreError, BundleSummary, StoredBundle, UserBundle, is_safe_segment,
};
use tinystoragedrivers_core::{
    BlobStore, DocumentStore, ErrorKind, Key, MAX_ID_LEN, Precondition, Query, StorageError,
    Version, WriteOptions,
};

use crate::BUNDLE_GENERATION_PREFIX;

/// The collection holding current user-bundle heads.
pub const BUNDLE_COLLECTION: &str = "skills.bundles";
pub(crate) const BUNDLE_SCHEMA: u32 = 1;
const MAX_FILES: usize = 65_536;
const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    bytes: u64,
    digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Head {
    revision: u64,
    tombstone: bool,
    primary: String,
    digest: String,
    generation: String,
    manifest: Vec<ManifestEntry>,
}

/// A `BundleStore` backed by CAS documents and immutable blob generations.
#[derive(Clone)]
pub struct DocumentBundleStore {
    documents: Arc<dyn DocumentStore>,
    blobs: Arc<dyn BlobStore>,
}

impl std::fmt::Debug for DocumentBundleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentBundleStore")
            .finish_non_exhaustive()
    }
}

impl DocumentBundleStore {
    /// Bind the store to already-scoped storage handles supplied by the host.
    #[must_use]
    pub fn new(documents: Arc<dyn DocumentStore>, blobs: Arc<dyn BlobStore>) -> Self {
        Self { documents, blobs }
    }

    fn id(id: &str) -> Result<String, BundleStoreError> {
        if !is_safe_segment(id) || id.len() > MAX_ID_LEN {
            return Err(BundleStoreError::InvalidId);
        }
        Key::new(id.to_owned())
            .map(|key| key.as_str().to_owned())
            .map_err(|_| BundleStoreError::InvalidId)
    }

    async fn head(&self, id: &str) -> Result<Option<(Head, Version)>, BundleStoreError> {
        let Some(row) = self
            .documents
            .get(BUNDLE_COLLECTION, id)
            .await
            .map_err(backend)?
        else {
            return Ok(None);
        };
        if row.meta.schema != BUNDLE_SCHEMA {
            return Err(BundleStoreError::Corrupt);
        }
        let head = serde_json::from_value(row.doc).map_err(|_| BundleStoreError::Corrupt)?;
        Ok(Some((head, row.version)))
    }

    async fn files(&self, id: &str, head: &Head) -> Result<UserBundle, BundleStoreError> {
        if !valid_digest(&head.digest)
            || head.manifest.is_empty()
            || head.manifest.len() > MAX_FILES
        {
            return Err(BundleStoreError::Corrupt);
        }
        let prefix = format!("{BUNDLE_GENERATION_PREFIX}/{id}/{}/", head.generation);
        let mut listed = self.blobs.list(&prefix).await.map_err(backend)?;
        listed.sort_by(|a, b| a.key.cmp(&b.key));
        if listed.len() != head.manifest.len() {
            return Err(BundleStoreError::Corrupt);
        }
        let mut expected_total = 0usize;
        for (meta, expected) in listed.iter().zip(&head.manifest) {
            let path = meta
                .key
                .strip_prefix(&prefix)
                .ok_or(BundleStoreError::Corrupt)?;
            let bytes = usize::try_from(expected.bytes).map_err(|_| BundleStoreError::Corrupt)?;
            expected_total = expected_total
                .checked_add(bytes)
                .ok_or(BundleStoreError::Corrupt)?;
            if path != expected.path
                || expected.path.len() > MAX_PATH_BYTES
                || bytes > MAX_FILE_BYTES
                || !valid_digest(&expected.digest)
                || expected_total > MAX_TOTAL_BYTES
                || meta.len != expected.bytes
            {
                return Err(BundleStoreError::Corrupt);
            }
        }
        let max_file_bytes = head.manifest.iter().try_fold(0usize, |max, entry| {
            usize::try_from(entry.bytes)
                .map(|bytes| max.max(bytes))
                .map_err(|_| BundleStoreError::Corrupt)
        })?;
        let max_path_bytes = head
            .manifest
            .iter()
            .map(|entry| entry.path.len())
            .max()
            .unwrap_or(0);
        let max_depth = head
            .manifest
            .iter()
            .map(|entry| entry.path.split('/').count())
            .max()
            .unwrap_or(0);
        let mut files = Vec::with_capacity(listed.len());
        for (meta, expected) in listed.into_iter().zip(&head.manifest) {
            let blob = self
                .blobs
                .get(&meta.key)
                .await
                .map_err(backend)?
                .ok_or(BundleStoreError::Corrupt)?;
            if blob.bytes.len() as u64 != expected.bytes || sha256(&blob.bytes) != expected.digest {
                return Err(BundleStoreError::Corrupt);
            }
            files.push(BundleFile {
                path: meta.key[prefix.len()..].to_owned(),
                bytes: blob.bytes,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let primary = if head.primary == "skill" {
            BundleDocument::Skill
        } else if head.primary == "workflow" {
            BundleDocument::Workflow
        } else {
            return Err(BundleStoreError::Corrupt);
        };
        let primary_file = primary.file_name();
        let document = String::from_utf8(
            files
                .iter()
                .find(|file| file.path == primary_file)
                .ok_or(BundleStoreError::Corrupt)?
                .bytes
                .clone(),
        )
        .map_err(|_| BundleStoreError::Corrupt)?;
        let resources = files
            .into_iter()
            .filter(|file| file.path != primary_file)
            .collect();
        let content = UserBundle::new(
            primary,
            document,
            resources,
            BundleLimits {
                max_files: head.manifest.len(),
                max_file_bytes,
                max_total_bytes: expected_total,
                max_path_bytes,
                max_depth,
            },
        )
        .map_err(|_| BundleStoreError::Corrupt)?;
        if content.digest() != head.digest {
            return Err(BundleStoreError::Corrupt);
        }
        Ok(content)
    }

    async fn write_content(
        &self,
        id: &str,
        revision: u64,
        content: &UserBundle,
    ) -> Result<String, BundleStoreError> {
        let manifest = content.manifest();
        let total = manifest
            .iter()
            .try_fold(0usize, |total, entry| {
                let bytes = usize::try_from(entry.bytes).ok()?;
                total.checked_add(bytes)
            })
            .ok_or(BundleStoreError::Corrupt)?;
        if manifest.len() > MAX_FILES
            || total > MAX_TOTAL_BYTES
            || manifest.iter().any(|entry| {
                entry.bytes > MAX_FILE_BYTES as u64 || entry.path.len() > MAX_PATH_BYTES
            })
        {
            return Err(BundleStoreError::Corrupt);
        }
        let generation = format!("{revision}-{}", content.digest());
        let prefix = format!("{BUNDLE_GENERATION_PREFIX}/{id}/{generation}");
        let expected_keys: std::collections::BTreeSet<_> = content
            .files()
            .iter()
            .map(|file| format!("{prefix}/{}", file.path))
            .collect();
        let mut existing = self
            .blobs
            .list(&format!("{prefix}/"))
            .await
            .map_err(backend)?;
        existing.sort_by(|a, b| a.key.cmp(&b.key));
        if existing
            .iter()
            .any(|meta| !expected_keys.contains(&meta.key))
        {
            return Err(BundleStoreError::Corrupt);
        }
        for file in content.files() {
            let key = format!("{prefix}/{}", file.path);
            if let Some(meta) = existing.iter().find(|meta| meta.key == key)
                && meta.len
                    != u64::try_from(file.bytes.len()).map_err(|_| BundleStoreError::Corrupt)?
            {
                return Err(BundleStoreError::Corrupt);
            }
            match self.blobs.get(&key).await.map_err(backend)? {
                Some(existing) if existing.bytes == file.bytes => {}
                Some(_) => return Err(BundleStoreError::Corrupt),
                None => {
                    self.blobs
                        .put(&key, file.bytes.clone(), None)
                        .await
                        .map_err(backend)?;
                }
            }
        }
        Ok(generation)
    }
}

fn backend(error: StorageError) -> BundleStoreError {
    BundleStoreError::Backend(Box::new(error))
}
fn primary_name(primary: BundleDocument) -> &'static str {
    match primary {
        BundleDocument::Skill => "skill",
        BundleDocument::Workflow => "workflow",
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(result, "{byte:02x}");
    }
    result
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl BundleStore for DocumentBundleStore {
    fn read<'a>(&'a self, id: &'a str) -> BundleFuture<'a, Option<StoredBundle>> {
        Box::pin(async move {
            let id = Self::id(id)?;
            let Some((head, _)) = self.head(&id).await? else {
                return Ok(None);
            };
            if head.tombstone {
                return Ok(None);
            }
            let content = self.files(&id, &head).await?;
            Ok(Some(StoredBundle {
                revision: BundleRevision(head.revision),
                content,
            }))
        })
    }

    fn publish<'a>(
        &'a self,
        id: &'a str,
        expected: Option<BundleRevision>,
        content: &'a UserBundle,
    ) -> BundleFuture<'a, BundleRevision> {
        Box::pin(async move {
            let id = Self::id(id)?;
            let current = self.head(&id).await?;
            let current_visible = current
                .as_ref()
                .filter(|(h, _)| !h.tombstone)
                .map(|(h, _)| BundleRevision(h.revision));
            if current_visible != expected {
                return Err(BundleStoreError::Conflict {
                    current: current_visible,
                });
            }
            let revision = current
                .as_ref()
                .map_or(Some(1), |(head, _)| head.revision.checked_add(1))
                .ok_or(BundleStoreError::Corrupt)?;
            let generation = self.write_content(&id, revision, content).await?;
            let head = Head {
                revision,
                tombstone: false,
                primary: primary_name(content.primary()).into(),
                digest: content.digest(),
                generation,
                manifest: content
                    .manifest()
                    .into_iter()
                    .map(|entry| ManifestEntry {
                        path: entry.path,
                        bytes: entry.bytes,
                        digest: entry.digest,
                    })
                    .collect(),
            };
            let precondition = current
                .as_ref()
                .map_or(Precondition::Absent, |(_, version)| {
                    Precondition::Version(*version)
                });
            if let Err(error) = self
                .documents
                .put_with(
                    BUNDLE_COLLECTION,
                    &id,
                    serde_json::to_value(head)
                        .map_err(|e| BundleStoreError::Backend(Box::new(e)))?,
                    precondition,
                    WriteOptions::new().schema(BUNDLE_SCHEMA),
                )
                .await
            {
                if error.kind() == ErrorKind::Conflict || error.kind() == ErrorKind::AlreadyExists {
                    let now = self.head(&id).await?;
                    return Err(BundleStoreError::Conflict {
                        current: now
                            .filter(|(head, _)| !head.tombstone)
                            .map(|(head, _)| BundleRevision(head.revision)),
                    });
                }
                return Err(backend(error));
            }
            Ok(BundleRevision(revision))
        })
    }

    fn list<'a>(
        &'a self,
        after: Option<&'a str>,
        limit: usize,
    ) -> BundleFuture<'a, Vec<BundleSummary>> {
        Box::pin(async move {
            if let Some(cursor) = after {
                Self::id(cursor)?;
            }
            let mut query = Query::all().limit(limit.max(1));
            let mut output = Vec::new();
            loop {
                let page = self
                    .documents
                    .query(BUNDLE_COLLECTION, &query)
                    .await
                    .map_err(backend)?;
                if limit == 0 {
                    return Ok(output);
                }
                for row in page.items {
                    Self::id(&row.id).map_err(|_| BundleStoreError::Corrupt)?;
                    if after.is_some_and(|cursor| row.id.as_str() <= cursor) {
                        continue;
                    }
                    if row.meta.schema != BUNDLE_SCHEMA {
                        return Err(BundleStoreError::Corrupt);
                    }
                    let head: Head =
                        serde_json::from_value(row.doc).map_err(|_| BundleStoreError::Corrupt)?;
                    if head.tombstone {
                        continue;
                    }
                    output.push(BundleSummary {
                        id: row.id,
                        revision: BundleRevision(head.revision),
                        primary: if head.primary == "skill" {
                            BundleDocument::Skill
                        } else if head.primary == "workflow" {
                            BundleDocument::Workflow
                        } else {
                            return Err(BundleStoreError::Corrupt);
                        },
                        digest: if valid_digest(&head.digest) {
                            head.digest
                        } else {
                            return Err(BundleStoreError::Corrupt);
                        },
                    });
                    if output.len() >= limit {
                        return Ok(output);
                    }
                }
                match page.next {
                    Some(cursor) => query.cursor = Some(cursor),
                    None => return Ok(output),
                }
            }
        })
    }

    fn delete<'a>(
        &'a self,
        id: &'a str,
        expected: BundleRevision,
    ) -> BundleFuture<'a, BundleRevision> {
        Box::pin(async move {
            let id = Self::id(id)?;
            let Some((mut head, version)) = self.head(&id).await? else {
                return Err(BundleStoreError::Conflict { current: None });
            };
            let current = (!head.tombstone).then_some(BundleRevision(head.revision));
            if current != Some(expected) {
                return Err(BundleStoreError::Conflict { current });
            }
            let revision = head
                .revision
                .checked_add(1)
                .ok_or(BundleStoreError::Corrupt)?;
            head.revision = revision;
            head.tombstone = true;
            if let Err(error) = self
                .documents
                .put_with(
                    BUNDLE_COLLECTION,
                    &id,
                    serde_json::to_value(head)
                        .map_err(|e| BundleStoreError::Backend(Box::new(e)))?,
                    Precondition::Version(version),
                    WriteOptions::new().schema(BUNDLE_SCHEMA),
                )
                .await
            {
                if error.kind() == ErrorKind::Conflict || error.kind() == ErrorKind::AlreadyExists {
                    let now = self.head(&id).await?;
                    return Err(BundleStoreError::Conflict {
                        current: now
                            .filter(|(head, _)| !head.tombstone)
                            .map(|(head, _)| BundleRevision(head.revision)),
                    });
                }
                return Err(backend(error));
            }
            Ok(BundleRevision(revision))
        })
    }
}

#[cfg(test)]
#[path = "bundle_store_tests.rs"]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests;
