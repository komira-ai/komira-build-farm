//! Lease kinds in placement: a request goes only to a worker whose node report lists a
//! driver serving its kind, and a whole-machine lease books its worker alone. One that
//! fits nowhere holds a worker in queue (QoS) order, so less urgent work stops landing
//! there until it has emptied.

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind,
    Outcome, Qos, Resources, StartLease, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const CAPACITY: Resources = Resources::new(8_000, 16 * GIB);

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// Action `n` of `kind` at `qos`, one core and 1 GiB.
fn request(n: u8, kind: LeaseKind, qos: Qos) -> Request {
    Request {
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
    }
}

fn action(n: u8) -> Request {
    request(n, LeaseKind::Action, Qos::Ci)
}

fn whole(n: u8) -> Request {
    request(n, LeaseKind::WholeMachine, Qos::Ci)
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A scheduler at a fixed farm time: nothing here waits on a grace period.
struct Harness(Scheduler);

impl Harness {
    fn new() -> Self {
        Self(Scheduler::new(1))
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.0
            .apply(Input::new(FarmTime::from_millis(1_000), event))
    }

    /// Registers `name` with 8 cores and 16 GiB, running `drivers`.
    fn worker(&mut self, name: &str, drivers: &[&str]) {
        self.worker_of(name, drivers, CAPACITY);
    }

    /// Registers `name` with `capacity`, running `drivers`.
    fn worker_of(&mut self, name: &str, drivers: &[&str], capacity: Resources) {
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .unwrap()
            .with_drivers(drivers.iter().copied());
        self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            capacity,
            caps,
        });
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// Ticks; the grants proposed, and the reasons given to waiting operations.
    fn tick(&mut self) -> (Vec<LeaseGrant>, Vec<String>) {
        let mut grants = Vec::new();
        let mut reasons = Vec::new();
        for effect in self.feed(Event::Tick) {
            match effect {
                Effect::Commit(ControlRecord::Lease(g)) => grants.push(g),
                Effect::Waiting(waiting) => reasons.extend(waiting.reason),
                other => panic!("a tick proposed {other:?}"),
            }
        }
        (grants, reasons)
    }

    fn grants(&mut self) -> Vec<LeaseGrant> {
        self.tick().0
    }

    /// Commits `grant`; the `Start` it emits.
    fn start(&mut self, grant: &LeaseGrant) -> StartLease {
        let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        let [Effect::Start(start)] = effects.as_slice() else {
            panic!("{effects:?}");
        };
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
        start.clone()
    }

    /// Reports `grant` completed and commits the result.
    fn finish(&mut self, grant: &LeaseGrant) {
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome: Outcome::Completed {
                action_result: digest(200),
            },
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("{proposed:?}");
        };
        let answered = self.feed(Event::Committed(record.clone()));
        assert!(
            matches!(answered.as_slice(), [Effect::Answer(_)]),
            "{answered:?}"
        );
    }
}

/// Catches: the lease kind ignored in placement. `a` runs only the whole-machine
/// driver and sorts first, so placement that ignores the kind gives it the action (and
/// the daemon refuses the `Start`), and may give the whole-machine lease to `b`, whose
/// container driver cannot run it. Also: a `Start` that drops the kind, or books a
/// whole-machine lease's request (one core) instead of the whole worker.
#[test]
fn each_kind_goes_only_to_a_worker_whose_driver_serves_it() {
    let mut h = Harness::new();
    h.worker("a", &["native-whole-machine"]);
    h.worker("b", &["container"]);

    h.submit(1, action(1));
    let [grant] = h.grants().try_into().unwrap();
    assert_eq!(
        grant.worker,
        w("b"),
        "an action on a whole-machine-only worker"
    );
    let start = h.start(&grant);
    assert_eq!(start.kind, LeaseKind::Action);
    assert_eq!(start.resources, Resources::new(1_000, GIB));

    h.submit(2, whole(2));
    let [grant] = h.grants().try_into().unwrap();
    assert_eq!(grant.worker, w("a"));
    let start = h.start(&grant);
    assert_eq!(start.kind, LeaseKind::WholeMachine);
    assert_eq!(
        start.resources, CAPACITY,
        "a whole-machine lease books it all"
    );
    assert_eq!(h.0.booked(&w("a")), Some(CAPACITY));
}

