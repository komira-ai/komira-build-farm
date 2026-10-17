//! The simulator the rollout's tests run against: a fleet that implements
//! [`Transport`], keeps a log of every call and a clock of its own (moved only by
//! [`Transport::sleep`]), and fails on request.
//!
//! A [`Fleet`] is a handle: clones share one fleet, which is how a test starts a second
//! rollout while the first is in the middle of its run.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::Duration;

use super::inventory::{Inventory, Role};
use super::transport::{Commit, Health, HostName, Lock, RunId, Transport, TransportError};

/// What a call asked of a host (or of the frontend, for the fleet-level calls).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Lock(RunId),
    Unlock(RunId),
    Backup,
    UnderReplicated,
    Stage(Commit),
    Drain,
    InFlight,
    /// What the fleet looked like at the instant of the swap: the work in flight on
    /// the host, and the blobs under-replicated in the cluster.
    Swap {
        commit: Commit,
        in_flight: u64,
        under_replicated: u64,
    },
    Check(Commit),
    Confirm,
    Undrain,
    Rollback,
}

impl Call {
    fn is_fleet_level(&self) -> bool {
        matches!(
            self,
            Call::Lock(_) | Call::Unlock(_) | Call::Backup | Call::UnderReplicated
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub host: HostName,
    pub call: Call,
}

/// How the new commit behaves on a host that runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// Starts, answers, never registers.
    NeverRegisters,
    /// Crash loops: the unit is never active.
    CrashLoops,
    /// Registers, but `/readyz` never goes true.
    NotReady,
}

/// A call that fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fault {
    Lock,
    Unlock,
    Backup,
    UnderReplicated,
    Stage,
    Drain,
    InFlight,
    /// The connection drops before the swap reaches the host: nothing changed.
    SwapNotApplied,
    /// The swap is done on the host and the reply is lost.
    SwapLostReply,
    Check,
    Confirm,
    Undrain,
    Rollback,
}

#[derive(Clone, Copy, Debug)]
enum Busy {
    Idle,
    /// Work in flight until this long after the drain.
    Until(Duration),
    Forever,
}

struct Node {
    role: Role,
    original: Commit,
    current: Commit,
    previous: Option<Commit>,
    staged: Option<Commit>,
    drained: bool,
    drained_at: Duration,
    busy: Busy,
    bad: Option<Bad>,
    faults: BTreeSet<Fault>,
    /// Blobs left under-replicated by a swap of this node, and how long they take to
    /// heal (`None`: never).
    degrades: u64,
    heals_after: Option<Duration>,
    /// How long after the swap the new process takes to register.
    warmup: Duration,
    swapped_at: Duration,
}

struct State {
    nodes: BTreeMap<HostName, Node>,
    events: Vec<Event>,
    elapsed: Duration,
    lock: Option<RunId>,
    /// Under-replicated blobs, since when, and how long they take to heal.
    degraded: Option<(u64, Duration, Option<Duration>)>,
}

impl State {
    fn under_replicated(&self) -> u64 {
        match self.degraded {
            Some((n, since, heals)) if heals.is_none_or(|h| self.elapsed < since + h) => n,
            _ => 0,
        }
    }
}

#[derive(Clone)]
pub struct Fleet(Rc<RefCell<State>>);

fn name(s: &str) -> HostName {
    HostName::new(s).expect("a test host name")
}

pub fn commit(c: char) -> Commit {
    Commit::new(&c.to_string().repeat(40)).expect("a test commit")
}

pub fn run_id(s: &str) -> RunId {
    RunId::new(s).expect("a test run id")
}

impl Fleet {
    /// A fleet of the inventory's hosts, all running commit `original`.
    pub fn new(inventory: &Inventory, original: &Commit) -> Fleet {
        let nodes = inventory
            .ordered()
            .into_iter()
            .map(|h| {
                let node = Node {
                    role: h.role,
                    original: original.clone(),
                    current: original.clone(),
                    previous: None,
                    staged: None,
                    drained: false,
                    drained_at: Duration::ZERO,
                    busy: Busy::Idle,
                    bad: None,
                    faults: BTreeSet::new(),
                    degrades: 0,
                    heals_after: None,
                    warmup: Duration::ZERO,
                    swapped_at: Duration::ZERO,
                };
                (h.name, node)
            })
            .collect();
        Fleet(Rc::new(RefCell::new(State {
            nodes,
            events: Vec::new(),
            elapsed: Duration::ZERO,
            lock: None,
            degraded: None,
        })))
    }

    fn with_node(&self, host: &str, f: impl FnOnce(&mut Node)) -> &Fleet {
        let mut s = self.0.borrow_mut();
        f(s.nodes.get_mut(&name(host)).expect("a host of the fleet"));
        self
    }

