//! Store-backed authoring, install, import, and removal behavior.

use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tinyskills::{
    BundleDocument, BundleFuture, BundleLimits, BundleRevision, BundleSpec, BundleStore,
    BundleStoreError, BundleSummary, BundledFile, BundledSkill, StoredBundle, UserBundle,
    install_compiled_in_store, install_document_in_store, remove_bundle_from_store,
    scaffold_bundle_in_store,
};

trait TestResultExt<T> {
    fn into_test_result(self) -> Result<T, Box<dyn std::error::Error>>;
}

impl<T, E: std::error::Error + 'static> TestResultExt<T> for Result<T, E> {
    fn into_test_result(self) -> Result<T, Box<dyn std::error::Error>> {
        self.map_err(Into::into)
    }
}

impl<T> TestResultExt<T> for Option<T> {
    fn into_test_result(self) -> Result<T, Box<dyn std::error::Error>> {
        self.ok_or_else(|| "expected an item".into())
    }
}

#[derive(Default)]
struct MemoryStore {
    rows: Mutex<BTreeMap<String, StoredBundle>>,
    fail_reads: AtomicBool,
    fail_publishes: AtomicBool,
}

impl BundleStore for MemoryStore {
    fn read<'a>(&'a self, id: &'a str) -> BundleFuture<'a, Option<StoredBundle>> {
        Box::pin(async move {
            if self.fail_reads.load(Ordering::SeqCst) {
                return Err(BundleStoreError::Backend(Box::new(std::io::Error::other(
                    "injected read refusal",
                ))));
            }
            Ok(self.rows.lock().map_err(poison)?.get(id).cloned())
        })
    }
    fn publish<'a>(
        &'a self,
        id: &'a str,
        expected: Option<BundleRevision>,
        content: &'a UserBundle,
    ) -> BundleFuture<'a, BundleRevision> {
        Box::pin(async move {
            if self.fail_publishes.load(Ordering::SeqCst) {
                return Err(BundleStoreError::Backend(Box::new(std::io::Error::other(
                    "injected publish refusal",
                ))));
            }
            let mut rows = self.rows.lock().map_err(poison)?;
            let current = rows.get(id).map(|row| row.revision);
            if current != expected {
                return Err(BundleStoreError::Conflict { current });
            }
            let revision = BundleRevision(current.map_or(1, |value| value.0 + 1));
            rows.insert(
                id.to_owned(),
                StoredBundle {
                    revision,
                    content: content.clone(),
                },
            );
            Ok(revision)
        })
    }
    fn list<'a>(
        &'a self,
        after: Option<&'a str>,
        limit: usize,
    ) -> BundleFuture<'a, Vec<BundleSummary>> {
        Box::pin(async move {
            Ok(self
                .rows
                .lock()
                .map_err(poison)?
                .iter()
                .filter(|(id, _)| after.is_none_or(|cursor| id.as_str() > cursor))
                .take(limit)
                .map(|(id, row)| BundleSummary {
                    id: id.clone(),
                    revision: row.revision,
                    primary: row.content.primary(),
                    digest: row.content.digest(),
                })
                .collect())
        })
    }
    fn delete<'a>(
        &'a self,
        id: &'a str,
        expected: BundleRevision,
    ) -> BundleFuture<'a, BundleRevision> {
        Box::pin(async move {
            let mut rows = self.rows.lock().map_err(poison)?;
            let current = rows.get(id).map(|row| row.revision);
            if current != Some(expected) {
                return Err(BundleStoreError::Conflict { current });
            }
            rows.remove(id);
            Ok(BundleRevision(expected.0 + 1))
        })
    }
}

fn poison(
    _: std::sync::PoisonError<std::sync::MutexGuard<'_, BTreeMap<String, StoredBundle>>>,
) -> BundleStoreError {
    BundleStoreError::Backend(Box::new(std::io::Error::other("test lock poisoned")))
}

