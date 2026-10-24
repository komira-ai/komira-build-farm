//! What a frontend takes in from the other frontends of its state store: the intake poll.
//!
//! Several frontends may share one store. Each schedules onto the daemons connected to it,
//! and the store, written conditionally ([`transitions`](super::transitions)), decides
//! which of them runs an operation. A frontend learns what the others did by looking at the
//! store every [`INTAKE_POLL`] ([`Farm::intake`]):
//!
//! - **Epoch.** It reads the epoch row again. A frontend serves the epoch it started under
//!   (the operator restarts frontends after a bump, as `docs/deploy/kbf-units.md` says), so
//!   a stored epoch past the served one is only reported ([`Farm::epoch_ahead`]).
//! - **Queue.** An operation another frontend queued is taken in ([`State::take_in_stored`]),
//!   with the `loss/`, `fate/`, `subject/` and size-key rows the store holds now (unless it
//!   holds a lease by then: its node's frontend owns it). An operation held queued whose
//!   `queue/` row is gone has been granted, finished or refused by another frontend: it
//!   leaves the scheduler (nothing is recorded as given up) and its callers are kept, to be
//!   answered from the store. One whose `queue/` row is a newer write than the one read
//!   before was granted and queued again meanwhile, and is taken in again with the rows of
//!   now. An operation whose requeue the store has not taken yet has no `queue/` row and
//!   stays.
//! - **Callers.** The callers of an operation another frontend took are shown what the store
//!   says of it each poll: executing while it holds a lease, and its result once the `done/`
//!   row is there ([`State::follow_elsewhere`]); the callers of a result or a refusal this
//!   frontend could not write (it lost the race, or the node moved to another frontend)
//!   follow it the same way. They are kept for the finished retention after, as the callers
//!   of an operation this frontend finished are. A recorded call among them is ended in the
//!   invocation record when the result is shown ([`State::record_followed`]), with no lease
//!   and no listing of the result.
//! - **Joiners.** The callers of a request that joined an operation leased on a node another
//!   frontend owns are followed the same way from the start ([`Held::Elsewhere`]): the
//!   scheduler never held the operation.
//! - **Watched operations.** The operations clients wait on or ask about by name that the
//!   scheduler does not hold are looked at each poll too ([`State::follow_watched`]).
//! - **A lost grant.** A grant this frontend wrote that the store refuses as stale was
//!   taken by another frontend: the placement is undone ([`kbf_sched::Scheduler::unplace`]),
//!   and the operation follows the store as above.
//! - **Operator state.** The `cordon/` rows other frontends wrote since the last poll are
//!   applied (a drain's deadline is on the store's clock, so frontends with clocks of their
//!   own agree on when it ends), and a memory floor raised past this frontend's is raised
//!   here too.
//! - **Orphans.** A lease whose node has no session anywhere is requeued after
//!   [`ORPHAN_WAIT`] (the `sweep` module).
//!
//! A frontend that is fenced by a retire or a drain's close, or whose store cannot be read, does
//! nothing. One whose intake is closed by a drain ([`Farm::close_intake`]) takes in no operation
//! of the others' queue and does the rest.

use std::collections::BTreeSet;

use kbf_sched::{Cordon, Event, Input};
use kbf_types::{StartLease, StateMachine};

use super::owner::owner_key;
use super::sweep::Orphan;
use super::*;
use crate::farm::handover::{cordon_event, note, show};

/// How often the intake poll looks at the state store: often enough that an operation
/// another frontend queued waits a fifth of a second for a frontend with room, and a
/// cordon an operator set through one frontend stops the grants of the others as soon,
/// and rare enough that the scans it makes (the queue, the cordons, the floors, the leases
/// and the sessions) are a small share of the store's load.
pub const INTAKE_POLL: Duration = Duration::from_millis(200);

/// What one look at the state store finds that the intake poll acts on.
pub(super) struct Polled {
    /// The epoch row.
    epoch: u64,
    /// The `queue/` rows: each queued operation, with the version of its row.
    queued: BTreeMap<OperationId, Version>,
    /// The `cordon/` rows.
    cordons: BTreeMap<WorkerId, StoredCordon>,
    /// The memory floors.
    floors: Vec<Floor>,
    /// The key floors (none unless the learned sizes are read).
    key_floors: Vec<KeyFloor>,
    /// The store's clock, in milliseconds.
    at: u64,
}