    /// `host`'s new commit misbehaves as `bad`.
    pub fn bad(&self, host: &str, bad: Bad) -> &Fleet {
        self.with_node(host, |n| n.bad = Some(bad))
    }

    /// `host`'s new process registers only `after` its swap.
    pub fn slow_to_register(&self, host: &str, after: Duration) -> &Fleet {
        self.with_node(host, |n| n.warmup = after)
    }

    pub fn fault(&self, host: &str, fault: Fault) -> &Fleet {
        self.with_node(host, |n| {
            n.faults.insert(fault);
        })
    }

    /// Work stays in flight on `host` until `after` its drain.
    pub fn busy_until(&self, host: &str, after: Duration) -> &Fleet {
        self.with_node(host, |n| n.busy = Busy::Until(after))
    }

    pub fn busy_forever(&self, host: &str) -> &Fleet {
        self.with_node(host, |n| n.busy = Busy::Forever)
    }

    /// Swapping `host` leaves `blobs` under-replicated until `heals_after` (never when
    /// `None`).
    pub fn degrades_on_swap(
        &self,
        host: &str,
        blobs: u64,
        heals_after: Option<Duration>,
    ) -> &Fleet {
        self.with_node(host, |n| {
            n.degrades = blobs;
            n.heals_after = heals_after;
        })
    }

    /// The cluster starts with `blobs` under-replicated, never healing.
    pub fn start_degraded(&self, blobs: u64) -> &Fleet {
        self.0.borrow_mut().degraded = Some((blobs, Duration::ZERO, None));
        self
    }

    /// Take the lock as `run`, as another rollout in the middle of its run holds it.
    pub fn hold_lock(&self, run: &str) -> &Fleet {
        self.0.borrow_mut().lock = Some(run_id(run));
        self
    }

    pub fn events(&self) -> Vec<Event> {
        self.0.borrow().events.clone()
    }

    /// Every call about `host` itself, in order: not the frontend's fleet-level calls
    /// (the lock, the backup, the replication count), which name the frontend too.
    pub fn calls_to(&self, host: &str) -> Vec<Call> {
        let host = name(host);
        self.events()
            .into_iter()
            .filter(|e| e.host == host && !e.call.is_fleet_level())
            .map(|e| e.call)
            .collect()
    }

    /// The calls that change something (not the polls), as `(host, call)`.
    pub fn changes(&self) -> Vec<(String, Call)> {
        self.events()
            .into_iter()
            .filter(|e| {
                !matches!(
                    e.call,
                    Call::InFlight | Call::Check(_) | Call::UnderReplicated
                )
            })
            .map(|e| (e.host.to_string(), e.call))
            .collect()
    }

    pub fn current(&self, host: &str) -> Commit {
        self.0.borrow().nodes[&name(host)].current.clone()
    }

    pub fn is_drained(&self, host: &str) -> bool {
        self.0.borrow().nodes[&name(host)].drained
    }

    pub fn lock_holder(&self) -> Option<RunId> {
        self.0.borrow().lock.clone()
    }

    pub fn elapsed(&self) -> Duration {
        self.0.borrow().elapsed
    }

    pub fn roles(&self) -> BTreeMap<HostName, Role> {
        self.0
            .borrow()
            .nodes
            .iter()
            .map(|(h, n)| (h.clone(), n.role))
            .collect()
    }
}

/// The most storage-and-daemon nodes drained at once, over the log: a node is out from
/// its drain until its undrain.
pub fn most_storage_nodes_out(events: &[Event], roles: &BTreeMap<HostName, Role>) -> usize {
    let mut out: BTreeSet<&HostName> = BTreeSet::new();
    let mut most = 0;
    for e in events {
        if roles.get(&e.host) != Some(&Role::StorageAndDaemon) {
            continue;
        }
        match e.call {
            Call::Drain => {
                out.insert(&e.host);
            }
            Call::Undrain => {
                out.remove(&e.host);
            }
            _ => {}
        }
        most = most.max(out.len());
    }
    most
}

impl State {
    fn log(&mut self, host: &HostName, call: Call) {
        self.events.push(Event {
            host: host.clone(),
            call,
        });
    }

    /// Log `call` on `host`, and fail it when the host has `fault`.
    fn enter(&mut self, host: &HostName, call: Call, fault: Fault) -> Result<(), TransportError> {
        self.log(host, call);
        if self.nodes[host].faults.contains(&fault) {
            return Err(TransportError::Unreachable {
                host: host.clone(),
                why: format!("planted {fault:?}"),
            });
        }
        Ok(())
    }

    fn in_flight(&self, host: &HostName) -> u64 {
        let n = &self.nodes[host];
        match n.busy {
            Busy::Idle => 0,
            Busy::Forever => 3,
            Busy::Until(after) if self.elapsed < n.drained_at + after => 2,
            Busy::Until(_) => 0,
        }
    }
}

