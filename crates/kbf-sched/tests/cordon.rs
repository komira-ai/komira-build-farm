//! Cordon and drain, one scenario each, through the scheduler's public inputs.

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{Cordon, Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, OperationId,
    Outcome, Qos, Resources, StateMachine, WaiterId, Waiting, WorkerId,
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

    /// Uncordons `name`; the grants placed at once (waiting reasons cleared too).
    fn uncordon(&mut self, name: &str) -> Vec<LeaseGrant> {
        let effects = self.feed(Event::Uncordon { worker: w(name) });
        effects
            .into_iter()
            .filter_map(|e| match e {
                Effect::Commit(ControlRecord::Lease(g)) => Some(g),
                Effect::Waiting(Waiting { reason: None, .. }) => None,
                e => panic!("an uncordon proposed {e:?}"),
            })
            .collect()
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
/// (or given another wrong reason), work refused because only cordoned workers could
/// run it (a cordon is temporary: it must wait, however long), the cordoned reason
/// given when an uncordoned worker could run it, and an uncordon that leaves the work
/// queued until the next tick.
#[test]
fn work_only_cordoned_workers_can_run_waits_and_is_never_refused() {
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
    for t in [5, 10, 100, 10_000] {
        h.at(t);
        h.heartbeat("a");
        h.heartbeat("b");
        let (grants, effects) = h.tick();
        assert!(
            grants.is_empty() && effects.is_empty(),
            "still waiting at {t} s, told once: {effects:?}"
        );
        assert_eq!(h.state(op), &OpState::Queued);
    }
    assert_eq!(
        h.s.waiting(op),
        Some("every live worker that can run it is cordoned: a, b")
    );
    // The uncordon itself places the work: no tick in between.
    let [grant] = h.uncordon("b").try_into().expect("placed by the uncordon");
    assert_eq!((grant.operation, grant.worker), (op, w("b")));
    assert_eq!(h.s.waiting(op), None);

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

/// Catches: the cordoned workers consulted when an uncordoned worker could run the
/// work once its bookings end (the work would be told it waits for a cordon, and
/// would never be refused even if that worker left).
#[test]
fn work_a_busy_uncordoned_worker_can_run_does_not_wait_for_a_cordon() {
    let mut h = Harness::new();
    h.worker("a");
    h.worker("b");
    h.cordon("a");
    for n in 1..=4 {
        h.submit(n);
    }
    assert_eq!(h.place().len(), 4, "b is full");
    h.submit(5);
    let (grants, effects) = h.tick();
    assert!(
        grants.is_empty() && effects.is_empty(),
        "{grants:?} {effects:?}"
    );
    let queued: Vec<_> = h.s.queued().collect();
    assert_eq!(queued.len(), 1);
    assert_eq!(h.s.waiting(queued[0]), None);
}

/// Catches: the unservable wait dropped for work that no worker, cordoned or not,
/// could ever run (a cordoned worker in the farm must not keep it queued forever), and
/// time spent waiting for a cordon counted toward the refusal (work would be refused
/// the moment the cordoned worker that could run it leaves).
#[test]
fn work_no_worker_could_run_is_refused_even_with_cordons() {
    let wait = Duration::from_secs(10);
    let mut h = Harness::with(Scheduler::new(1).with_unservable_wait(wait));
    h.worker("a");
    h.cordon("a");
    let mut huge = request(1);
    huge.resources = Resources::new(64_000, GIB);
    h.feed(Event::Submit {
        waiter: WaiterId(0),
        request: huge,
    });
    let (_, effects) = h.tick();
    let [Effect::Waiting(waiting)] = effects.as_slice() else {
        panic!("expected a reason, got {effects:?}");
    };
    let op = waiting.operation;
    h.at(10);
    h.heartbeat("a");
    let (_, effects) = h.tick();
    assert!(
        matches!(effects.as_slice(), [Effect::Commit(ControlRecord::Refusal(r))]
            if r.operation == op && r.reason.starts_with("every connected worker is cordoned")),
        "{effects:?}"
    );

    // Unservable, then only a cordoned worker could run it, then unservable again:
    // the wait starts again when it becomes unservable.
    let mut h = Harness::with(Scheduler::new(1).with_unservable_wait(wait));
    h.worker("a");
    h.submit(2);
    assert_eq!(h.place().len(), 1, "a takes op 2");
    h.submit(3);
    h.cordon("a");
    // `a` holds a lease and is cordoned: only it could run op 3, so op 3 waits for it.
    let (_, effects) = h.tick();
    let [Effect::Waiting(waiting)] = effects.as_slice() else {
        panic!("expected a reason, got {effects:?}");
    };
    let op = waiting.operation;
    assert_eq!(
        waiting.reason.as_deref(),
        Some("every live worker that can run it is cordoned: a")
    );
    // `a` goes silent and is gone: now nothing could run it.
    h.at(100);
    let (_, effects) = h.tick();
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Waiting(w)
            if w.operation == op && w.reason.as_deref() == Some("no worker is connected"))),
        "{effects:?}"
    );
    assert_eq!(
        h.state(op),
        &OpState::Queued,
        "refused for time spent on a cordon"
    );
    h.at(109);
    let (_, effects) = h.tick();
    assert!(effects.is_empty(), "{effects:?}");
    h.at(110);
    let (_, effects) = h.tick();
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Commit(ControlRecord::Refusal(r)) if r.operation == op)),
        "{effects:?}"
    );

    // Unservable, then a cordoned worker that could run it arrives: it waits for the
    // cordon from then on, past the wait it had started.
    let mut h = Harness::with(Scheduler::new(1).with_unservable_wait(wait));
    h.submit(4);
    let (_, effects) = h.tick();
    let [Effect::Waiting(waiting)] = effects.as_slice() else {
        panic!("expected a reason, got {effects:?}");
    };
    let op = waiting.operation;
    h.at(5);
    h.cordon("a");
    h.worker("a");
    for t in [5, 10, 50] {
        h.at(t);
        h.heartbeat("a");
        h.tick();
        assert_eq!(h.state(op), &OpState::Queued, "refused at {t} s");
    }
    assert_eq!(
        h.s.waiting(op),
        Some("every live worker that can run it is cordoned: a")
    );
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
