//! The scheduler's state in the state store (`docs/design/architecture-v2.md`, sections
//! 4.2 and 4.4): every change is written through [`kbf_state::Store`] before it is
//! acknowledged, and a scheduler that starts over a store takes up what the store holds.
//!
//! Rows:
//!
//! - `op/<id>`: an unfinished operation's request, written at submit, deleted when it
//!   finishes or is refused;
//! - `queue/<id>`: present while the operation waits for a lease. A grant requires it
//!   and deletes it, so one operation never holds two live leases;
//! - `lease/<lease>`: the fencing token of a committed grant, present while the
//!   operation holds the lease. A result is accepted only while it is there, which is the
//!   compare-and-set on the operation's lease;
//! - `session/<worker>`: what the scheduler knows of a daemon's newest session (its
//!   instance, capacity and capabilities). It expires unless renewed: one write per
//!   session per [`SESSION_RENEWAL`], however many leases the daemon holds (a lease row
//!   is written at the grant and at the end, never to renew it);
//! - `loss/<operation>`: how many of the operation's leases were lost, while it is
//!   unfinished and has lost any ([`kbf_sched::LOSS_REQUEUES`] refuses it);
//! - `fate/<operation>`: what the operation's kills for memory changed: the raised
//!   booking and the memory runs ([`kbf_sched::Fate`]), written in the transaction of the
//!   requeue that follows a kill and deleted with the operation's other rows, so a
//!   successor (after a handover or a crash) books the next rung instead of starting the
//!   ladder over. A store with no row for an operation books and counts as it did before
//!   the row existed;
//! - `done/<operation>`: how a finished or refused operation ended, kept for the finished
//!   retention (at least [`DONE_FLOOR`]) so a process that did not finish it can still
//!   answer the callers of it: a tag byte and the `ExecuteResponse` ([`kbf_front::Finished`]);
//! - `cordon/<worker>`: how an operator placed a node (cordoned, or draining until a
//!   wall-clock deadline), kept until it is returned to placement, so a successor
//!   process serves the node as the operator left it;
//! - `meta/epoch`, `meta/next_op`, `meta/next_seq`: the store incarnation and the first
//!   operation id and lease sequence no earlier start used;
//! - `meta/generation`: the writer generation ([`kbf_state::Fenced`]); every write of the
//!   scheduler is checked against it, and operation names carry it.
//!
//! Values are the `kbf-log` encoding of the scheduler's own inputs (a `Submit`, the
//! `Committed` lease, a `WorkerUp`), so the strict codec the replicated log already runs
//! decides what a row may hold.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::{Described, Finished, Stage};
use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{DaemonInstance, Fate, Recovered, RecoveredLease, Request, Requeue};
use kbf_state::{Cond, Conflict, GENERATION, Store, StoreError, Txn, Versioned};
use kbf_types::{
    ControlRecord, Effect, FarmTime, LeaseGrant, LeaseId, OperationId, StartLease, WaiterId,
    WorkerId,
};

use tokio::sync::watch;
use tonic::Status;

use super::{Farm, FarmMachine, Reported, Sent, Stamp, State, Waiter, process_term};
use crate::action_usage::Subject;
use crate::state_store::StateStore;

/// How often a daemon session's row in the store is renewed while the daemon heartbeats:
/// a third of the lease grace, so the row outlives two missed renewals. One write per
/// session per interval, whatever the number of leases it holds.
pub const SESSION_RENEWAL: Duration = Duration::from_secs(LEASE_GRACE.as_secs() / 3);

const EPOCH: &str = "meta/epoch";
const ALIVE: &str = "meta/alive";
const NEXT_OP: &str = "meta/next_op";
const NEXT_SEQ: &str = "meta/next_seq";
const CORDON: &str = "cordon/";
const FATE: &str = "fate/";

/// The least time a `done/` row is kept, whatever the finished retention: a process that
/// follows an operation it did not finish needs the row for as long as a stream it kept
/// open can take to notice it.
pub const DONE_FLOOR: Duration = Duration::from_secs(600);