impl Transport for Fleet {
    fn lock(&mut self, frontend: &HostName, run: &RunId) -> Result<Lock, TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(frontend, Call::Lock(run.clone()), Fault::Lock)?;
        if let Some(by) = s.lock.clone() {
            return Ok(Lock::Held { by });
        }
        s.lock = Some(run.clone());
        Ok(Lock::Acquired)
    }

    fn unlock(&mut self, frontend: &HostName, run: &RunId) -> Result<(), TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(frontend, Call::Unlock(run.clone()), Fault::Unlock)?;
        if s.lock.as_ref() == Some(run) {
            s.lock = None;
        }
        Ok(())
    }

    fn backup_state(&mut self, frontend: &HostName) -> Result<(), TransportError> {
        self.0
            .borrow_mut()
            .enter(frontend, Call::Backup, Fault::Backup)
    }

    fn under_replicated_blobs(&mut self, frontend: &HostName) -> Result<u64, TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(frontend, Call::UnderReplicated, Fault::UnderReplicated)?;
        Ok(s.under_replicated())
    }

    fn stage(&mut self, host: &HostName, commit: &Commit) -> Result<(), TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(host, Call::Stage(commit.clone()), Fault::Stage)?;
        s.nodes.get_mut(host).expect("a host of the fleet").staged = Some(commit.clone());
        Ok(())
    }

    fn drain(&mut self, host: &HostName) -> Result<(), TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(host, Call::Drain, Fault::Drain)?;
        let now = s.elapsed;
        let n = s.nodes.get_mut(host).expect("a host of the fleet");
        n.drained = true;
        n.drained_at = now;
        Ok(())
    }

    fn in_flight(&mut self, host: &HostName) -> Result<u64, TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(host, Call::InFlight, Fault::InFlight)?;
        Ok(s.in_flight(host))
    }

    fn swap(&mut self, host: &HostName, commit: &Commit) -> Result<(), TransportError> {
        let mut s = self.0.borrow_mut();
        let call = Call::Swap {
            commit: commit.clone(),
            in_flight: s.in_flight(host),
            under_replicated: s.under_replicated(),
        };
        s.log(host, call);
        let elapsed = s.elapsed;
        let n = s.nodes.get_mut(host).expect("a host of the fleet");
        if n.faults.contains(&Fault::SwapNotApplied) {
            return Err(TransportError::Unreachable {
                host: host.clone(),
                why: "planted SwapNotApplied".to_owned(),
            });
        }
        n.previous = Some(n.current.clone());
        n.current = commit.clone();
        n.swapped_at = elapsed;
        let (blobs, heals) = (n.degrades, n.heals_after);
        let lost = n.faults.contains(&Fault::SwapLostReply);
        if blobs > 0 {
            s.degraded = Some((blobs, elapsed, heals));
        }
        if lost {
            return Err(TransportError::Unreachable {
                host: host.clone(),
                why: "planted SwapLostReply".to_owned(),
            });
        }
        Ok(())
    }

    fn check(&mut self, host: &HostName, commit: &Commit) -> Result<Health, TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(host, Call::Check(commit.clone()), Fault::Check)?;
        let n = &s.nodes[host];
        let warming = s.elapsed < n.swapped_at + n.warmup;
        let misbehaving = n.current != n.original;
        let health = match n.bad {
            Some(Bad::NeverRegisters) if misbehaving => (true, true, false),
            Some(Bad::CrashLoops) if misbehaving => (false, false, false),
            Some(Bad::NotReady) if misbehaving => (true, false, true),
            _ => (true, true, &n.current == commit && !warming),
        };
        Ok(Health {
            unit_active: health.0,
            ready: health.1,
            registered_active: health.2,
        })
    }

    fn confirm(&mut self, host: &HostName) -> Result<(), TransportError> {
        self.0
            .borrow_mut()
            .enter(host, Call::Confirm, Fault::Confirm)
    }

    fn undrain(&mut self, host: &HostName) -> Result<(), TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(host, Call::Undrain, Fault::Undrain)?;
        s.nodes.get_mut(host).expect("a host of the fleet").drained = false;
        Ok(())
    }

    fn rollback(&mut self, host: &HostName) -> Result<(), TransportError> {
        let mut s = self.0.borrow_mut();
        s.enter(host, Call::Rollback, Fault::Rollback)?;
        let n = s.nodes.get_mut(host).expect("a host of the fleet");
        if let Some(previous) = n.previous.take() {
            n.current = previous;
        }
        Ok(())
    }

    fn sleep(&mut self, for_: Duration) {
        self.0.borrow_mut().elapsed += for_;
    }
}
