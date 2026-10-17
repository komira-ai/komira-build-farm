//! What the picker and the node step read: plain data a controller fills from the
//! `Farm`, `Members`, the hosts' heartbeats and the `update/*` rows. Nothing here reads
//! a clock or a store.
//!
//! **An absent metric is `None`, and `None` fails closed.** A lease count, a write
//! count, a repair debt, a placement or a boot counter the controller could not read is
//! never filled in as zero: every use below treats it as "not proven", so the gate it
//! feeds stays shut.
//!
//! **Facts may lag.** A heartbeat is up to a time to live old, and a member row can say
//! `Active` for a node whose update the record already started. So *what is out* comes
//! from the records ([`HostRecord`]) and the slot, which the controller wrote itself,
//! never from member state. A member that was not heard within its time to live counts as
//! down ([`MemberFacts::is_fresh`]); the host metrics carry no observation time, so the
//! controller fills them from the same heartbeat look it reads the members from.

use std::collections::BTreeSet;
use std::time::Duration;

use kbf_map::{MemberId, MemberState};
use kbf_types::{FarmTime, WorkerId};

use super::command::Command;
use super::gate::{GateFail, GateMode, storage_gate};
use super::node_step::HostPhase;
use super::policy::Policy;
use super::slot::{Slot, SlotClass};

/// A release, by whatever the controller compares: a commit or a software-set digest.
/// Only equality is defined, so a rollback is another target and never "an older one".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release(String);

impl Release {
    /// The release called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Its name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The generation of a run (`update/desired`): it grows with every change of the desired
/// release, and a rollback is the older release under a higher generation (section 2.11;
/// `pull-updates.md` section 5). A higher generation is a new run. A host row carries the
/// generation of the run that wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);

/// The id of an order to a host's helper: the store's epoch and a number drawn from a
/// counter of that epoch. A helper refuses a lower id than it has seen, and a status it
/// reports names the order it answers, so a status left over from an earlier update is never
/// read as the answer to this one (section 6.1).
///
/// The epoch comes first in the order, so a store restored from a backup, whose counters have
/// gone back, still issues ids above every id of the store it replaced: it opens at a later
/// epoch ([`super::Controller::new`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OrderId {
    /// The epoch of the store that issued it.
    pub epoch: u64,
    /// The number within the epoch.
    pub seq: u64,
}

impl OrderId {
    /// The id `seq` of `epoch`.
    #[must_use]
    pub const fn new(epoch: u64, seq: u64) -> Self {
        Self { epoch, seq }
    }

    /// The id after this one in the same epoch.
    #[must_use]
    pub const fn next(self) -> Self {
        Self {
            epoch: self.epoch,
            seq: self.seq.saturating_add(1),
        }
    }
}

/// What kind of Mac a host is: the class a queued operation asks for (`os`, hardware and
/// Xcode, as the platform match sees them).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlatformClass(String);

impl PlatformClass {
    /// The class called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

/// The operating system of a host, and for a Mac its class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Platform {
    /// A Linux host.
    Linux,
    /// A Mac of this class (section 2.10).
    Mac(PlatformClass),
}

/// What a host runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKind {
    /// A storage half and a daemon half in one process: all ten steps apply. Only a Linux
    /// host is one; the facts cannot say otherwise, but a controller should not build a
    /// `StorageAndDaemon` Mac.
    StorageAndDaemon,
    /// A daemon alone (the Macs, a cloud daemon group): no storage steps (section 2.6).
    DaemonOnly,
}

/// What the scheduler says a daemon is doing. Idle is all three at rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaemonLoad {
    /// Leases the host holds.
    pub leases: u32,
    /// Claims granted to the host that its daemon has not been sent (section 2.7: a lease
    /// is not over until it has been delivered and ended).
    pub undelivered: u32,
    /// A whole-machine hold, as a Mac's `Preparing` and `Restoring` phases take (section
    /// 2.10: a Mac with no leases may still be switching its session).
    pub whole_machine_hold: bool,
}

impl DaemonLoad {
    /// A host with nothing to wait for (section 2.7, daemon half).
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.leases == 0 && self.undelivered == 0 && !self.whole_machine_hold
    }

    /// The load D3 ranks hosts by: leases and undelivered claims. A whole-machine hold
    /// weighs as one lease.
    #[must_use]
    pub fn weight(&self) -> u64 {
        u64::from(self.leases) + u64::from(self.undelivered) + u64::from(self.whole_machine_hold)
    }
}

