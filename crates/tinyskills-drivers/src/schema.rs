//! The `skills` owner's storage schema: what `tinyskills-drivers` keeps in the
//! host's storage, and how the v1 shapes become the v2 ones.
//!
//! | v1 | v2 | scope |
//! |---|---|---|
//! | collection `tinyskills_catalogs` | collection `skills.catalogs` (schema 2) | tenant |
//! | blobs `<host prefix>/<bundle id>/…` | blobs `skills/bundles/<bundle id>/…` | tenant |
//! | — | streams `skills/run_logs/<run id>` | tenant |
//!
//! The host composes [`storage_schema`] into its `Registry`, which declares
//! the items, derives the module grant and runs the steps.

use tinyskills::StoredCatalog;
use tinystoragedrivers_core::{Key, Owner, Schema, ScopeClass, SourceRecord, Step, TargetRecord};

use crate::blob_bundles::BUNDLE_PREFIX;
use crate::document_store::CatalogBody;
use crate::stream_run_log::RUN_LOG_PREFIX;

/// The collection that holds one document per registry catalog.
pub const CATALOG_COLLECTION: &str = "skills.catalogs";

/// The v1 name of [`CATALOG_COLLECTION`].
pub(crate) const CATALOG_V1_COLLECTION: &str = "tinyskills_catalogs";

/// The body version of [`CATALOG_COLLECTION`]; version 1 is the legacy shape.
pub(crate) const CATALOG_SCHEMA: u32 = 2;

/// Everything `tinyskills-drivers` stores, as the owner `skills`: the catalog
/// collection, bundle blob prefix, and run-log stream prefix are tenant scoped,
/// plus the step that moves `tinyskills_catalogs` into `skills.catalogs`.
///
/// # Panics
///
/// Never in practice: the owner name is a constant that passes
/// `Owner::new`.
#[must_use]
pub fn storage_schema() -> Schema {
    #[allow(clippy::expect_used)]
    let owner = Owner::new("skills").expect("`skills` is a valid owner name");
    Schema::new(owner)
        .collection(CATALOG_COLLECTION, CATALOG_SCHEMA, ScopeClass::Tenant)
        .blobs(BUNDLE_PREFIX, ScopeClass::Tenant)
        .stream(RUN_LOG_PREFIX, ScopeClass::Tenant)
        .step(catalogs_step())
}

fn catalogs_step() -> Step {
    Step::new(
        "catalogs_v2",
        CATALOG_COLLECTION,
        1,
        CATALOG_SCHEMA,
        |batch| Ok(batch.into_iter().filter_map(catalog_v2).collect()),
    )
    .source(CATALOG_V1_COLLECTION)
}

/// One v1 catalog row as its v2 record. A row that is not a catalog this
/// version can read (another `format`, or a body that does not decode) is
/// dropped: it already read as absent or corrupt, and a catalog is a cache the
/// registry fetches again.
fn catalog_v2(record: SourceRecord) -> Option<TargetRecord> {
    let stored: StoredCatalog = serde_json::from_value(record.body).ok()?;
    if stored.format != StoredCatalog::FORMAT {
        return None;
    }
    let body = serde_json::to_value(CatalogBody::from_stored(&stored)).ok()?;
    let id = Key::of([record.id]).ok()?;
    Some(
        TargetRecord::new(CATALOG_COLLECTION, id.as_str(), body)
            .created_ms(record.meta.created_ms)
            .expires_ms(record.meta.expires_ms),
    )
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
