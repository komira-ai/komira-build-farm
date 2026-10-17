//! A model farm for the controller tests: hosts that take and finish leases, take writes,
//! restart and register again, rebuild and repair, and storage members with heartbeats and
//! debts. One [`World`] is shared by handles that implement the controller's two traits, so
//! a test runs the real [`kbf_server::update::Controller`] against it and then looks at what
//! the farm *is*, which the controller can only read as it was a few seconds ago.
//!
//! **What lags.** [`FactsSource::look`] answers from a snapshot of the world 0 to `lag`
//! seconds old (placement, load, writes, release, helper status, member rows, heartbeat
//! times). The controller's own rows are not the world's; the effects of its orders show in
//! the readings a second later, like any other change.
//!
//! **The helper** acts on an order once and refuses an id it has seen (the highest it acted
//! on is kept): a repeat of the same id does nothing, as the real helper must do. It asserts
//! what the design says a controller guarantees at the moment of the order, in the world as
//! it *is*: no lease and no write open on the host, the host cordoned.
//!
//! The farm has no faults except the ones a test plants.

#![allow(dead_code)]

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kbf_map::{MemberId, MemberState};
use kbf_server::update::{
    Alert, DaemonLoad, FactsSource, FarmView, HelperReport, HelperStatus, HostFacts, HostKind,
    HostRecord, MemberFacts, OrderId, Platform, PlatformClass, Release, SourceError, Transport,
    TransportError, Work,
};
use kbf_sim::{Chance, SimRng};
use kbf_types::{FarmTime, WorkerId};

use super::{BOOT, NOW, OLD, t};

/// How long a heartbeat counts: the membership time to live, in seconds.
const TTL: u64 = 15;

/// The farm to build.
#[derive(Clone, Debug)]
pub struct Spec {
    /// Storage-and-daemon hosts `s1..`, each with member `n`.
    pub storage: u64,
    /// Linux daemon-only hosts `d1..`.
    pub linux: u64,
    /// Macs `m1..` of one class.
    pub macs: u64,
    /// Percent chance per second that an uncordoned host is given a lease.
    pub lease_pct: u32,
    /// The longest a lease lasts, in seconds.
    pub lease_max: u64,
    /// Seconds the controller's readings lag behind the farm, at most.
    pub lag: usize,
    /// Whether a restarted storage half rebuilds its index (`CatchingUp`).
    pub rebuild: bool,
    /// Seconds a rebuild takes: `.0 + below(.1)`.
    pub rebuild_for: (u64, u64),
    /// Seconds a restart takes: `.0 + below(.1)`.
    pub restart: (u64, u64),
}

impl Default for Spec {
    /// A quiet farm: no leases, no lag, quick restarts, no rebuild.
    fn default() -> Self {
        Self {
            storage: 3,
            linux: 1,
            macs: 1,
            lease_pct: 0,
            lease_max: 30,
            lag: 0,
            rebuild: false,
            rebuild_for: (10, 200),
            restart: (3, 4),
        }
    }
}

/// One order the farm took, as the helper or the placement saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gave {
    /// A cordon.
    Cordon,
    /// An uncordon.
    Uncordon,
    /// The planned-down mark of the host's member.
    MarkDown,
    /// The mark removed.
    Undown,
    /// An activate order to the helper, naming the release it flips to.
    Activate(OrderId, String),
    /// A confirm order to the helper.
    Confirm(OrderId),
}

/// An order and when and for whom it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Given {
    /// The second of the farm's clock.
    pub at: u64,
    /// The host.
    pub host: String,
    /// The order.
    pub order: Gave,
}