/// Catches: a whole-machine lease offered to workers that do not serve it (the daemon
/// refuses its `Start`, as every daemon on `main` does), and one waiting without
/// saying why, or said to wait for a cordoned worker that does not serve it either. A
/// worker that reports no driver at all serves nothing.
#[test]
fn a_whole_machine_lease_waits_while_no_live_worker_serves_it() {
    let mut h = Harness::new();
    h.worker("a", &["container"]);
    h.worker("b", &["native"]);
    h.worker("c", &[]);
    h.feed(Event::Cordon { worker: w("a") });
    h.submit(1, whole(1));
    let (grants, reasons) = h.tick();
    assert!(grants.is_empty(), "{grants:?}");
    let [why] = reasons.as_slice() else {
        panic!("{reasons:?}");
    };
    assert_eq!(
        why,
        "none of the 2 live worker(s) serves lease kind whole_machine: that needs a driver \
         among native-whole-machine"
    );

    h.submit(2, action(2));
    let [grant] = h.grants().try_into().unwrap();
    assert_eq!(grant.worker, w("b"), "an action on a worker with no driver");
}

/// Catches: the platform checked against workers that do not serve the kind, so the
/// reason names a closest worker that could never run the lease.
#[test]
fn a_platform_miss_is_explained_among_the_workers_that_serve_the_kind() {
    let mut h = Harness::new();
    h.worker("a", &["container"]);
    h.worker("b", &["native-whole-machine"]);
    let mac = kbf_caps::Request::parse([("os", "macos")]).unwrap();
    h.submit(
        1,
        Request {
            needs: mac,
            ..whole(1)
        },
    );
    let (grants, reasons) = h.tick();
    assert!(grants.is_empty(), "{grants:?}");
    let [why] = reasons.as_slice() else {
        panic!("{reasons:?}");
    };
    assert_eq!(
        why,
        "none of the 1 live worker(s) serving lease kind whole_machine satisfies the \
         action's platform; the closest, b, lacks os=macos"
    );
}

/// Catches: two waiting whole-machine leases holding the same worker (the second then
/// holds nothing back and is passed by every younger action), and a hold on a worker
/// too small to ever run the lease. A third, with no worker left to hold, holds none.
#[test]
fn waiting_whole_machine_leases_hold_different_workers() {
    let mut h = Harness::new();
    let both = ["container", "native-whole-machine"];
    h.worker_of("a", &both, Resources::new(500, GIB));
    h.worker("b", &both);
    h.worker("c", &both);
    for n in 1..=9 {
        h.submit(u64::from(n), action(n));
    }
    // `a` is too small for an action: eight on `b`, one on `c`.
    let grants = h.grants();
    assert_eq!(grants.iter().filter(|g| g.worker == w("c")).count(), 1);

    for n in 10..=12 {
        h.submit(u64::from(n), whole(n));
    }
    assert!(h.grants().is_empty());
    let queued: Vec<_> = h.0.queued().collect();
    let held: Vec<_> = queued.iter().map(|op| h.0.reservation(*op)).collect();
    assert_eq!(held, [Some(&w("c")), Some(&w("b")), None]);
}

