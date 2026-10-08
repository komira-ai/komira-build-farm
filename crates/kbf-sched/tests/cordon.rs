//! Cordon and drain, one scenario each, through the scheduler's public inputs.

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{Cordon, Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, OperationId,
    Outcome, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn request(n: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn secs(s: u64) -> FarmTime {
    FarmTime::from_millis(s * 1_000)
}

struct Harness {
    s: Scheduler,
    now: FarmTime,
    next_waiter: u64,
}

impl Harness {
    fn new() -> Self {
        Self::with(Scheduler::new(1))
    }

    fn with(s: Scheduler) -> Self {
        Self {
            s,
            now: FarmTime::default(),
            next_waiter: 0,
        }
    }

    fn at(&mut self, s: u64) -> &mut Self {
        self.now = secs(s);
        self
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    /// Registers `name` (again) with 4 CPUs and 8 GiB.
    fn worker(&mut self, name: &str) {
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).unwrap();
        self.feed(Event::WorkerUp {
            worker: w(name),
            capacity: Resources::new(4_000, 8 * GIB),
            caps,
        });
    }

    fn submit(&mut self, n: u8) {
        let waiter = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        self.feed(Event::Submit {
            waiter,
            request: request(n),
        });
    }

    /// Ticks; the grants proposed, and every other effect.
    fn tick(&mut self) -> (Vec<LeaseGrant>, Vec<Effect>) {
        let mut grants = Vec::new();
        let mut other = Vec::new();
        for e in self.feed(Event::Tick) {
            match e {
                Effect::Commit(ControlRecord::Lease(g)) => grants.push(g),
                e => other.push(e),
            }
        }
        (grants, other)
    }

    /// Ticks, commits every grant and reports each started; the grants.
    fn place(&mut self) -> Vec<LeaseGrant> {
        let (grants, _) = self.tick();
        for g in &grants {
            self.feed(Event::Committed(ControlRecord::Lease(g.clone())));
            self.feed(Event::Started {
                operation: g.operation,
                lease: g.lease,
            });
        }
        grants
    }

    /// Reports `grant` done and commits the result.
    fn finish(&mut self, grant: &LeaseGrant) {
        let outcome = Outcome::Completed {
            action_result: digest(200),
        };
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome,
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("report proposed {proposed:?}");
        };
        let record = record.clone();
        self.feed(Event::Committed(record));
    }

    fn cordon(&mut self, name: &str) {
        assert!(self.feed(Event::Cordon { worker: w(name) }).is_empty());
    }

    fn drain(&mut self, name: &str, deadline: u64) {
        let deadline = secs(deadline);
        assert!(
            self.feed(Event::Drain {
                worker: w(name),
                deadline
            })
            .is_empty()
        );
    }

    fn uncordon(&mut self, name: &str) {
        assert!(self.feed(Event::Uncordon { worker: w(name) }).is_empty());
    }

    fn state(&self, op: OperationId) -> &OpState {
        self.s.state(op).expect("the operation exists")
    }

    fn heartbeat(&mut self, name: &str) {
        let running = self.s.leases_on(&w(name));
        self.feed(Event::Heartbeat {
            worker: w(name),
            running,
        });
    }
}

/// Catches: cordon ignored in placement (new work lands on the cordoned worker), a
/// cordon that stops or gives up the leases already running there, and an uncordon
/// that does not return the worker to placement.
#[test]
fn a_cordoned_worker_gets_no_new_lease_and_keeps_its_running_ones() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.submit(1);
    let [first] = h.place().try_into().expect("one grant");
    assert_eq!(first.worker, w("a"), "first fit takes a");

    h.cordon("a");
    assert_eq!(h.s.cordon(&w("a")), Some(&Cordon::Cordoned));
    h.submit(2);
    h.submit(3);
    let grants = h.place();
    assert_eq!(grants.len(), 2);
    assert!(grants.iter().all(|g| g.worker == w("b")), "{grants:?}");
    assert!(matches!(h.state(first.operation), OpState::Running { .. }));
    assert_eq!(h.s.leases_on(&w("a")), [first.lease]);
    h.finish(&first);
    assert!(matches!(
        h.state(first.operation),
        OpState::Completed { .. }
    ));

    h.uncordon("a");
    assert_eq!(h.s.cordon(&w("a")), None);
    h.cordon("b");
    h.submit(4);
    let [back] = h.place().try_into().expect("one grant");
    assert_eq!(back.worker, w("a"));
}

/// Catches: a cordon kept per session (a node that reboots during its update would come
/// back serving), and a cordon refused for a worker not registered yet.
#[test]
fn a_cordon_holds_across_registrations_and_before_the_first() {
    let mut h = Harness::new();
    h.cordon("a");
    h.worker("a");
    h.worker("b");
    h.cordon("b");
    h.worker("b");
    h.submit(1);
    let (grants, _) = h.tick();
    assert!(grants.is_empty(), "{grants:?}");
}

