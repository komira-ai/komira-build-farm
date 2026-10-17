//! Memory kills (failure classes, 6.1), one scenario each, through the scheduler's
//! public inputs: the ladder after the action passes its own limit, its cap at the
//! largest node that could run it, the floor its action key keeps, and a busy node's
//! kill, which reruns at the same booking and counts against the node.

use kbf_caps::NodeCaps;
use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{
    DaemonInstance, Event, FARM_RERUNS, Floor, FloorChange, Input, MemoryLadder, OpState, Request,
    Requeue, RequeueReason, Scheduler,
};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, Failure, FarmFault, FarmRun,
    FarmTime, LeaseKind, LostWhy, MemoryKill, MemoryRun, OperationId, Outcome, Qos, Resources,
    ResultRecord, StartLease, StateMachine, WaiterId, Waiting, WorkerId,
};

const GIB: u64 = 1 << 30;
const OOM: Outcome = Outcome::Failed(Failure::OutOfMemory);
const PRESSURE: Outcome = Outcome::Failed(Failure::NodeMemoryPressure);

fn key(n: u8) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action: Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n)),
    }
}

/// Action `n`: one core and `memory` bytes, on Linux.
fn request(n: u8, memory: u64) -> Request {
    Request {
        key: key(n),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, memory),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).unwrap(),
    }
}

fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

/// A Linux node that also serves whole-machine leases.
fn whole() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container", "native-whole-machine"])
}

fn mac() -> NodeCaps {
    NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .unwrap()
        .with_drivers(["native"])
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn ok() -> Outcome {
    Outcome::Completed {
        action_result: Digest::new(DigestFunction::Sha256, [200; 32], 1),
    }
}

/// A scheduler for term 1 that records requeues, with a clock the test moves. Every
/// commit is fed straight back, as the single-node server does.
struct Farm {
    s: Scheduler,
    now: FarmTime,
}

impl Farm {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1).recording_requeues(),
            now: FarmTime::default(),
        }
    }

    fn at_secs(&mut self, secs: u64) {
        self.now = FarmTime::from_millis(secs * 1_000);
    }

    /// Feeds `event`, then every commit it asks for, and returns the other effects.
    fn feed(&mut self, event: Event) -> Vec<Effect> {
        let mut pending = self.s.apply(Input::new(self.now, event));
        let mut out = Vec::new();
        while !pending.is_empty() {
            let mut more = Vec::new();
            for effect in pending {
                match effect {
                    Effect::Commit(record) => {
                        more.extend(self.s.apply(Input::new(self.now, Event::Committed(record))));
                    }
                    other => out.push(other),
                }
            }
            pending = more;
        }
        out
    }

    fn node(&mut self, name: &str, memory_gib: u64, caps: NodeCaps) {
        let event = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(8_000, memory_gib * GIB),
            caps,
        };
        assert!(self.feed(event).is_empty());
    }

    fn heartbeat(&mut self, name: &str, running: &[&StartLease]) {
        let running = running.iter().map(|s| s.lease).collect();
        let event = Event::Heartbeat {
            worker: w(name),
            running,
        };
        assert!(self.feed(event).is_empty());
    }

    fn submit(&mut self, waiter: u64, request: Request) -> Vec<Effect> {
        let waiter = WaiterId(waiter);
        self.feed(Event::Submit { waiter, request })
    }

    /// A placement round, which must start exactly one lease.
    fn start(&mut self) -> StartLease {
        let effects = self.feed(Event::Tick);
        let starts: Vec<&StartLease> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Start(s) => Some(s),
                _ => None,
            })
            .collect();
        let [start] = starts[..] else {
            panic!("want one Start: {effects:?}");
        };
        start.clone()
    }

    /// `start`'s lease ends with `outcome`; the effects, and those of a placement round.
    fn end(&mut self, start: &StartLease, outcome: Outcome) -> Vec<Effect> {
        let event = Event::Report {
            operation: start.operation,
            lease: start.lease,
            outcome,
        };
        let mut effects = self.feed(event);
        effects.extend(self.feed(Event::Tick));
        effects
    }

    /// `start`'s lease is killed for memory as `outcome` says, and the operation runs
    /// again: the next `Start`, which is for the same operation.
    fn killed(&mut self, start: &StartLease, outcome: Outcome) -> StartLease {
        let effects = self.end(start, outcome);
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Answer(_))),
            "answered after {outcome:?}: {effects:?}"
        );
        let next = effects
            .iter()
            .find_map(|e| match e {
                Effect::Start(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no rerun after {outcome:?}: {effects:?}"));
        assert_eq!(next.operation, start.operation);
        assert_ne!(next.lease, start.lease);
        next
    }

    /// `start`'s lease ends with `outcome`, which must finish its operation with it.
    fn finishes(&mut self, start: &StartLease, outcome: Outcome) {
        let effects = self.end(start, outcome);
        let answers: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Answer(a) => Some(a),
                _ => None,
            })
            .collect();
        let [answer] = answers[..] else {
            panic!("want one answer: {effects:?}");
        };
        assert_eq!(answer.outcome, outcome);
        assert_eq!(answer.operation, start.operation);
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Start(_))),
            "{effects:?}"
        );
    }

    fn requeues(&mut self) -> Vec<RequeueReason> {
        self.s
            .take_requeues()
            .into_iter()
            .map(|r| r.reason)
            .collect()
    }
}

