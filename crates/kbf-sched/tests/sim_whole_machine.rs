//! Whole-machine leases under a stream of actions, over a seed sweep (issue #169 is
//! the action-lease side of the same starvation). Three workers of 4 cores heartbeat
//! every second: `w0` and `w1` run the container and whole-machine drivers, `w2` the
//! container driver only. One-core CI actions arrive at random and run 5-60 s, about
//! 80% of the farm, and first fit keeps `w0` and `w1` busy: without a reservation they
//! never empty while the stream lasts. Now and then a whole-machine lease arrives, at
//! CI or batch QoS, and runs 20-90 s. The control log is in process: a `Commit` is fed
//! straight back.
//!
//! The checks, after every input of every seed:
//! - a whole-machine lease never runs beside another lease, and never on `w2`;
//! - no worker ever books more than its capacity;
//! - every CI whole-machine lease is granted within [`CI_BOUND`] of its submission,
//!   though actions keep arriving the whole time (the sweep shows reservations held);
//! - every operation is granted once and answered once, and all of them run by the end;
//! - a seed replays to the same trace.

use std::collections::BTreeMap;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind,
    OperationId, Outcome, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const SEEDS: u64 = 48;
const GIB: u64 = 1 << 30;
const WORKERS: [(&str, &[&str]); 3] = [
    ("w0", &["container", "native-whole-machine"]),
    ("w1", &["container", "native-whole-machine"]),
    ("w2", &["container"]),
];
const CAPACITY: Resources = Resources::new(4_000, 8 * GIB);
/// Submissions happen before this second.
const ACTIVE_UNTIL: u64 = 1_200;
const END: u64 = 2_400;
/// The longest a CI whole-machine lease may wait while actions keep arriving: the
/// reserved worker's longest action (60 s), behind at most the whole-machine leases
/// queued before it (each up to 90 s on one of the two workers), with room to spare.
const CI_BOUND: u64 = 400;

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

struct World {
    sched: Scheduler,
    rng: SimRng,
    now: u64,
    next: u64,
    /// Running leases: when each reports.
    running: BTreeMap<u64, Vec<LeaseGrant>>,
    /// Each submitted operation's kind, QoS and submission second, by waiter.
    submitted: BTreeMap<u64, (LeaseKind, Qos, u64)>,
    /// The waiter of each operation granted, by operation.
    waiter_of: BTreeMap<OperationId, u64>,
    grants: BTreeMap<OperationId, u32>,
    answers: BTreeMap<OperationId, u32>,
    /// The longest a CI whole-machine lease waited for its grant (seconds).
    longest_ci_whole_wait: u64,
    /// Ticks after which some whole-machine operation held a reservation.
    reserved_ticks: u64,
    trace: Vec<String>,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut w = Self {
            sched: Scheduler::new(1),
            rng: SimRng::from_seed(seed),
            now: 0,
            next: 0,
            running: BTreeMap::new(),
            submitted: BTreeMap::new(),
            waiter_of: BTreeMap::new(),
            grants: BTreeMap::new(),
            answers: BTreeMap::new(),
            longest_ci_whole_wait: 0,
            reserved_ticks: 0,
            trace: Vec::new(),
        };
        for (name, drivers) in WORKERS {
            let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
                .unwrap()
                .with_drivers(drivers.iter().copied());
            w.feed(Event::WorkerUp {
                worker: WorkerId::new(name),
                instance: DaemonInstance::new(name),
                capacity: CAPACITY,
                caps,
            });
        }
        w
    }

    fn feed(&mut self, event: Event) {
        self.trace.push(format!("{} {event:?}", self.now));
        let now = FarmTime::from_millis(self.now * 1_000);
        let mut effects: Vec<Effect> = self.sched.apply(Input::new(now, event));
        while !effects.is_empty() {
            let mut more = Vec::new();
            for effect in effects {
                self.trace.push(format!("  {effect:?}"));
                more.extend(self.carry_out(now, effect));
            }
            effects = more;
        }
        self.check_workers();
    }

    fn carry_out(&mut self, now: FarmTime, effect: Effect) -> Vec<Effect> {
        match effect {
            Effect::Commit(record) => {
                if let ControlRecord::Lease(grant) = &record {
                    *self.grants.entry(grant.operation).or_default() += 1;
                }
                self.sched.apply(Input::new(now, Event::Committed(record)))
            }
            Effect::Start(start) => {
                let waiter = self.sched.waiters(start.operation).expect("live")[0].0;
                let (kind, qos, since) = self.submitted[&waiter].clone();
                assert_eq!(start.kind, kind, "the Start names the request's kind");
                self.waiter_of.insert(start.operation, waiter);
                let runs = if kind == LeaseKind::WholeMachine {
                    assert_ne!(start.worker.as_str(), "w2", "whole machine on w2");
                    assert_eq!(start.resources, CAPACITY, "a whole-machine booking");
                    if qos == Qos::Ci {
                        let waited = self.now - since;
                        self.longest_ci_whole_wait = self.longest_ci_whole_wait.max(waited);
                    }
                    self.rng.between(20, 90)
                } else {
                    self.rng.between(5, 60)
                };
                let grant = LeaseGrant {
                    lease: start.lease,
                    operation: start.operation,
                    worker: start.worker,
                };
                self.running.entry(self.now + runs).or_default().push(grant);
                Vec::new()
            }
            Effect::Answer(answer) => {
                *self.answers.entry(answer.operation).or_default() += 1;
                Vec::new()
            }
            Effect::Waiting(waiting) => {
                panic!("every request is servable here: {waiting:?}")
            }
            Effect::Refuse(refusal) => panic!("refused {refusal:?}"),
        }
    }

    /// No worker holds a whole-machine lease beside another, or books past capacity.
    fn check_workers(&self) {
        for (name, _) in WORKERS {
            let worker = WorkerId::new(name);
            let leases = self.sched.leases_on(&worker);
            let wholes = self
                .running
                .values()
                .flatten()
                .filter(|g| g.worker == worker && leases.contains(&g.lease))
                .filter(|g| {
                    self.submitted[&self.waiter_of[&g.operation]].0 == LeaseKind::WholeMachine
                })
                .count();
            assert!(
                wholes == 0 || leases.len() == 1,
                "{name} runs a whole-machine lease beside others: {leases:?}"
            );
            let booked = self.sched.booked(&worker).unwrap_or_default();
            assert!(CAPACITY.fits(&booked), "{name} booked {booked:?}");
        }
    }

    fn submit(&mut self, kind: LeaseKind, qos: Qos) {
        let n = self.next;
        self.next += 1;
        self.submitted.insert(n, (kind, qos.clone(), self.now));
        let request = Request {
            key: ActionKey {
                instance: "main".to_owned(),
                action: digest(n),
            },
            qos,
            kind,
            resources: Resources::new(1_000, GIB),
            hermetic: true,
            do_not_cache: false,
            needs: kbf_caps::Request::default(),
        };
        self.feed(Event::Submit {
            waiter: WaiterId(n),
            request,
        });
    }

    fn second(&mut self, active: bool) {
        for (name, _) in WORKERS {
            let worker = WorkerId::new(name);
            let running = self.sched.leases_on(&worker);
            self.feed(Event::Heartbeat { worker, running });
        }
        if active && self.rng.chance(Chance::percent(30)) {
            self.submit(LeaseKind::Action, Qos::Ci);
        }
        if active && self.rng.chance(Chance::percent(1)) {
            let qos = if self.rng.chance(Chance::percent(50)) {
                Qos::Ci
            } else {
                Qos::Batch
            };
            self.submit(LeaseKind::WholeMachine, qos);
        }
        if let Some(done) = self.running.remove(&self.now) {
            for grant in done {
                self.feed(Event::Report {
                    operation: grant.operation,
                    lease: grant.lease,
                    outcome: Outcome::Completed {
                        action_result: digest(1_000_000 + grant.operation.0),
                    },
                });
            }
        }
        self.feed(Event::Tick);
        if self
            .sched
            .queued()
            .any(|op| self.sched.reservation(op).is_some())
        {
            self.reserved_ticks += 1;
        }
    }
}

