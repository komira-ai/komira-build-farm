//! Repair: the debt a chain write leaves when the replica could not take its copy
//! (`docs/design/architecture-v2.md`, section 8: with the replica down, the primary
//! acknowledges after its own durable write and records `repair/<digest>`).
//!
//! A [`RepairRecord`] says "this node holds a blob that a second owner does not". The
//! record is the whole difference between a one-copy write that is acknowledged and one
//! that is a silent loss of redundancy, so it is kept in the [`Journal`]: a file on the
//! node's own disk, made durable before the write is acknowledged and kept until the
//! second copy exists. [`Repairs`] is the journal behind an async interface and the seam to
//! the frontend ([`RepairLedger`]); [`RepairWorker`] makes the copies owed, to the owners
//! the map names now, when they can take them, and only then settles the debt.

mod journal;
mod ledger;
#[cfg(test)]
mod tests;
mod worker;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

pub use journal::{JOURNAL, JOURNAL_TMP, Journal, JournalError, Options};
use kbf_map::MemberId;
use kbf_store::Fs;
use kbf_types::Digest;
pub use ledger::{NoLedger, RepairLedger};
pub use worker::{Pass, RepairConfig, RepairWorker};

use crate::{Storage, StorageError};

/// A blob this node holds that another owner does not yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepairRecord {
    /// The blob.
    pub digest: Digest,
    /// The owner that missed it. Advisory: whoever the owners are when the repair runs
    /// is who gets the copy, because the map may have turned since.
    pub replica: MemberId,
    /// The map generation the write was acknowledged under.
    pub generation: u64,
}

/// What [`Repairs`] needs of a journal, so it need not be generic over the disk it is on.
trait Debts: Send {
    fn owed(&mut self, record: RepairRecord) -> Result<(), JournalError>;
    fn done(&mut self, digest: &Digest) -> Result<(), JournalError>;
    fn records(&self) -> Vec<RepairRecord>;
}

impl<F: Fs + Send> Debts for Journal<F>
where
    F::File: Send,
{
    fn owed(&mut self, record: RepairRecord) -> Result<(), JournalError> {
        Journal::owed(self, record)
    }

    fn done(&mut self, digest: &Digest) -> Result<(), JournalError> {
        Journal::done(self, digest)
    }

    fn records(&self) -> Vec<RepairRecord> {
        Journal::records(self).copied().collect()
    }
}

/// This node's own store did not say whether it holds a blob.
#[derive(Debug, thiserror::Error)]
#[error("the store did not say whether it holds the blob within {after:?}")]
struct StoreNoAnswer {
    after: Duration,
}

/// The writes whose debt is journalled and whose blob is not yet committed here.
#[derive(Debug, Default)]
struct InFlight {
    /// How many writes of each blob are between [`Repairs::owe`] and the end of their
    /// commit.
    digests: BTreeMap<Digest, usize>,
    /// How many writes have ever begun owing: a worker that saw this unchanged since before
    /// it read the store knows no write began owing meanwhile.
    begun: u64,
}

fn in_flight(state: &Mutex<InFlight>) -> MutexGuard<'_, InFlight> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A write that owes a copy and has not yet committed its blob here ([`Repairs::owe`]).
/// While it exists the blob may be absent from this node's store without having been
/// evicted, so [`RepairWorker`] neither repairs nor drops its debt. Dropped when the
/// commit has finished or the write has been abandoned.
#[derive(Debug)]
#[must_use = "dropping it says the write is over"]
pub struct Pending {
    state: Arc<Mutex<InFlight>>,
    digest: Digest,
}