/// What a start does with the store's epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochPolicy {
    /// Keep it: the scheduler resumes the store's incarnation, and a daemon that
    /// reconnects keeps the runs of its leases.
    Keep,
    /// Take the next one: the start is a new incarnation, and a daemon that sees it
    /// kills the runs of the leases it held before.
    Bump,
}

/// Why the state store could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The backend failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A row changed under the scheduler.
    #[error("a row changed under the scheduler: {0}")]
    Conflict(#[from] Conflict),
    /// The row a write depends on is not there: the write is stale (a result of a lease
    /// the operation no longer holds, a grant of an operation that is not queued).
    #[error("the state store has no {0}: the write is stale")]
    Missing(String),
    /// A row cannot be read.
    #[error("the state store row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
    /// A value cannot be encoded.
    #[error("a state store value cannot be encoded: {0}")]
    Encode(#[from] kbf_log::EncodeError),
    /// This build has no backend for the store's scheme.
    #[error("this build has no {0} state store backend")]
    NoBackend(&'static str),
    /// The store could not be opened: its file is held by another process or is not a
    /// state store, or its directory cannot be made.
    #[error("the state store cannot be opened: {0}")]
    Open(String),
}

/// What a store holds of the scheduler, as a start reads it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stored {
    /// The first operation id no earlier start used.
    pub next_operation: u64,
    /// The first lease sequence no earlier start used.
    pub next_seq: u64,
    /// Every unfinished operation, in id order.
    pub operations: Vec<StoredOperation>,
    /// Every node an operator placed, in node-id order.
    pub cordons: Vec<(WorkerId, StoredCordon)>,
}

/// How an operator placed a node, as its `cordon/` row keeps it: a tag byte, and for a
/// drain the deadline as eight bytes, big-endian, milliseconds since the Unix epoch (a
/// successor process has a clock of its own for farm time, and the wall clock is the
/// one both share).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StoredCordon {
    /// Placement skips the node.
    Cordoned,
    /// Placement skips the node and waits for its leases until the deadline.
    Draining { deadline_unix_ms: u64 },
}

impl StoredCordon {
    fn to_row(self) -> Vec<u8> {
        match self {
            Self::Cordoned => vec![0],
            Self::Draining { deadline_unix_ms } => {
                [&[1][..], &deadline_unix_ms.to_be_bytes()].concat()
            }
        }
    }

    fn from_row(key: &str, row: &[u8]) -> Result<Self, StateError> {
        match row {
            [0] => Ok(Self::Cordoned),
            [1, deadline @ ..] => {
                let deadline = <[u8; 8]>::try_from(deadline)
                    .map_err(|_| corrupt(key, "a drain deadline is not 8 bytes"))?;
                Ok(Self::Draining {
                    deadline_unix_ms: u64::from_be_bytes(deadline),
                })
            }
            _ => Err(corrupt(key, "not a cordon")),
        }
    }
}

/// Where an operation stands in the state store, as a process that did not run it sees it.
#[derive(Debug, PartialEq)]
pub(super) enum Progress {
    /// It ended, and this is how.
    Ended(Finished),
    /// It waits for a lease.
    Queued,
    /// It holds a lease.
    Running,
    /// The store has no trace of it: not unfinished and not ended. Its writer ends an
    /// operation in two transactions, so for a moment after the first this is also what
    /// a successful finish looks like.
    Gone,
}

impl Progress {
    pub(super) const fn is_gone(&self) -> bool {
        matches!(self, Self::Gone)
    }
}

/// An unfinished operation of a store.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StoredOperation {
    pub operation: OperationId,
    pub request: Request,
    /// The committed lease it holds.
    pub lease: Option<LeaseGrant>,
    /// How many of its leases were lost.
    pub losses: u32,
    /// What its kills for memory changed, if it was killed.
    pub fate: Option<Fate>,
}

impl StoredOperation {
    /// This operation as the scheduler takes it up, held for `waiter`.
    pub(super) fn recovered(self, waiter: WaiterId) -> Recovered {
        Recovered {
            operation: self.operation,
            request: self.request,
            waiter,
            lease: self.lease.map(|grant| RecoveredLease {
                lease: grant.lease,
                worker: grant.worker,
            }),
            losses: self.losses,
            fate: self.fate,
        }
    }
}