/// What a frontend keeps of the state store between two intake polls.
#[derive(Debug)]
pub(in crate::farm) struct Intake {
    /// The callers of operations another frontend took (or won the race to finish), by
    /// operation: the store answers them.
    elsewhere: BTreeMap<OperationId, Vec<WaiterId>>,
    /// The version of the `queue/` row of each operation held queued, as last read.
    queue_seen: BTreeMap<OperationId, Version>,
    /// The operations other frontends finished whose callers are kept, until when.
    kept: BTreeMap<OperationId, FarmTime>,
    /// The `cordon/` rows as the last poll (or the start) found them.
    pub(in crate::farm) cordons: BTreeMap<WorkerId, StoredCordon>,
    /// The leases whose node had no session row at the last sweep.
    orphans: BTreeMap<LeaseId, Orphan>,
    /// How long such a lease is left before it is requeued.
    orphan_wait: Duration,
    /// The stored epoch, if it is past the one this frontend serves.
    epoch_ahead: Option<u64>,
}

impl Default for Intake {
    fn default() -> Self {
        Self {
            elsewhere: BTreeMap::new(),
            queue_seen: BTreeMap::new(),
            kept: BTreeMap::new(),
            cordons: BTreeMap::new(),
            orphans: BTreeMap::new(),
            orphan_wait: ORPHAN_WAIT,
            epoch_ahead: None,
        }
    }
}

impl Intake {
    /// Whether the callers of the finished `operation` are kept: another frontend finished
    /// it, and its retention has not passed.
    pub(in crate::farm) fn keeps(&self, operation: OperationId) -> bool {
        self.kept.contains_key(&operation)
    }

    /// Forgets the version read of `operation`'s `queue/` row: this frontend wrote the row
    /// again (a lease it gave up), so a newer version is no other frontend's doing.
    pub(in crate::farm) fn rewrote(&mut self, operation: OperationId) {
        self.queue_seen.remove(&operation);
    }
}

#[cfg(test)]
impl Intake {
    /// How many operations' callers are followed.
    pub(super) fn following(&self) -> usize {
        self.elsewhere.len()
    }

    /// How many leases are remembered as orphans.
    pub(super) fn orphans_seen(&self) -> usize {
        self.orphans.len()
    }
}

impl Durable {
    /// The store's clock, in milliseconds.
    pub(in crate::farm) fn store_ms(&self) -> u64 {
        u64::try_from(self.store.now().as_duration().as_millis()).unwrap_or(u64::MAX)
    }

    /// Everything one intake poll reads.
    ///
    /// # Errors
    /// The store cannot be read, or a row read is not one the scheduler wrote.
    pub(super) fn poll(&self) -> Result<Polled, StateError> {
        Ok(Polled {
            epoch: self.counter(EPOCH)?,
            queued: self.queued()?,
            cordons: self.cordons()?.into_iter().collect(),
            floors: self.floors()?,
            key_floors: self.key_floors()?,
            at: self.store_ms(),
        })
    }

    /// The version of `operation`'s `queue/` row, if the operation is queued.
    pub(super) fn queue_version(
        &self,
        operation: OperationId,
    ) -> Result<Option<Version>, StateError> {
        Ok(self
            .store
            .get(&queue_key(operation))?
            .map(|row| row.version))
    }

    /// Every queued operation, with the version of its `queue/` row.
    fn queued(&self) -> Result<BTreeMap<OperationId, Version>, StateError> {
        let mut queued = BTreeMap::new();
        for (key, row) in self.store.scan_prefix("queue/")? {
            queued.insert(parse_operation(&key, "queue/")?, row.version);
        }
        Ok(queued)
    }

    /// Every memory floor the store holds.
    fn floors(&self) -> Result<Vec<Floor>, StateError> {
        let mut floors = Vec::new();
        for (key, row) in self.store.scan_prefix(FLOOR)? {
            floors.push(floor::decode(&row.value).map_err(|why| corrupt(&key, why))?);
        }
        Ok(floors)
    }

    /// Every key floor the store holds, if the learned sizes are read (a store that never
    /// had them on holds none, and with them off a row an earlier run left is not read).
    fn key_floors(&self) -> Result<Vec<KeyFloor>, StateError> {
        let read = self.read_size_keys.load(Ordering::SeqCst);
        let mut floors = Vec::new();
        for (key, row) in self
            .store
            .scan_prefix(KEY_FLOOR)?
            .into_iter()
            .filter(|_| read)
        {
            let (floor, _) = floor::decode_key(&row.value).map_err(|why| corrupt(&key, why))?;
            floors.push(floor);
        }
        Ok(floors)
    }