/// Catches: a whole-machine lease placed beside an action (a worker that is not empty
/// taken as one with room), an action placed beside a whole-machine lease (its booking
/// read as a share, or a capacity that grew while it ran read as room), and a
/// whole-machine booking not given back in full when it ends. Also: younger work of
/// the same QoS placed on the worker a waiting whole-machine lease holds, which keeps
/// it from ever emptying.
#[test]
fn a_whole_machine_lease_runs_alone_on_its_worker() {
    let mut h = Harness::new();
    h.worker("a", &["container", "native-whole-machine"]);

    h.submit(1, action(1));
    let [first] = h.grants().try_into().unwrap();
    h.start(&first);

    h.submit(2, whole(2));
    h.submit(3, action(3));
    assert!(
        h.grants().is_empty(),
        "a whole-machine lease beside an action, or work past its reservation"
    );
    let waiting =
        h.0.queued()
            .next()
            .expect("the whole-machine lease is queued");
    assert_eq!(h.0.reservation(waiting), Some(&w("a")));

    h.finish(&first);
    let [lease] = h.grants().try_into().unwrap();
    assert_eq!(lease.operation, waiting);
    assert_eq!(h.0.reservation(waiting), None);
    let start = h.start(&lease);
    assert_eq!(start.resources, CAPACITY);

    // The daemon reports more room while it runs: still nothing beside it.
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container", "native-whole-machine"]);
    h.feed(Event::Capacity {
        worker: w("a"),
        capacity: Resources::new(16_000, 32 * GIB),
        caps,
    });
    h.submit(4, action(4));
    assert!(
        h.grants().is_empty(),
        "an action beside a whole-machine lease"
    );

    h.finish(&lease);
    assert_eq!(h.0.booked(&w("a")), Some(Resources::default()));
    assert_eq!(h.grants().len(), 2, "the actions run once it ends");
}

/// Catches: a reservation held against every QoS level, so a batch whole-machine lease
/// waiting for a worker delays CI work there; and a reservation that does not hold
/// back less urgent work.
#[test]
fn a_reservation_holds_back_only_less_urgent_work() {
    let mut h = Harness::new();
    h.worker("a", &["container", "native-whole-machine"]);
    h.submit(1, request(1, LeaseKind::Action, Qos::Batch));
    let [first] = h.grants().try_into().unwrap();
    h.start(&first);

    h.submit(2, request(2, LeaseKind::WholeMachine, Qos::Batch));
    h.submit(3, request(3, LeaseKind::Action, Qos::Batch));
    assert!(h.grants().is_empty(), "batch work past a batch reservation");

    h.submit(4, request(4, LeaseKind::Action, Qos::Ci));
    let [ci] = h.grants().try_into().unwrap();
    assert_eq!(
        ci.worker,
        w("a"),
        "CI work held back by a batch reservation"
    );
}

/// Catches: a reservation that does not follow the worker that could run it. A
/// whole-machine lease holds the emptier of two workers, keeps holding it while it
/// empties, and moves when that worker is cordoned; the other worker takes new work
/// meanwhile.
#[test]
fn a_reservation_holds_one_worker_and_moves_when_it_cannot_run_there() {
    let mut h = Harness::new();
    let both = ["container", "native-whole-machine"];
    h.worker("a", &both);
    h.worker("b", &both);
    for n in 1..=9 {
        h.submit(u64::from(n), action(n));
    }
    // First fit: eight on `a`, one on `b`.
    let grants = h.grants();
    assert_eq!(grants.iter().filter(|g| g.worker == w("b")).count(), 1);

    h.submit(10, whole(10));
    assert!(h.grants().is_empty());
    let waiting = h.0.queued().next().expect("queued");
    assert_eq!(
        h.0.reservation(waiting),
        Some(&w("b")),
        "the emptier worker"
    );

    let a_done = grants.iter().find(|g| g.worker == w("a")).unwrap().clone();
    h.start(&a_done);
    h.finish(&a_done);
    assert!(h.grants().is_empty());
    assert_eq!(
        h.0.reservation(waiting),
        Some(&w("b")),
        "kept while it empties"
    );

    h.submit(11, action(11));
    let [next] = h.grants().try_into().unwrap();
    assert_eq!(
        next.worker,
        w("a"),
        "work past a reservation goes elsewhere"
    );

    h.feed(Event::Cordon { worker: w("b") });
    assert!(h.grants().is_empty());
    assert_eq!(
        h.0.reservation(waiting),
        Some(&w("a")),
        "moved off a cordon"
    );
}
