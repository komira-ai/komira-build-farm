//! Writes: to the first write owner that is not down, and on to the next if that one cannot
//! be reached.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_meta::{ActionRecord, Role};
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_storage::{Storage, StorageError};
use kbf_types::Digest;

use crate::router::{Router, unavailable, write_order};
use crate::source::MapView;

/// Why a write to one owner failed, and so whether the next owner is asked.
enum Tried {
    /// The owner the write was made to failed: the next owner is asked if the failure says
    /// nothing of the request ([`may_try_next`]).
    Node(StorageError),
    /// Something else failed (reading a blob of an entry's closure from the farm, for
    /// one): no other owner of the write would do better, so the write ends.
    Elsewhere(StorageError),
}

/// Whether a write that failed like this at one owner is made at the next.
///
/// Only [`StorageError::Unavailable`]: the owner could not be reached or did not answer
/// (a refused connection, a reset, a deadline, UNAVAILABLE), which says nothing of the
/// request, and a write is the blob or the entry by its own digest, so making it twice is
/// harmless. Every other failure is an answer from a node that was reached, and the next
/// owner would give the same one or be wrong to: `STALE_MAP` (the map is fetched and the
/// whole call run again, [`Router::routed`]), `Invalid` and `DigestMismatch` (the
/// request), a refused action-cache write, `Internal` (a node that declined or is broken:
/// a primary does not answer with one copy over a replica's refusal, and the router does
/// not either).
const fn may_try_next(failure: &StorageError) -> bool {
    matches!(failure, StorageError::Unavailable(_))
}

/// The blobs of an entry's closure copied to the action's owner at once by one commit.
/// Each copy holds one whole blob in memory; what all the commits of a router hold
/// together is bounded in bytes too ([`Router::with_copy_budget`]).
const COPIES_IN_FLIGHT: usize = 4;

impl Router {
    /// Sends `bytes` to the first of the blob's write owners that is not `Down` in the map,
    /// and answers the digest that node gives back, which must be the digest of the bytes
    /// (hashed here to find the owner). That node answers when it has stored the blob; the
    /// other owners are not written to while it does. Whether the node passes the blob on
    /// to a replica is the storage service's (av2-11 in `kbf-storage`), not the router's.
    ///
    /// **An owner that cannot be reached** (dead, but the map has not marked it `Down` yet:
    /// that takes the member lease, 15 s) is not the end of the write: the next write owner
    /// that is not `Down` is sent the blob, whole and from the start, and takes it as the
    /// primary, owing the first owner its copy (that is the node's chain,
    /// `kbf_storage::replicate`). Only [`StorageError::Unavailable`] (and a member that
    /// cannot be dialled) moves on; a node that answered and declined does not, and
    /// `STALE_MAP` has its own rule. If every owner fails, the first one's failure is the
    /// caller's. The router remembers nothing of a failed owner: the next write of a blob it
    /// owns tries it first again.
    ///
    /// The bytes are hashed here, and again by the node that stores them.
    ///
    /// # Errors
    /// Every owner of the blob is down; the node's own error, or the first owner's if all
    /// could not be reached.
    pub async fn put(&self, bytes: Bytes) -> Result<Digest, StorageError> {
        let digest = kbf_segments::sha256(&bytes);
        let stored = self
            .routed(|view| self.put_in(view, digest, bytes.clone()))
            .await?;
        if stored != digest {
            return Err(StorageError::Internal(format!(
                "a node stored the blob {digest} as {stored}"
            )));
        }
        Ok(stored)
    }

