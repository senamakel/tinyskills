#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use tinystoragedrivers_core::{
    CollectionSpec, Item, MemoryStorage, MigrateOptions, Precondition, Registry, Scope,
    ScopedStorage, StorageBackend, WriteOptions,
};

use super::*;
use crate::DocumentCatalogStore;
use std::sync::Arc;
use tinyskills::{CatalogStore, Validators};

fn scoped() -> ScopedStorage {
    MemoryStorage::new().for_scope(&Scope::local()).unwrap()
}

fn registry() -> Registry {
    Registry::compose([storage_schema()]).unwrap()
}

fn v1_body() -> Value {
    json!({
        "format": 1,
        "entries": [{
            "entry": {
                "id": "a", "name": "a", "description": "", "source": "", "category": "",
                "author": null, "version": null, "tags": [], "platforms": [],
                "download_url": "", "source_url": null, "docs_path": null,
                "commands": [], "env_vars": [], "license": null
            },
            "overview": "o",
            "category_label": null, "fixed_category": null, "install_identifier": null
        }],
        "fetched_at": 42,
        "validators": {"etag": "\"e\"", "last_modified": null},
        "skipped": 3
    })
}

async fn seed_v1(storage: &ScopedStorage, id: &str, body: Value, created_ms: i64) {
    let documents = storage.documents();
    documents
        .ensure_collection(&CollectionSpec::new(CATALOG_V1_COLLECTION))
        .await
        .unwrap();
    documents
        .put_with(
            CATALOG_V1_COLLECTION,
            id,
            body,
            Precondition::None,
            WriteOptions::new().created_ms(created_ms),
        )
        .await
        .unwrap();
}

#[test]
fn schema_declares_the_v2_items_under_the_skills_owner() {
    let registry = registry();
    assert_eq!(registry.owners().collect::<Vec<_>>(), ["skills"]);
    assert_eq!(registry.collection_version(CATALOG_COLLECTION), Some(2));
    assert_eq!(CATALOG_COLLECTION, "skills.catalogs");
    let schema = storage_schema();
    let names: Vec<_> = schema.items.iter().map(Item::name).collect();
    assert_eq!(
        names,
        ["skills.catalogs", "skills/bundles", "skills.run_logs"]
    );
    assert!(
        schema
            .items
            .iter()
            .all(|item| item.scope() == ScopeClass::Tenant)
    );
}

#[tokio::test]
async fn catalog_step_rewrites_a_v1_row_into_the_v2_body() {
    let storage = scoped();
    seed_v1(&storage, "example", v1_body(), 7).await;
    let registry = registry();
    assert!(registry.check_ready(CATALOG_COLLECTION).is_err());

    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .unwrap();
    registry.check_ready(CATALOG_COLLECTION).unwrap();

    let found = storage
        .documents()
        .get(CATALOG_COLLECTION, "example")
        .await
        .unwrap()
        .unwrap();
    let mut expected = v1_body();
    let object = expected.as_object_mut().unwrap();
    object.remove("format");
    object.remove("fetched_at");
    object.insert("fetched_ms".to_owned(), json!(42_000));
    assert_eq!(found.doc, expected);
    assert_eq!(found.meta.schema, 2);
    assert_eq!(found.meta.created_ms, 7);
    assert!(
        storage
            .documents()
            .get(CATALOG_V1_COLLECTION, "example")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn catalog_step_drops_rows_that_do_not_read_as_a_catalog() {
    let storage = scoped();
    seed_v1(&storage, "future", json!({"format": 9, "entries": []}), 1).await;
    seed_v1(&storage, "broken", json!({"format": 1, "entries": "x"}), 1).await;
    seed_v1(&storage, "good", v1_body(), 1).await;

    registry()
        .migrate(&storage, &MigrateOptions::new())
        .await
        .unwrap();

    let documents = storage.documents();
    assert!(
        documents
            .get(CATALOG_COLLECTION, "future")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        documents
            .get(CATALOG_COLLECTION, "broken")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        documents
            .get(CATALOG_COLLECTION, "good")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn migration_is_idempotent() {
    let storage = scoped();
    seed_v1(&storage, "example", v1_body(), 7).await;
    let registry = registry();
    registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .unwrap();
    let first = storage
        .documents()
        .get(CATALOG_COLLECTION, "example")
        .await
        .unwrap();
    let report = registry
        .migrate(&storage, &MigrateOptions::new())
        .await
        .unwrap();
    assert_eq!(report.ran.len(), 0);
    assert_eq!(
        storage
            .documents()
            .get(CATALOG_COLLECTION, "example")
            .await
            .unwrap(),
        first
    );
}

#[tokio::test]
async fn a_migrated_row_loads_through_the_catalog_store() {
    let storage = scoped();
    seed_v1(&storage, "example", v1_body(), 7).await;
    registry()
        .migrate(&storage, &MigrateOptions::new())
        .await
        .unwrap();

    let store = DocumentCatalogStore::new(Arc::clone(storage.documents()));
    let loaded = store.load("example").await.unwrap().unwrap();
    assert_eq!(loaded.fetched_at, 42);
    assert_eq!(loaded.skipped, 3);
    assert_eq!(loaded.entries.len(), 1);
    let mut validators = Validators::default();
    validators.etag = Some("\"e\"".to_owned());
    assert_eq!(loaded.validators, validators);
}