fn own(worker: &str, booked_gib: u64, cap_gib: u64) -> MemoryRun {
    MemoryRun {
        worker: w(worker),
        booked: booked_gib * GIB,
        kill: MemoryKill::OwnLimit { cap: cap_gib * GIB },
    }
}

fn busy(worker: &str, booked_gib: u64) -> MemoryRun {
    MemoryRun {
        worker: w(worker),
        booked: booked_gib * GIB,
        kill: MemoryKill::NodePressure,
    }
}

fn oom(booked_gib: u64, raised_gib: u64) -> RequeueReason {
    RequeueReason::OutOfMemory {
        booked: booked_gib * GIB,
        raised: raised_gib * GIB,
    }
}

/// Catches: a ladder that does not start from the booking (4 GiB, the front's first
/// rung); a ladder that doubles (4 -> 8), skips a rung or stops at 32 GiB instead of booking a whole host;
/// a whole host that is not the largest node's memory; a ladder that never stops (a
/// kill at the whole host run again); a rung that is not committed as a requeue, or is
/// answered; and runs not recorded for the answer's text.
#[test]
fn an_unsized_action_climbs_4_16_32_gib_then_a_whole_host() {
    let mut f = Farm::new();
    f.node("a-small", 8, linux());
    f.node("b-large", 64, linux());
    assert!(f.submit(1, request(1, 4 * GIB)).is_empty());
    let first = f.start();
    assert_eq!(first.resources.memory_bytes, 4 * GIB);
    assert_eq!(first.worker, w("a-small"));

    let second = f.killed(&first, OOM);
    assert_eq!(second.resources.memory_bytes, 16 * GIB);
    assert_eq!(second.worker, w("b-large"), "only the large node holds 16");
    let third = f.killed(&second, OOM);
    assert_eq!(third.resources.memory_bytes, 32 * GIB);
    let fourth = f.killed(&third, OOM);
    assert_eq!(
        fourth.resources.memory_bytes,
        64 * GIB,
        "a whole host: the largest node's memory"
    );
    assert_eq!(f.requeues(), vec![oom(4, 16), oom(16, 32), oom(32, 64)]);

    f.finishes(&fourth, OOM);
    assert!(f.requeues().is_empty(), "a kill at the cap was run again");
    let op = first.operation;
    assert!(matches!(
        f.s.state(op),
        Some(OpState::Failed {
            failure: Failure::OutOfMemory,
            ..
        })
    ));
    assert_eq!(
        f.s.memory_runs(op).unwrap(),
        [
            own("a-small", 4, 64),
            own("b-large", 16, 64),
            own("b-large", 32, 64),
            own("b-large", 64, 64)
        ]
    );
    assert_eq!(f.s.memory_floor(&key(1)), Some(64 * GIB));
}