#[derive(Clone)]
pub struct HostTruth {
    pub name: String,
    pub kind: HostKind,
    pub platform: Platform,
    pub member: Option<u64>,
    /// The release its daemon last reported.
    pub reported: Release,
    pub connected: bool,
    pub cordoned: bool,
    /// The seconds at which each lease ends.
    pub leases: Vec<u64>,
    /// The seconds at which each open write session ends.
    pub writes: Vec<u64>,
    /// Writes that started this second (the router's in flight).
    pub routed: u64,
    pub helper: Option<HelperReport>,
    pub work: Work,
    pub uncordoned_at: Option<u64>,
    /// When the restarted process is up.
    pub restart_at: Option<u64>,
    pub confirm_at: Option<u64>,
    /// The highest order the helper has acted on: it refuses a repeat.
    pub last_order: Option<OrderId>,
    /// The release the helper flipped to, which the restarted process reports once it is up.
    pub flipped_to: Option<Release>,
    /// Restarts since the last [`World::new_run`].
    pub activations: u32,
    pub lease_pct: u32,
    pub lease_max: u64,
}

#[derive(Clone)]
pub struct MemberTruth {
    pub id: u64,
    pub process_up: bool,
    pub state: MemberState,
    pub planned_down: bool,
    pub down_since: Option<u64>,
    pub heard_at: u64,
    pub boot: u64,
    pub debt: u64,
    pub catching_up_until: u64,
}

#[derive(Clone)]
struct Snapshot {
    hosts: Vec<HostTruth>,
    members: Vec<MemberTruth>,
}

/// The farm itself.
pub struct Inner {
    rng: SimRng,
    pub now: u64,
    lag: usize,
    rebuild: bool,
    rebuild_range: (u64, u64),
    restart_range: (u64, u64),
    pub hosts: Vec<HostTruth>,
    pub members: Vec<MemberTruth>,
    /// The Mac classes some queued operation asks for.
    pub queued: Option<BTreeSet<PlatformClass>>,
    history: VecDeque<Snapshot>,
    /// Every order taken, in order.
    pub given: Vec<Given>,
    /// Every alert sent.
    pub alerts: Vec<(String, Alert)>,
    /// Activate orders the helper refused as repeats of an id it had seen.
    pub repeats: u64,
    /// Cordons ordered for a host that still held a lease (a drain D3 forced).
    pub busy_cordons: u64,
    /// Restarts that came back rebuilding.
    pub rebuilds: u64,
    /// Seconds from now on during which every call fails (a transport outage), if set.
    pub down_until: Option<u64>,
}

fn pct(rng: &mut SimRng, p: u32) -> bool {
    rng.chance(Chance::percent(p))
}

/// A handle on a farm. Clones share it.
#[derive(Clone)]
pub struct World(Arc<Mutex<Inner>>);

impl World {
    /// A farm of `spec`, seeded: every host at [`OLD`], connected, uncordoned, idle.
    pub fn new(seed: u64, spec: &Spec) -> Self {
        let rng = SimRng::from_seed(seed);
        let mut hosts = Vec::new();
        let mut members = Vec::new();
        let host = |name: String, kind, platform, member, lease_pct, lease_max| HostTruth {
            name,
            kind,
            platform,
            member,
            reported: Release::new(OLD),
            connected: true,
            cordoned: false,
            leases: Vec::new(),
            writes: Vec::new(),
            routed: 0,
            helper: None,
            work: Work::Waiting,
            uncordoned_at: None,
            restart_at: None,
            confirm_at: None,
            last_order: None,
            flipped_to: None,
            activations: 0,
            lease_pct,
            lease_max,
        };
        for n in 1..=spec.storage {
            let platform = Platform::Linux;
            let (pct, max) = (spec.lease_pct, spec.lease_max);
            let kind = HostKind::StorageAndDaemon;
            hosts.push(host(format!("s{n}"), kind, platform, Some(n), pct, max));
            members.push(MemberTruth {
                id: n,
                process_up: true,
                state: MemberState::Active,
                planned_down: false,
                down_since: None,
                heard_at: NOW,
                boot: BOOT,
                debt: 0,
                catching_up_until: 0,
            });
        }
        for n in 1..=spec.linux {
            let (pct, max) = (spec.lease_pct, spec.lease_max);
            let kind = HostKind::DaemonOnly;
            hosts.push(host(format!("d{n}"), kind, Platform::Linux, None, pct, max));
        }
        for n in 1..=spec.macs {
            let platform = Platform::Mac(PlatformClass::new("arm64"));
            // A Mac is idle most of the time: it is only ever updated when it is, and the
            // run must end.
            let pct = spec.lease_pct.min(1);
            let kind = HostKind::DaemonOnly;
            hosts.push(host(format!("m{n}"), kind, platform, None, pct, 100));
        }
        let mut inner = Inner {
            rng,
            now: NOW,
            lag: spec.lag,
            rebuild: spec.rebuild,
            rebuild_range: spec.rebuild_for,
            restart_range: spec.restart,
            hosts,
            members,
            queued: Some(BTreeSet::new()),
            history: VecDeque::new(),
            given: Vec::new(),
            alerts: Vec::new(),
            repeats: 0,
            busy_cordons: 0,
            rebuilds: 0,
            down_until: None,
        };
        let seen = inner.snapshot();
        inner.history.push_back(seen);
        Self(Arc::new(Mutex::new(inner)))
    }