fn content(
    primary: BundleDocument,
    document: &str,
    resources: Vec<tinyskills::BundleFile>,
) -> Result<UserBundle, tinyskills::BundleContentError> {
    UserBundle::new(
        primary,
        document.to_owned(),
        resources,
        BundleLimits::default(),
    )
}

#[tokio::test]
async fn authoring_reads_store_content_preserves_resources_and_publishes_cas_revision()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let original = content(
        BundleDocument::Workflow,
        "---\nname: demo\ndescription: old\n---\n\nkeep body\n",
        vec![tinyskills::BundleFile {
            path: "references/guide.md".into(),
            bytes: b"reference".to_vec(),
        }],
    )?;
    assert_eq!(
        store.publish("demo", None, &original).await?,
        BundleRevision(1)
    );
    let temp = tempfile::tempdir()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "updated".into(),
        ..BundleSpec::default()
    };
    let outcome = scaffold_bundle_in_store(
        &store,
        temp.path(),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            ..Default::default()
        },
        Some(BundleRevision(1)),
    )
    .await?;
    assert_eq!(outcome.revision, BundleRevision(2));
    let stored = store.read("demo").await?.ok_or("expected stored bundle")?;
    let document = std::str::from_utf8(
        stored
            .content
            .file("WORKFLOW.md")
            .ok_or("missing workflow document")?,
    )?;
    assert!(document.contains("keep body"));
    assert!(document.contains("description: updated"));
    assert_eq!(
        stored.content.file("references/guide.md"),
        Some(&b"reference"[..])
    );
    Ok(())
}

#[tokio::test]
async fn fetched_install_and_remove_use_store_revisions() -> Result<(), Box<dyn std::error::Error>>
{
    let store = MemoryStore::default();
    let fetched = b"---\nname: New Skill\ndescription: useful\n---\nbody\n";
    let installed = install_document_in_store(&store, fetched)
        .await
        .into_test_result()?;
    assert_eq!(installed.id, "new-skill");
    assert_eq!(
        store
            .read("new-skill")
            .await
            .into_test_result()?
            .into_test_result()?
            .content
            .file("SKILL.md"),
        Some(&fetched[..])
    );
    assert_eq!(
        install_document_in_store(&store, fetched)
            .await
            .into_test_result()?
            .revision,
        installed.revision
    );
    assert_eq!(
        remove_bundle_from_store(&store, "new-skill", installed.revision)
            .await
            .into_test_result()?,
        BundleRevision(2)
    );
    assert!(matches!(
        remove_bundle_from_store(&store, "new-skill", installed.revision).await,
        Err(tinyskills::BundleOperationError::Store(
            BundleStoreError::Conflict { current: None }
        ))
    ));
    assert!(store.read("new-skill").await.into_test_result()?.is_none());
    Ok(())
}

#[tokio::test]
async fn legacy_import_is_read_only_and_store_content_wins_on_edit()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir().into_test_result()?;
    let legacy = temp.path().join("legacy");
    std::fs::create_dir_all(legacy.join("demo/references")).into_test_result()?;
    std::fs::write(
        legacy.join("demo/WORKFLOW.md"),
        "---\nname: demo\ndescription: old\n---\nlegacy body\n",
    )
    .into_test_result()?;
    std::fs::write(legacy.join("demo/references/old.md"), "old resource").into_test_result()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "new".into(),
        ..Default::default()
    };
    let options = tinyskills::ScaffoldOptions {
        overwrite: true,
        legacy_roots: vec![legacy.clone()],
        ..Default::default()
    };
    let first = scaffold_bundle_in_store(
        &store,
        &temp.path().join("stage-one"),
        &spec,
        &options,
        None,
    )
    .await
    .into_test_result()?;
    assert!(
        std::fs::read_to_string(legacy.join("demo/WORKFLOW.md"))
            .into_test_result()?
            .contains("description: old")
    );
    assert_eq!(
        store
            .read("demo")
            .await
            .into_test_result()?
            .into_test_result()?
            .content
            .file("references/old.md"),
        Some(&b"old resource"[..])
    );
    let newer = content(
        BundleDocument::Workflow,
        "---\nname: demo\ndescription: cloud\n---\ncloud body\n",
        vec![],
    )?;
    store
        .publish("demo", Some(first.revision), &newer)
        .await
        .into_test_result()?;
    let second = scaffold_bundle_in_store(
        &store,
        &temp.path().join("stage-two"),
        &spec,
        &options,
        Some(BundleRevision(1)),
    )
    .await;
    assert!(matches!(
        second,
        Err(tinyskills::BundleOperationError::Store(
            BundleStoreError::Conflict {
                current: Some(BundleRevision(2))
            }
        ))
    ));
    Ok(())
}