/// Catches: a size the platform names that is replaced by the first rung (the explicit
/// ask must book as asked), one that does not climb from where it is (a 6 GiB ask
/// killed must book 16, not 4), and one asking more than every rung that does not go
/// straight to the whole host.
#[test]
fn an_explicit_ask_books_as_asked_and_climbs_from_there() {
    let mut f = Farm::new();
    f.node("large", 64, linux());
    f.submit(1, request(1, GIB));
    let one = f.start();
    assert_eq!(one.resources.memory_bytes, GIB, "kbf-book-mem-gib=1");
    let rerun = f.killed(&one, OOM);
    assert_eq!(rerun.resources.memory_bytes, 4 * GIB);
    f.finishes(&rerun, ok());

    f.submit(2, request(2, 6 * GIB));
    let six = f.start();
    assert_eq!(six.resources.memory_bytes, 6 * GIB);
    let rerun = f.killed(&six, OOM);
    assert_eq!(rerun.resources.memory_bytes, 16 * GIB);
    // A whole host waits for an empty node.
    f.finishes(&rerun, ok());

    f.submit(3, request(3, 40 * GIB));
    let forty = f.start();
    assert_eq!(forty.resources.memory_bytes, 40 * GIB);
    assert_eq!(f.killed(&forty, OOM).resources.memory_bytes, 64 * GIB);
}

/// Catches a ladder that ignores the scheduler's own rungs (`--memory-ladder-gib`):
/// with rungs 2 and 8, a kill at 2 GiB books 8, not 4 or 16.
#[test]
fn the_scheduler_climbs_the_rungs_it_is_given() {
    let mut f = Farm::new();
    f.s.set_memory_ladder(MemoryLadder::from_gib(&[2, 8]).unwrap());
    f.node("n", 64, linux());
    f.submit(1, request(1, 2 * GIB));
    let first = f.start();
    let second = f.killed(&first, OOM);
    assert_eq!(second.resources.memory_bytes, 8 * GIB);
    let third = f.killed(&second, OOM);
    assert_eq!(
        third.resources.memory_bytes,
        64 * GIB,
        "after the last rung"
    );
}

/// Catches a cap taken from the wrong nodes: one that ignores a cordoned node (the
/// action would be told it needs more memory than any node has while the 16 GiB node
/// is only cordoned), one that counts a node that does not satisfy the platform or a
/// silent one (the rung would wait for a node that cannot or will not run it), and a
/// cap that is not the node's exact memory when that is not whole GiB.
#[test]
fn the_cap_is_the_largest_live_node_that_fits_cordoned_or_not() {
    let mut f = Farm::new();
    f.node("small", 4, linux());
    f.node("cordoned", 16, linux());
    f.node("mac", 64, mac());
    f.node("silent", 32, linux());
    f.feed(Event::Cordon {
        worker: w("cordoned"),
    });
    f.at_secs(LEASE_GRACE.as_secs());
    f.heartbeat("small", &[]);
    f.heartbeat("cordoned", &[]);
    f.heartbeat("mac", &[]);
    assert!(f.submit(1, request(1, 4 * GIB)).is_empty());
    let first = f.start();
    assert_eq!(first.worker, w("small"));
    f.heartbeat("small", &[&first]);

    let effects = f.end(&first, OOM);
    assert_eq!(
        f.requeues(),
        vec![oom(4, 16)],
        "the 16 GiB cordoned node counts"
    );
    assert_eq!(
        effects,
        vec![Effect::Waiting(Waiting {
            operation: first.operation,
            reason: Some("every live worker that can run it is cordoned: cordoned".to_owned()),
        })]
    );
    assert_eq!(
        f.s.memory_runs(first.operation).unwrap(),
        [own("small", 4, 16)]
    );

    // A node of 6 GiB and a little: the cap is its exact memory.
    let mut f = Farm::new();
    let odd = Resources::new(8_000, 6 * GIB + 4096);
    f.feed(Event::WorkerUp {
        worker: w("odd"),
        instance: DaemonInstance::new("odd"),
        predecessor_stopped: false,
        capacity: odd,
        caps: linux(),
    });
    f.submit(1, request(1, 4 * GIB));
    let first = f.start();
    let second = f.killed(&first, OOM);
    assert_eq!(second.resources.memory_bytes, 6 * GIB + 4096);
    f.finishes(&second, OOM);
}

