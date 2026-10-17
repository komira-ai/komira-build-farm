//! The `update/*` rows in the state store (`docs/design/split-mode-updates.md` section 6.1):
//! what the controller resumes from, and what the operator's calls write.
//!
//! | key | value | written by |
//! |---|---|---|
//! | `update/desired` | `{generation, commit, set_at}`, and whether the run is paused ([`Desired`]) | an offer, a pause |
//! | `update/policy` | the eight settings, and the node step's limits when they are not the default ([`Policy`]) | the operator |
//! | `update/slot` | the places in use, `{host, order_id, epoch}` each ([`Slot`]): one storage, up to `max_hosts_out` others | the controller |
//! | `update/host/<id>` | `{phase, order_id, generation, since, boot_before}`, and the run's commit ([`HostRow`]) | the controller |
//!
//! and two counters: `update/generation`, from which generations are drawn, and
//! `update/order/<epoch>`, from which order ids are drawn. (The release rows belong to the
//! operator API.) No row has a time to live: a controller that dies leaves every row as it
//! was, and the next one resumes from them. Every write the controller makes is a
//! compare-and-set on the version read, so a controller that was overtaken (another one
//! moved the row) gets a [`RowError::Conflict`] and re-reads; the controller writes over no
//! row it did not read. The one write that is not conditional is the operator's:
//! [`Rows::set_policy`] replaces the policy whole.
//!
//! The rows are plain key-value rows of the state store, so they work on every backend and
//! need no table of their own; `kbf-state` is unchanged.

use std::collections::BTreeMap;
use std::sync::Arc;

use kbf_state::{Cond, Conflict, Store, StoreError, Txn, Version, Versioned};
use kbf_types::{FarmTime, WorkerId};

use super::codec::{
    Corrupt, Desired, HostRow, decode_desired, decode_host, decode_policy, decode_slot,
    encode_desired, encode_host, encode_policy, encode_slot,
};
use super::facts::{Generation, OrderId, Release};
use super::policy::{Policy, PolicyError};
use super::slot::{Holder, Slot, SlotClass};

const DESIRED: &str = "update/desired";
/// The counter the generations are drawn from.
const GENERATION: &str = "update/generation";
const POLICY: &str = "update/policy";
pub(super) const SLOT: &str = "update/slot";
const HOSTS: &str = "update/host/";
/// Reads and writes tried before a call gives up on writers it keeps racing.
const ATTEMPTS: usize = 16;

/// The key of host `id`'s row.
pub(super) fn host_key(id: &WorkerId) -> String {
    format!("{HOSTS}{id}")
}

/// The counter order ids of `epoch` are drawn from. A counter of its own per epoch: a store
/// restored from a backup has its counters back where they were, so the epoch (which a
/// restore moves forward) is what keeps its ids above the replaced store's.
fn order_counter(epoch: u64) -> String {
    format!("update/order/{epoch:016x}")
}

/// Why a row could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RowError {
    /// The store failed, or this handle is no longer its writer.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A row's bytes are not a row this build reads. The run does not guess.
    #[error("the row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
    /// A row changed since it was read: another controller moved. Read again.
    #[error("a row changed under the writer: {0}")]
    Conflict(#[from] Conflict),
    /// Other writers kept changing the row for as long as the call tried.
    #[error("{0} kept changing under the writer")]
    Contended(&'static str),
    /// The policy cannot be kept.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// An offer names no release.
    #[error("an offer must name a release")]
    NoRelease,
}

/// A row as it was read: its value, and the version a write of it must name. `version` is
/// `None` for a row that does not exist, which is written with "must be absent".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored<T> {
    /// The value; the default for a row that does not exist.
    pub value: T,
    /// The version read, or `None` if there was no row.
    pub version: Option<Version>,
}

impl<T> Stored<T> {
    /// The precondition that the row is still as it was read.
    #[must_use]
    pub fn cond(&self) -> Cond {
        self.version.map_or(Cond::Absent, Cond::Version)
    }
}

/// The update rows of one state store.
#[derive(Clone)]
pub struct Rows {
    store: Arc<dyn Store>,
}

impl std::fmt::Debug for Rows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rows")
    }
}

fn read<T>(
    key: &str,
    row: &Versioned,
    decode: impl FnOnce(&[u8]) -> Result<T, Corrupt>,
) -> Result<T, RowError> {
    decode(&row.value).map_err(|Corrupt(why)| RowError::Corrupt {
        key: key.to_owned(),
        why,
    })
}

