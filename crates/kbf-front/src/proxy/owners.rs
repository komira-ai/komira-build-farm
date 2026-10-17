//! Which storage node owns a blob. A farm the frontend knows only from `--storage` is a
//! fixed list of nodes, every digest asking them in order ([`Owners::ranked`]). A farm
//! with a map is routed by it ([`Owners::routed`], architecture-v2 section 8): the nodes
//! of a digest are the ones the map names for it, and the whole-blob calls
//! ([`Owners::read_blob`], [`Owners::put`], [`Owners::find_missing`],
//! [`Owners::action_result`]) are the router's, with its generation tag, `STALE_MAP`
//! rule, digest check and failover (a write goes on to the blob's next write owner when the
//! first cannot be reached; [`Owners::writer_after`] is that for the raw upload). The list is the fallback while the map has fewer
//! than two members yet ([`Routed::active`]).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use kbf_proto::reapi::ActionResult;
use kbf_storage::Storage as _;
use kbf_storage::service::RemoteStorage;
use kbf_types::Digest;
use tonic::Status;
use tonic::transport::Channel;

use crate::routed::Routed;

/// One storage node: a name for logs and for telling two apart, and the connection.
#[derive(Clone, Debug)]
pub struct Owner {
    id: Arc<str>,
    pub(super) channel: Channel,
    pub(super) storage: RemoteStorage,
    /// The map generation every call to the node is tagged with: the one it was routed
    /// by, or 0 for a node known only from `--storage`.
    pub(super) generation: u64,
}

impl Owner {
    /// The node `channel` reaches, called `id`.
    #[must_use]
    pub fn new(id: &str, channel: Channel) -> Self {
        Self::at(id, channel, 0)
    }

    /// The node `channel` reaches, called `id`, as map generation `generation` places it.
    fn at(id: &str, channel: Channel, generation: u64) -> Self {
        Self {
            id: Arc::from(id),
            storage: RemoteStorage::new(channel.clone(), generation),
            channel,
            generation,
        }
    }

    /// The node, tagged with map `generation` from now on (on every clone of it).
    fn tagged(&self, generation: u64) -> Self {
        self.storage.set_map_generation(generation);
        Self {
            generation,
            ..self.clone()
        }
    }

    /// The node's name.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// The nodes a frontend calls, in the order a digest asks them. Cheap to clone; every
/// clone sees a [`replace`](Owners::replace).
#[derive(Clone, Debug)]
pub struct Owners {
    /// The nodes `--storage` names: every digest's, when there is no map to route by.
    nodes: Arc<RwLock<Arc<[Owner]>>>,
    routed: Option<Arc<Routed>>,
}

impl Owners {
    /// A farm of one node.
    #[must_use]
    pub fn single(owner: Owner) -> Self {
        Self::ranked(vec![owner])
    }

    /// These nodes, first the one a digest asks first, every digest asking them in this
    /// order.
    #[must_use]
    pub fn ranked(nodes: Vec<Owner>) -> Self {
        Self {
            nodes: Arc::new(RwLock::new(nodes.into())),
            routed: None,
        }
    }

    /// The nodes the map `routed` names for a digest, once it has two members or more;
    /// until then, `bootstrap` (the node `--storage` names).
    #[must_use]
    pub fn routed(routed: Arc<Routed>, bootstrap: Owner) -> Self {
        Self {
            routed: Some(routed),
            ..Self::single(bootstrap)
        }
    }

    /// The nodes now, as a map change makes them. A call in flight keeps the nodes it
    /// asked for, except an upload, which ends UNAVAILABLE if its primary is no longer
    /// first. Replaces the fixed list: the nodes of a routed farm change with its map.
    pub fn replace(&self, nodes: Vec<Owner>) {
        // A poisoned lock still holds a whole list: nobody writes half of one.
        *self
            .nodes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = nodes.into();
    }

    /// The router, when there is a map with two members or more to route by.
    fn routing(&self) -> Option<&Routed> {
        self.routed.as_deref().filter(|routed| routed.active())
    }