mod fate;

/// The scheduler's handle on its state store.
pub(super) struct Durable {
    store: Arc<dyn Store>,
    /// A write failed because the store could not be reached, and none has been taken
    /// since ([`State::watch_store`] tells the scheduler).
    pub(super) unreachable: AtomicBool,
    /// A write failed because another writer holds a newer generation of the store: this
    /// scheduler was superseded and takes no more work. Never cleared; unlike an outage,
    /// the store coming back does not end it.
    pub(super) fenced: AtomicBool,
}

impl std::fmt::Debug for Durable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Durable")
    }
}

fn op_key(operation: OperationId) -> String {
    format!("op/{:016x}", operation.0)
}

fn queue_key(operation: OperationId) -> String {
    format!("queue/{:016x}", operation.0)
}

fn loss_key(operation: OperationId) -> String {
    format!("loss/{:016x}", operation.0)
}

fn fate_key(operation: OperationId) -> String {
    format!("{FATE}{:016x}", operation.0)
}

fn done_key(operation: OperationId) -> String {
    format!("done/{:016x}", operation.0)
}

fn cordon_key(worker: &WorkerId) -> String {
    format!("{CORDON}{worker}")
}

fn lease_key(lease: LeaseId) -> String {
    format!("lease/{:016x}.{:016x}", lease.term, lease.seq)
}

fn corrupt(key: &str, why: &str) -> StateError {
    StateError::Corrupt {
        key: key.to_owned(),
        why: why.to_owned(),
    }
}

fn encode(input: FarmInput) -> Result<Vec<u8>, StateError> {
    let batch = FarmBatch {
        now: FarmTime::default(),
        wall: UNIX_EPOCH,
        inputs: vec![input],
    };
    Ok(kbf_log::encode(Format::V1, &LogCommand::Farm(batch))?)
}

fn decode(key: &str, bytes: &[u8]) -> Result<FarmInput, StateError> {
    match kbf_log::decode(Format::V1, bytes) {
        Ok(LogCommand::Farm(FarmBatch { mut inputs, .. })) if inputs.len() == 1 => {
            Ok(inputs.remove(0))
        }
        Ok(_) => Err(corrupt(key, "not a single scheduler input")),
        Err(e) => Err(corrupt(key, &e.to_string())),
    }
}

fn parse_operation(key: &str, prefix: &str) -> Result<OperationId, StateError> {
    let id = u64::from_str_radix(&key[prefix.len()..], 16)
        .map_err(|_| corrupt(key, "not an operation id"))?;
    Ok(OperationId(id))
}

fn number(key: &str, row: &Versioned) -> Result<u64, StateError> {
    let bytes = <[u8; 8]>::try_from(row.value.as_slice())
        .map_err(|_| corrupt(key, "not an 8-byte number"))?;
    Ok(u64::from_be_bytes(bytes))
}

impl Durable {
    /// Opens the scheduler's rows in `store` and settles its epoch: a store that has
    /// none gets a first one that no other store's epoch is likely to equal (a daemon
    /// must not mistake a new store for the one it knew), `policy` decides for one that
    /// has. Returns the handle and the epoch.
    pub(super) fn open(
        store: Arc<dyn Store>,
        policy: EpochPolicy,
        first: u64,
    ) -> Result<(Self, u64), StateError> {
        let row = store.get(EPOCH)?;
        let (epoch, write) = match (&row, policy) {
            (None, _) => (first, Some(Cond::Absent)),
            (Some(row), EpochPolicy::Keep) => (number(EPOCH, row)?, None),
            (Some(row), EpochPolicy::Bump) => {
                (number(EPOCH, row)? + 1, Some(Cond::Version(row.version)))
            }
        };
        let durable = Self {
            store,
            unreachable: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
        };
        if let Some(cond) = write {
            durable.commit(Txn::new().put(EPOCH, epoch.to_be_bytes(), cond))?;
        }
        Ok((durable, epoch))
    }

