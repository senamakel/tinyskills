//! Store-backed refusals and conflict paths.

#[path = "support/store_entrypoint_support.rs"]
mod support;

use std::sync::atomic::Ordering;
use support::{MemoryStore, TestResultExt, content, poison};
use tinyskills::{
    BundleDocument, BundleRevision, BundleSpec, BundleStore, BundleStoreError, BundledFile,
    BundledSkill, install_compiled_in_store, install_document_in_store, scaffold_bundle_in_store,
};

#[tokio::test]
async fn store_refusal_stops_authoring_before_staging_anything()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    store.fail_reads.store(true, Ordering::SeqCst);
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
    assert!(!scratch.path().join("demo").exists());
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
async fn fetched_install_conflicts_with_a_different_existing_bundle()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let current = content(
        BundleDocument::Skill,
        "---\nname: New Skill\ndescription: existing\n---\nexisting body\n",
        vec![],
    )?;
    store.publish("new-skill", None, &current).await?;
    let fetched = b"---\nname: New Skill\ndescription: fetched\n---\nfetched body\n";
    let result = install_document_in_store(&store, fetched).await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Store(
            BundleStoreError::Conflict {
                current: Some(BundleRevision(1))
            }
        ))
    ));
    let stored = store.read("new-skill").await?.ok_or("bundle missing")?;
    assert_eq!(
        stored.content.file("SKILL.md"),
        Some(&b"---\nname: New Skill\ndescription: existing\n---\nexisting body\n"[..])
    );
    Ok(())
}

#[tokio::test]
async fn compiled_workflow_install_preserves_its_primary_document()
-> Result<(), Box<dyn std::error::Error>> {
    static FILES: &[BundledFile] = &[BundledFile {
        path: "WORKFLOW.md",
        contents: "---\nname: built-in workflow\ndescription: built-in\n---\n",
    }];
    let store = MemoryStore::default();
    let compiled = [BundledSkill {
        dir_name: "built-in-workflow",
        files: FILES,
    }];
    let outcome = install_compiled_in_store(&store, &compiled)
        .await
        .into_iter()
        .next()
        .into_test_result()?
        .into_test_result()?;
    assert_eq!(outcome.id, "built-in-workflow");
    let stored = store
        .read("built-in-workflow")
        .await?
        .ok_or("bundle missing")?;
    assert_eq!(stored.content.primary(), BundleDocument::Workflow);
    Ok(())
}

#[tokio::test]
async fn stored_content_is_checked_against_each_requested_limit()
-> Result<(), Box<dyn std::error::Error>> {
    let document = "---\nname: demo\ndescription: valid\n---\nbody\n";
    let cases = [
        (
            tinyskills::BundleLimits {
                max_files: 1,
                ..Default::default()
            },
            "file count",
        ),
        (
            tinyskills::BundleLimits {
                max_path_bytes: 5,
                ..Default::default()
            },
            "path bytes",
        ),
        (
            tinyskills::BundleLimits {
                max_depth: 1,
                ..Default::default()
            },
            "depth",
        ),
        (
            tinyskills::BundleLimits {
                max_total_bytes: document.len(),
                ..Default::default()
            },
            "total bytes",
        ),
    ];
    for (limits, expected_limit) in cases {
        let store = MemoryStore::default();
        let stored = content(
            BundleDocument::Skill,
            document,
            vec![tinyskills::BundleFile {
                path: "references/guide.md".into(),
                bytes: b"resource".to_vec(),
            }],
        )?;
        store.publish("demo", None, &stored).await?;
        let scratch = tempfile::tempdir()?;
        let spec = BundleSpec {
            slug: "demo".into(),
            description: "edited".into(),
            ..BundleSpec::default()
        };
        let result = scaffold_bundle_in_store(
            &store,
            scratch.path(),
            &spec,
            &tinyskills::ScaffoldOptions {
                overwrite: true,
                limits,
                ..Default::default()
            },
            Some(BundleRevision(1)),
        )
        .await;
        assert!(matches!(
            result,
            Err(tinyskills::BundleOperationError::Content(
                tinyskills::BundleContentError::Limit(limit)
            )) if limit == expected_limit
        ));
        assert!(!scratch.path().join("demo").exists());
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_import_rejects_a_symlinked_root_before_staging()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::os::unix::fs::symlink(outside.path(), temp.path().join("legacy"))?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "new".into(),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        temp.path(),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            legacy_roots: vec![temp.path().join("legacy")],
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Io(_))
    ));
    assert!(!temp.path().join("demo").exists());
    Ok(())
}

#[tokio::test]
async fn edit_without_a_matching_legacy_bundle_creates_a_new_store_head()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir()?;
    let legacy = temp.path().join("legacy");
    std::fs::create_dir(&legacy)?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "new".into(),
        ..BundleSpec::default()
    };
    let outcome = scaffold_bundle_in_store(
        &store,
        temp.path(),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            legacy_roots: vec![legacy],
            ..Default::default()
        },
        None,
    )
    .await?;
    assert_eq!(outcome.revision, BundleRevision(1));
    assert!(store.read("demo").await?.is_some());
    Ok(())
}

#[tokio::test]
async fn legacy_import_rejects_an_invalid_primary_document_before_staging()
-> Result<(), Box<dyn std::error::Error>> {
    let store = MemoryStore::default();
    let temp = tempfile::tempdir()?;
    let legacy = temp.path().join("legacy/demo");
    std::fs::create_dir_all(&legacy)?;
    std::fs::write(legacy.join("SKILL.md"), [0xff, 0xfe])?;
    let spec = BundleSpec {
        slug: "demo".into(),
        description: "edit".into(),
        ..BundleSpec::default()
    };
    let result = scaffold_bundle_in_store(
        &store,
        temp.path(),
        &spec,
        &tinyskills::ScaffoldOptions {
            overwrite: true,
            legacy_roots: vec![temp.path().join("legacy")],
            ..Default::default()
        },
        None,
    )
    .await;
    assert!(matches!(
        result,
        Err(tinyskills::BundleOperationError::Io(_))
    ));
    assert!(!temp.path().join("demo").exists());
    assert!(store.read("demo").await?.is_none());
    Ok(())
}