    /// The second half of an action-cache write: commits the entry for `action` at the
    /// first of the action's write owners that is not `Down`, which refuses it unless it
    /// holds every blob of the entry itself: [`Router::commit_with_closure`] puts them
    /// there first. (The first half, `prepare_action_result`, takes no action digest and
    /// so has no owner to be routed to.)
    ///
    /// **This does not fail over.** The caller has put the entry's blobs at the first
    /// owner, and the next owner does not hold them, so it would refuse the entry as
    /// `Absent` and hide the UNAVAILABLE the caller could retry on. A commit that should
    /// survive a dead owner is [`Router::commit_with_closure`], which copies the blobs to
    /// the owner it commits at.
    ///
    /// # Errors
    /// Every owner of the action is down; the node's refusal or error.
    pub async fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        self.routed(|view| self.commit_in(view, role, action, record.clone()))
            .await
    }

    /// [`Router::commit_action_record`], for an entry whose blobs are not all where the
    /// action is. A storage node checks an entry against the blobs **it** holds, and the
    /// action's owner and the owners of its outputs are different nodes (each is chosen by
    /// its own digest), so the commit alone is refused as `Absent` unless the owner holds
    /// the result blob and the whole closure.
    ///
    /// This asks the action's owner which blobs of the entry it lacks, reads each from
    /// wherever the map has it ([`Router::read_blob`]: routed, failing over, and hashed
    /// against its digest, so a bad copy is never passed on), stores it on the owner
    /// ([`Storage::put_unowned`], checked to answer the digest), and then commits. A commit holds
    /// at most [`COPIES_IN_FLIGHT`] blobs at a time, each whole, and every commit on a
    /// router and its clones together hold at most [`Router::with_copy_budget`] bytes
    /// ([`crate::DEFAULT_COPY_BYTES`] unless set): a copy waits for room before it reads, and a
    /// blob larger than the budget is copied when it has the budget to itself.
    ///
    /// The blobs pass through this process once. The owner takes a blob it does not own
    /// through [`Storage::put_unowned`]: a node that serves a chain refuses a plain write
    /// of a blob it is not a write owner of. When a write is acknowledged by its replicas
    /// (av2-11, av2-12), the entry carries those acknowledgements and this copy goes away.
    /// The copy is an extra, not the replicated one: each blob is still held by its own
    /// owners (the daemon's upload or [`Router::put`] went through the chain), and the node
    /// takes the marker only from the frontend's certificate.
    ///
    /// **An owner that cannot be reached** is followed by the next, as for
    /// [`Router::put`]: when the action's owner fails to answer FindMissing, to take a
    /// copy or to commit (UNAVAILABLE), the next write owner is asked what it lacks, is
    /// copied what it lacks and commits. Copies already made to the first owner stay
    /// there, and a failure to *read* a blob of the entry from the farm ends the commit
    /// at once (another owner of the action would read it no better).
    ///
    /// # Errors
    /// Every owner of the action is down; a blob of the entry that no owner holds (the
    /// error is that read's: nothing is committed); the node's refusal or error, or the
    /// first owner's if all could not be reached.
    pub async fn commit_with_closure(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        self.routed(|view| self.commit_closed_in(view, role, action, record.clone()))
            .await
    }

    async fn commit_closed_in(
        &self,
        view: MapView,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        self.at_a_write_owner(&view, &action, |node| {
            let record = record.clone();
            async move { self.commit_closed_at(&node, role, action, record).await }
        })
        .await
    }

    /// [`Router::commit_with_closure`] at one owner, `node`.
    async fn commit_closed_at(
        &self,
        node: &Arc<dyn Storage>,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), Tried> {
        let needed: Vec<Digest> = std::iter::once(record.result)
            .chain(record.closure.iter().copied())
            .collect();
        // A node reports the empty blob held whether or not it was ever stored, but its
        // commit checks its index: the empty blob (an `ActionResult` with nothing in it
        // is one) is stored on the owner like any other.
        let mut lacking: Vec<Digest> = needed
            .iter()
            .filter(|d| d.size_bytes == 0)
            .copied()
            .collect();
        for chunk in needed.chunks(MAX_FIND_MISSING_DIGESTS) {
            lacking.extend(
                node.find_missing(chunk)
                    .await
                    .map_err(Tried::Node)?
                    .into_iter()
                    .filter(|d| d.size_bytes != 0),
            );
        }
        futures::stream::iter(lacking)
            .map(|digest| self.copy_to(node, digest))
            .buffer_unordered(COPIES_IN_FLIGHT)
            .try_collect::<()>()
            .await?;
        node.commit_action_record(role, action, record)
            .await
            .map_err(Tried::Node)
    }

    /// The blob `digest`, read from whichever owner has it and stored on `node`.
    async fn copy_to(&self, node: &Arc<dyn Storage>, digest: Digest) -> Result<(), Tried> {
        let room = digest
            .size_bytes
            .div_ceil(1024)
            .clamp(1, u64::from(self.copy_kib));
        // At least a KiB, and at most the whole budget, so a blob larger than it waits
        // for the budget to itself rather than for room that never comes. The semaphore
        // is never closed, so the wait ends in a permit.
        let _room = self
            .copying
            .acquire_many(u32::try_from(room).unwrap_or(self.copy_kib))
            .await
            .ok();
        let bytes = if digest.size_bytes == 0 {
            Bytes::new()
        } else {
            self.read_blob(&digest).await.map_err(Tried::Elsewhere)?
        };
        // The action's owner is rarely the blob's: it holds the blob as one more copy.
        let stored = node.put_unowned(bytes).await.map_err(Tried::Node)?;
        if stored != digest {
            return Err(Tried::Node(StorageError::Internal(format!(
                "a node stored the blob {digest} as {stored}"
            ))));
        }
        Ok(())
    }

    async fn put_in(
        &self,
        view: MapView,
        digest: Digest,
        bytes: Bytes,
    ) -> Result<Digest, StorageError> {
        self.at_a_write_owner(&view, &digest, |node| {
            let bytes = bytes.clone();
            async move { node.put(bytes).await.map_err(Tried::Node) }
        })
        .await
    }

    /// The plain commit, at the first write owner only: it does not fail over, because the
    /// next owner does not hold the entry's blobs (the caller put them at the first) and
    /// would refuse the entry as `Absent`, hiding the UNAVAILABLE the caller could retry on.
    async fn commit_in(
        &self,
        view: MapView,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        let owner = write_order(&view.map, &action)
            .first()
            .map(|m| (*m).clone())
            .ok_or_else(|| unavailable(format!("every owner of {action} is down")))?;
        self.nodes
            .node(&owner, view.map.generation())?
            .commit_action_record(role, action, record)
            .await
    }

    /// Runs `attempt` against the first of `digest`'s write owners that is not down, and,
    /// while it fails in a way that says nothing of the request ([`may_try_next`]), against
    /// the next. The first failure is the answer if no owner takes it.
    async fn at_a_write_owner<T, Fut>(
        &self,
        view: &MapView,
        digest: &Digest,
        attempt: impl Fn(Arc<dyn Storage>) -> Fut,
    ) -> Result<T, StorageError>
    where
        Fut: Future<Output = Result<T, Tried>>,
    {
        let generation = view.map.generation();
        let mut failure = None;
        for member in write_order(&view.map, digest) {
            // A member that cannot be dialled (its address is not one) is passed over, as
            // a read does.
            let node = match self.nodes.node(member, generation) {
                Ok(node) => node,
                Err(e) => {
                    failure.get_or_insert(e);
                    continue;
                }
            };
            match attempt(node).await {
                Ok(done) => return Ok(done),
                Err(Tried::Node(e)) if may_try_next(&e) => {
                    failure.get_or_insert(e);
                }
                Err(Tried::Node(e) | Tried::Elsewhere(e)) => return Err(e),
            }
        }
        Err(failure.unwrap_or_else(|| unavailable(format!("every owner of {digest} is down"))))
    }
}