    /// Whether a frontend of the store has ever owned `worker`: its `owner/` row is there.
    pub(super) fn node_known(&self, worker: &WorkerId) -> Result<bool, StateError> {
        Ok(self.store.get(&owner_key(worker))?.is_some())
    }
}

impl Farm {
    /// One intake poll (see the module docs): takes in what the other frontends of the
    /// state store queued, forgets what they took, answers the callers of what they
    /// finished, applies the cordons and floors they set, and requeues the leases of nodes
    /// no frontend hears. Cheap when nothing changed. A farm with no state store, and one a
    /// retire fenced, do nothing; so does one whose store cannot be read.
    ///
    /// Placement follows at the next tick.
    pub fn intake(&self) {
        let now = self.now();
        self.lock().take_intake(now, self.lease_epoch);
    }

    /// Polls [`Farm::intake`] every [`INTAKE_POLL`], for as long as it is polled.
    pub async fn poll_intake(&self) {
        let mut tick = tokio::time::interval(INTAKE_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.intake();
        }
    }

    /// The epoch the state store holds, if it is past the one this frontend serves, as the
    /// last intake poll read it: the operator raised it ([`Farm::bump_epoch`]), and this
    /// frontend serves the epoch it started under until it is restarted.
    #[must_use]
    pub fn epoch_ahead(&self) -> Option<u64> {
        self.lock().intake.epoch_ahead
    }

    /// This farm, requeueing a lease whose node has no session row after `wait` (instead of
    /// [`ORPHAN_WAIT`]): for a test of the sweep over a store whose clock cannot be moved.
    #[doc(hidden)]
    #[must_use]
    pub fn with_orphan_wait(self, wait: Duration) -> Self {
        self.lock().intake.orphan_wait = wait;
        self
    }
}

impl State {
    /// [`Farm::intake`] under the lock.
    pub(in crate::farm) fn take_intake(&mut self, now: FarmTime, served: u64) {
        if self.fenced {
            return;
        }
        let mut polled = None;
        self.durably("an intake poll", |d| {
            polled = Some(d.poll()?);
            Ok(())
        });
        let Some(polled) = polled else {
            return;
        };
        self.note_epoch(polled.epoch, served);
        self.release_followed(now);
        self.take_queued(&polled.queued);
        self.follow_elsewhere(now);
        self.follow_watched();
        self.apply_cordons(now, polled.cordons, polled.at);
        self.apply_floors(&polled.floors, &polled.key_floors);
        self.sweep_orphans();
    }

    /// Reports the stored epoch `stored` once when it is past `served`.
    fn note_epoch(&mut self, stored: u64, served: u64) {
        let ahead = Some(stored).filter(|epoch| *epoch > served);
        if let Some(epoch) = ahead.filter(|_| ahead != self.intake.epoch_ahead) {
            tracing::warn!(
                stored = epoch,
                served,
                "the state store holds a newer epoch than this frontend serves: restart it"
            );
        }
        self.intake.epoch_ahead = ahead;
    }

    /// Forgets the callers of the operations other frontends finished whose retention has
    /// passed.
    fn release_followed(&mut self, now: FarmTime) {
        self.intake.kept.retain(|_, until| now < *until);
        self.forget_dropped();
    }

    /// Makes the scheduler's queue the store's: reconciles each operation held queued with
    /// its `queue/` row in `queued`, then takes in the queued operations it does not hold.
    fn take_queued(&mut self, queued: &BTreeMap<OperationId, Version>) {
        let held: Vec<OperationId> = self.sched.queued().collect();
        for operation in held {
            self.reconcile(operation, queued.get(&operation).copied());
        }
        // A farm whose intake is closed ([`Farm::close_intake`]) takes in nothing new.
        if !self.closed {
            for (&operation, &version) in queued {
                if !self.known_operation(operation) {
                    self.take_in(operation, version);
                }
            }
        }
        self.intake
            .queue_seen
            .retain(|operation, _| queued.contains_key(operation));
    }

    /// What the store says of `operation`, which the scheduler holds queued, given the
    /// version of its `queue/` row (`None` if it has none).
    ///
    /// No row: another frontend took it, and it leaves the scheduler; unless the scheduler
    /// gave a lease of it up and the store has not taken the requeue yet, when the row is
    /// still to come. A row that is not the one read before: the operation was granted and
    /// queued again, and is taken in again with the rows of now, unless the intake is closed:
    /// then it is only handed off.
    fn reconcile(&mut self, operation: OperationId, row: Option<Version>) {
        let Some(version) = row else {
            if !self.requeue_pending(operation) {
                self.hand_off(operation);
            }
            return;
        };
        let before = self.intake.queue_seen.insert(operation, version);
        if before.is_some_and(|before| before != version) {
            self.hand_off(operation);
            if !self.closed {
                self.take_in(operation, version);
            }
        }
    }

