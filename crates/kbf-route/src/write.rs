//! Writes: to the first write owner that is not down.

use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_meta::{ActionRecord, Role};
use kbf_storage::service::MAX_FIND_MISSING_DIGESTS;
use kbf_storage::{Storage, StorageError};
use kbf_types::Digest;

use crate::router::{Router, unavailable, write_order};
use crate::source::MapView;

/// The blobs of an entry's closure copied to the action's owner at once by one commit.
/// Each copy holds one whole blob in memory; what all the commits of a router hold
/// together is bounded in bytes too ([`Router::with_copy_budget`]).
const COPIES_IN_FLIGHT: usize = 4;

impl Router {
    /// Sends `bytes` to the first of the blob's write owners that is not `Down` in the map,
    /// once, and answers the digest that node gives back, which must be the digest of the
    /// bytes (hashed here to find the owner). That node answers when it has stored the
    /// blob; the other owners are not written to by this call, and a node that fails is not
    /// followed by a write to the next owner. Whether the node passes the blob on to a
    /// replica is the storage service's (av2-11 in `kbf-storage`), not the router's.
    ///
    /// The bytes are hashed here, and again by the node that stores them.
    ///
    /// # Errors
    /// Every owner of the blob is down; the node's own error.
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
    /// ([`Storage::put`], checked to answer the digest), and then commits. A commit holds
    /// at most [`COPIES_IN_FLIGHT`] blobs at a time, each whole, and every commit on a
    /// router and its clones together hold at most [`Router::with_copy_budget`] bytes
    /// ([`crate::DEFAULT_COPY_BYTES`] unless set): a copy waits for room before it reads, and a
    /// blob larger than the budget is copied when it has the budget to itself.
    ///
    /// The blobs pass through this process once. The owner takes a blob it does not own
    /// through [`Storage::put_unowned`]: a node that serves a chain refuses a plain write
    /// of a blob it is not the primary of. When a write is acknowledged by its replicas
    /// (av2-11, av2-12), the entry carries those acknowledgements and this copy goes away.
    /// The copy is an extra, not the replicated one: each blob is still held by its own
    /// owners (the daemon's upload or [`Router::put`] went through the chain), and the node
    /// takes the marker only from the frontend's certificate.
    ///
    /// # Errors
    /// Every owner of the action is down; a blob of the entry that no owner holds (the
    /// error is that read's: nothing is committed); the node's refusal or error.
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
        let owner = primary(&view, &action)?;
        let node = self.nodes.node(&owner, view.map.generation())?;
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
                    .await?
                    .into_iter()
                    .filter(|d| d.size_bytes != 0),
            );
        }
        futures::stream::iter(lacking)
            .map(|digest| self.copy_to(&node, digest))
            .buffer_unordered(COPIES_IN_FLIGHT)
            .try_collect::<()>()
            .await?;
        node.commit_action_record(role, action, record).await
    }

    /// The blob `digest`, read from whichever owner has it and stored on `node`.
    async fn copy_to(&self, node: &Arc<dyn Storage>, digest: Digest) -> Result<(), StorageError> {
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
            self.read_blob(&digest).await?
        };
        // The action's owner is rarely the blob's: it holds the blob as one more copy.
        let stored = node.put_unowned(bytes).await?;
        if stored != digest {
            return Err(StorageError::Internal(format!(
                "a node stored the blob {digest} as {stored}"
            )));
        }
        Ok(())
    }

    async fn put_in(
        &self,
        view: MapView,
        digest: Digest,
        bytes: Bytes,
    ) -> Result<Digest, StorageError> {
        let primary = primary(&view, &digest)?;
        self.nodes
            .node(&primary, view.map.generation())?
            .put(bytes)
            .await
    }

    async fn commit_in(
        &self,
        view: MapView,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        let primary = primary(&view, &action)?;
        self.nodes
            .node(&primary, view.map.generation())?
            .commit_action_record(role, action, record)
            .await
    }
}

/// The owner a write of `digest` goes to: the first write owner that is not down.
fn primary(view: &MapView, digest: &Digest) -> Result<kbf_map::Member, StorageError> {
    write_order(&view.map, digest)
        .first()
        .map(|m| (*m).clone())
        .ok_or_else(|| unavailable(format!("every owner of {digest} is down")))
}
