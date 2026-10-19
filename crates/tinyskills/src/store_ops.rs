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
) -> Result<UserBundle, BundleOperationError> {
    let document = fs::read_to_string(dir.join(primary.file_name()))?;
    let mut resources = Vec::new();
    for entry in walkdir(dir)? {
        let relative = entry.strip_prefix(dir).map_err(std::io::Error::other)?;
        let name = relative.to_string_lossy().replace('\\', "/");
        if name == primary.file_name() || name == crate::SKILL_MD || name == crate::WORKFLOW_MD {
            continue;
        }
        resources.push(BundleFile {
            path: name,
            bytes: fs::read(entry)?,
        });
    }
    Ok(bundle_with_exact_limits(primary, document, resources)?)
}

fn bundle_with_exact_limits(
    primary: BundleDocument,
    document: String,
    resources: Vec<BundleFile>,
) -> Result<UserBundle, crate::BundleContentError> {
    let file_count = resources
        .len()
        .checked_add(1)
        .ok_or(crate::BundleContentError::Limit("file count"))?;
    let mut total = document.len();
    let mut max_file = document.len();
    let mut max_path = primary.file_name().len();
    for file in &resources {
        total = total
            .checked_add(file.bytes.len())
            .ok_or(crate::BundleContentError::Limit("total bytes"))?;
        max_file = max_file.max(file.bytes.len());
        max_path = max_path.max(file.path.len());
    }
    UserBundle::new(
        primary,
        document,
        resources,
        BundleLimits {
            max_files: file_count,
            max_file_bytes: max_file,
            max_total_bytes: total,
            max_path_bytes: max_path,
        },
    )
}

fn walkdir(root: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut output = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(std::io::Error::other("symlink in staged bundle"));
            }
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                output.push(entry.path());
            }
        }
    }
    Ok(output)
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

fn copy_legacy_bundle(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination)?;
    for path in walkdir(source)? {
        let relative = path.strip_prefix(source).map_err(std::io::Error::other)?;
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(path, target)?;
    }
    Ok(())
}