    /// The row at `key`, which the write that asks depends on.
    fn row(&self, key: &str) -> Result<Versioned, StateError> {
        let row = self.store.get(key)?;
        row.ok_or_else(|| StateError::Missing(key.to_owned()))
    }

    fn commit(&self, txn: Txn) -> Result<(), StateError> {
        self.store.txn(txn)??;
        Ok(())
    }

    /// What the store holds: the unfinished operations, each with its lease, and the
    /// first ids to hand out.
    pub(super) fn load(&self) -> Result<Stored, StateError> {
        let mut leases = std::collections::BTreeMap::new();
        for (key, row) in self.store.scan_prefix("lease/")? {
            let FarmInput::Committed(ControlRecord::Lease(grant)) = decode(&key, &row.value)?
            else {
                return Err(corrupt(&key, "not a lease grant"));
            };
            leases.insert(grant.operation, grant);
        }
        let mut losses = std::collections::BTreeMap::new();
        for (key, row) in self.store.scan_prefix("loss/")? {
            let operation = parse_operation(&key, "loss/")?;
            let count = u32::try_from(number(&key, &row)?)
                .map_err(|_| corrupt(&key, "not a loss count"))?;
            losses.insert(operation, count);
        }
        let mut fates = std::collections::BTreeMap::new();
        for (key, row) in self.store.scan_prefix(FATE)? {
            let operation = parse_operation(&key, FATE)?;
            let fate = fate::decode(&row.value).map_err(|why| corrupt(&key, why))?;
            fates.insert(operation, fate);
        }
        let mut operations = Vec::new();
        for (key, row) in self.store.scan_prefix("op/")? {
            let FarmInput::Submit(request) = decode(&key, &row.value)? else {
                return Err(corrupt(&key, "not a request"));
            };
            let operation = parse_operation(&key, "op/")?;
            let lease = leases.remove(&operation);
            operations.push(StoredOperation {
                operation,
                request,
                lease,
                losses: losses.remove(&operation).unwrap_or(0),
                fate: fates.remove(&operation),
            });
        }
        Ok(Stored {
            next_operation: self.counter(NEXT_OP)?,
            next_seq: self.counter(NEXT_SEQ)?,
            operations,
            cordons: self.cordons()?,
        })
    }

    /// The nodes an operator placed.
    fn cordons(&self) -> Result<Vec<(WorkerId, StoredCordon)>, StateError> {
        let rows = self.store.scan_prefix(CORDON)?;
        rows.into_iter()
            .map(|(key, row)| {
                let cordon = StoredCordon::from_row(&key, &row.value)?;
                Ok((WorkerId::new(&key[CORDON.len()..]), cordon))
            })
            .collect()
    }

    /// An operator placed `worker` as `cordon`, or returned it to placement (`None`).
    pub(super) fn cordoned(
        &self,
        worker: &WorkerId,
        cordon: Option<StoredCordon>,
    ) -> Result<(), StateError> {
        let key = cordon_key(worker);
        let txn = match cordon {
            Some(cordon) => Txn::new().put(key, cordon.to_row(), Cond::Any),
            None => Txn::new().delete(key, Cond::Any),
        };
        self.commit(txn)
    }

    /// Where `operation` stands.
    pub(super) fn progress(&self, operation: OperationId) -> Result<Progress, StateError> {
        if let Some(finished) = self.ended(operation)? {
            return Ok(Progress::Ended(finished));
        }
        let unfinished = self.store.get(&op_key(operation))?.is_some();
        let queued = self.store.get(&queue_key(operation))?.is_some();
        Ok(match (unfinished, queued) {
            (false, _) => Progress::Gone,
            (true, true) => Progress::Queued,
            (true, false) => Progress::Running,
        })
    }

    fn counter(&self, key: &str) -> Result<u64, StateError> {
        self.store.get(key)?.map_or(Ok(0), |row| number(key, &row))
    }