    /// The farm, locked.
    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A handle that is the controller's transport.
    pub fn transport(&self) -> Box<dyn Transport> {
        Box::new(self.clone())
    }

    /// A handle that is the controller's source of facts.
    pub fn source(&self) -> Box<dyn FactsSource> {
        Box::new(self.clone())
    }

    /// The farm's clock, in seconds, as a farm time.
    pub fn time(&self) -> FarmTime {
        t(self.lock().now)
    }

    /// One second passes.
    pub fn advance(&self) {
        self.lock().advance();
    }

    /// A host that has a lease for `secs` seconds from now (a busy host).
    pub fn lease(&self, host: &str, secs: u64) {
        let mut w = self.lock();
        let end = w.now + secs;
        let at = w.host_index(host);
        w.hosts[at].leases.push(end);
    }
}

impl Inner {
    pub fn host_index(&self, name: &str) -> usize {
        self.hosts
            .iter()
            .position(|h| h.name == name)
            .expect("a host")
    }

    pub fn host(&self, name: &str) -> &HostTruth {
        &self.hosts[self.host_index(name)]
    }

    pub fn member(&self, n: u64) -> &MemberTruth {
        self.members.iter().find(|m| m.id == n).expect("a member")
    }

    /// The orders taken for `host`, in order.
    pub fn given_to(&self, host: &str) -> Vec<&Gave> {
        let mine = self.given.iter().filter(|g| g.host == host);
        mine.map(|g| &g.order).collect()
    }

