use std::{collections::BTreeMap, sync::Mutex};

use tinyskills::{
    BundleDocument, BundleFuture, BundleLimits, BundleRevision, BundleSpec, BundleStore,
    BundleStoreError, BundleSummary, BundledFile, BundledSkill, StoredBundle, UserBundle,
    install_compiled_in_store, install_document_in_store, remove_bundle_from_store,
    scaffold_bundle_in_store,
};

#[derive(Default)]
struct MemoryStore(Mutex<BTreeMap<String, StoredBundle>>);

impl BundleStore for MemoryStore {
    fn read<'a>(&'a self, id: &'a str) -> BundleFuture<'a, Option<StoredBundle>> {
        Box::pin(async move { Ok(self.0.lock().map_err(poison)?.get(id).cloned()) })
    }
    fn publish<'a>(
        &'a self,
        id: &'a str,
        expected: Option<BundleRevision>,
        content: &'a UserBundle,
    ) -> BundleFuture<'a, BundleRevision> {
        Box::pin(async move {
            let mut rows = self.0.lock().map_err(poison)?;
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
                .0
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
            let mut rows = self.0.lock().map_err(poison)?;
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
) -> UserBundle {
    UserBundle::new(
        primary,
        document.to_owned(),
        resources,
        BundleLimits::default(),
    )
    .expect("valid test bundle")
}

#[tokio::test]
async fn authoring_reads_store_content_preserves_resources_and_publishes_cas_revision() {
    let store = MemoryStore::default();
    let original = content(
        BundleDocument::Workflow,
        "---\nname: demo\ndescription: old\n---\n\nkeep body\n",
        vec![tinyskills::BundleFile {
            path: "references/guide.md".into(),
            bytes: b"reference".to_vec(),
        }],
    );
    assert_eq!(
        store.publish("demo", None, &original).await.unwrap(),
        BundleRevision(1)
    );
    let temp = tempfile::tempdir().unwrap();
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
    .await
    .unwrap();
    assert_eq!(outcome.revision, BundleRevision(2));
    let stored = store.read("demo").await.unwrap().unwrap();
    let document = std::str::from_utf8(stored.content.file("WORKFLOW.md").unwrap()).unwrap();
    assert!(document.contains("keep body"));
    assert!(document.contains("description: updated"));
    assert_eq!(
        stored.content.file("references/guide.md"),
        Some(&b"reference"[..])
    );
}

#[tokio::test]
async fn fetched_install_and_remove_use_store_revisions() {
    let store = MemoryStore::default();
    let fetched = b"---\nname: New Skill\ndescription: useful\n---\nbody\n";
    let installed = install_document_in_store(&store, fetched).await.unwrap();
    assert_eq!(installed.id, "new-skill");
    assert_eq!(
        store
            .read("new-skill")
            .await
            .unwrap()
            .unwrap()
            .content
            .file("SKILL.md"),
        Some(&fetched[..])
    );
    assert!(install_document_in_store(&store, fetched).await.is_err());
    assert_eq!(
        remove_bundle_from_store(&store, "new-skill", installed.revision)
            .await
            .unwrap(),
        BundleRevision(2)
    );
    assert!(store.read("new-skill").await.unwrap().is_none());
}

#[tokio::test]
async fn legacy_import_is_read_only_and_store_content_wins_on_edit() {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir().unwrap();
    let legacy = temp.path().join("legacy");
    std::fs::create_dir_all(legacy.join("demo/references")).unwrap();
    std::fs::write(
        legacy.join("demo/WORKFLOW.md"),
        "---\nname: demo\ndescription: old\n---\nlegacy body\n",
    )
    .unwrap();
    std::fs::write(legacy.join("demo/references/old.md"), "old resource").unwrap();
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
    .unwrap();
    assert!(
        std::fs::read_to_string(legacy.join("demo/WORKFLOW.md"))
            .unwrap()
            .contains("description: old")
    );
    assert_eq!(
        store
            .read("demo")
            .await
            .unwrap()
            .unwrap()
            .content
            .file("references/old.md"),
        Some(&b"old resource"[..])
    );
    let newer = content(
        BundleDocument::Workflow,
        "---\nname: demo\ndescription: cloud\n---\ncloud body\n",
        vec![],
    );
    store
        .publish("demo", Some(first.revision), &newer)
        .await
        .unwrap();
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
}

#[tokio::test]
async fn compiled_install_publishes_files_and_does_not_replace_user_heads() {
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
    let installed = first.into_iter().next().unwrap().unwrap();
    let persisted = store.read("builtin").await.unwrap().unwrap();
    assert_eq!(
        persisted.content.file("references/info.md"),
        Some(&b"resource"[..])
    );
    let second = install_compiled_in_store(&store, &compiled).await;
    assert!(matches!(
        second.into_iter().next().unwrap(),
        Err(tinyskills::BundleOperationError::Store(
            BundleStoreError::Conflict { current: Some(_) }
        ))
    ));
    assert_eq!(
        store.read("builtin").await.unwrap().unwrap().revision,
        installed.revision
    );
}
