//! The chain: who a write goes to under the map a node holds, and the call that carries
//! it to the replica.

use std::sync::Arc;
use std::time::Duration;

use kbf_map::{Member, MemberId, MemberState};
use kbf_types::Digest;

use tonic::transport::Channel;

use super::leg::{Leg, ReplicaLeg};
use super::peers::Peers;
use super::{MapSource, View, check};
use crate::StorageError;
use crate::repair::{Pending, RepairRecord, Repairs};

/// The most owners the chain serves: a primary and one replica.
const MAX_REPLICATION: usize = 2;

/// How a chain waits on its replica. Hang bounds, not speed: a replica that takes
/// longer is judged gone, however fast the disk it is slow to.
#[derive(Clone, Copy, Debug)]
pub struct ChainConfig {
    /// The longest the replica may take to accept one message of the blob.
    pub stall: Duration,
    /// The longest the replica may take to verify and commit the blob once it has all of
    /// it.
    pub commit: Duration,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(30),
            commit: Duration::from_secs(120),
        }
    }
}

/// What this node does with a write it is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Stores the blob here and nowhere else: the only owner (R = 1), or a replica
    /// receiving the copy its primary sent.
    Local,
    /// The primary: stores the blob here and at `replica`, and answers only when both
    /// hold it.
    Primary { replica: Member },
}

/// There are fewer write owners than the map's replication factor.
#[derive(Debug, thiserror::Error)]
#[error("the map has {owners} write owners for this blob and a replication factor of {r}")]
struct TooFewOwners {
    owners: usize,
    r: usize,
}

/// The node holds a map but has not registered, so does not know which member it is.
#[derive(Debug, thiserror::Error)]
#[error("this node has not been told its member id (it has not registered)")]
struct NotRegistered;

/// A replica the map says is down, and so is not called.
#[derive(Debug, thiserror::Error)]
#[error("the replica {0} is down")]
struct ReplicaDown(MemberId);

/// R = 2 chain replication on one node: the map it judges by, the channels to its peers,
/// and the journal of copies it owes.
pub struct Chain {
    source: Arc<dyn MapSource>,
    peers: Peers,
    repairs: Arc<Repairs>,
    config: ChainConfig,
}

impl Chain {
    /// A chain judging by `source`'s map, calling peers through `peers`, and recording the
    /// copies it could not make in `repairs`.
    #[must_use]
    pub fn new(
        source: Arc<dyn MapSource>,
        peers: Peers,
        repairs: Arc<Repairs>,
        config: ChainConfig,
    ) -> Self {
        Self {
            source,
            peers,
            repairs,
            config,
        }
    }

    /// The view this node holds now, if it has a map and knows which member it is.
    pub(crate) fn current(&self) -> Option<View> {
        let map = self.source.current()?;
        let me = self.source.me()?;
        Some(View { map, me })
    }

    /// The channel to the peer dialled at `addr`.
    ///
    /// # Errors
    /// [`StorageError::Internal`] if `addr` is not one a channel can be made to.
    pub(crate) fn peer(&self, addr: &str) -> Result<Channel, StorageError> {
        self.peers.channel(addr)
    }

    /// A copy of a blob of `size` bytes to `owner`, sent by this node whether or not it owns
    /// the blob.
    ///
    /// # Errors
    /// [`StorageError::Internal`] if `owner` has no address a channel can be made to.
    pub(crate) fn open_copy(
        &self,
        view: &View,
        owner: &Member,
        size: u64,
    ) -> Result<Leg, StorageError> {
        Ok(Leg::open(
            self.peers.channel(&owner.addr)?,
            view.map.generation(),
            view.me,
            size,
            self.config.stall,
            self.config.commit,
        ))
    }

    /// The map source, for a service that judges its other calls by it too.
    pub(crate) fn source(&self) -> Arc<dyn MapSource> {
        Arc::clone(&self.source)
    }

    /// The map a write routed by generation `requested` is judged by, with this node's
    /// member id.
    ///
    /// # Errors
    /// [`StorageError::StaleMap`], or [`StorageError::Unavailable`] for a caller ahead of
    /// the node, as [`crate::replicate`] says; [`StorageError::Unavailable`] if the node
    /// holds a map but has not been told which member it is.
    pub(crate) async fn view(
        &self,
        requested: u64,
        refetch_wait: Duration,
    ) -> Result<View, StorageError> {
        let map = check(&*self.source, requested, refetch_wait).await?;
        let me = self
            .source
            .me()
            .ok_or_else(|| StorageError::Unavailable(Arc::new(NotRegistered)))?;
        Ok(View { map, me })
    }

