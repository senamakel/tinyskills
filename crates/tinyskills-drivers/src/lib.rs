//! `tinyskills` on the host's `tinystoragedrivers` ports.
//!
//! The crate links the storage ports only, never a database client: the host
//! picks the backend (`SQLite`, `MongoDB`, memory) and the tenant scope when it
//! builds the handles, and passes them in.
//!
//! - [`DocumentCatalogStore`]: fetched registry catalogs as documents, a
//!   [`CatalogStore`](tinyskills::CatalogStore) for
//!   [`SkillRegistry`](tinyskills::SkillRegistry).
//! - [`BlobBundles`]: skill bundle files as a blob tree, materialized to a
//!   cache directory for discovery with [`BlobBundles::materialize`].
//! - [`StreamRunLogStore`]: one append-only stream per run, with tenant scope
//!   bound into opaque cursors.
//! - [`storage_schema`]: the owner `skills` declaration of (the catalog
//!   collection, the bundle blob prefix, and the run-log stream prefix) and the
//!   v1 to v2 migration steps, for the host's `Registry::compose`.

mod blob_bundles;
mod document_store;
mod schema;
mod stream_run_log;

pub use blob_bundles::{BUNDLE_PREFIX, BlobBundleError, BlobBundles};
pub use document_store::DocumentCatalogStore;
pub use schema::{CATALOG_COLLECTION, storage_schema};
pub use stream_run_log::{RUN_LOG_PREFIX, StreamRunLogStore};
