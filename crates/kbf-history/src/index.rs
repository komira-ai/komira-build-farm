//! History's index as the writer and reader reach it: a blocking [`Store`] under an
//! [`IndexRoot`], every call run through an [`Offload`].
//!
//! [`Store`] calls block until the backend answers (a SQLite writer thread, Postgres
//! connections), and `kbf-state` asks a caller on an async runtime to call from a
//! blocking task. The writer's and reader's `async fn`s never call the store on the
//! thread that polls them: each call goes to the [`Offload`] the server chose (its
//! runtime's blocking pool), and the `async fn` waits for the answer. [`Inline`] runs
//! the call on the polling thread instead, for a caller that is off the runtime's
//! workers already and for tests.

use std::future::Future;
use std::num::NonZeroUsize;
use std::ops::Bound;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use kbf_state::{
    Commit, Conflict, Direction, ScanPage, ScanRange, Store, StoreError, Txn, Versioned,
};

use crate::keys::IndexRoot;

/// The future an [`Offload`] returns.
pub type Offloaded = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Where history's blocking index calls run.
pub trait Offload: Send + Sync {
    /// Runs `job` where blocking is allowed. The future resolves once `job` has run,
    /// or once it never will (its task was cancelled or panicked); the caller then
    /// answers with a [`StoreError`].
    fn offload(&self, job: Box<dyn FnOnce() + Send>) -> Offloaded;
}

/// Runs each job at once on the calling thread, blocking it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inline;

impl Offload for Inline {
    fn offload(&self, job: Box<dyn FnOnce() + Send>) -> Offloaded {
        job();
        Box::pin(std::future::ready(()))
    }
}

/// History's index: the state store, the root its rows live under, and where its
/// calls run.
#[derive(Clone)]
pub struct HistoryIndex {
    store: Arc<dyn Store>,
    root: IndexRoot,
    offload: Arc<dyn Offload>,
}

impl HistoryIndex {
    /// The rows under `root` in `store`, each call run by `offload`.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, root: IndexRoot, offload: Arc<dyn Offload>) -> HistoryIndex {
        HistoryIndex {
            store,
            root,
            offload,
        }
    }

    /// The root the rows live under.
    #[must_use]
    pub fn root(&self) -> &IndexRoot {
        &self.root
    }

    /// Runs `call` on the store through the offload and returns its answer.
    async fn call<R: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn Store) -> Result<R, StoreError> + Send + 'static,
    ) -> Result<R, StoreError> {
        let slot = Arc::new(Mutex::new(None));
        let (store, answer) = (Arc::clone(&self.store), Arc::clone(&slot));
        let job = Box::new(move || {
            let got = call(store.as_ref());
            *answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(got);
        });
        self.offload.offload(job).await;
        let got = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        got.unwrap_or_else(|| {
            Err(StoreError::Failed(
                "an offloaded index call never ran".into(),
            ))
        })
    }

    pub(crate) async fn get(&self, key: String) -> Result<Option<Versioned>, StoreError> {
        self.call(move |s| s.get(&key)).await
    }

    pub(crate) async fn scan_prefix(
        &self,
        prefix: String,
    ) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.call(move |s| s.scan_prefix(&prefix)).await
    }

    /// One page of at most `limit` rows under `prefix` between `lower` and `upper`, in
    /// `direction` (C8): a bounded read, unlike [`HistoryIndex::scan_prefix`].
    pub(crate) async fn scan_range(
        &self,
        prefix: String,
        bounds: (Bound<String>, Bound<String>),
        direction: Direction,
        limit: NonZeroUsize,
    ) -> Result<ScanPage, StoreError> {
        self.call(move |s| {
            let (lower, upper) = &bounds;
            s.scan_range(ScanRange {
                prefix: &prefix,
                lower: lower.as_ref().map(String::as_str),
                upper: upper.as_ref().map(String::as_str),
                direction,
                limit,
            })
        })
        .await
    }

    pub(crate) async fn txn(&self, txn: Txn) -> Result<Result<Commit, Conflict>, StoreError> {
        self.call(move |s| s.txn(txn)).await
    }
}