    /// What this node does with a write of `digest` under `view`, `leg_from` being the
    /// member that sent it as a replica's copy, if one did.
    ///
    /// # Errors
    /// [`StorageError::Invalid`] if this node is not where the map says the write goes: a
    /// client write to a node that is not the primary, a copy sent to a node that does not
    /// own the blob or by the node itself, or a replication factor above 2;
    /// [`StorageError::Unavailable`] if the map has fewer owners than its factor.
    pub(crate) fn plan(
        &self,
        view: &View,
        digest: &Digest,
        leg_from: Option<MemberId>,
    ) -> Result<Plan, StorageError> {
        let r = usize::from(view.map.r());
        if r > MAX_REPLICATION {
            return Err(StorageError::Invalid(format!(
                "the map's replication factor is {r}; chain replication serves {MAX_REPLICATION} \
                 at most"
            )));
        }
        let owners = view.map.write_owners(digest);
        if owners.len() < r {
            return Err(StorageError::Unavailable(Arc::new(TooFewOwners {
                owners: owners.len(),
                r,
            })));
        }
        let position = owners.iter().position(|owner| *owner == view.me);
        let member = |id: MemberId| {
            view.map
                .member(id)
                .cloned()
                .ok_or(StorageError::Internal("an owner is not in the map".into()))
        };
        match (leg_from, position, owners.as_slice()) {
            (Some(sender), Some(_), _) if sender != view.me => Ok(Plan::Local),
            (Some(sender), _, _) => Err(StorageError::Invalid(format!(
                "{view_me} was sent a copy of {digest} by {sender}, and does not own it",
                view_me = view.me
            ))),
            (None, Some(0), [_, replica]) => Ok(Plan::Primary {
                replica: member(*replica)?,
            }),
            // The primary is down: the next owner takes its place for the write, and the
            // primary is the copy owed.
            (None, Some(1), [first, _]) if member(*first)?.state == MemberState::Down => {
                Ok(Plan::Primary {
                    replica: member(*first)?,
                })
            }
            (None, Some(0), _) => Ok(Plan::Local),
            (None, _, _) => Err(StorageError::Invalid(format!(
                "{} is not the primary of {digest}",
                view.me
            ))),
        }
    }

    /// Whether the placement `plan` made for `digest` under `view` still holds: if the map
    /// turned while the blob was being written, the write is answered only when the new
    /// map places it the same way.
    ///
    /// # Errors
    /// [`StorageError::StaleMap`] naming the new generation if the placement changed.
    pub(crate) fn confirm(
        &self,
        view: &View,
        digest: &Digest,
        leg_from: Option<MemberId>,
        plan: &Plan,
    ) -> Result<(), StorageError> {
        match self.current() {
            Some(now) if now.map.generation() != view.map.generation() => {
                match self.plan(&now, digest, leg_from) {
                    Ok(ref placed) if placed == plan => Ok(()),
                    _ => Err(StorageError::StaleMap {
                        current: now.map.generation(),
                    }),
                }
            }
            _ => Ok(()),
        }
    }

    /// The call that carries a blob of `size` bytes to `replica`; for a replica the map says
    /// is down, one that was never made (it is the copy owed at once).
    ///
    /// # Errors
    /// [`StorageError::Internal`] if `replica` has no address a channel can be made to.
    pub(crate) fn open_leg(
        &self,
        view: &View,
        replica: &Member,
        size: u64,
    ) -> Result<ReplicaLeg, StorageError> {
        if replica.state == MemberState::Down {
            return Ok(ReplicaLeg::lost(StorageError::Unavailable(Arc::new(
                ReplicaDown(replica.id),
            ))));
        }
        let channel = self.peers.channel(&replica.addr)?;
        Ok(ReplicaLeg::open(
            channel,
            view.map.generation(),
            view.me,
            size,
            self.config.stall,
            self.config.commit,
        ))
    }

    /// The one place a write is answered with one copy: the copy owed to `replica` is
    /// recorded, durably, before this returns. A caller that does not wait for it, or
    /// goes on when it fails, would answer a write whose second copy nobody owes. The
    /// [`Pending`] returned is held until the blob is committed here (or the write ends
    /// without it): the repair worker leaves a debt alone while it is.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the record could not be made durable: the write is
    /// not answered.
    pub(crate) async fn degrade(
        &self,
        view: &View,
        replica: &Member,
        digest: &Digest,
        why: &StorageError,
    ) -> Result<Pending, StorageError> {
        tracing::warn!(%digest, replica = %replica.id, %why, "answering with one copy and a repair owed");
        self.repairs
            .owe(RepairRecord {
                digest: *digest,
                replica: replica.id,
                generation: view.map.generation(),
            })
            .await
    }
}