    /// Whether a lease of `operation` the scheduler gave up is not yet written to the store
    /// as a requeue: the operation has no `queue/` row for that reason.
    fn requeue_pending(&self, operation: OperationId) -> bool {
        self.unsettled
            .iter()
            .any(|(requeue, ..)| requeue.operation == operation)
    }

    /// Forgets `operation`, which another frontend took, and keeps its callers.
    fn hand_off(&mut self, operation: OperationId) {
        let callers = self.sched.forget_queued(operation).unwrap_or_default();
        self.keep_following(operation, callers);
    }

    /// Keeps `callers`, who wait for `operation`, to be answered from the store.
    pub(in crate::farm) fn keep_following(
        &mut self,
        operation: OperationId,
        callers: Vec<WaiterId>,
    ) {
        let followed = self.intake.elsewhere.entry(operation).or_default();
        followed.extend(callers);
    }

    /// Takes in the queued `operation`, whose `queue/` row is at `version`, with the rows
    /// the store holds of it now. An operation that has no `op/` row any more (it finished
    /// meanwhile) is not taken in, and neither is one that holds a lease now: another
    /// frontend granted it since the poll read the queue, and its lease is taken up only
    /// when its node says `Hello` here.
    pub(super) fn take_in(&mut self, operation: OperationId, version: Version) {
        let mut found = None;
        self.durably("a queued operation", |d| {
            found = d.stored_with(operation, None)?;
            Ok(())
        });
        found
            .filter(|stored| stored.lease.is_none())
            .into_iter()
            .for_each(|stored| {
                self.take_in_stored(stored);
                self.intake.queue_seen.insert(operation, version);
            });
    }

    /// Undoes what this frontend did for the operations of the effects the store refused as
    /// stale: another frontend's work stands in their place. A grant is undone, so the
    /// worker's booking is released and the scheduler does not keep a lease the store does
    /// not hold; the callers of a result or a refusal that lost are answered from the store.
    pub(in crate::farm) fn lost_race(&mut self, dropped: &VecDeque<Effect>) {
        for effect in dropped {
            if let Effect::Start(start) = effect {
                self.lose_grant(start);
            }
        }
        self.follow_finishes(dropped);
    }

    /// Keeps the callers of the results and refusals among the `dropped` effects, to be
    /// answered from the store: a finish that was not written (the daemon moved to another
    /// frontend, or another frontend finished the operation) is the store's to tell.
    pub(in crate::farm) fn follow_finishes(&mut self, dropped: &VecDeque<Effect>) {
        for effect in dropped {
            let ended = match effect {
                Effect::Answer(answer) => Some((answer.operation, answer.waiters.clone())),
                Effect::Refuse(refusal) => Some((refusal.operation, refusal.waiters.clone())),
                _ => None,
            };
            if let Some((operation, callers)) = ended {
                self.keep_following(operation, callers);
            }
        }
    }

    /// The grant `start` lost to another frontend: the scheduler forgets the placement and
    /// asks the store where the operation stands.
    fn lose_grant(&mut self, start: &StartLease) {
        let unplaced = self.sched.unplace(start.lease);
        unplaced
            .into_iter()
            .for_each(|operation| self.undone(operation, start));
    }

    /// `operation`, whose grant `start` was undone: asks the store where it stands. A row
    /// that cannot be read is looked at again by the next poll.
    fn undone(&mut self, operation: OperationId, start: &StartLease) {
        tracing::info!(
            %operation,
            lease = %start.lease,
            node = %start.worker,
            "another frontend took the operation: the placement is undone"
        );
        let mut looked = None;
        self.durably("a queue row", |d| {
            looked = Some(d.queue_version(operation)?);
            Ok(())
        });
        looked
            .into_iter()
            .for_each(|row| self.reconcile(operation, row));
    }

