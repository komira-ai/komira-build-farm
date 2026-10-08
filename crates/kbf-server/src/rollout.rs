//! Rollouts on the server: where the record is kept ([`RolloutStore`]) and the driver
//! that moves nodes through it ([`RolloutDriver`]), as far as `applying`
//! (`docs/design/fleet-updates.md` sections 4.1 and 4.2). The record and its rules are
//! `kbf_types::Rollout`.
//!
//! **The record is written before anything moves.** Each step is recorded through the
//! store first, and only then does the driver act on the node (cordon, drain, hand the
//! update over). A store that refuses the write leaves the node untouched.
//!
//! **Where it is kept.** [`MemoryRolloutStore`] keeps records in this process, so a
//! restart forgets them, as it forgets the scheduler's state. The control log is not
//! replicated yet (`crate::farm`); the store that writes each step to disk, or to the
//! Raft log once it is wired, is the next slice, behind the same trait. Rollouts that
//! touch real nodes (phases P2 and P3) wait for it.
//!
//! **Where the driver stops.** It cordons up to `max_unavailable` nodes, drains each,
//! and once a node is drained records `applying` and hands the update to an
//! [`Applier`]. What follows (`rebooting`, `qualifying`, `done`) needs `kbf-updater`
//! and the node's own report, which are later slices. A drain that pauses at its
//! deadline, a node returned to placement by someone else, or a refused hand-over holds
//! the node and the rollout: nothing proceeds by itself (section 4.3). The canary,
//! soak, `min_serving` and per-pool slots are phase P2.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use kbf_front::MetaLog;
use kbf_objstore::ObjectStore;
use kbf_types::{IllegalStep, NodeStep, Rollout, RolloutId, RolloutState, WorkerId};

use crate::farm::{Farm, NodeAction};
use crate::fleet::PlacementView;

/// Why a store did not keep a change.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// A rollout with this id is already recorded.
    #[error("{0} is already recorded")]
    Exists(RolloutId),
    /// No rollout with this id is recorded.
    #[error("no {0} is recorded")]
    Unknown(RolloutId),
    /// The rollout rules refuse the change.
    #[error(transparent)]
    Illegal(#[from] IllegalStep),
    /// The store could not write.
    #[error("the rollout store could not write: {0}")]
    Unavailable(String),
}

/// A change to one rollout, applied to a copy and kept only if it succeeds.
pub type Change<'a> = &'a dyn Fn(&mut Rollout) -> Result<(), IllegalStep>;

/// Where rollout records are kept. A write that returns `Ok` is kept; one that returns
/// an error changed nothing.
pub trait RolloutStore: Send + Sync {
    /// Records a new rollout.
    ///
    /// # Errors
    /// Its id is taken, or the store cannot write.
    fn create(&self, rollout: Rollout) -> Result<(), StoreError>;

    /// The rollout `id`, if recorded.
    fn get(&self, id: RolloutId) -> Option<Rollout>;

    /// Applies `change` to rollout `id` and keeps the result, all or nothing.
    ///
    /// # Errors
    /// `id` is unknown, the rules refuse the change, or the store cannot write.
    fn update(&self, id: RolloutId, change: Change<'_>) -> Result<Rollout, StoreError>;
}

/// Rollout records in this process: gone after a restart (see the module docs).
#[derive(Debug, Default)]
pub struct MemoryRolloutStore {
    rollouts: Mutex<BTreeMap<RolloutId, Rollout>>,
}

impl MemoryRolloutStore {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<RolloutId, Rollout>> {
        self.rollouts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl RolloutStore for MemoryRolloutStore {
    fn create(&self, rollout: Rollout) -> Result<(), StoreError> {
        let mut rollouts = self.lock();
        if rollouts.contains_key(&rollout.id) {
            return Err(StoreError::Exists(rollout.id));
        }
        rollouts.insert(rollout.id, rollout);
        Ok(())
    }

    fn get(&self, id: RolloutId) -> Option<Rollout> {
        self.lock().get(&id).cloned()
    }

    fn update(&self, id: RolloutId, change: Change<'_>) -> Result<Rollout, StoreError> {
        let mut rollouts = self.lock();
        let stored = rollouts.get_mut(&id).ok_or(StoreError::Unknown(id))?;
        let mut next = stored.clone();
        change(&mut next)?;
        stored.clone_from(&next);
        Ok(next)
    }
}

/// What the driver asks of the fleet: cordon or drain a node, and where it is.
pub trait Fleet {
    /// Carries out `action` on `node`.
    ///
    /// # Errors
    /// The node is unknown.
    fn place(&self, node: &WorkerId, action: NodeAction) -> Result<(), String>;

