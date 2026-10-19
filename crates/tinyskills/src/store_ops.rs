//! Store-backed entrypoints for authored and installed bundles.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use crate::{
    BundleDocument, BundleFile, BundleLimits, BundleRevision, BundleSpec, BundleStore,
    BundleStoreError, BundledSkill, ScaffoldOptions, UserBundle, validate_fetched_document,
};

/// A durable result of publishing a bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedBundle {
    /// Bundle identifier within the bound store.
    pub id: String,
    /// Committed head revision.
    pub revision: BundleRevision,
    /// Digest of the committed content.
    pub digest: String,
}

/// Errors from store-backed bundle operations.
#[derive(Debug, thiserror::Error)]
pub enum BundleOperationError {
    /// Store operation refused or conflicted.
    #[error(transparent)]
    Store(#[from] BundleStoreError),
    /// Filesystem staging or legacy import failed.
    #[error("bundle staging failed: {0}")]
    Io(#[from] std::io::Error),
    /// Bundle content failed validation.
    #[error(transparent)]
    Content(#[from] crate::BundleContentError),
    /// Authoring request failed validation.
    #[error(transparent)]
    Authoring(#[from] crate::AuthoringError),
    /// Fetched document failed validation.
    #[error(transparent)]
    Document(#[from] crate::DocumentError),
    /// A compiled bundle failed validation or omitted its primary document.
    #[error("compiled bundle is invalid")]
    InvalidCompiledBundle,
}

/// Scaffold or edit a bundle in caller-owned scratch space and publish it.
///
/// `scratch_root` is staging only; it is never used as a persistent fallback.
/// An existing store head is authoritative. Legacy roots are read-only import
/// sources and are consulted only when the store has no head.
///
/// # Errors
///
/// Returns an error when reading or publishing the store head, preparing the
/// caller-owned staging tree, importing a legacy bundle, or validating the
/// authored bundle fails.
pub async fn scaffold_bundle_in_store<S: BundleStore + ?Sized>(
    store: &S,
    scratch_root: &Path,
    spec: &BundleSpec,
    options: &ScaffoldOptions,
    expected: Option<BundleRevision>,
) -> Result<PublishedBundle, BundleOperationError> {
    crate::authoring::validate_slug(&spec.slug)?;
    crate::validate_description(&spec.description)?;
    let current = store.read(&spec.slug).await?;
    if current.as_ref().map(|stored| stored.revision) != expected {
        return Err(BundleOperationError::Store(BundleStoreError::Conflict {
            current: current.as_ref().map(|stored| stored.revision),
        }));
    }
    let stage = scratch_root.join(&spec.slug);
    match fs::symlink_metadata(&stage) {
        Ok(_) => {
            return Err(BundleOperationError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "caller-provided staging path already exists",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(stored) = &current {
        let bounded = validate_stored_bundle(&stored.content, options.limits)?;
        write_staged_bundle(&stage, &bounded)?;
    } else if options.overwrite
        && let Some(legacy) = find_legacy_bundle(&options.legacy_roots, &spec.slug)?
    {
        let imported = read_bundle_dir(&legacy, options.document, options.limits)?;
        write_staged_bundle(&stage, &imported)?;
    }

    let stage_options = ScaffoldOptions {
        overwrite: stage.exists(),
        legacy_roots: Vec::new(),
        document: options.document,
        limits: options.limits,
    };
    crate::scaffold_bundle_in_scratch(scratch_root, spec, &stage_options)?;
    let content = read_staged_bundle(&stage, options.document, options.limits)?;
    let revision = store.publish(&spec.slug, expected, &content).await?;
    Ok(PublishedBundle {
        id: spec.slug.clone(),
        revision,
        digest: content.digest(),
    })
}

/// Install a validated fetched skill document if its slug is absent.
///
/// # Errors
///
/// Returns an error when the fetched document is invalid or the store refuses
/// to read or publish its bundle.
pub async fn install_document_in_store<S: BundleStore + ?Sized>(
    store: &S,
    bytes: &[u8],
) -> Result<PublishedBundle, BundleOperationError> {
    let fetched = validate_fetched_document(bytes)?;
    let content = UserBundle::new(
        BundleDocument::Skill,
        fetched.content,
        Vec::new(),
        BundleLimits::default(),
    )?;
    if let Some(existing) = store.read(&fetched.slug).await? {
        if existing.content.digest() == content.digest() {
            return Ok(PublishedBundle {
                id: fetched.slug,
                revision: existing.revision,
                digest: content.digest(),
            });
        }
        return Err(BundleOperationError::Store(BundleStoreError::Conflict {
            current: Some(existing.revision),
        }));
    }
    let revision = store.publish(&fetched.slug, None, &content).await?;
    Ok(PublishedBundle {
        id: fetched.slug,
        revision,
        digest: content.digest(),
    })
}

/// Publish compiled bundles only when no user bundle already owns the id.
pub async fn install_compiled_in_store<S: BundleStore + ?Sized>(
    store: &S,
    bundles: &[BundledSkill],
) -> Vec<Result<PublishedBundle, BundleOperationError>> {
    let mut results = Vec::with_capacity(bundles.len());
    for bundle in bundles {
        let result = (|| async {
            bundle
                .validate()
                .map_err(|_| BundleOperationError::InvalidCompiledBundle)?;
            let primary = if bundle.files.iter().any(|file| file.path == crate::SKILL_MD) {
                BundleDocument::Skill
            } else {
                BundleDocument::Workflow
            };
            let document = bundle
                .files
                .iter()
                .find(|file| file.path == primary.file_name())
                .ok_or(BundleOperationError::InvalidCompiledBundle)?
                .contents
                .to_owned();
            let resources = bundle
                .files
                .iter()
                .filter(|file| file.path != primary.file_name())
                .map(|file| BundleFile {
                    path: file.path.to_owned(),
                    bytes: file.contents.as_bytes().to_vec(),
                })
                .collect();
            let content = bundle_with_exact_limits(primary, document, resources)?;
            if let Some(existing) = store.read(bundle.dir_name).await? {
                if existing.content.digest() == content.digest() {
                    return Ok(PublishedBundle {
                        id: bundle.dir_name.to_owned(),
                        revision: existing.revision,
                        digest: content.digest(),
                    });
                }
                return Err(BundleOperationError::Store(BundleStoreError::Conflict {
                    current: Some(existing.revision),
                }));
            }
            let revision = store.publish(bundle.dir_name, None, &content).await?;
            Ok(PublishedBundle {
                id: bundle.dir_name.to_owned(),
                revision,
                digest: content.digest(),
            })
        })()
        .await;
        results.push(result);
    }
    results
}

/// Remove a bundle only at the caller's observed revision.
///
/// # Errors
///
/// Returns an error when the identifier is invalid or the store refuses the
/// conditional delete.
pub async fn remove_bundle_from_store<S: BundleStore + ?Sized>(
    store: &S,
    id: &str,
    expected: BundleRevision,
) -> Result<BundleRevision, BundleOperationError> {
    crate::authoring::validate_slug(id)?;
    Ok(store.delete(id, expected).await?)
}

fn write_staged_bundle(dir: &Path, bundle: &UserBundle) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    for file in bundle.files() {
        let path = dir.join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &file.bytes)?;
    }
    Ok(())
}

fn read_staged_bundle(
    dir: &Path,
    primary: BundleDocument,
    limits: BundleLimits,
) -> Result<UserBundle, BundleOperationError> {
    read_bundle_dir(dir, primary, limits)
}

fn validate_stored_bundle(
    bundle: &UserBundle,
    limits: BundleLimits,
) -> Result<UserBundle, BundleOperationError> {
    check_file_limits(bundle.files(), limits)?;
    let primary = bundle.primary();
    let document = std::str::from_utf8(
        bundle
            .file(primary.file_name())
            .ok_or(crate::BundleContentError::MissingDocument)?,
    )?
    .to_owned();
    let resources = bundle
        .files()
        .iter()
        .filter(|file| file.path != primary.file_name())
        .cloned()
        .collect();
    Ok(UserBundle::new(primary, document, resources, limits)?)
}

fn check_file_limits(
    files: &[BundleFile],
    limits: BundleLimits,
) -> Result<(), crate::BundleContentError> {
    if files.len() > limits.max_files {
        return Err(crate::BundleContentError::Limit("file count"));
    }
    let mut total = 0usize;
    for file in files {
        if file.path.len() > limits.max_path_bytes {
            return Err(crate::BundleContentError::Limit("path bytes"));
        }
        if file.path.split('/').count() > limits.max_depth {
            return Err(crate::BundleContentError::Limit("depth"));
        }
        if file.bytes.len() > limits.max_file_bytes {
            return Err(crate::BundleContentError::Limit("file bytes"));
        }
        total = total
            .checked_add(file.bytes.len())
            .ok_or(crate::BundleContentError::Limit("total bytes"))?;
        if total > limits.max_total_bytes {
            return Err(crate::BundleContentError::Limit("total bytes"));
        }
    }
    Ok(())
}

fn walk_bundle_files(
    root: &Path,
    limits: BundleLimits,
) -> Result<Vec<(PathBuf, String, u64)>, BundleOperationError> {
    let mut output = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut total_bytes = 0u64;
    while let Some((path, parent_depth)) = stack.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(std::io::Error::other("symlink in staged bundle").into());
            }
            let entry_path = entry.path();
            let relative = entry_path
                .strip_prefix(root)
                .map_err(std::io::Error::other)?;
            let depth = parent_depth
                .checked_add(1)
                .ok_or(crate::BundleContentError::Limit("depth"))?;
            if depth > limits.max_depth {
                return Err(crate::BundleContentError::Limit("depth").into());
            }
            if relative.as_os_str().as_encoded_bytes().len() > limits.max_path_bytes {
                return Err(crate::BundleContentError::Limit("path bytes").into());
            }
            if kind.is_dir() {
                stack.push((entry_path, depth));
            } else if kind.is_file() {
                if output.len() >= limits.max_files {
                    return Err(crate::BundleContentError::Limit("file count").into());
                }
                let metadata = entry.metadata()?;
                if metadata.len() > limits.max_file_bytes as u64 {
                    return Err(crate::BundleContentError::Limit("file bytes").into());
                }
                total_bytes = total_bytes
                    .checked_add(metadata.len())
                    .ok_or(crate::BundleContentError::Limit("total bytes"))?;
                if total_bytes > limits.max_total_bytes as u64 {
                    return Err(crate::BundleContentError::Limit("total bytes").into());
                }
                let name = relative
                    .to_str()
                    .ok_or(crate::BundleContentError::InvalidPath)?
                    .replace('\\', "/");
                output.push((entry_path, name, metadata.len()));
            }
        }
    }
    Ok(output)
}

fn read_bundle_dir(
    root: &Path,
    primary: BundleDocument,
    limits: BundleLimits,
) -> Result<UserBundle, BundleOperationError> {
    let paths = walk_bundle_files(root, limits)?;
    let preferred = primary.file_name();
    let fallback = primary.other();
    let document_path = paths
        .iter()
        .find(|(_, name, _)| name == preferred)
        .or_else(|| paths.iter().find(|(_, name, _)| name == fallback))
        .map(|(path, name, _)| (path.clone(), name.clone()))
        .ok_or(crate::BundleContentError::MissingDocument)?;
    let mut resources = Vec::new();
    let mut document = None;
    let mut total = 0usize;
    for (path, name, size) in paths {
        let allowed = limits.max_total_bytes.saturating_sub(total);
        let limit = limits.max_file_bytes.min(allowed);
        if size > limit as u64 {
            return Err(crate::BundleContentError::Limit(if allowed < limits.max_file_bytes {
                "total bytes"
            } else {
                "file bytes"
            })
            .into());
        }
        let bytes = read_bounded(&path, limit)?;
        total = total
            .checked_add(bytes.len())
            .ok_or(crate::BundleContentError::Limit("total bytes"))?;
        if path == document_path.0 {
            document = Some(String::from_utf8(bytes).map_err(std::io::Error::other)?);
        } else if name != preferred && name != fallback {
            resources.push(BundleFile { path: name, bytes });
        }
    }
    Ok(UserBundle::new(
        primary,
        document.ok_or(crate::BundleContentError::MissingDocument)?,
        resources,
        limits,
    )?)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, BundleOperationError> {
    let file = fs::File::open(path)?;
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(crate::BundleContentError::Limit("file bytes").into());
    }
    Ok(bytes)
}

fn find_legacy_bundle(
    roots: &[std::path::PathBuf],
    id: &str,
) -> std::io::Result<Option<std::path::PathBuf>> {
    for root in roots {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(std::io::Error::other("legacy root is not a real directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let canonical_root = fs::canonicalize(root)?;
        let candidate = canonical_root.join(id);
        match fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                return Ok(Some(candidate));
            }
            Ok(_) => {
                return Err(std::io::Error::other(
                    "legacy bundle is not a real directory",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

