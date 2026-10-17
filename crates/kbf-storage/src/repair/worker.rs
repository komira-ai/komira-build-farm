//! The repair worker: makes the second copy a node owes, when its owner can take it, and
//! only then settles the debt.
//!
//! For each record in the journal the worker looks at the *current* map, not the one the
//! write was answered under: the owners it must copy to are the write owners of the blob
//! now, other than this node. The record's `replica` is advisory. So a replica that was
//! replaced while the debt stood is not waited for, a node that is no longer an owner at
//! all copies to both owners, and a debt is never settled against a node that no longer owns
//! the blob. For each such owner:
//! - **down in the map:** wait (the debt stays);
//! - **already holds the blob:** a `FindMissing`, not a copy (the write that left the debt
//!   may have reached it after all);
//! - **otherwise:** the blob is read from this node's store, verified as the store verifies
//!   a read, and streamed to the owner as a copy ([`crate::replicate::LEG_HEADER`]) which
//!   verifies it against its digest and makes it durable before answering.
//!
//! A debt is settled only when every owner holds the blob. Any failure leaves it owed for the
//! next pass: the worker never gives up on a blob, and never concludes from an error that a
//! copy exists. The one debt it drops without a copy is one whose blob this node no longer
//! holds (its store evicted it): there is nothing left to copy, and the loss is the cache's
//! to account for. A debt is journalled before the blob is committed here, so a debt whose
//! write is still committing ([`super::Pending`]) is left alone, and a debt is dropped for
//! an absent blob only if no write began owing since the pass started
//! and the store, asked again after that, still lacks the blob ([`Repairs::settle_evicted`]);
//! otherwise a write answered with one copy and no debt would be possible.
//!
//! An owner that is up and does not answer is waited for like one that is down: the question
//! "do you hold it" is bounded by [`RepairConfig::probe`], and `run` stops mid-pass. So is
//! the question this node's own store is asked before an evicted debt is dropped. A pass
//! stopped in the middle of dropping a debt leaves the drop to finish ([`Repairs::settle_evicted`]).
//!
//! **Bounded work.** At most [`RepairConfig::concurrency`] blobs are repaired at once, one
//! pass after another, [`RepairConfig::interval`] apart; a copy streams like a write
//! (a few messages in flight, whatever the size).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use kbf_map::{Member, MemberId, MemberState};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_types::Digest;

use super::{RepairRecord, Repairs};
use crate::replicate::{Chain, Leg, View};
use crate::service::RemoteStorage;
use crate::{Storage, StorageError};

/// How the worker is paced.
#[derive(Clone, Copy, Debug)]
pub struct RepairConfig {
    /// The time between passes over the debts.
    pub interval: Duration,
    /// The most blobs repaired at once.
    pub concurrency: usize,
    /// How long an owner may take to say whether it holds a blob, and so may this node's own
    /// store when the worker asks it whether an evicted blob is held (that look is made
    /// under the gate every write that owes waits for). An owner that is up and does not
    /// answer is a debt waiting, not a pass that never ends. (A copy is bounded like a
    /// write: [`crate::replicate::ChainConfig`].)
    pub probe: Duration,
}

impl Default for RepairConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            concurrency: 4,
            probe: Duration::from_secs(10),
        }
    }
}

/// What one pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    /// Debts settled: every owner holds the blob.
    pub settled: usize,
    /// Debts still owed: an owner is down, unreachable, or refused.
    pub waiting: usize,
}

/// Why a debt could not be settled this pass.
enum Waiting {
    /// There is no map yet, or an owner is down in it.
    NoPlacement,
    /// This node's own store could not read the blob.
    Local(StorageError),
    /// An owner could not be reached or refused the copy.
    Owner(StorageError),
}

/// An owner that is up and did not say whether it holds a blob.
#[derive(Debug, thiserror::Error)]
#[error("member {owner} did not say whether it holds the blob within {after:?}")]
struct NoAnswer {
    owner: MemberId,
    after: Duration,
}

/// Settles what a node owes.
pub struct RepairWorker {
    storage: Arc<dyn Storage>,
    chain: Arc<Chain>,
    repairs: Arc<Repairs>,
    config: RepairConfig,
    uploads: AtomicU64,
}

impl RepairWorker {
    /// A worker repairing from `storage` (this node's store) what `repairs` records, to the
    /// owners `chain`'s map names.
    #[must_use]
    pub fn new(
        storage: Arc<dyn Storage>,
        chain: Arc<Chain>,
        repairs: Arc<Repairs>,
        config: RepairConfig,
    ) -> Self {
        Self {
            storage,
            chain,
            repairs,
            config,
            uploads: AtomicU64::new(0),
        }
    }