    /// Where `node` is in placement, if it is known.
    fn placement(&self, node: &WorkerId) -> Option<PlacementView>;
}

impl<M: MetaLog, O: ObjectStore> Fleet for Farm<M, O> {
    fn place(&self, node: &WorkerId, action: NodeAction) -> Result<(), String> {
        Farm::place(self, node, action)
            .map(drop)
            .map_err(|e| e.to_string())
    }

    fn placement(&self, node: &WorkerId) -> Option<PlacementView> {
        self.node_view(node).map(|view| view.placement)
    }
}

/// Hands a drained node its update. The steps after the hand-over are later slices.
pub trait Applier {
    /// Hands `target` (a software set digest) to `node`.
    ///
    /// # Errors
    /// The hand-over was refused; the node is held.
    fn apply(&self, node: &WorkerId, target: &str) -> Result<(), String>;
}

/// Why the driver stopped.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DriveError {
    /// The store did not keep a step; nothing was done to the node it named.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Moves rollouts' nodes on, one call of [`Self::step`] at a time.
pub struct RolloutDriver<'a> {
    store: &'a dyn RolloutStore,
    fleet: &'a dyn Fleet,
    applier: &'a dyn Applier,
}

impl<'a> RolloutDriver<'a> {
    /// A driver keeping records in `store`, acting through `fleet` and `applier`.
    pub const fn new(
        store: &'a dyn RolloutStore,
        fleet: &'a dyn Fleet,
        applier: &'a dyn Applier,
    ) -> Self {
        Self {
            store,
            fleet,
            applier,
        }
    }

    /// Records `rollout` (pending) and starts it (running). No node moves until the
    /// next [`Self::step`].
    ///
    /// # Errors
    /// The store refuses it.
    pub fn start(&self, rollout: Rollout) -> Result<Rollout, DriveError> {
        let id = rollout.id;
        self.store.create(rollout)?;
        Ok(self
            .store
            .update(id, &|r| r.set_state(RolloutState::Running))?)
    }

    /// Moves rollout `id` on as far as it can now: drained nodes get their update,
    /// cordoned nodes are drained, and pending nodes are cordoned while fewer than
    /// `max_unavailable` are out. A node that cannot go on is held, and the rollout with
    /// it; then nothing more moves. A rollout that is not running is left as it is.
    ///
    /// # Errors
    /// The store refuses a step (`id` unknown, or a write failed).
    pub fn step(&self, id: RolloutId) -> Result<Rollout, DriveError> {
        let rollout = self.store.get(id).ok_or(StoreError::Unknown(id))?;
        if rollout.state() != RolloutState::Running {
            return Ok(rollout);
        }
        let at = |step: NodeStep| -> Vec<WorkerId> {
            let nodes = rollout.nodes().iter();
            nodes
                .filter(|(_, p)| p.step() == step)
                .map(|(n, _)| n.clone())
                .collect()
        };
        for node in at(NodeStep::Draining) {
            match self.fleet.placement(&node) {
                Some(PlacementView::Draining { .. }) => {}
                Some(PlacementView::Drained) => {
                    self.record(id, &node, NodeStep::Applying)?;
                    if let Err(why) = self.applier.apply(&node, &rollout.target) {
                        return self.hold(id, &node, &why);
                    }
                }
                other => return self.hold(id, &node, &format!("{other:?} while draining")),
            }
        }
        for node in at(NodeStep::Cordoned) {
            self.record(id, &node, NodeStep::Draining)?;
            let drain = NodeAction::Drain(rollout.strategy.drain_deadline);
            if let Err(why) = self.fleet.place(&node, drain) {
                return self.hold(id, &node, &why);
            }
        }
        let out = rollout
            .nodes()
            .values()
            .filter(|p| p.step().is_out())
            .count();
        let room = usize::try_from(rollout.strategy.max_unavailable).unwrap_or(usize::MAX);
        for node in at(NodeStep::Pending)
            .into_iter()
            .take(room.saturating_sub(out))
        {
            self.record(id, &node, NodeStep::Cordoned)?;
            if let Err(why) = self.fleet.place(&node, NodeAction::Cordon) {
                return self.hold(id, &node, &why);
            }
        }
        Ok(self.store.get(id).ok_or(StoreError::Unknown(id))?)
    }

    fn record(&self, id: RolloutId, node: &WorkerId, step: NodeStep) -> Result<(), DriveError> {
        self.store.update(id, &|r| r.advance(node, step))?;
        Ok(())
    }

    /// Holds `node` and the rollout. Nothing proceeds until an operator acts.
    fn hold(&self, id: RolloutId, node: &WorkerId, why: &str) -> Result<Rollout, DriveError> {
        tracing::warn!(rollout = %id, %node, why, "rollout held");
        Ok(self.store.update(id, &|r| {
            r.advance(node, NodeStep::Held)?;
            r.set_state(RolloutState::Held)
        })?)
    }
}