    /// Every activate order taken, as (host, order).
    pub fn activations(&self) -> Vec<(String, OrderId)> {
        let mut all = Vec::new();
        for given in &self.given {
            if let Gave::Activate(order, _) = &given.order {
                all.push((given.host.clone(), *order));
            }
        }
        all
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            hosts: self.hosts.clone(),
            members: self.members.clone(),
        }
    }

    /// One second of the farm: leases and writes end and begin, restarted processes come up
    /// and register, heartbeats are heard, debts are repaired.
    pub fn advance(&mut self) {
        self.now += 1;
        let now = self.now;
        for member in &mut self.members {
            if member.process_up {
                member.heard_at = now;
                member.state = if now < member.catching_up_until {
                    MemberState::CatchingUp
                } else {
                    MemberState::Active
                };
                member.debt = member.debt.saturating_sub(1);
            } else if now - member.heard_at > TTL {
                member.state = MemberState::Down;
            }
        }
        for at in 0..self.hosts.len() {
            let host = &mut self.hosts[at];
            host.leases.retain(|end| *end > now);
            host.writes.retain(|end| *end > now);
            host.routed = 0;
            let mut got_lease = false;
            if host.connected && !host.cordoned && pct(&mut self.rng, host.lease_pct) {
                let length = 5 + self.rng.below(host.lease_max);
                host.leases.push(now + length);
                got_lease = true;
            }
            if let Some(n) = host.member {
                let member = self.members.iter().find(|m| m.id == n).expect("member");
                // The router stops choosing a marked or stopped node as a write primary.
                if member.process_up && !member.planned_down && pct(&mut self.rng, 10) {
                    host.writes.push(now + 1 + self.rng.below(15));
                    host.routed = 1;
                }
            }
            if let Some(u) = host.uncordoned_at {
                if got_lease {
                    host.work = Work::Leased;
                } else if now >= u + 2 && host.work == Work::Waiting && host.leases.is_empty() {
                    host.work = Work::QueueEmpty;
                }
            }
            if host.confirm_at.is_some_and(|at| now >= at) {
                host.confirm_at = None;
                if let Some(report) = &mut host.helper {
                    report.status = HelperStatus::Confirmed;
                }
            }
            if host.restart_at.is_some_and(|at| now >= at) {
                self.finish_restart(at);
            }
        }
        let seen = self.snapshot();
        self.history.push_back(seen);
        while self.history.len() > self.lag + 1 {
            self.history.pop_front();
        }
    }

    /// The restarted process is up: the daemon reconnects at the release it was flipped to,
    /// the helper reports the flip, and a storage half registers (the mark deleted, the boot
    /// counter larger), owing nothing itself while its peers owe it the copies it missed.
    fn finish_restart(&mut self, at: usize) {
        let now = self.now;
        let rebuild = self.rebuild;
        let rebuild_for = self.rebuild_range.0 + self.rng.below(self.rebuild_range.1);
        let host = &mut self.hosts[at];
        host.restart_at = None;
        host.connected = true;
        host.reported = host.flipped_to.take().expect("a release was flipped to");
        let order = host.last_order.expect("an order was given");
        host.helper = Some(HelperReport {
            order,
            status: HelperStatus::ActivatedUnconfirmed,
        });
        let Some(n) = host.member else {
            return;
        };
        let owed: Vec<u64> = self
            .members
            .iter()
            .map(|_| 1 + self.rng.below(30))
            .collect();
        for (member, owes) in self.members.iter_mut().zip(owed) {
            if member.id == n {
                member.process_up = true;
                member.planned_down = false;
                member.down_since = None;
                member.heard_at = now;
                member.boot += 1;
                member.catching_up_until = if rebuild { now + rebuild_for } else { 0 };
                self.rebuilds += u64::from(rebuild);
            } else {
                member.debt = member.debt.max(owes);
            }
        }
    }

    /// Starts a new run on the farm: what a host has been given is "since its uncordon",
    /// nothing, until the run uncordons it again, and restarts are counted afresh.
    pub fn new_run(&mut self) {
        for host in &mut self.hosts {
            host.activations = 0;
            host.work = Work::Waiting;
            host.uncordoned_at = None;
        }
    }

    fn outage(&self) -> Result<(), TransportError> {
        match self.down_until {
            Some(until) if self.now < until => {
                Err(TransportError::new("the farm is not answering"))
            }
            _ => Ok(()),
        }
    }

    fn note(&mut self, host: &WorkerId, order: Gave) {
        let at = self.now;
        self.given.push(Given {
            at,
            host: host.as_str().to_owned(),
            order,
        });
    }

    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.outage()?;
        let at = self.host_index(host.as_str());
        let truth = &mut self.hosts[at];
        if !truth.cordoned && !truth.leases.is_empty() {
            self.busy_cordons += 1;
        }
        truth.cordoned = true;
        self.note(host, Gave::Cordon);
        Ok(())
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.outage()?;
        let now = self.now;
        let at = self.host_index(host.as_str());
        let truth = &mut self.hosts[at];
        if truth.cordoned {
            truth.cordoned = false;
            truth.uncordoned_at = Some(now);
            truth.work = Work::Waiting;
        }
        self.note(host, Gave::Uncordon);
        Ok(())
    }

    fn mark(
        &mut self,
        host: &WorkerId,
        member: MemberId,
        down: bool,
    ) -> Result<(), TransportError> {
        self.outage()?;
        let now = self.now;
        let m = self
            .members
            .iter_mut()
            .find(|m| m.id == member.0)
            .expect("a member");
        if down {
            m.planned_down = true;
            m.down_since.get_or_insert(now);
        } else {
            m.planned_down = false;
            m.down_since = None;
        }
        self.note(host, if down { Gave::MarkDown } else { Gave::Undown });
        Ok(())
    }

    /// The helper acts on an order once; a repeat of an id it has seen is refused. The world
    /// as it is, not as the controller read it, must hold no lease and no write.
    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.outage()?;
        let now = self.now;
        let restart_in = self.restart_range.0 + self.rng.below(self.restart_range.1);
        self.note(host, Gave::Activate(order, release.as_str().to_owned()));
        let at = self.host_index(host.as_str());
        let target = release.clone();
        let truth = &mut self.hosts[at];
        if truth.last_order.is_some_and(|last| last >= order) {
            self.repeats += 1;
            return Ok(());
        }
        truth.last_order = Some(order);
        let name = &truth.name;
        assert!(
            truth.leases.is_empty(),
            "{name} was activated at {now} with leases to {:?}",
            truth.leases
        );
        assert!(
            truth.writes.is_empty(),
            "{name} was activated at {now} with writes open"
        );
        assert!(truth.cordoned, "{name} was activated uncordoned");
        truth.activations += 1;
        truth.connected = false;
        truth.helper = None;
        truth.flipped_to = Some(target);
        truth.restart_at = Some(now + restart_in);
        if let Some(n) = truth.member {
            let m = self.members.iter_mut().find(|m| m.id == n).expect("member");
            m.process_up = false;
        }
        Ok(())
    }

    fn confirm(&mut self, host: &WorkerId, order: OrderId) -> Result<(), TransportError> {
        self.outage()?;
        let now = self.now;
        self.note(host, Gave::Confirm(order));
        let at = self.host_index(host.as_str());
        let truth = &mut self.hosts[at];
        if truth.last_order == Some(order) && truth.confirm_at.is_none() {
            truth.confirm_at = Some(now + 1);
        }
        Ok(())
    }

    /// The facts as the controller reads them: the readings of the oldest snapshot kept.
    fn view(&self) -> FarmView {
        let seen = self.history.front().expect("a snapshot");
        let hosts = seen
            .hosts
            .iter()
            .map(|h| HostFacts {
                id: WorkerId::new(h.name.clone()),
                kind: h.kind,
                platform: h.platform.clone(),
                member: h.member.map(MemberId),
                release: Some(h.reported.clone()),
                connected: h.connected,
                cordoned: Some(h.cordoned),
                load: Some(DaemonLoad {
                    leases: u32::try_from(h.leases.len()).expect("few leases"),
                    undelivered: 0,
                    whole_machine_hold: false,
                }),
                open_writes: h.member.map(|_| h.writes.len() as u64),
                routed_writes: h.member.map(|_| h.routed),
                helper: h.helper,
                work: Some(h.work),
                record: HostRecord::unstarted(),
            })
            .collect();
        let members = seen
            .members
            .iter()
            .map(|m| MemberFacts {
                id: MemberId(m.id),
                state: m.state,
                planned_down: m.planned_down,
                heard_at: Some(t(m.heard_at)),
                repair_debt: Some(m.debt),
                down_since: m.down_since.map(t),
                boot: Some(m.boot),
            })
            .collect();
        FarmView {
            hosts,
            members,
            queued_classes: self.queued.clone(),
        }
    }
}

impl Transport for World {
    fn cordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.lock().cordon(host)
    }

    fn uncordon(&mut self, host: &WorkerId) -> Result<(), TransportError> {
        self.lock().uncordon(host)
    }

    fn mark_down(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.lock().mark(host, member, true)
    }

    fn undown(&mut self, host: &WorkerId, member: MemberId) -> Result<(), TransportError> {
        self.lock().mark(host, member, false)
    }

    fn activate(
        &mut self,
        host: &WorkerId,
        order: OrderId,
        release: &Release,
    ) -> Result<(), TransportError> {
        self.lock().activate(host, order, release)
    }

    fn confirm(&mut self, host: &WorkerId, order: OrderId) -> Result<(), TransportError> {
        self.lock().confirm(host, order)
    }

    fn alert(&mut self, host: &WorkerId, alert: &Alert) -> Result<(), TransportError> {
        let mut w = self.lock();
        w.outage()?;
        w.alerts.push((host.as_str().to_owned(), alert.clone()));
        Ok(())
    }
}

impl FactsSource for World {
    fn look(&mut self, _now: FarmTime) -> Result<FarmView, SourceError> {
        Ok(self.lock().view())
    }
}