    /// Tells the frontend what is owed (a frontend that was away, or lost its state, learns
    /// it again), then passes over the debts every [`RepairConfig::interval`] until `stop`
    /// resolves.
    pub async fn run(&self, stop: impl Future<Output = ()>) {
        self.repairs.announce();
        let mut ticks = tokio::time::interval(self.config.interval);
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => return,
                _ = ticks.tick() => {
                    // A pass is dropped where it stands when `stop` resolves: what it had
                    // begun is abandoned (a copy half sent is dropped uncommitted by its
                    // owner), and the debts stay owed.
                    tokio::select! {
                        () = &mut stop => return,
                        _ = self.pass() => {}
                    }
                }
            }
        }
    }

    /// One pass over every debt.
    pub async fn pass(&self) -> Pass {
        let records = self.repairs.open_records();
        let outcomes: Vec<bool> = futures::stream::iter(records)
            .map(|record| self.repair(record))
            .buffer_unordered(self.config.concurrency.max(1))
            .collect()
            .await;
        let settled = outcomes.iter().filter(|settled| **settled).count();
        Pass {
            settled,
            waiting: outcomes.len() - settled,
        }
    }

    /// Repairs one debt; whether it is settled.
    async fn repair(&self, record: RepairRecord) -> bool {
        // Read before the store is: a write that begins owing after this is not one this
        // pass can tell from an eviction.
        let begun = self.repairs.begun();
        match self.copy_to_owners(&record.digest).await {
            Ok(()) => self.repairs.settle(&record.digest).await.is_ok(),
            Err(Waiting::Local(StorageError::NotFound(_))) => {
                // Not held here and no write is on its way to holding it: evicted. Nothing
                // is left to copy, and the loss is the cache's to account for.
                let dropped = self
                    .repairs
                    .settle_evicted(
                        &record.digest,
                        begun,
                        self.storage.as_ref(),
                        self.config.probe,
                    )
                    .await
                    .unwrap_or(false);
                if dropped {
                    tracing::warn!(digest = %record.digest, "a blob owed to another owner is no longer held here");
                }
                dropped
            }
            Err(waiting) => {
                log(&record, &waiting);
                false
            }
        }
    }

    /// Makes every owner of `digest` but this node hold it.
    async fn copy_to_owners(&self, digest: &Digest) -> Result<(), Waiting> {
        let view = self.chain.current().ok_or(Waiting::NoPlacement)?;
        let mut owners = Vec::new();
        for id in view.map.write_owners(digest) {
            let owner = view.map.member(id).ok_or(Waiting::NoPlacement)?;
            if owner.id != view.me {
                owners.push(owner);
            }
        }
        if owners.iter().any(|owner| owner.state == MemberState::Down) {
            return Err(Waiting::NoPlacement);
        }
        for owner in owners {
            self.ensure(&view, owner, digest).await?;
        }
        Ok(())
    }

    /// `owner` holds `digest`, having been sent it if it did not.
    async fn ensure(&self, view: &View, owner: &Member, digest: &Digest) -> Result<(), Waiting> {
        let channel = self.chain.peer(&owner.addr).map_err(Waiting::Owner)?;
        let generation = view.map.generation();
        let remote = RemoteStorage::new(channel, generation);
        let digests = [*digest];
        let asked = remote.find_missing(&digests);
        let missing = tokio::time::timeout(self.config.probe, asked)
            .await
            .map_err(|_| {
                Waiting::Owner(StorageError::Unavailable(Arc::new(NoAnswer {
                    owner: owner.id,
                    after: self.config.probe,
                })))
            })?
            .map_err(Waiting::Owner)?;
        if missing.is_empty() {
            return Ok(());
        }
        let leg = self
            .chain
            .open_copy(view, owner, digest.size_bytes)
            .map_err(Waiting::Owner)?;
        self.send(leg, digest).await
    }

    /// Streams the blob from this node's store to `leg`.
    async fn send(&self, mut leg: Leg, digest: &Digest) -> Result<(), Waiting> {
        let name = format!(
            "uploads/repair-{}/blobs/{}/{}",
            self.uploads.fetch_add(1, Ordering::Relaxed),
            digest.hash_hex(),
            digest.size_bytes
        );
        let mut chunks = self
            .storage
            .read_range(digest, 0, 0)
            .await
            .map_err(Waiting::Local)?
            .peekable();
        let mut offset = 0;
        let mut first = true;
        loop {
            // An empty blob is one empty message that finishes the write.
            let data = chunks
                .next()
                .await
                .transpose()
                .map_err(Waiting::Local)?
                .unwrap_or_default();
            let finish = Pin::new(&mut chunks).peek().await.is_none();
            let message = WriteRequest {
                resource_name: if first { name.clone() } else { String::new() },
                write_offset: offset,
                finish_write: finish,
                data: data.to_vec(),
            };
            first = false;
            offset += data.len() as i64;
            leg.send(message).await.map_err(Waiting::Owner)?;
            if finish {
                break;
            }
        }
        leg.finish().await.map_err(Waiting::Owner)
    }
}

fn log(record: &RepairRecord, why: &Waiting) {
    match why {
        Waiting::NoPlacement => {
            tracing::debug!(digest = %record.digest, "a copy is owed and no owner can take it yet");
        }
        Waiting::Local(e) | Waiting::Owner(e) => {
            tracing::warn!(digest = %record.digest, %e, "a copy is owed and could not be made this pass");
        }
    }
}