    /// An operation submitted: its request, and its place in the queue.
    pub(super) fn submitted(
        &self,
        operation: OperationId,
        request: &Request,
    ) -> Result<(), StateError> {
        let value = encode(FarmInput::Submit(request.clone()))?;
        self.commit(
            Txn::new()
                .put(op_key(operation), value, Cond::Absent)
                .put(queue_key(operation), Vec::new(), Cond::Absent)
                .put(NEXT_OP, (operation.0 + 1).to_be_bytes(), Cond::Any),
        )
    }

    /// A grant committed: the operation leaves the queue and holds `grant`'s lease. The
    /// operation must be queued, as the transaction reads it.
    fn granted(&self, start: &StartLease) -> Result<(), StateError> {
        let queued = queue_key(start.operation);
        let row = self.row(&queued)?;
        let grant = LeaseGrant {
            lease: start.lease,
            operation: start.operation,
            worker: start.worker.clone(),
        };
        let value = encode(FarmInput::Committed(ControlRecord::Lease(grant)))?;
        self.commit(
            Txn::new()
                .delete(queued, Cond::Version(row.version))
                .put(lease_key(start.lease), value, Cond::Absent)
                .put(NEXT_SEQ, (start.lease.seq + 1).to_be_bytes(), Cond::Any),
        )
    }

    /// A lease given up: the operation holds nothing and is queued again. Repeatable:
    /// the farm writes a give-up the store refused again until the store takes it, so
    /// the lease row may be gone already, and the queue row may be there. An operation
    /// that has finished has nothing to put back.
    ///
    /// `losses` is the operation's count of lost leases, to write down if the give-up is
    /// one ([`kbf_sched::RequeueReason::is_loss`]). `fate` is what the kill changed, if
    /// the give-up is a kill for memory: it goes in the same transaction as the queue row.
    pub(super) fn requeued(
        &self,
        requeue: &Requeue,
        losses: Option<u32>,
        fate: Option<&Fate>,
    ) -> Result<(), StateError> {
        if self.store.get(&op_key(requeue.operation))?.is_none() {
            return Ok(());
        }
        let key = lease_key(requeue.lease);
        let mut txn = Txn::new();
        if let Some(row) = self.store.get(&key)? {
            txn = txn.delete(key, Cond::Version(row.version));
        }
        if let Some(count) = losses {
            let count = u64::from(count).to_be_bytes();
            txn = txn.put(loss_key(requeue.operation), count, Cond::Any);
        }
        if let Some(fate) = fate {
            let row = fate::encode(fate).ok_or_else(|| {
                corrupt(&fate_key(requeue.operation), "a fate is too large to write")
            })?;
            txn = txn.put(fate_key(requeue.operation), row, Cond::Any);
        }
        self.commit(txn.put(queue_key(requeue.operation), Vec::new(), Cond::Any))
    }

    /// A result accepted from `lease`: the operation is finished. Compare-and-set on
    /// the operation's lease: a result of a lease the operation no longer holds is
    /// refused, whatever was proposed.
    fn finished(&self, operation: OperationId, lease: LeaseId) -> Result<(), StateError> {
        let key = lease_key(lease);
        let row = self.row(&key)?;
        self.commit(
            Txn::new()
                .delete(key, Cond::Version(row.version))
                .delete(op_key(operation), Cond::Any)
                .delete(queue_key(operation), Cond::Any)
                .delete(loss_key(operation), Cond::Any)
                .delete(fate_key(operation), Cond::Any),
        )
    }

    /// A refusal committed: the operation is finished without a lease.
    fn refused(&self, operation: OperationId) -> Result<(), StateError> {
        self.commit(
            Txn::new()
                .delete(op_key(operation), Cond::Any)
                .delete(queue_key(operation), Cond::Any)
                .delete(loss_key(operation), Cond::Any)
                .delete(fate_key(operation), Cond::Any),
        )
    }

    /// Writes what the scheduler's effects of a committed record settle, in order.
    /// Nothing the effects carry (a `Start`, an answer) is carried out unless this
    /// returns `Ok`.
    pub(super) fn persist(&self, effects: &[Effect]) -> Result<(), StateError> {
        for effect in effects {
            match effect {
                Effect::Start(start) => self.granted(start)?,
                Effect::Answer(answer) => self.finished(answer.operation, answer.lease)?,
                Effect::Refuse(refusal) => self.refused(refusal.operation)?,
                Effect::Commit(_) | Effect::Waiting(_) => {}
            }
        }
        Ok(())
    }

