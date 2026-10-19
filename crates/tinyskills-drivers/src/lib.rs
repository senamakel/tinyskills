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
//! - [`DocumentBundleStore`]: CAS-protected user-bundle heads referencing
//!   immutable generations under [`BUNDLE_GENERATION_PREFIX`], separate from
//!   the deletable [`BUNDLE_PREFIX`] tree.
//! - [`storage_schema`]: the owner `skills` declaration of the catalog and
//!   bundle-head collections, both blob prefixes, the run-log stream prefix,
//!   and catalog migration steps,
//!   for the host's `Registry::compose`.

mod blob_bundles;
mod bundle_store;
mod document_store;
mod schema;
mod stream_run_log;

pub use blob_bundles::{BUNDLE_GENERATION_PREFIX, BUNDLE_PREFIX, BlobBundleError, BlobBundles};
pub use bundle_store::{BUNDLE_COLLECTION, DocumentBundleStore};
pub use document_store::DocumentCatalogStore;
pub use schema::{CATALOG_COLLECTION, storage_schema};
pub use stream_run_log::{RUN_LOG_PREFIX, StreamRunLogStore};