/// What the host's root helper last reported about the order it was given (section 2.6,
/// steps 6 and 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperStatus {
    /// `ACTIVATED_UNCONFIRMED`: flipped and restarted, the revert timer armed.
    ActivatedUnconfirmed,
    /// `CONFIRMED`: the revert timer is cancelled.
    Confirmed,
    /// `REVERTED`: the helper put the previous release back (it did so itself).
    Reverted,
    /// `REFUSED`: a revert that could not run, because the previous release is missing.
    Refused,
}

/// A helper's status and the order it answers. The node step reads a status only when
/// `order` is the activation order of the host's current update; any other is left over
/// from an earlier update, or not this controller's, and reads as no report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperReport {
    /// The activation order the status is about.
    pub order: OrderId,
    /// What the helper did with it.
    pub status: HelperStatus,
}

/// What a host that was uncordoned has been given (section 2.6, step 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// The first lease was granted to the node.
    Leased,
    /// No queued operation could run on the node for one heartbeat.
    QueueEmpty,
    /// Neither yet.
    Waiting,
}

/// One host's `update/host/<id>` row: the controller resumes from it after a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRecord {
    /// Where the host is in the node step.
    pub phase: HostPhase,
    /// When it entered that phase.
    pub since: FarmTime,
    /// The generation of the run that wrote the row. A row of an earlier run does not say
    /// this run's host is done ([`HostRecord::is_open_in`]).
    pub generation: Generation,
    /// The activation order of this update: set with the row that enters `Activating`
    /// ([`super::Transition::order`]), and what a status must name to count.
    pub order: OrderId,
    /// The member's boot counter when the planned-down mark was written
    /// ([`super::Transition::boot`]): the new process must report a larger one.
    pub boot_before: Option<u64>,
}

impl HostRecord {
    /// The row of a host no run has touched: `Pending`, written by no run (generation 0,
    /// before the first offer), with no order and no mark.
    #[must_use]
    pub const fn unstarted() -> Self {
        Self {
            phase: HostPhase::Pending,
            since: FarmTime::from_millis(0),
            generation: Generation(0),
            order: OrderId::new(0, 0),
            boot_before: None,
        }
    }

    /// Whether the picker may start the host in the run `run`: a host that has not begun
    /// or was deferred, and a finished host of an *earlier* run, because a rollback walks
    /// back the hosts the forward run finished (section 2.11). `Held` and every step are
    /// never open: they hold the run.
    #[must_use]
    pub fn is_open_in(&self, run: Generation) -> bool {
        match self.phase {
            HostPhase::Pending | HostPhase::Deferred => true,
            HostPhase::Done => self.generation < run,
            HostPhase::Selected
            | HostPhase::Cordoned
            | HostPhase::WaitingLeases
            | HostPhase::PlannedDown
            | HostPhase::Gated
            | HostPhase::Activating
            | HostPhase::RejoiningStorage
            | HostPhase::Repairing
            | HostPhase::Confirming
            | HostPhase::RejoiningWork
            | HostPhase::Held { .. } => false,
        }
    }

    /// Whether the host was deferred in the run `run` (it missed its lease wait once, and
    /// is not forced again). A `Deferred` row of an earlier run is a fresh start.
    #[must_use]
    pub fn was_deferred_in(&self, run: Generation) -> bool {
        self.phase == HostPhase::Deferred && self.generation >= run
    }

    /// The commands a controller that restarted runs again for the phase it reads: the
    /// phase's entry commands, with this row's own activation order.
    #[must_use]
    pub fn entry_commands(&self) -> Vec<Command> {
        self.phase.entry_commands(self.order)
    }
}

/// What a host's storage half is, as `Members` holds it. One per storage member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberFacts {
    /// The member's id.
    pub id: MemberId,
    /// Its state in the map.
    pub state: MemberState,
    /// Whether the `down/<id>` mark of a planned restart exists. Read as a flag of its
    /// own, because the map can lag the mark: a member can still read `Active` after the
    /// mark was written (section 2.6, step 4).
    pub planned_down: bool,
    /// When the controller last heard it (a heartbeat); `None` if never or unknown.
    pub heard_at: Option<FarmTime>,
    /// The blobs it owes copies of (its repair journal); `None` if it does not report.
    pub repair_debt: Option<u64>,
    /// When the planned-down mark was written; `None` if there is no mark.
    pub down_since: Option<FarmTime>,
    /// Its boot counter as it registered (larger with every process start); `None` if it
    /// does not report one.
    pub boot: Option<u64>,
}