/// Catches a cap that counts a node that could never run the action: one with no
/// driver for its lease kind, too few CPUs or too few GPUs for its request. Each is
/// larger than the node that ran it, so counting any of them would rerun the action
/// for a node it cannot use instead of telling the client at once.
#[test]
fn the_cap_counts_only_nodes_that_hold_the_whole_request() {
    let mut f = Farm::new();
    let up = |f: &mut Farm, name: &str, capacity: Resources, caps: NodeCaps| {
        let event = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity,
            caps,
        };
        assert!(f.feed(event).is_empty());
    };
    let gpu =
        |cpus: u64, gib: u64, gpus: u64| Resources::new(cpus * 1_000, gib * GIB).with_gpus(gpus);
    let whole_only = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["native-whole-machine"]);
    up(&mut f, "fits", gpu(8, 4, 1), linux());
    up(&mut f, "no-gpu", gpu(8, 64, 0), linux());
    up(&mut f, "one-core", gpu(1, 64, 1), linux());
    up(&mut f, "whole-only", gpu(8, 64, 1), whole_only);
    let mut wide = request(1, 4 * GIB);
    wide.resources = gpu(2, 4, 1);
    f.submit(1, wide);
    let start = f.start();
    assert_eq!(start.worker, w("fits"));
    f.finishes(&start, OOM);
    assert_eq!(
        f.s.memory_runs(start.operation).unwrap(),
        [own("fits", 4, 4)]
    );
}

/// Catches: a remembered ask ignored (a later submission of the key starts at the
/// first rung and climbs again), a floor kept per operation rather than per action key
/// (another key would start raised), and a floor that lowers a request that already
/// asks for more.
#[test]
fn a_later_submission_of_the_key_starts_at_the_remembered_ask() {
    let mut f = Farm::new();
    f.node("large", 64, linux());
    f.submit(1, request(1, 4 * GIB));
    let first = f.start();
    let second = f.killed(&first, OOM);
    assert_eq!(second.resources.memory_bytes, 16 * GIB);
    f.finishes(&second, ok());
    assert_eq!(f.s.memory_floor(&key(1)), Some(16 * GIB));

    f.submit(2, request(1, 4 * GIB));
    let again = f.start();
    assert_ne!(again.operation, first.operation);
    assert_eq!(again.resources.memory_bytes, 16 * GIB, "the floor");
    f.finishes(&again, ok());

    f.submit(3, request(2, 4 * GIB));
    let other = f.start();
    assert_eq!(other.resources.memory_bytes, 4 * GIB, "another key's floor");
    f.finishes(&other, ok());

    f.submit(4, request(1, 32 * GIB));
    let larger = f.start();
    assert_eq!(
        larger.resources.memory_bytes,
        32 * GIB,
        "lowered to the floor"
    );
}

/// Catches a floor kept by action digest alone rather than by instance and digest:
/// the same action under another instance name would start at the first instance's
/// raised ask instead of its own, and its floor would be shared.
#[test]
fn two_instances_with_the_same_action_keep_separate_floors() {
    let mut f = Farm::new();
    f.node("large", 16, linux());
    f.submit(1, request(1, GIB));
    let first = f.start();
    let second = f.killed(&first, OOM);
    assert_eq!(second.resources.memory_bytes, 4 * GIB);
    f.finishes(&second, ok());
    assert_eq!(f.s.memory_floor(&key(1)), Some(4 * GIB));

    let mut other = request(1, GIB);
    other.key.instance = "other".to_owned();
    assert_eq!(other.key.action, key(1).action);
    assert_eq!(
        f.s.memory_floor(&other.key),
        None,
        "a floor from another instance"
    );
    f.submit(2, other.clone());
    let elsewhere = f.start();
    assert_ne!(elsewhere.operation, first.operation);
    assert_eq!(
        elsewhere.resources.memory_bytes, GIB,
        "booked at another instance's floor"
    );
    let raised = f.killed(&elsewhere, OOM);
    let raised = f.killed(&raised, OOM);
    assert_eq!(raised.resources.memory_bytes, 16 * GIB);
    f.finishes(&raised, ok());
    assert_eq!(f.s.memory_floor(&other.key), Some(16 * GIB));
    assert_eq!(
        f.s.memory_floor(&key(1)),
        Some(4 * GIB),
        "raised by another instance"
    );
}