impl Pending {
    fn begin(state: &Arc<Mutex<InFlight>>, digest: Digest) -> Self {
        let mut flight = in_flight(state);
        *flight.digests.entry(digest).or_default() += 1;
        flight.begun += 1;
        Self {
            state: Arc::clone(state),
            digest,
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        let mut flight = in_flight(&self.state);
        match flight.digests.get_mut(&self.digest) {
            Some(count) if *count > 1 => *count -= 1,
            _ => {
                flight.digests.remove(&self.digest);
            }
        }
    }
}

/// The copies this node owes: the journal on its disk, and the seam through which a
/// frontend is told ([`RepairLedger`]).
///
/// **The rule this exists for.** A write is answered with one copy only after
/// [`Repairs::owe`] has returned: the record is on the disk. The frontend is told after
/// that, and never waited for.
///
/// **The window.** The record is journalled before the blob is committed here (so a blob
/// present here is replicated or owed), which makes a record for a blob that is absent for
/// a moment. The [`Pending`] that [`Repairs::owe`] returns marks it: a debt is dropped for
/// an absent blob ([`Repairs::settle_evicted`]) only when no write is in that window.
pub struct Repairs {
    debts: Arc<Mutex<Box<dyn Debts>>>,
    in_flight: Arc<Mutex<InFlight>>,
    /// Held by [`Repairs::owe`] while it journals, and by [`Repairs::settle_evicted`] from
    /// its look at the in-flight writes to the debt it drops: no debt is added between the
    /// two, so a debt is dropped only on what the store held after every write that had
    /// finished committing. The guard that goes to the journal call is the blocking call's
    /// own, so it is held until the call has written even if the settle is cancelled.
    gate: Arc<tokio::sync::Mutex<()>>,
    ledger: Arc<dyn RepairLedger>,
}

impl Repairs {
    /// The debts in `fs`'s directory (creating the journal), reporting to `ledger`.
    ///
    /// # Errors
    /// The journal cannot be opened ([`Journal::open`]).
    pub fn open<F>(
        fs: F,
        options: Options,
        ledger: Arc<dyn RepairLedger>,
    ) -> Result<Self, JournalError>
    where
        F: Fs + Send + 'static,
        F::File: Send,
    {
        let journal = Journal::open(fs, options)?;
        Ok(Self {
            debts: Arc::new(Mutex::new(Box::new(journal))),
            in_flight: Arc::default(),
            gate: Arc::default(),
            ledger,
        })
    }

    fn lock(debts: &Mutex<Box<dyn Debts>>) -> MutexGuard<'_, Box<dyn Debts>> {
        debts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `record` as owed. It is durable when this returns; the ledger is told
    /// afterwards, off to the side. The write is marked in flight from before the record
    /// can be seen, until the [`Pending`] returned is dropped: the caller holds it through
    /// the commit of the blob.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the journal could not make it durable (or has
    /// stopped after an earlier failure): nothing is owed, and the caller must not answer
    /// the write with one copy.
    pub async fn owe(&self, record: RepairRecord) -> Result<Pending, StorageError> {
        let pending = Pending::begin(&self.in_flight, record.digest);
        let _gate = self.gate.lock().await;
        let debts = Arc::clone(&self.debts);
        tokio::task::spawn_blocking(move || Self::lock(&debts).owed(record))
            .await
            .map_err(|e| StorageError::Internal(format!("the repair journal: {e}")))?
            .map_err(|e| StorageError::Unavailable(Arc::new(e)))?;
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move { ledger.owed(&record).await });
        Ok(pending)
    }

    /// How many writes have begun owing, to be given back to
    /// [`Repairs::settle_evicted`].
    pub(crate) fn begun(&self) -> u64 {
        in_flight(&self.in_flight).begun
    }

    /// Whether a write of `digest` has journalled its debt and not finished committing.
    #[cfg(test)]
    pub(crate) fn is_in_flight(&self, digest: &Digest) -> bool {
        in_flight(&self.in_flight).digests.contains_key(digest)
    }

    /// Whether the gate is held now.
    #[cfg(test)]
    pub(crate) fn gate_is_held(&self) -> bool {
        self.gate.try_lock().is_err()
    }

