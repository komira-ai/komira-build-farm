//! Writes across storage nodes (`docs/design/architecture-v2.md`, sections 6 and 8): which
//! map a node judges a call by, and what it answers a caller whose map is not the
//! current one.
//!
//! **The map a node holds.** A node learns the current map from the frontend (the
//! registration stream's `Welcome` and `MapUpdate`, a separate change), and this crate
//! does not: it asks a [`MapSource`], which says what the node holds now. A node given no
//! source is a one-member farm and judges nothing. A node given one is a member of a
//! farm and fails closed: until its source has a map it fetches one, and if it gets
//! none it serves no call (UNAVAILABLE), whatever generation the caller names.
//!
//! **STALE_MAP.** Every call carries the generation of the map its caller routed by
//! (`kbf.storage.v1`'s `map_generation`, or the `kbf-map-generation` entry on the standard
//! calls). A write to a node that holds a different map is not served:
//! - a caller behind the node (an older generation, or 0, "no map seen") gets
//!   [`StorageError::StaleMap`] with the node's generation, so it can fetch that map and
//!   try again: FAILED_PRECONDITION with the generation in the `kbf-map-generation`
//!   trailer;
//! - a caller ahead of the node (the node has not been told of the generation the caller
//!   has) makes the node fetch it: [`MapSource::refetch`], waited for at most
//!   [`REFETCH_WAIT`]. If the map arrives the call goes on under it (served if it is the
//!   caller's generation, STALE_MAP if the node is now past it). If it does not, the call
//!   gets UNAVAILABLE: the caller's map may be the right one, and this node cannot tell,
//!   so it neither serves the call nor claims the caller is stale. A lost push is
//!   harmless: the node learns of the map, and the same call then succeeds.
//!
//! The check runs on every call (the blob calls, the action-cache calls and the segment
//! calls) before it reads a byte or looks at what the node holds, so no answer to a stale
//! caller, not even "already stored", is given under a placement the node no longer
//! holds.
//!
//! # The chain (R = 2)
//!
//! A write goes once to the **primary**, the first of [`kbf_map::Map::write_owners`]. Its
//! `Write` handler passes each message on to the **replica**, the second, over the same
//! ByteStream `Write` marked as a copy (the [`LEG_HEADER`] entry, admitted from the
//! `storage` role only: `Endpoint::WriteLeg`). A node judges what it is given by its
//! `Chain::plan`:
//! - a client write to a node that is not the primary is INVALID_ARGUMENT, so is a copy
//!   sent to a node that does not own the blob (or by a node to itself);
//! - a map with more than two owners per blob is refused (INVALID_ARGUMENT), and one
//!   with fewer write owners than its factor is UNAVAILABLE: neither is answered with
//!   one copy.
//!
//! **Streamed, never held whole.** Where the store offers a [`crate::PutStream`], each
//! message goes to it (through a thread of its own, hashing as it goes) and to the replica
//! at once. At most a few messages are held, whatever the blob's size: queues of two in
//! front of the store and the replica, and the one in hand. A primary whose store cannot
//! stream refuses (FAILED_PRECONDITION) rather than hold the blob; a replica that cannot
//! stream takes the copy the buffered way.
//!
//! **The order of commits.** The replica verifies the blob against its digest and makes it
//! durable *before* the primary makes its own copy present, and the primary answers only
//! after both. So a blob that is present on a primary has a copy on its replica, and a
//! crash between the two leaves the blob absent on the primary: the writer's
//! FindMissing says so and it writes again, and the replica, which holds it, answers at
//! once. (A blob already present on the primary is answered without calling the replica
//! for the same reason.)
//!
//! **Refusals are answers.** The replica's refusal (a stale map, a digest that does not
//! match, a role that may not copy) is the writer's answer; so is a failure of the
//! primary's own store. If the map turned while the blob was written, the primary checks
//! the new map still places it as it did (STALE_MAP otherwise) before it commits.
//!
//! # One copy, and the copy owed
//!
//! A replica that cannot be reached (the connection fails, it takes no message within
//! [`ChainConfig::stall`], it does not commit within [`ChainConfig::commit`], or the map
//! names it down) does not fail the write, and a write is not answered with one copy
//! unless the copy is owed: **the primary answers only after it has made a record of the
//! copy durable in its repair journal** ([`crate::repair::Journal`]), and the record is made
//! before the blob is present on the primary. A failure to make it is the write's: the
//! primary does not hold the blob and does not answer. The frontend is told after (the
//! [`crate::repair::RepairLedger`] seam) and never waited for.
//!
//! What counts as unreachable is `StorageError::Unavailable` of the replica's answer; its
//! refusals are not (a stale map, a digest that does not match, a role that may not copy, a
//! failure of its store).
//!
//! If the *primary* is the owner the map names down, the next owner takes the write as
//! if it were primary and owes the first owner its copy.

