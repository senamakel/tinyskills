use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use tinyskills::{
    BundleDocument, BundleFuture, BundleLimits, BundleRevision, BundleStore, BundleStoreError,
    BundleSummary, StoredBundle, UserBundle,
};

pub(crate) trait TestResultExt<T> {
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
pub(crate) struct MemoryStore {
    pub(crate) rows: Mutex<BTreeMap<String, StoredBundle>>,
    pub(crate) fail_reads: AtomicBool,
    pub(crate) fail_publishes: AtomicBool,
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

pub(crate) fn poison(
    _: std::sync::PoisonError<std::sync::MutexGuard<'_, BTreeMap<String, StoredBundle>>>,
) -> BundleStoreError {
    BundleStoreError::Backend(Box::new(std::io::Error::other("test lock poisoned")))
}

pub(crate) fn content(
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
