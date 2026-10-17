//! [`Router`]: the map, the nodes and the rules that every call shares.

use std::future::Future;
use std::sync::Arc;

use kbf_map::{Map, Member, MemberState};
use kbf_storage::StorageError;
use kbf_types::Digest;
use tokio::sync::Semaphore;

use crate::flights::Flights;
use crate::nodes::Nodes;
use crate::repair::{Limiter, ReadRepair, RepairLimit};
use crate::source::{MapSource, MapView};

/// What [`Router::commit_with_closure`] holds in memory, across all its calls, unless
/// [`Router::with_copy_budget`] says otherwise: 256 MiB.
pub const DEFAULT_COPY_BYTES: u64 = 256 << 20;

const DEFAULT_COPY_KIB: u32 = (DEFAULT_COPY_BYTES / 1024) as u32;

/// Calls to the storage nodes of a farm, routed by the storage map.
///
/// Cheap to clone; every clone shares the map source, the nodes, the repair limit and the
/// reads in flight. What each call does, and what it does not, is in the crate
/// documentation.
#[derive(Clone)]
pub struct Router {
    pub(crate) maps: Arc<dyn MapSource>,
    pub(crate) nodes: Arc<dyn Nodes>,
    pub(crate) repair: Option<Arc<Repair>>,
    pub(crate) flights: Arc<Flights>,
    /// The bytes of blobs that [`Router::commit_with_closure`] holds in memory to copy, in
    /// KiB, shared by every clone.
    pub(crate) copying: Arc<Semaphore>,
    pub(crate) copy_kib: u32,
}

/// The read-repair hook and its limit.
pub(crate) struct Repair {
    pub hook: Arc<dyn ReadRepair>,
    pub limiter: Limiter,
}

/// Who holds a digest, as a caller that talks to the nodes itself (the frontend's
/// pass-through of a ByteStream) needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The generation of the map this was worked out from.
    pub generation: u64,
    /// The members to read from, in the order to try them: this generation's owners in
    /// rank order, then the members that may not hold the blob yet, then, while a change
    /// settles, the previous generation's owners. Empty when every owner is down.
    pub read: Vec<Member>,
    /// The member a write goes to: the first owner that is not down. `None` when every
    /// owner is down.
    pub write: Option<Member>,
}

impl Router {
    /// A router over `maps`, calling nodes through `nodes`.
    #[must_use]
    pub fn new(maps: Arc<dyn MapSource>, nodes: Arc<dyn Nodes>) -> Self {
        Self {
            maps,
            nodes,
            repair: None,
            flights: Arc::default(),
            copying: Arc::new(Semaphore::new(DEFAULT_COPY_KIB as usize)),
            copy_kib: DEFAULT_COPY_KIB,
        }
    }

    /// The same router, holding at most `bytes` of blobs in memory at once (counted in
    /// KiB, at least one) across every [`Router::commit_with_closure`] on it and its
    /// clones. A blob larger than that is copied alone. The default is
    /// [`DEFAULT_COPY_BYTES`].
    #[must_use]
    pub fn with_copy_budget(mut self, bytes: u64) -> Self {
        let kib = u32::try_from(bytes / 1024).unwrap_or(u32::MAX).max(1);
        self.copying = Arc::new(Semaphore::new(kib as usize));
        self.copy_kib = kib;
        self
    }

    /// The same router, telling `hook` about a blob a read found away from where it
    /// belongs, at most as often as `limit` allows.
    #[must_use]
    pub fn with_read_repair(mut self, hook: Arc<dyn ReadRepair>, limit: RepairLimit) -> Self {
        self.repair = Some(Arc::new(Repair {
            hook,
            limiter: Limiter::new(limit),
        }));
        self
    }

    /// Who holds `digest` in the map as it is now.
    ///
    /// # Errors
    /// No map has arrived yet.
    pub fn place(&self, digest: &Digest) -> Result<Placement, StorageError> {
        let view = self.view()?;
        Ok(Placement {
            generation: view.map.generation(),
            read: read_order(&view, digest).into_iter().cloned().collect(),
            write: write_order(&view.map, digest).first().map(|m| (*m).clone()),
        })
    }

    /// How many blobs are being read for waiting callers right now: for a gauge, and for
    /// tests.
    #[must_use]
    pub fn reads_in_flight(&self) -> usize {
        self.flights.len()
    }

    /// The map now, or UNAVAILABLE before the first one.
    pub(crate) fn view(&self) -> Result<MapView, StorageError> {
        self.maps
            .current()
            .ok_or_else(|| unavailable("no storage map has arrived yet".to_owned()))
    }

    /// Runs `op` against the map as it is now. If a node answers `STALE_MAP`, fetches the
    /// newer map and runs it once more against that; a second `STALE_MAP`, or a fetch
    /// that does not bring a newer map, is UNAVAILABLE rather than a loop.
    pub(crate) async fn routed<T, Fut>(
        &self,
        op: impl Fn(MapView) -> Fut,
    ) -> Result<T, StorageError>
    where
        Fut: Future<Output = Result<T, StorageError>>,
    {
        let view = self.view()?;
        let used = view.map.generation();
        let first = op(view).await;
        let Err(StorageError::StaleMap { current }) = first else {
            return first;
        };
        let fresh = self
            .maps
            .refetch(current)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        if fresh.map.generation() <= used {
            return Err(unavailable(format!(
                "a node holds map generation {current}, but the newest map is {}",
                fresh.map.generation()
            )));
        }
        match op(fresh).await {
            Err(StorageError::StaleMap { current }) => Err(unavailable(format!(
                "a node holds map generation {current}, newer than the map just fetched"
            ))),
            done => done,
        }
    }
}

/// This generation's read order of `digest`, then the previous generation's.
pub(crate) fn read_order<'a>(view: &'a MapView, digest: &Digest) -> Vec<&'a Member> {
    view.map
        .read_order_with_previous(view.previous.as_deref(), digest)
}

/// The owners a write of `digest` may go to: its write owners that are not down, primary
/// first. A `Down` member keeps its slot in the map for the dwell; a caller skips it.
pub(crate) fn write_order<'a>(map: &'a Map, digest: &Digest) -> Vec<&'a Member> {
    map.write_owners(digest)
        .into_iter()
        .filter_map(|id| map.member(id))
        .filter(|m| m.state != MemberState::Down)
        .collect()
}

/// An outage of the routing itself.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Routing(String);

pub(crate) fn unavailable(message: String) -> StorageError {
    StorageError::Unavailable(Arc::new(Routing(message)))
}
