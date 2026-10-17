//! One read of a blob for every caller that asks for it at once.
//!
//! The first caller of a digest starts a flight, a future that tries the digest's owners
//! in order; callers that ask while it is in progress wait on it instead of asking the
//! owners again, whichever owner ends up answering. The flight is not spawned: its
//! waiters drive it, so when every waiter has gone it stops where it is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared, WeakShared};
use kbf_storage::StorageError;
use kbf_types::Digest;

/// What a flight answers every waiter: the blob, or the failure (a [`StorageError`] can
/// be cloned for each waiter).
pub(crate) type Read = Result<Bytes, StorageError>;

type Work = BoxFuture<'static, Read>;

/// A read in progress, shared by its waiters.
pub(crate) type Flight = Shared<Work>;

/// The flights in progress. Weak: a flight no waiter holds is dropped, not kept alive by
/// the table.
type Table = HashMap<Digest, WeakShared<Work>>;

/// The flights in progress, by digest.
#[derive(Default)]
pub(crate) struct Flights {
    in_progress: Mutex<Table>,
}

impl Flights {
    /// The flight of `digest`: the one in progress, or a new one running `read`.
    pub(crate) fn join(self: &Arc<Self>, digest: Digest, read: impl FnOnce() -> Work) -> Flight {
        let mut in_progress = self.lock();
        if let Some(flight) = in_progress.get(&digest).and_then(WeakShared::upgrade) {
            return flight;
        }
        let landing = Landing {
            flights: Arc::clone(self),
            digest,
        };
        let read = read();
        let flight: Flight = async move {
            // Out of the table when the read lands, or when it is dropped unfinished.
            let _landing = landing;
            read.await
        }
        .boxed()
        .shared();
        in_progress.extend(flight.downgrade().map(|weak| (digest, weak)));
        flight
    }

    /// Flights in progress.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        // Every change is one insert or one remove.
        self.in_progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Takes its flight out of the table when dropped. (When a caller starts a new flight of
/// the digest in the instant between this flight's last waiter leaving and this running,
/// that one leaves the table early: its late joiners start another read, which costs a
/// repeat and nothing else.)
struct Landing {
    flights: Arc<Flights>,
    digest: Digest,
}

impl Drop for Landing {
    fn drop(&mut self) {
        self.flights.lock().remove(&self.digest);
    }
}