    /// A daemon registered: its session row, as of the registration.
    pub(super) fn session(
        &self,
        worker: &WorkerId,
        instance: &DaemonInstance,
        predecessor_stopped: bool,
        reported: &Reported,
    ) -> Result<(), StateError> {
        let up = FarmInput::WorkerUp {
            worker: worker.clone(),
            instance: instance.clone(),
            predecessor_stopped,
            capacity: reported.capacity,
            caps: reported.caps.clone(),
        };
        let value = encode(up)?;
        let key = format!("session/{worker}");
        self.commit(Txn::new().put_ttl(key, value, LEASE_GRACE, Cond::Any))
    }

    /// Extends `worker`'s session row by the lease grace, from the store's clock. A
    /// daemon with no row (its registration was not written, or the row expired) is left
    /// for its next registration to write.
    pub(super) fn renew(&self, worker: &WorkerId) -> Result<(), StateError> {
        let key = format!("session/{worker}");
        let Some(row) = self.store.get(&key)? else {
            return Ok(());
        };
        let renewed = Txn::new().put_ttl(key, row.value, LEASE_GRACE, Cond::Version(row.version));
        self.commit(renewed)
    }

    /// The writer generation the store holds, if it has one (a store opened by
    /// [`crate::StateStoreUri::open`] always does).
    fn generation(&self) -> Result<Option<u64>, StateError> {
        let row = self.store.get(GENERATION)?;
        row.map(|row| number(GENERATION, &row)).transpose()
    }

    /// How `operation` ended, kept for `retention` (never less than [`DONE_FLOOR`]).
    pub(super) fn done(
        &self,
        operation: OperationId,
        finished: &Finished,
        retention: Duration,
    ) -> Result<(), StateError> {
        let ttl = retention.max(DONE_FLOOR);
        self.commit(Txn::new().put_ttl(done_key(operation), finished.to_row(), ttl, Cond::Any))
    }

    /// How `operation` ended, if its `done/` row is still there.
    fn ended(&self, operation: OperationId) -> Result<Option<Finished>, StateError> {
        let key = done_key(operation);
        let Some(row) = self.store.get(&key)? else {
            return Ok(None);
        };
        Finished::from_row(&row.value)
            .map(Some)
            .map_err(|why| corrupt(&key, &why))
    }