impl MemberFacts {
    /// Whether it was heard within `ttl` of `now`. A time in the future is not: it would
    /// make a row look fresh by a clock that is wrong.
    #[must_use]
    pub fn is_fresh(&self, now: FarmTime, ttl: Duration) -> bool {
        self.heard_at
            .is_some_and(|heard| heard <= now && now.saturating_duration_since(heard) <= ttl)
    }
}

/// One host as the controller sees it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// The daemon's node id: the key of its `update/host/<id>` row.
    pub id: WorkerId,
    /// What it runs.
    pub kind: HostKind,
    /// Its operating system.
    pub platform: Platform,
    /// The storage member its storage half is, joined by exact address. `None` is "not
    /// linked": for a [`HostKind::StorageAndDaemon`] host that makes it ineligible, never
    /// a host without storage steps.
    pub member: Option<MemberId>,
    /// The release it runs, as it reported it; `None` if it has not.
    pub release: Option<Release>,
    /// Whether its daemon stream is open.
    pub connected: bool,
    /// Whether placement has it cordoned; `None` if unknown.
    pub cordoned: Option<bool>,
    /// Its daemon half's load; `None` if unknown.
    pub load: Option<DaemonLoad>,
    /// Write sessions open on its storage half; `None` if it does not report them.
    pub open_writes: Option<u64>,
    /// Writes in flight to it from the frontend's router; `None` if unknown.
    pub routed_writes: Option<u64>,
    /// What its helper last reported, and the order it answers; `None` if nothing yet.
    pub helper: Option<HelperReport>,
    /// What it has been given since its uncordon; `None` if unknown.
    pub work: Option<Work>,
    /// Its row in this run.
    pub record: HostRecord,
}

impl HostFacts {
    /// Whether it has a storage half.
    #[must_use]
    pub const fn is_storage(&self) -> bool {
        matches!(self.kind, HostKind::StorageAndDaemon)
    }

    /// Whether it is a Mac.
    #[must_use]
    pub const fn is_mac(&self) -> bool {
        matches!(self.platform, Platform::Mac(_))
    }

    /// Whether it reports both write counts.
    #[must_use]
    pub const fn writes_reported(&self) -> bool {
        self.open_writes.is_some() && self.routed_writes.is_some()
    }

    /// Whether no write is open on it or on its way to it. An unreported count is not
    /// zero.
    #[must_use]
    pub fn writes_quiet(&self) -> bool {
        self.open_writes == Some(0) && self.routed_writes == Some(0)
    }

    /// Whether it reports `target` as its release and its stream is open: the daemon half
    /// is back, at the release, and can take work (section 2.6, step 7 and 2.10).
    #[must_use]
    pub fn is_back_at(&self, target: &Release) -> bool {
        self.connected && self.release.as_ref() == Some(target)
    }

    /// Whether it can take work now, as the capacity floor counts it: connected, placement
    /// shows it uncordoned (unknown is not), and its record says it is not in a step. The
    /// record counts as well as placement because placement lags a cordon that was ordered.
    #[must_use]
    pub fn is_serving(&self) -> bool {
        self.connected && self.cordoned == Some(false) && !self.record.phase.in_step()
    }

    /// Which slot the host takes when a run claims one for it.
    #[must_use]
    pub const fn slot_class(&self) -> SlotClass {
        if self.is_storage() {
            SlotClass::Storage
        } else {
            SlotClass::Host
        }
    }
}

/// The whole fleet at one look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The release the run drives every host to. A rollback is the same run with the old
    /// release here: hosts are compared with `==`, never ordered.
    pub target: Release,
    /// The generation of the run (`update/desired`).
    pub generation: Generation,
    /// The id the next order will carry: greater than every id issued so far. The
    /// controller draws it from the store's counter and, when a [`super::Transition`]
    /// names an order, counts it as used.
    pub next_order: OrderId,
    /// Every host of the run.
    pub hosts: Vec<HostFacts>,
    /// Every storage member.
    pub members: Vec<MemberFacts>,
    /// Who holds `update/slot`.
    pub slot: Slot,
    /// The Mac classes some queued operation asks for; `None` if unknown.
    pub queued_classes: Option<BTreeSet<PlatformClass>>,
    /// When the picker started waiting for an idle host: the run's start (E8) or the last
    /// host's uncordon. The D3 and `mac_force_after` windows count from here.
    pub wait_since: FarmTime,
}