#[tokio::test]
async fn compiled_install_publishes_files_and_does_not_replace_user_heads()
-> Result<(), Box<dyn std::error::Error>> {
    static FILES: &[BundledFile] = &[
        BundledFile {
            path: "SKILL.md",
            contents: "---\nname: builtin\ndescription: built in\n---\n",
        },
        BundledFile {
            path: "references/info.md",
            contents: "resource",
        },
    ];
    let store = MemoryStore::default();
    let compiled = [BundledSkill {
        dir_name: "builtin",
        files: FILES,
    }];
    let first = install_compiled_in_store(&store, &compiled).await;
    let installed = first
        .into_iter()
        .next()
        .into_test_result()?
        .into_test_result()?;
    let persisted = store
        .read("builtin")
        .await
        .into_test_result()?
        .into_test_result()?;
    assert_eq!(
        persisted.content.file("references/info.md"),
        Some(&b"resource"[..])
    );
    let second = install_compiled_in_store(&store, &compiled).await;
    assert_eq!(
        second
            .into_iter()
            .next()
            .into_test_result()?
            .into_test_result()?,
        installed
    );
    let newer = content(
        BundleDocument::Skill,
        "---\nname: builtin\ndescription: changed\n---\nuser version\n",
        vec![],
    )?;
    store
        .publish("builtin", Some(installed.revision), &newer)
        .await
        .into_test_result()?;
    let third = install_compiled_in_store(&store, &compiled).await;
    assert!(matches!(
        third.into_iter().next().into_test_result()?,
        Err(tinyskills::BundleOperationError::Store(
            BundleStoreError::Conflict {
                current: Some(BundleRevision(2))
            }
        ))
    ));
    assert_eq!(
        store
            .read("builtin")
            .await
            .into_test_result()?
            .into_test_result()?
            .revision,
        BundleRevision(2)
    );
    Ok(())
}

#[tokio::test]
async fn store_refusal_stops_authoring_before_staging_anything()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    store.fail_reads.store(true, Ordering::SeqCst);
    let temp = tempfile::tempdir().into_test_result()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "new".into(),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        temp.path(),
        &spec,
        &tinyskills::ScaffoldOptions::default(),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Store(_))
    ));
    assert!(!temp.path().join("demo").exists());
    Ok(())
}

#[tokio::test]
async fn publish_refusal_leaves_only_caller_owned_staging_content()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    store.fail_publishes.store(true, Ordering::SeqCst);
    let scratch = tempfile::tempdir()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "new".into(),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        scratch.path(),
        &spec,
        &tinyskills::ScaffoldOptions::default(),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Store(_))
    ));
    assert!(scratch.path().join("demo/WORKFLOW.md").is_file());
    assert!(store.rows.lock().map_err(poison)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn authoring_rejects_invalid_description_before_store_or_staging()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let scratch = tempfile::tempdir()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "\n".repeat(800),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        scratch.path(),
        &spec,
        &tinyskills::ScaffoldOptions::default(),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Authoring(_))
    ));
    assert!(!scratch.path().join("demo").exists());
    assert!(store.rows.lock().map_err(poison)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn fetched_install_rejects_an_invalid_document_before_store_access() {
    let store = MemoryStore::default();
    let result = install_document_in_store(&store, b"not a skill document").await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Document(_))
    ));
    assert!(store.rows.lock().is_ok_and(|rows| rows.is_empty()));
}