    /// Records what a refused write says about the store. Only a store that cannot be
    /// reached is an outage; a write that lost a compare-and-set, or found its row gone,
    /// is the store working; a write the store fenced means another writer holds a newer
    /// generation, which no return of the store undoes. Not generic, unlike
    /// [`State::durably`], so every caller's runs count toward the same function.
    fn note_failure(&self, error: &StateError) {
        if let StateError::Store(cause) = error {
            let flag = if matches!(cause, StoreError::Fenced) {
                &self.fenced
            } else {
                &self.unreachable
            };
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Whether a write was refused because a newer writer holds the store: nothing this
    /// handle writes can succeed any more.
    pub(super) fn superseded(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
    }

    /// A write that tells whether the store takes writes again.
    pub(super) fn ping(&self) -> Result<(), StateError> {
        self.commit(Txn::new().put(ALIVE, Vec::new(), Cond::Any))
    }
}

impl Farm {
    /// This farm, with `state` as the truth the scheduler keeps its state in, if given:
    /// every operation, lease grant, result and refusal is written to it before it is
    /// acknowledged, and the unfinished operations and leases it already holds are taken
    /// up again (see [`kbf_sched::Scheduler::recover`]; the callers they had are gone,
    /// and a client that asks for the action again joins the operation). With `None`
    /// the farm is as [`Farm::new`] made it: its state is in memory.
    ///
    /// The term and the lease epoch `Welcome` names are the store's epoch, which
    /// [`StateStore::epoch`] decides: kept, a daemon that reconnects keeps the runs of
    /// its leases; bumped, it kills them. Call it before the farm serves anything.
    ///
    /// # Errors
    /// The store cannot be read, or holds a row that is not one the scheduler wrote.
    pub fn with_state(mut self, state: Option<StateStore>) -> Result<Self, StateError> {
        let state_of_farm = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        let epoch = state_of_farm.keep_in(state)?;
        self.term = epoch.unwrap_or(self.term);
        self.lease_epoch = epoch.unwrap_or(self.lease_epoch);
        Ok(self)
    }
}

impl Farm {
    /// How `operation` ended, as the state store keeps it: `None` if the farm has no
    /// state store, or the operation has no `done/` row (it has not finished, or its row
    /// outlived the finished retention and the floor). The row is the answer a process
    /// that did not finish the operation gives the callers it kept waiting.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not a result.
    pub fn ended(&self, operation: OperationId) -> Result<Option<Finished>, StateError> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .durable
            .as_ref()
            .map_or(Ok(None), |d| d.ended(operation))
    }
}

impl State {
    /// Runs `write` on the state store, if the farm has one. Whether it took: a
    /// refusal is logged here, and the caller does not acknowledge what depends on it.
    pub(super) fn durably(
        &self,
        what: &str,
        write: impl FnOnce(&Durable) -> Result<(), StateError>,
    ) -> bool {
        let Some(durable) = &self.durable else {
            return true;
        };
        match write(durable) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(%error, "the state store did not take {what}");
                durable.note_failure(&error);
                false
            }
        }
    }

    /// Tells the scheduler when the state store went away and when it came back (see
    /// [`kbf_sched::Scheduler::store_unavailable`]). A write that could not reach it
    /// raises the flag; while it is raised each feed tries one write, and the first that
    /// takes ends the outage. Called at the start of every feed, so no lease is judged
    /// expired by a tick that comes before the farm knows the store is back.
    pub(super) fn watch_store(&mut self, now: FarmTime) {
        let Some(durable) = &self.durable else {
            return;
        };
        if durable.fenced.load(Ordering::SeqCst) {
            // Superseded: the store is reachable but this scheduler may not write it, so
            // it stays unavailable (no grant, no lease given up) and never pings back.
            if !self.store_down {
                tracing::error!("the state store has a newer writer: this scheduler takes no work");
                self.store_down = true;
                self.sched.store_unavailable();
            }
            return;
        }
        if durable.unreachable.load(Ordering::SeqCst) && !self.store_down {
            tracing::warn!("the state store is unavailable: no lease is given up or granted");
            self.store_down = true;
            self.sched.store_unavailable();
        }
        if self.store_down && durable.ping().is_ok() {
            tracing::info!("the state store is back: expiry waits for the lease grace");
            durable.unreachable.store(false, Ordering::SeqCst);
            self.store_down = false;
            self.sched.store_available(now);
        }
    }

    /// Writes the leases the scheduler has just given up, after the ones the state store
    /// has not taken, oldest first, and keeps the ones it still refuses for the next
    /// feed. A grant needs the operation's queue row, which only its requeue writes, so
    /// a requeue the store missed is made up before the grant that follows it, or that
    /// grant is refused and the operation never runs.
    pub(super) fn settle_requeues(&mut self, given_up: &[Requeue]) {
        for requeue in given_up {
            let losses = if requeue.reason.is_loss() {
                self.sched.losses(requeue.operation)
            } else {
                None
            };
            // Taken now, not when the store takes the write: a later kill must not
            // reach an earlier requeue's row.
            let fate = if requeue.reason.is_memory_kill() {
                self.sched.fate(requeue.operation)
            } else {
                None
            };
            self.unsettled.push((requeue.clone(), losses, fate));
        }
        let pending = std::mem::take(&mut self.unsettled);
        let refused: Vec<_> = pending
            .into_iter()
            .filter(|(requeue, losses, fate)| {
                !self.durably("a requeue", |d| d.requeued(requeue, *losses, fate.as_ref()))
            })
            .collect();
        self.unsettled = refused;
    }

    /// Renews `worker`'s session row if its last write is a renewal interval old: one
    /// write per daemon session, never one per lease. Called for a heartbeat taken.
    pub(super) fn renew_session(&mut self, now: FarmTime, worker: &WorkerId) {
        // A fenced farm writes nothing: the successor's rows are the truth.
        if self.durable.is_none() || self.fenced {
            return;
        }
        let due = self
            .renewed
            .get(worker)
            .is_none_or(|at| now >= at.saturating_add(SESSION_RENEWAL));
        if due && self.durably("a session renewal", |d| d.renew(worker)) {
            self.renewed.insert(worker.clone(), now);
        }
    }

    /// [`Farm::with_state`] on the farm's state: opens `state`, reads what it holds and
    /// takes it up. Returns the store's epoch, or `None` if there is no store. Not
    /// generic, unlike `Farm::with_state`, so every test binary's runs count toward the
    /// same function's coverage.
    fn keep_in(&mut self, state: Option<StateStore>) -> Result<Option<u64>, StateError> {
        let Some(StateStore { store, epoch, .. }) = state else {
            return Ok(None);
        };
        let (durable, epoch) = Durable::open(store, epoch, process_term())?;
        let stored = durable.load()?;
        // Operation names carry the writer generation, not the epoch: a kept epoch must
        // not let a process name an operation another one named (issue #154). A store
        // handed in without a fence has no generation, and the epoch names as it did.
        self.machine = FarmMachine::new(durable.generation()?.unwrap_or(epoch));
        let now = SystemTime::now();
        let mut recovered = Vec::new();
        for op in stored.operations {
            let key = op.request.key.clone();
            let waiter = self.machine.submit(Waiter {
                key: key.clone(),
                kind: op.request.kind,
                do_not_cache: op.request.do_not_cache,
                queued: now,
                // The command the callers described is not kept: a recovered operation's
                // `action usage:` line names the empty shape.
                usage: Subject::of(Described::default()).0,
            });
            self.stages.insert(waiter, watch::channel(Stage::Queued).0);
            if let Some(grant) = &op.lease {
                let stamp = Stamp {
                    queued: now,
                    started: now,
                };
                let sent = Sent {
                    operation: op.operation,
                    action: key.action,
                    stamp,
                };
                self.machine.start(grant.lease, sent);
            }
            recovered.push(op.recovered(waiter));
        }
        let next_operation = OperationId(stored.next_operation);
        self.sched
            .recover(epoch, next_operation, stored.next_seq, recovered);
        self.restore_cordons(stored.cordons);
        self.durable = Some(durable);
        Ok(Some(epoch))
    }

    /// Writes `request`, about to be queued, to the state store, unless it will join a
    /// twin already there. Not generic, unlike `Farm::submit`, so every test binary's
    /// runs count toward the same function's coverage.
    ///
    /// # Errors
    /// UNAVAILABLE if the store did not take it: the request is not queued, and the
    /// client, who is told nothing is queued, asks again.
    pub(super) fn queue(&self, request: &Request) -> Result<(), Status> {
        let id = self.sched.next_operation();
        let joins = self.sched.twin(request).is_some();
        if !joins && !self.durably("a submission", |d| d.submitted(id, request)) {
            if self
                .durable
                .as_ref()
                .is_some_and(|d| d.fenced.load(Ordering::SeqCst))
            {
                return Err(Status::failed_precondition(
                    "this scheduler was superseded by a newer writer of the state store",
                ));
            }
            return Err(Status::unavailable(
                "the scheduler's state store did not take the request",
            ));
        }
        Ok(())
    }

    /// Writes the session row of a daemon that registered. Not generic, unlike
    /// `Farm::register`.
    pub(super) fn record_session(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        instance: &DaemonInstance,
        predecessor_stopped: bool,
        reported: &Reported,
    ) {
        self.durably("a session", |d| {
            d.session(worker, instance, predecessor_stopped, reported)
        });
        self.renewed.insert(worker.clone(), now);
    }
}

#[cfg(test)]
mod tests;