mod chain;
mod leg;
mod peers;
mod staging;

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_map::{Map, MemberId};

pub(crate) use chain::Plan;
pub use chain::{Chain, ChainConfig};
pub use leg::LEG_HEADER;
pub(crate) use leg::{Leg, ReplicaLeg};
pub use peers::Peers;
pub(crate) use staging::Staging;

use crate::StorageError;

/// The map a node holds and which member of it the node is, read together for one write.
#[derive(Clone, Debug)]
pub struct View {
    /// The map the write is judged by.
    pub map: Arc<Map>,
    /// This node's member id.
    pub me: MemberId,
}

/// The map a node holds now.
pub trait MapSource: Send + Sync {
    /// The current map, or `None` before the node has learned one.
    fn current(&self) -> Option<Arc<Map>>;

    /// This node's member id, assigned by the state store at registration: `None` before
    /// the node has registered. Once known it does not change, so it is read apart from
    /// the map. A node that does not know it can judge and serve reads, but cannot take
    /// part in a chain (it cannot tell whether it is the primary).
    fn me(&self) -> Option<MemberId>;

    /// The map of generation `at_least` or a newer one, fetched from a frontend, or
    /// `None` if the node cannot get one. Called when a caller routed by a generation the
    /// node has not been told of; the caller of this method bounds the wait
    /// ([`REFETCH_WAIT`]), so an implementation may wait for the next push or ask.
    ///
    /// The default is a node that learns its map only from pushes: it fetches nothing.
    fn refetch(&self, at_least: u64) -> BoxFuture<'_, Option<Arc<Map>>> {
        let _ = at_least;
        Box::pin(async { None })
    }
}

/// How long a call routed by a map this node has not been told of waits for the node to
/// fetch it.
pub const REFETCH_WAIT: Duration = Duration::from_secs(2);

/// This node has no map yet, so it cannot say whether any caller's is the current one.
#[derive(Debug, thiserror::Error)]
#[error("this node has not been given a map yet (the caller routed by {requested})")]
struct NoMap {
    requested: u64,
}

/// This node is at a map generation older than the one its caller routed by.
#[derive(Debug, thiserror::Error)]
#[error(
    "this node holds map generation {current} and the caller routed by {requested}: the \
     node has not been told of it yet"
)]
struct NotYetSeen {
    requested: u64,
    current: u64,
}

/// Whether a call routed by map generation `requested` may be served by the node `source`
/// speaks for, and the map it is judged by. A caller ahead of the node, or a node with no
/// map yet, makes it fetch the map, waiting at most `wait`.
///
/// # Errors
/// [`StorageError::StaleMap`] if `requested` is older than the node's generation (0 is
/// older than every map's); [`StorageError::Unavailable`] if the caller is newer, or the
/// node has no map, and the node could not get one in time.
pub(crate) async fn check(
    source: &dyn MapSource,
    requested: u64,
    wait: Duration,
) -> Result<Arc<Map>, StorageError> {
    let held = source.current();
    let map = match held {
        Some(map) if requested <= map.generation() => map,
        held => {
            let fetched = tokio::time::timeout(wait, source.refetch(requested))
                .await
                .ok()
                .flatten();
            fetched
                .or(held)
                .ok_or_else(|| StorageError::Unavailable(Arc::new(NoMap { requested })))?
        }
    };
    check_generation(&map, requested)?;
    Ok(map)
}

/// Whether a call routed by map generation `requested` may be served by a node holding
/// `map`.
///
/// # Errors
/// [`StorageError::StaleMap`] if `requested` is older than the map's generation (0 is
/// older than every map's); [`StorageError::Unavailable`] if it is newer.
pub(crate) fn check_generation(map: &Map, requested: u64) -> Result<(), StorageError> {
    let current = map.generation();
    if requested < current {
        return Err(StorageError::StaleMap { current });
    }
    if requested > current {
        return Err(StorageError::Unavailable(Arc::new(NotYetSeen {
            requested,
            current,
        })));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