#[tokio::test]
async fn stale_authoring_revision_conflicts_before_creating_staging_content()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let current = content(
        BundleDocument::Workflow,
        "---\nname: demo\ndescription: current\n---\ncurrent body\n",
        vec![],
    )?;
    store.publish("demo", None, &current).await?;
    let scratch = tempfile::tempdir()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "stale edit".into(),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        scratch.path(),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            ..Default::default()
        },
        Some(BundleRevision(0)),
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Store(
            BundleStoreError::Conflict {
                current: Some(BundleRevision(1))
            }
        ))
    ));
    assert!(!scratch.path().join("demo").exists());
    Ok(())
}

#[tokio::test]
async fn authoring_refuses_existing_caller_staging_path() -> Result<(), Box<dyn std::error::Error>>
{
    let store = MemoryStore::default();
    let scratch = tempfile::tempdir()?;
    let stage = scratch.path().join("demo");
    std::fs::create_dir(&stage)?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "new".into(),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        scratch.path(),
        &spec,
        &tinyskills::ScaffoldOptions::default(),
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Io(error))
            if error.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert!(std::fs::read_dir(&stage)?.next().is_none());
    Ok(())
}

#[tokio::test]
async fn remove_rejects_an_unsafe_bundle_id_before_store_access()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let result = remove_bundle_from_store(&store, "../escape", BundleRevision(1)).await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Authoring(_))
    ));
    Ok(())
}

#[tokio::test]
async fn compiled_install_rejects_a_bundle_without_its_document()
-> Result<(), Box<dyn std::error::Error>> {
    static FILES: &[BundledFile] = &[BundledFile {
        path: "references/info.md",
        contents: "resource",
    }];
    let store = MemoryStore::default();
    let compiled = [BundledSkill {
        dir_name: "missing-document",
        files: FILES,
    }];
    let outcomes = install_compiled_in_store(&store, &compiled).await;
    assert!(matches!(
        outcomes.into_iter().next(),
        Some(Err(tinyskills::BundleOperationError::InvalidCompiledBundle))
    ));
    assert!(store.rows.lock().map_err(poison)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn authoring_rejects_traversal_before_writing_beyond_scratch()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let existing = content(
        BundleDocument::Workflow,
        "---\nname: escape\ndescription: stored\n---\nstored body\n",
        vec![],
    )?;
    store
        .publish("../escape", None, &existing)
        .await
        .into_test_result()?;
    let temp = tempfile::tempdir().into_test_result()?;
    let scratch = temp.path().join("scratch");
    let spec = BundleSpec {
        slug: "../escape".into(),
        description: "changed".into(),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        &scratch,
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            ..Default::default()
        },
        Some(BundleRevision(1)),
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Authoring(
            tinyskills::AuthoringError::InvalidSlug { .. }
        ))
    ));
    assert!(!temp.path().join("escape/WORKFLOW.md").exists());
    Ok(())
}

#[tokio::test]
async fn legacy_import_rejects_oversized_files_before_copying_them()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir().into_test_result()?;
    let legacy = temp.path().join("legacy");
    let bundle_dir = legacy.join("demo");
    std::fs::create_dir_all(bundle_dir.join("scripts")).into_test_result()?;
    std::fs::write(
        bundle_dir.join("WORKFLOW.md"),
        "---\nname: demo\ndescription: demo\n---\nbody\n",
    )
    .into_test_result()?;
    std::fs::File::create(bundle_dir.join("scripts/oversized.bin"))
        .into_test_result()?
        .set_len((BundleLimits::default().max_file_bytes + 1) as u64)
        .into_test_result()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "updated".into(),
        ..Default::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        &temp.path().join("scratch"),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            legacy_roots: vec![legacy],
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Content(
            tinyskills::BundleContentError::Limit("file bytes")
        ))
    ));
    assert!(
        !temp
            .path()
            .join("scratch/demo/scripts/oversized.bin")
            .exists(),
        "the oversized legacy file must not be copied to scratch"
    );
    assert!(store.read("demo").await.into_test_result()?.is_none());
    Ok(())
}