/// Catches: a busy node's kill that raises the booking or the floor (the mutant: it
/// is the node's fault, not the action's), one that is answered at once or rerun
/// without end, a node whose pressure is not counted (or counted against another
/// node), a rerun on the node that killed it while another can run it, and runs not
/// recorded. With both nodes avoided, the third run goes to the first that fits.
#[test]
fn a_busy_node_kill_reruns_with_the_same_ask_and_records_node_pressure() {
    let mut f = Farm::new();
    f.node("busy", 16, linux());
    f.node("other", 16, linux());
    f.submit(1, request(1, 2 * GIB));
    let mut start = f.start();
    assert_eq!(start.worker, w("busy"));
    let mut pressure = Vec::new();
    for n in 1..=FARM_RERUNS {
        let next = f.killed(&start, PRESSURE);
        assert_eq!(
            next.resources, start.resources,
            "rerun {n} changed the booking"
        );
        pressure.push((
            f.s.memory_pressure(&w("busy")),
            f.s.memory_pressure(&w("other")),
        ));
        start = next;
    }
    assert_eq!(pressure, [(1, 0), (1, 1)]);
    assert_eq!(start.worker, w("busy"));
    assert_eq!(f.requeues(), vec![RequeueReason::NodeMemoryPressure; 2]);
    assert_eq!(f.s.memory_floor(&key(1)), None, "a floor from a busy node");

    f.finishes(&start, PRESSURE);
    assert_eq!(f.s.memory_pressure(&w("busy")), 2);
    assert_eq!(f.s.memory_pressure(&w("other")), 1);
    assert_eq!(f.s.memory_pressure(&w("unknown")), 0);
    assert_eq!(
        f.s.memory_runs(start.operation).unwrap(),
        [busy("busy", 2), busy("other", 2), busy("busy", 2)]
    );
    assert!(f.s.memory_runs(OperationId(99)).is_none());
}

/// Catches either budget spending the other: busy-node kills that shorten the ladder
/// (the action told it needs more memory below the largest node), and a ladder that
/// uses up the busy-node reruns (a busy node's kill at the cap answered as the
/// action's error, or as the farm's at once).
#[test]
fn neither_budget_starves_the_other() {
    let mut f = Farm::new();
    f.node("large", 16, linux());
    f.submit(1, request(1, GIB));
    let mut start = f.start();
    for _ in 0..FARM_RERUNS {
        start = f.killed(&start, PRESSURE);
    }
    let start = f.killed(&start, OOM);
    let start = f.killed(&start, OOM);
    assert_eq!(start.resources.memory_bytes, 16 * GIB);
    f.finishes(&start, OOM);

    f.submit(2, request(2, GIB));
    let start = f.start();
    let start = f.killed(&start, OOM);
    let mut start = f.killed(&start, OOM);
    assert_eq!(start.resources.memory_bytes, 16 * GIB);
    for _ in 0..FARM_RERUNS {
        start = f.killed(&start, PRESSURE);
        assert_eq!(start.resources.memory_bytes, 16 * GIB);
    }
    f.finishes(&start, PRESSURE);
    assert_eq!(
        f.s.memory_runs(start.operation).unwrap(),
        [
            own("large", 1, 16),
            own("large", 4, 16),
            busy("large", 16),
            busy("large", 16),
            busy("large", 16)
        ]
    );
}

/// Catches a whole-machine lease's kill that raises its least worker size from the
/// request rather than from what it booked (the whole node it ran on), which would
/// place it on the same node again.
#[test]
fn a_whole_machine_kill_asks_for_a_larger_node() {
    let mut f = Farm::new();
    f.node("a-small", 4, whole());
    f.node("b-large", 8, whole());
    let mut whole = request(1, GIB);
    whole.kind = LeaseKind::WholeMachine;
    f.submit(1, whole);
    let first = f.start();
    assert_eq!(first.worker, w("a-small"));
    assert_eq!(first.resources.memory_bytes, 4 * GIB);
    let second = f.killed(&first, OOM);
    assert_eq!(second.worker, w("b-large"));
    assert_eq!(f.requeues(), vec![oom(4, 8)]);
    f.finishes(&second, OOM);
}