impl Facts {
    /// The host `id`, if the run has it.
    #[must_use]
    pub fn host(&self, id: &WorkerId) -> Option<&HostFacts> {
        self.hosts.iter().find(|h| &h.id == id)
    }

    /// The storage member `id`, if `Members` lists it.
    #[must_use]
    pub fn member(&self, id: MemberId) -> Option<&MemberFacts> {
        self.members.iter().find(|m| m.id == id)
    }

    /// How many hosts other than `host`, of its platform (a Linux host or a Mac of its
    /// class), can take work now: connected, not cordoned and not in a step
    /// ([`HostFacts::is_serving`]). Placement unknown does not count. This is what remains
    /// if `host` is cordoned (section 2.8, `min_serving`).
    #[must_use]
    pub fn serving_peers(&self, host: &HostFacts) -> usize {
        self.serving_peers_without(host, &[])
    }

    /// [`Facts::serving_peers`] with the hosts `gone` counted out as well: the hosts the
    /// picker has already chosen in the same look, which are not yet shown cordoned.
    #[must_use]
    pub fn serving_peers_without(&self, host: &HostFacts, gone: &[&WorkerId]) -> usize {
        self.hosts
            .iter()
            .filter(|other| {
                other.id != host.id
                    && other.platform == host.platform
                    && other.is_serving()
                    && !gone.contains(&&other.id)
            })
            .count()
    }

    /// What the node step reads about host `id`; `None` if the run does not have it.
    #[must_use]
    pub fn observe(&self, id: &WorkerId) -> Option<Observations<'_>> {
        self.host(id).map(|host| Observations { facts: self, host })
    }
}

/// Who holds `update/slot`, from one host's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotView {
    /// This host.
    Mine,
    /// Others: the slot of the host's class is full, so the claim was lost.
    Other,
    /// Room in the host's class, or the claim is not yet visible.
    Free,
}

/// What the node step reads about one host: its own facts, its row and the fleet's. A view
/// of a [`Facts`], so the two cannot disagree.
#[derive(Clone, Copy, Debug)]
pub struct Observations<'a> {
    facts: &'a Facts,
    host: &'a HostFacts,
}

impl<'a> Observations<'a> {
    /// The host.
    #[must_use]
    pub const fn host(&self) -> &'a HostFacts {
        self.host
    }

    /// The host's row.
    #[must_use]
    pub const fn record(&self) -> &'a HostRecord {
        &self.host.record
    }

    /// The fleet.
    #[must_use]
    pub const fn facts(&self) -> &'a Facts {
        self.facts
    }

    /// The release the run drives to.
    #[must_use]
    pub const fn target(&self) -> &'a Release {
        &self.facts.target
    }

    /// The host's storage member, if it is linked and `Members` lists it.
    #[must_use]
    pub fn member(&self) -> Option<&'a MemberFacts> {
        self.host.member.and_then(|id| self.facts.member(id))
    }

    /// Whether the host holds a slot: it does, or the slot of its class is full of others
    /// (`max_hosts_out` is the room for daemon-only hosts and Macs), or there is room and
    /// the claim is not visible yet.
    #[must_use]
    pub fn slot(&self, max_hosts_out: usize) -> SlotView {
        let slot = &self.facts.slot;
        if slot.holds(&self.host.id) {
            SlotView::Mine
        } else if slot.has_room(self.host.slot_class(), max_hosts_out) {
            SlotView::Free
        } else {
            SlotView::Other
        }
    }

    /// What the helper reported about this update's activation order. A status that names
    /// another order, an older update's or one this row does not know, is no report.
    #[must_use]
    pub fn report(&self) -> Option<HelperStatus> {
        self.host
            .helper
            .filter(|report| report.order == self.host.record.order)
            .map(|report| report.status)
    }

    /// G_S with this host out (or in), if it has a storage half; a daemon-only host has
    /// no gate (section 2.6).
    ///
    /// # Errors
    /// The first thing that keeps the gate shut, the worst first ([`GateFail`]).
    pub fn gate(&self, mode: GateMode, policy: &Policy, now: FarmTime) -> Result<(), GateFail> {
        if !self.host.is_storage() {
            return Ok(());
        }
        let own = self.host.member.ok_or(GateFail::Unlinked)?;
        storage_gate(self.facts, own, mode, policy, now)
    }
}