#[tokio::test]
async fn legacy_import_rejects_too_deep_trees_before_copying_them()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir().into_test_result()?;
    let legacy = temp.path().join("legacy");
    let bundle_dir = legacy.join("demo");
    std::fs::create_dir_all(&bundle_dir).into_test_result()?;
    std::fs::write(
        bundle_dir.join("WORKFLOW.md"),
        "---\nname: demo\ndescription: demo\n---\nbody\n",
    )
    .into_test_result()?;
    let deep = (0..65).fold(bundle_dir, |path, _| path.join("d"));
    std::fs::create_dir_all(&deep).into_test_result()?;
    std::fs::write(deep.join("leaf.txt"), "leaf").into_test_result()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "updated".into(),
        ..Default::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        &temp.path().join("scratch"),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            legacy_roots: vec![legacy],
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Content(
            tinyskills::BundleContentError::Limit("depth")
        ))
    ));
    assert!(
        !temp.path().join("scratch/demo").exists(),
        "the deep legacy tree must be rejected before any scratch copy"
    );
    assert!(store.read("demo").await.into_test_result()?.is_none());
    Ok(())
}

#[tokio::test]
async fn legacy_import_rejects_too_many_empty_entries_before_staging()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir().into_test_result()?;
    let legacy = temp.path().join("legacy/demo");
    std::fs::create_dir_all(&legacy).into_test_result()?;
    std::fs::write(
        legacy.join("SKILL.md"),
        "---\nname: demo\ndescription: demo\n---\nbody\n",
    )
    .into_test_result()?;
    for index in 0..7 {
        std::fs::create_dir(legacy.join(format!("empty-{index}"))).into_test_result()?;
    }
    let limits = BundleLimits {
        max_files: 2,
        max_depth: 3,
        ..BundleLimits::default()
    };
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "updated".into(),
        ..Default::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        &temp.path().join("scratch"),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            legacy_roots: vec![temp.path().join("legacy")],
            limits,
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Content(
            tinyskills::BundleContentError::Limit("entry count")
        ))
    ));
    assert!(!temp.path().join("scratch/demo").exists());
    assert!(store.read("demo").await.into_test_result()?.is_none());
    Ok(())
}

#[tokio::test]
async fn stored_bundle_over_the_requested_budget_is_rejected_before_staging()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let default_limits = BundleLimits::default();
    let oversized = UserBundle::new(
        BundleDocument::Workflow,
        "---\nname: demo\ndescription: stored\n---\nbody\n".into(),
        vec![tinyskills::BundleFile {
            path: "references/large.bin".into(),
            bytes: vec![b'x'; default_limits.max_file_bytes + 1],
        }],
        BundleLimits {
            max_files: 2,
            max_file_bytes: default_limits.max_file_bytes + 1,
            max_total_bytes: default_limits.max_total_bytes,
            max_path_bytes: default_limits.max_path_bytes,
            max_depth: default_limits.max_depth,
        },
    )
    .into_test_result()?;
    store
        .publish("demo", None, &oversized)
        .await
        .into_test_result()?;
    let temp = tempfile::tempdir().into_test_result()?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "updated".into(),
        ..Default::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        temp.path(),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            ..Default::default()
        },
        Some(BundleRevision(1)),
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Content(
            tinyskills::BundleContentError::Limit("file bytes")
        ))
    ));
    assert!(!temp.path().join("demo").exists());
    assert_eq!(
        store
            .read("demo")
            .await
            .into_test_result()?
            .into_test_result()?
            .revision,
        BundleRevision(1)
    );
    Ok(())
}