/// Catches a memory kill committed for a lease the operation no longer holds (it was
/// given up, and the result raced the log) that still raises the booking or a floor,
/// records a run, or counts against the node; and a lost run committed so that is
/// charged to the farm budget (issue #22) again: the silence charged one run, and the
/// three results committed after it add none.
#[test]
fn a_memory_kill_of_a_lease_given_up_changes_nothing() {
    let mut f = Farm::new();
    f.node("a", 16, linux());
    f.submit(1, request(1, GIB));
    let first = f.start();
    f.at_secs(LEASE_GRACE.as_secs());
    // Nowhere live to run it again: it waits.
    f.feed(Event::Tick);
    assert_eq!(
        f.s.take_requeues(),
        vec![Requeue {
            operation: first.operation,
            lease: first.lease,
            worker: w("a"),
            reason: RequeueReason::Silent,
        }]
    );
    let lost = Outcome::Failed(Failure::Lost(LostWhy::Fenced));
    for outcome in [OOM, PRESSURE, lost] {
        let record = ResultRecord {
            lease: first.lease,
            operation: first.operation,
            outcome,
        };
        let effects = f.feed(Event::Committed(ControlRecord::Result(record)));
        assert!(effects.is_empty(), "{effects:?}");
    }
    assert_eq!(f.s.state(first.operation), Some(&OpState::Queued));
    assert_eq!(f.s.memory_runs(first.operation).unwrap(), []);
    assert_eq!(f.s.memory_floor(&key(1)), None);
    assert_eq!(f.s.memory_pressure(&w("a")), 0);
    let silent = FarmRun {
        worker: w("a"),
        fault: FarmFault::Lost(LostWhy::Fenced),
    };
    assert_eq!(f.s.farm_runs(first.operation).unwrap(), [silent]);
    f.heartbeat("a", &[]);
    let again = f.start();
    assert_eq!(again.resources.memory_bytes, GIB);
    assert_eq!(again.worker, w("a"));
}

/// Catches floors kept without bound: past the bound the one raised longest ago is
/// forgotten, and a later submission of its key starts at its own ask again.
#[test]
fn floors_are_bounded() {
    let mut f = Farm::new();
    f.s = Scheduler::new(1).with_memory_floors(1);
    f.node("a", 16, linux());
    for n in 1..=2 {
        f.submit(u64::from(n), request(n, GIB));
        let start = f.start();
        let next = f.killed(&start, OOM);
        f.finishes(&next, ok());
    }
    assert_eq!(f.s.memory_floor(&key(1)), None);
    assert_eq!(f.s.memory_floor(&key(2)), Some(4 * GIB));
    f.submit(3, request(1, GIB));
    assert_eq!(f.start().resources.memory_bytes, GIB);
}

/// Catches a floor change that names the wrong floor or the wrong stamp, forgotten keys
/// not handed over once (a key deleted from the store twice, or never), a change for an
/// operation the scheduler no longer holds that loses the forgotten keys, and floors
/// given back to a scheduler that do not floor a fresh submission.
#[test]
fn a_floor_change_names_the_raise_and_the_forgotten_keys_once() {
    let mut f = Farm::new();
    f.s = Scheduler::new(1).with_memory_floors(1);
    f.node("a", 16, linux());
    let mut ops = Vec::new();
    for n in 1..=2 {
        f.submit(u64::from(n), request(n, GIB));
        let start = f.start();
        ops.push(start.operation);
        let next = f.killed(&start, OOM);
        f.finishes(&next, ok());
    }
    let first = f.s.take_floor_change(ops[0]);
    // Both kills happened before the first call: key 1 was forgotten, so the change for
    // its operation raises nothing, and reports the forgotten key.
    assert_eq!(first.raised, None);
    assert_eq!(first.evicted, vec![key(1)]);
    let second = f.s.take_floor_change(ops[1]);
    assert_eq!(
        second.raised,
        Some(Floor {
            key: key(2),
            ask: 4 * GIB,
            stamp: 1
        })
    );
    assert_eq!(second.evicted, vec![]);
    assert_eq!(
        f.s.take_floor_change(OperationId(99)),
        FloorChange::default()
    );

    let mut successor = Scheduler::new(2).with_memory_floors(1);
    successor.restore_floors(vec![
        Floor {
            key: key(1),
            ask: 4 * GIB,
            stamp: 0,
        },
        second.raised.expect("raised"),
    ]);
    assert_eq!(successor.memory_floor(&key(1)), None, "the bound");
    assert_eq!(successor.memory_floor(&key(2)), Some(4 * GIB));
    let forgotten = successor.take_floor_change(OperationId(99));
    assert_eq!(forgotten.evicted, vec![key(1)]);
    assert_eq!(forgotten.raised, None);
}