    /// The fixed list, each node tagged with the generation of the map held when there is
    /// a map to read it from: a node that holds a map refuses a call tagged 0.
    fn fixed(&self) -> Arc<[Owner]> {
        let nodes = Arc::clone(
            &self
                .nodes
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let Some(routed) = &self.routed else {
            return nodes;
        };
        let generation = routed.bootstrap_generation();
        nodes.iter().map(|owner| owner.tagged(generation)).collect()
    }

    /// The nodes that hold `digest`, in the order a read tries them: the map's, this
    /// generation's owners in rank order and then the previous generation's, or the fixed
    /// list. THE ROUTING SEAM for the raw ByteStream pass-through, which talks to the
    /// nodes itself; empty when every owner is down.
    #[must_use]
    pub fn for_digest(&self, digest: &Digest) -> Arc<[Owner]> {
        let Some(routed) = self.routing() else {
            return self.fixed();
        };
        routed.place(digest).map_or_else(
            |_| Arc::from(Vec::new()),
            |placement| {
                (placement.read.iter())
                    .filter_map(|member| owner_of(routed, member, placement.generation))
                    .collect()
            },
        )
    }

    /// The node an upload of `digest` goes to: the map's first owner that is not down
    /// (not necessarily the first a read asks), or the first of the fixed list. UNAVAILABLE
    /// when there is none.
    pub(super) fn writer(&self, digest: &Digest) -> Result<Owner, Status> {
        self.writer_after(digest, &[])
    }

    /// [`Owners::writer`] for an upload that has failed to reach the nodes `failed`: the
    /// first of the map's write owners that is not down and not one of them, which is
    /// where the router sends a write after its first owner cannot be reached
    /// ([`kbf_route::Router::put`]). UNAVAILABLE when there is none. Only a routed farm
    /// has a next owner: a fixed list of nodes is not a placement, and what one node of it
    /// holds another may not.
    pub(super) fn writer_after(
        &self,
        digest: &Digest,
        failed: &[Arc<str>],
    ) -> Result<Owner, Status> {
        let Some(routed) = self.routing() else {
            return match failed {
                [] => self.fixed().first().cloned().ok_or_else(no_storage),
                _ => Err(no_storage()),
            };
        };
        let placement = routed.place(digest)?;
        placement
            .writers
            .iter()
            .filter(|member| !failed.iter().any(|id| **id == *member.id.to_string()))
            // A member whose address is not one is passed over, as the router's write does.
            .find_map(|member| owner_of(routed, member, placement.generation))
            .ok_or_else(no_storage)
    }

    /// The whole blob from the first node that has it.
    pub(super) async fn read_blob(&self, digest: &Digest) -> Result<Bytes, Status> {
        if let Some(routed) = self.routing() {
            return Ok(routed.router().read_blob(digest).await?);
        }
        let mut last = no_storage();
        for owner in self.fixed().iter() {
            match owner.storage.read_blob(digest).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => last = e.into(),
            }
        }
        Err(last)
    }

    /// Stores `bytes`, which are the blob `digest`, on its writer.
    pub(super) async fn put(&self, digest: &Digest, bytes: Bytes) -> Result<(), Status> {
        if let Some(routed) = self.routing() {
            routed.router().put(bytes).await?;
            return Ok(());
        }
        self.writer(digest)?.storage.put(bytes).await?;
        Ok(())
    }

    /// The digests among `asked` the farm does not hold: each is asked of its primary,
    /// one call to each node that is some digest's primary.
    pub(super) async fn find_missing(&self, asked: &[Digest]) -> Result<BTreeSet<Digest>, Status> {
        if let Some(routed) = self.routing() {
            return Ok(routed
                .router()
                .find_missing(asked)
                .await?
                .into_iter()
                .collect());
        }
        let mut by_node: BTreeMap<String, (Owner, Vec<Digest>)> = BTreeMap::new();
        for digest in asked {
            let owner = self.writer(digest)?;
            by_node
                .entry(owner.id().to_owned())
                .or_insert_with(|| (owner, Vec::new()))
                .1
                .push(*digest);
        }
        let answers = futures::future::try_join_all(
            by_node
                .values()
                .map(|(owner, digests)| owner.storage.find_missing(digests)),
        )
        .await?;
        Ok(answers.into_iter().flatten().collect())
    }

    /// The action-cache entry for `action` and the blob it was read from, if an owner has
    /// one that passes its closure check.
    pub(super) async fn action_result(
        &self,
        action: &Digest,
    ) -> Result<Option<(Digest, ActionResult)>, Status> {
        if let Some(routed) = self.routing() {
            return Ok(routed.router().action_result(action).await?);
        }
        Ok(self.writer(action)?.storage.action_result(action).await?)
    }
}

/// The member as a node to call at map generation `generation`; none if its address is
/// not one.
fn owner_of(routed: &Routed, member: &kbf_map::Member, generation: u64) -> Option<Owner> {
    let channel = routed.channel(member).ok()?;
    Some(Owner::at(&member.id.to_string(), channel, generation))
}

/// The answer when no node holds a blob.
pub(super) fn no_storage() -> Status {
    Status::unavailable("the frontend has no storage node for this blob")
}