impl Rows {
    /// The rows of `store`.
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    /// The run, if one was ever offered.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn desired(&self) -> Result<Stored<Option<Desired>>, RowError> {
        match self.store.get(DESIRED)? {
            None => Ok(Stored {
                value: None,
                version: None,
            }),
            Some(row) => Ok(Stored {
                value: Some(read(DESIRED, &row, decode_desired)?),
                version: Some(row.version),
            }),
        }
    }

    /// Sets the desired release to `commit`, under a higher generation, with the run not
    /// paused unless it was. A rollback is the older commit offered the same way (section
    /// 2.11). Setting the commit that is already desired changes nothing and keeps the
    /// generation (`pull-updates.md` section 5), so a repeated click is not a new run. The
    /// generation is drawn from the counter `update/generation`; it is never below the
    /// current run's plus one, whatever the counter says.
    ///
    /// A host that has done nothing irreversible is given back when the generation moves
    /// (see [`super::Controller`]); a host past the activate order finishes its step.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or `commit` is empty.
    pub fn offer(&self, commit: &Release, now: FarmTime) -> Result<Desired, RowError> {
        if commit.as_str().is_empty() {
            return Err(RowError::NoRelease);
        }
        for _ in 0..ATTEMPTS {
            let current = self.desired()?;
            if let Some(same) = current.value.as_ref().filter(|d| d.commit == *commit) {
                return Ok(same.clone());
            }
            let drawn = self.store.txn(Txn::new().next(GENERATION))??;
            let counter = drawn.counters.first().copied().ok_or_else(|| {
                StoreError::Failed("the store returned no value for a counter".to_owned())
            })?;
            let above = current
                .value
                .as_ref()
                .map_or(1, |d| d.generation.0.saturating_add(1));
            let next = Desired {
                generation: Generation(counter.max(above)),
                commit: commit.clone(),
                set_at: now,
                paused: current.value.as_ref().is_some_and(|d| d.paused),
            };
            let txn = Txn::new().put(DESIRED, encode_desired(&next), current.cond());
            if self.store.txn(txn)?.is_ok() {
                return Ok(next);
            }
        }
        Err(RowError::Contended(DESIRED))
    }

    /// Pauses or resumes the run: while paused no host is started and hosts in a step
    /// finish. Returns whether there is a run to pause.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn set_paused(&self, paused: bool) -> Result<bool, RowError> {
        for _ in 0..ATTEMPTS {
            let current = self.desired()?;
            let Some(desired) = &current.value else {
                return Ok(false);
            };
            let next = Desired {
                paused,
                ..desired.clone()
            };
            let txn = Txn::new().put(DESIRED, encode_desired(&next), current.cond());
            if self.store.txn(txn)?.is_ok() {
                return Ok(true);
            }
        }
        Err(RowError::Contended(DESIRED))
    }

    /// The policy: the row if there is one, else the design's defaults.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn policy(&self) -> Result<Policy, RowError> {
        match self.store.get(POLICY)? {
            None => Ok(Policy::default()),
            Some(row) => read(POLICY, &row, decode_policy),
        }
    }

    /// Writes the policy, after checking it can be kept. The row is replaced whole, whatever
    /// it held: the policy has one writer, the operator, and the controller only reads it.
    ///
    /// # Errors
    /// [`RowError::Policy`] for a policy that cannot be kept; otherwise the store's.
    pub fn set_policy(&self, policy: &Policy) -> Result<(), RowError> {
        policy.validate()?;
        self.store
            .txn(Txn::new().put(POLICY, encode_policy(policy), Cond::Any))??;
        Ok(())
    }

    /// The places in use.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn slot(&self) -> Result<Stored<Slot>, RowError> {
        match self.store.get(SLOT)? {
            None => Ok(Stored {
                value: Slot::default(),
                version: None,
            }),
            Some(row) => Ok(Stored {
                value: read(SLOT, &row, decode_slot)?,
                version: Some(row.version),
            }),
        }
    }

    /// Every host's row, by host.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or a row cannot be read.
    pub fn hosts(&self) -> Result<BTreeMap<WorkerId, Stored<HostRow>>, RowError> {
        let mut hosts = BTreeMap::new();
        for (key, row) in self.store.scan_prefix(HOSTS)? {
            let id = WorkerId::new(key.strip_prefix(HOSTS).unwrap_or(&key));
            let value = read(&key, &row, decode_host)?;
            hosts.insert(
                id,
                Stored {
                    value,
                    version: Some(row.version),
                },
            );
        }
        Ok(hosts)
    }

    /// Replaces the row of `host` if it is still `current`.
    ///
    /// # Errors
    /// [`RowError::Conflict`] when it is not (another controller moved: read again);
    /// otherwise the store's.
    pub fn put_host(
        &self,
        host: &WorkerId,
        current: &Stored<HostRow>,
        row: &HostRow,
    ) -> Result<(), RowError> {
        let txn = Txn::new().put(host_key(host), encode_host(row), current.cond());
        self.store.txn(txn)??;
        Ok(())
    }

    /// Replaces the slot if it is still `current`.
    ///
    /// # Errors
    /// [`RowError::Conflict`] when it is not; otherwise the store's.
    pub fn put_slot(&self, current: &Stored<Slot>, slot: &Slot) -> Result<(), RowError> {
        let txn = Txn::new().put(SLOT, encode_slot(slot), current.cond());
        self.store.txn(txn)??;
        Ok(())
    }

    /// Whether this handle can still write: a transaction that changes nothing. A handle
    /// that another writer has fenced out fails it, so a controller that was superseded
    /// gives no order that is not preceded by a write of its own, or by this (see
    /// `Controller`). It relies on the store judging the fence in every transaction, an
    /// empty one included, as [`kbf_state::Fenced`] does.
    ///
    /// # Errors
    /// [`RowError::Store`], [`StoreError::Fenced`] among them, when it cannot.
    pub fn check_writer(&self) -> Result<(), RowError> {
        self.store.txn(Txn::new())??;
        Ok(())
    }

    /// Draws the next order id of `epoch`. The counter is the store's, so the number is
    /// durable the moment it is returned and no caller, on this handle or another, draws it
    /// again; a number drawn and not used is skipped, never reused. Ids ascend within an
    /// epoch and the epoch ascends across a restore ([`OrderId`]).
    ///
    /// # Errors
    /// [`RowError::Store`] when the store fails.
    pub fn mint_order(&self, epoch: u64) -> Result<OrderId, RowError> {
        let commit = self.store.txn(Txn::new().next(order_counter(epoch)))??;
        let seq = commit.counters.first().copied().ok_or_else(|| {
            StoreError::Failed("the store returned no value for a counter".to_owned())
        })?;
        Ok(OrderId::new(epoch, seq))
    }

    /// Gives `host` a place of `class`, with the claim named by `order`, unless the class is
    /// full. Repeatable: a host that holds a place keeps it. Returns the slot as written, or
    /// `None` when the class is full.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or writers kept racing.
    pub fn claim(
        &self,
        class: SlotClass,
        host: &WorkerId,
        order: OrderId,
        max_hosts_out: usize,
    ) -> Result<Option<Slot>, RowError> {
        for _ in 0..ATTEMPTS {
            let current = self.slot()?;
            let mut slot = current.value.clone();
            let holder = Holder {
                host: host.clone(),
                order,
            };
            if !slot.claim(class, holder, max_hosts_out) {
                return Ok(None);
            }
            if slot == current.value {
                return Ok(Some(slot));
            }
            match self.put_slot(&current, &slot) {
                Ok(()) => return Ok(Some(slot)),
                // Another writer moved the slot: read it again, and judge the room by what
                // it holds now. A lost race is not a full class.
                Err(RowError::Conflict(_)) => {}
                Err(other) => return Err(other),
            }
        }
        Err(RowError::Contended(SLOT))
    }

    /// Frees the place `host` holds, if it holds one. Returns the slot as written.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or writers kept racing.
    pub fn release(&self, host: &WorkerId) -> Result<Slot, RowError> {
        self.release_where(host, |_| true)
    }

    /// Frees the place `host` holds if it holds it by the claim `order`, and leaves it
    /// alone if it holds one by another claim: a release decided from an older look must not
    /// free the place of a claim made since. Returns the slot as written.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or writers kept racing.
    pub fn release_claim(&self, host: &WorkerId, order: OrderId) -> Result<Slot, RowError> {
        self.release_where(host, |held| held.order == order)
    }

    fn release_where(
        &self,
        host: &WorkerId,
        wanted: impl Fn(&Holder) -> bool,
    ) -> Result<Slot, RowError> {
        for _ in 0..ATTEMPTS {
            let current = self.slot()?;
            let mut slot = current.value.clone();
            if !slot.holder(host).is_some_and(&wanted) || !slot.release(host) {
                return Ok(slot);
            }
            match self.put_slot(&current, &slot) {
                Ok(()) => return Ok(slot),
                Err(RowError::Conflict(_)) => {}
                Err(other) => return Err(other),
            }
        }
        Err(RowError::Contended(SLOT))
    }
}