fn run(seed: u64) -> World {
    let mut w = World::new(seed);
    while w.now < END {
        w.second(w.now < ACTIVE_UNTIL);
        w.now += 1;
    }
    w
}

/// Catches: placement with no reservation (a CI whole-machine lease waits until the
/// action stream stops, far past the bound); a whole-machine lease placed beside an
/// action, or on a worker without the whole-machine driver; a whole-machine booking
/// that is not the whole worker; a reservation never released, or one that leaves work
/// stuck (not everything runs by the end); and a lease granted twice.
#[test]
fn whole_machine_leases_are_not_starved_by_a_stream_of_actions() {
    let mut longest = 0;
    let mut reserved_ticks = 0;
    let mut wholes = 0;
    for seed in 0..SEEDS {
        let w = run(seed);
        longest = longest.max(w.longest_ci_whole_wait);
        reserved_ticks += w.reserved_ticks;
        wholes += w
            .submitted
            .values()
            .filter(|(kind, ..)| *kind == LeaseKind::WholeMachine)
            .count();
        assert_eq!(
            w.answers.len(),
            w.submitted.len(),
            "seed {seed}: all answered"
        );
        assert!(
            w.answers.values().all(|&n| n == 1),
            "seed {seed}: answered once"
        );
        assert!(
            w.grants.values().all(|&n| n == 1),
            "seed {seed}: granted once"
        );
    }
    assert!(
        longest <= CI_BOUND,
        "a CI whole-machine lease waited {longest} s, past the {CI_BOUND} s bound"
    );
    // The sweep exercises the reservation: whole-machine leases were submitted and some
    // had to hold a worker while it emptied.
    assert!(wholes > SEEDS as usize, "{wholes} whole-machine leases");
    assert!(reserved_ticks > 0, "no reservation was ever held");
}

/// Catches: placement or reservations that depend on anything but the seed.
#[test]
fn a_seed_replays() {
    assert_eq!(run(5).trace, run(5).trace);
}