    /// Records that `digest` is no longer owed because `storage` no longer holds it, unless
    /// a write of it is between its debt and its commit, any write began owing since `begun`
    /// was read (before the store was), or `storage` holds it now. The first two mean the
    /// blob may simply not be committed yet. The last is a write that committed after the
    /// store was read: the blob is held with one copy, and the debt stays for the next
    /// pass to copy. The store is asked *after* the in-flight test, with no debt added
    /// meanwhile (`gate`), so a write that is no longer in flight has already committed
    /// and shows. Whether the debt was dropped.
    ///
    /// The store is asked under the gate, so it is given `probe` to answer: every write that
    /// owes waits behind the gate, and a store that hangs must not stall them. The debt is
    /// dropped by a journal call that holds the gate to its end, so cancelling this future
    /// (a worker that is stopped) cannot let a new write's debt be journalled and then erased
    /// by the late call.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the journal could not write it, or the store did not
    /// answer within `probe` (the debt stays); the store's own error if it could not say
    /// whether it holds the blob (the debt stays).
    pub(crate) async fn settle_evicted(
        &self,
        digest: &Digest,
        begun: u64,
        storage: &dyn Storage,
        probe: Duration,
    ) -> Result<bool, StorageError> {
        let gate = Arc::clone(&self.gate).lock_owned().await;
        {
            let flight = in_flight(&self.in_flight);
            if flight.begun != begun || flight.digests.contains_key(digest) {
                return Ok(false);
            }
        }
        // Bounded: the gate is held, and every write that owes waits for it.
        let missing = tokio::time::timeout(probe, storage.find_missing(&[*digest]))
            .await
            .map_err(|_| StorageError::Unavailable(Arc::new(StoreNoAnswer { after: probe })))??;
        if missing.is_empty() {
            return Ok(false);
        }
        // From here the debt is dropped whatever becomes of this future. The journal call
        // cannot be cancelled once it is queued, so the gate goes into it: dropped with this
        // future, the gate would let a new write of the digest journal its debt and the
        // late `done` erase it, leaving a blob with one copy and no debt. The ledger is told
        // from there too, so a journal that forgot the debt is not one the frontend keeps.
        let debts = Arc::clone(&self.debts);
        let ledger = Arc::clone(&self.ledger);
        let runtime = tokio::runtime::Handle::current();
        let settled = *digest;
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            Self::lock(&debts).done(&settled)?;
            runtime.spawn(async move { ledger.settled(&settled).await });
            Ok::<(), JournalError>(())
        })
        .await
        .map_err(|e| StorageError::Internal(format!("the repair journal: {e}")))?
        .map_err(|e| StorageError::Unavailable(Arc::new(e)))?;
        Ok(true)
    }

    /// Records that `digest` is no longer owed: every owner holds it. Not synced (a debt that
    /// returns after a power cut is settled again at the cost of a check). The ledger is
    /// told afterwards, off to the side.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the journal could not write it (the debt stays).
    pub(crate) async fn settle(&self, digest: &Digest) -> Result<(), StorageError> {
        let debts = Arc::clone(&self.debts);
        let settled = *digest;
        tokio::task::spawn_blocking(move || Self::lock(&debts).done(&settled))
            .await
            .map_err(|e| StorageError::Internal(format!("the repair journal: {e}")))?
            .map_err(|e| StorageError::Unavailable(Arc::new(e)))?;
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move { ledger.settled(&settled).await });
        Ok(())
    }

    /// Tells the ledger of every copy owed, off to the side: a frontend that was away, or
    /// lost what it knew, learns it again.
    pub(crate) fn announce(&self) {
        let records = self.open_records();
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move {
            for record in records {
                ledger.owed(&record).await;
            }
        });
    }

    /// Every copy owed now, in digest order.
    #[must_use]
    pub fn open_records(&self) -> Vec<RepairRecord> {
        Self::lock(&self.debts).records()
    }
}