/// Catches: work that only a cordoned worker can run told that no worker is connected
/// (or given another wrong reason), work refused before the unservable wait, and the
/// cordoned reason given when an uncordoned worker could run it.
#[test]
fn work_only_cordoned_workers_can_run_waits_with_that_reason() {
    let mut h = Harness::with(Scheduler::new(1).with_unservable_wait(Duration::from_secs(10)));
    h.worker("a");
    h.worker("b");
    h.cordon("a");
    h.cordon("b");
    h.submit(1);
    let (grants, effects) = h.tick();
    assert!(grants.is_empty());
    let [Effect::Waiting(waiting)] = effects.as_slice() else {
        panic!("expected a reason, got {effects:?}");
    };
    assert_eq!(
        waiting.reason.as_deref(),
        Some("every live worker that can run it is cordoned: a, b")
    );
    let op = waiting.operation;
    h.at(5);
    h.heartbeat("a");
    h.heartbeat("b");
    let (_, effects) = h.tick();
    assert!(effects.is_empty(), "still waiting, told once: {effects:?}");
    h.at(10);
    h.heartbeat("a");
    h.heartbeat("b");
    let (_, effects) = h.tick();
    assert!(
        matches!(effects.as_slice(), [Effect::Commit(ControlRecord::Refusal(r))] if r.operation == op),
        "{effects:?}"
    );

    // A request no worker is large enough for keeps its own reason, cordoned or not.
    let mut h = Harness::new();
    h.worker("a");
    h.cordon("a");
    let mut huge = request(2);
    huge.resources = Resources::new(64_000, GIB);
    h.feed(Event::Submit {
        waiter: WaiterId(9),
        request: huge,
    });
    let (_, effects) = h.tick();
    let [Effect::Waiting(waiting)] = effects.as_slice() else {
        panic!("expected a reason, got {effects:?}");
    };
    let reason = waiting.reason.as_deref().unwrap_or_default();
    assert_eq!(reason, "every connected worker is cordoned", "{reason}");

    // So does a request whose platform the cordoned worker does not satisfy.
    let mut mac = request(3);
    mac.needs = kbf_caps::Request::from_platform([("OSFamily", "Darwin")]).unwrap();
    h.feed(Event::Submit {
        waiter: WaiterId(10),
        request: mac,
    });
    let (_, effects) = h.tick();
    let [Effect::Waiting(waiting)] = effects.as_slice() else {
        panic!("expected a reason, got {effects:?}");
    };
    let reason = waiting.reason.as_deref().unwrap_or_default();
    assert_eq!(reason, "every connected worker is cordoned", "{reason}");

    // An uncordoned worker too small for it: the size is the reason, not the cordon.
    h.worker("small");
    h.feed(Event::Capacity {
        worker: w("small"),
        capacity: Resources::new(1_000, GIB),
        caps: NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).unwrap(),
    });
    let (_, effects) = h.tick();
    let [Effect::Waiting(huge), Effect::Waiting(_)] = effects.as_slice() else {
        panic!("expected new reasons, got {effects:?}");
    };
    let reason = huge.reason.as_deref().unwrap_or_default();
    assert!(reason.contains("are all smaller than"), "{reason}");
}

/// Catches: a drain that gives up or requeues a running lease, one that reports drained
/// while a lease runs, and one that never reports drained once the last lease ends.
#[test]
fn a_drain_waits_for_leases_and_never_gives_them_up() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.submit(1);
    let [lease] = h.place().try_into().expect("one grant");
    h.drain("a", 60);
    assert_eq!(
        h.s.cordon(&w("a")),
        Some(&Cordon::Draining { deadline: secs(60) })
    );
    for t in [10, 20, 59] {
        h.at(t);
        h.heartbeat("a");
        h.heartbeat("b");
        h.tick();
        assert_eq!(
            h.s.cordon(&w("a")),
            Some(&Cordon::Draining { deadline: secs(60) })
        );
        assert!(matches!(h.state(lease.operation), OpState::Running { .. }));
    }
    // A cordon while draining keeps the drain.
    h.cordon("a");
    assert_eq!(
        h.s.cordon(&w("a")),
        Some(&Cordon::Draining { deadline: secs(60) })
    );
    h.finish(&lease);
    assert_eq!(h.s.cordon(&w("a")), Some(&Cordon::Drained));
    assert!(h.s.leases_on(&w("a")).is_empty());
    // An idle worker drains at once, and a drained worker still gets no work.
    h.drain("b", 100);
    assert_eq!(h.s.cordon(&w("b")), Some(&Cordon::Drained));
    h.submit(2);
    assert!(h.place().is_empty());
}

/// Catches: a drain that kills or requeues work at its deadline instead of pausing, one
/// that never pauses, one that resumes by itself once the leases end, and a second
/// drain that does not restart a paused one.
#[test]
fn a_drain_pauses_at_its_deadline_and_stays_paused() {
    let mut h = Harness::new();
    h.worker("a");
    h.submit(1);
    let [lease] = h.place().try_into().expect("one grant");
    h.drain("a", 10);
    h.at(9);
    h.heartbeat("a");
    h.tick();
    assert_eq!(
        h.s.cordon(&w("a")),
        Some(&Cordon::Draining { deadline: secs(10) })
    );
    h.at(10);
    h.heartbeat("a");
    let (grants, effects) = h.tick();
    assert!(
        grants.is_empty() && effects.is_empty(),
        "{grants:?} {effects:?}"
    );
    assert_eq!(
        h.s.cordon(&w("a")),
        Some(&Cordon::Paused { deadline: secs(10) })
    );
    assert!(matches!(h.state(lease.operation), OpState::Running { .. }));
    assert_eq!(h.s.leases_on(&w("a")), [lease.lease]);

    h.finish(&lease);
    assert_eq!(
        h.s.cordon(&w("a")),
        Some(&Cordon::Paused { deadline: secs(10) }),
        "nothing proceeds by itself"
    );
    h.drain("a", 30);
    assert_eq!(h.s.cordon(&w("a")), Some(&Cordon::Drained));
}