    /// Shows the callers kept for the operations other frontends took what the store says
    /// of each: executing while it holds a lease, its result once it has one. The callers
    /// of an operation that ended, or whose own streams are all gone, are kept for the
    /// finished retention and then forgotten.
    fn follow_elsewhere(&mut self, now: FarmTime) {
        for (operation, callers) in std::mem::take(&mut self.intake.elsewhere) {
            let live = self.waiting_among(&callers);
            if live.is_empty() {
                self.retire(operation, callers, now);
                continue;
            }
            let mut looked = None;
            self.durably("a followed operation", |d| {
                looked = Some(d.progress(operation)?);
                Ok(())
            });
            let untraced = &mut self.untraced_kept;
            let stage = looked.and_then(|progress| note(untraced, operation, &progress));
            if let Some(stage) = &stage {
                for channel in live.iter().filter_map(|caller| self.stages.get(caller)) {
                    show(channel, stage.clone());
                }
            }
            if let Some(Stage::Done(finished)) = &stage {
                self.record_followed(operation, finished);
                self.retire(operation, callers, now);
            } else {
                self.intake.elsewhere.insert(operation, callers);
            }
        }
    }

    /// Ends the following of `operation`: its `callers` are kept for the finished
    /// retention, as the callers of an operation this frontend finished are.
    fn retire(&mut self, operation: OperationId, callers: Vec<WaiterId>, now: FarmTime) {
        self.untraced_kept.remove(&operation);
        let until = now.saturating_add(self.finished_retention);
        self.intake.kept.insert(operation, until);
        self.machine.keep_finished(operation, callers);
        self.forget_dropped();
    }

    /// Places the nodes as the `cordon/` rows `rows` say now, `at` milliseconds on the
    /// store's clock: a row that is not as the last poll found it is applied, and a node
    /// whose row is gone returns to placement.
    fn apply_cordons(&mut self, now: FarmTime, rows: BTreeMap<WorkerId, StoredCordon>, at: u64) {
        let seen = std::mem::replace(&mut self.intake.cordons, rows.clone());
        for (worker, cordon) in &rows {
            if seen.get(worker) != Some(cordon) {
                tracing::info!(%worker, ?cordon, "another frontend placed the node");
                self.feed_quiet(now, cordon_event(worker.clone(), *cordon, now, at));
            }
        }
        for worker in seen.keys().filter(|worker| !rows.contains_key(*worker)) {
            tracing::info!(%worker, "another frontend returned the node to placement");
            let worker = worker.clone();
            self.feed_quiet(now, Event::Uncordon { worker });
        }
    }

    /// Puts `worker` back in the drain its `cordon/` row says, if the drain ended for want of
    /// a lease. A start takes up no leased operation, so a drain taken up at the start finds
    /// its node holding no lease and ends; the node's leases are taken up when it says
    /// `Hello` ([`State::claim_worker`], which calls this after taking them up), and a node
    /// that runs work is not drained.
    pub(super) fn resume_drain(&mut self, now: FarmTime, worker: &WorkerId) {
        if self.sched.cordon(worker) != Some(&Cordon::Drained) {
            return;
        }
        let mut stored = None;
        self.durably("a cordon look", |d| {
            stored = Some((d.cordons()?, d.store_ms()));
            Ok(())
        });
        let drain = stored.and_then(|(rows, at)| {
            let row = rows.into_iter().find(|(named, _)| named == worker);
            row.map(|(named, cordon)| cordon_event(named, cordon, now, at))
        });
        drain.into_iter().for_each(|event| {
            let none = self.sched.apply(Input::new(now, event));
            debug_assert!(none.is_empty(), "a cordon has no effect: {none:?}");
        });
    }

    /// Raises the memory floors the scheduler holds lower than the store's: another
    /// frontend's kills raised them.
    fn apply_floors(&mut self, floors: &[Floor], key_floors: &[KeyFloor]) {
        for floor in floors {
            self.sched.raise_floor(floor);
        }
        for floor in key_floors {
            self.sched.raise_key_floor(floor);
        }
    }

    /// Whether the state store knows `worker`: a frontend has owned it. An operator may
    /// place a node connected to another frontend through this one.
    pub(in crate::farm) fn known_elsewhere(&self, worker: &WorkerId) -> bool {
        let mut known = false;
        self.durably("a node look", |d| {
            known = d.node_known(worker)?;
            Ok(())
        });
        known
    }

    /// Requeues the leases whose node has no session row anywhere (see the `sweep` module).
    fn sweep_orphans(&mut self) {
        let connected: BTreeSet<WorkerId> = self
            .links
            .iter()
            .filter(|(_, link)| !link.outbound.is_closed())
            .map(|(worker, _)| worker.clone())
            .collect();
        let wait = self.intake.orphan_wait;
        let mut orphans = std::mem::take(&mut self.intake.orphans);
        self.durably("an orphan sweep", |d| {
            d.sweep(wait, &connected, &mut orphans)
        });
        self.intake.orphans = orphans;
    }
}
