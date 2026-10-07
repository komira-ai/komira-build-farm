//! The scheduler's rules, one scenario each, driven through its public inputs.

use kbf_sched::{Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, Answer, ControlRecord, Digest, DigestFunction, Effect, Failure, FarmTime,
    FencePolicy, LeaseGrant, LeaseId, OperationId, Outcome, Qos, Resources, ResultRecord,
    StartLease, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn key(instance: &str, n: u8) -> ActionKey {
    ActionKey {
        instance: instance.to_owned(),
        action: digest(n),
    }
}

fn request(n: u8) -> Request {
    Request {
        key: key("main", n),
        qos: Qos::Ci,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
    }
}

fn ok(n: u8) -> Outcome {
    Outcome::Completed {
        action_result: digest(100 + n),
    }
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A scheduler for term 1 with a clock the test moves.
struct Harness {
    s: Scheduler,
    now: FarmTime,
}

impl Harness {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1),
            now: FarmTime::default(),
        }
    }

    fn at_secs(&mut self, secs: u64) -> &mut Self {
        self.now = FarmTime::from_millis(secs * 1_000);
        self
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    fn worker(&mut self, name: &str, cpu_millis: u64, memory: u64) {
        let capacity = Resources::new(cpu_millis, memory);
        assert!(
            self.feed(Event::WorkerUp {
                worker: w(name),
                capacity
            })
            .is_empty()
        );
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    fn heartbeat(&mut self, name: &str) {
        assert!(self.feed(Event::Heartbeat { worker: w(name) }).is_empty());
    }

    /// Ticks and returns the grants proposed.
    fn tick(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .map(|e| match e {
                Effect::Commit(ControlRecord::Lease(g)) => g,
                other => panic!("a tick proposed {other:?}"),
            })
            .collect()
    }

    fn commit(&mut self, record: ControlRecord) -> Vec<Effect> {
        self.feed(Event::Committed(record))
    }

    /// Commits `grant`, expects exactly its Start, and reports it started.
    fn commit_and_start(&mut self, grant: &LeaseGrant) -> StartLease {
        let effects = self.commit(ControlRecord::Lease(grant.clone()));
        let [Effect::Start(start)] = effects.as_slice() else {
            panic!("committing {grant:?} gave {effects:?}");
        };
        let start = start.clone();
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
        start
    }

    fn report(&mut self, grant: &LeaseGrant, outcome: Outcome) -> Vec<Effect> {
        self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome,
        })
    }

    /// Reports and expects the result proposed for commit.
    fn propose(&mut self, grant: &LeaseGrant, outcome: Outcome) -> ControlRecord {
        let proposed = self.report(grant, outcome);
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("report of {grant:?} proposed {proposed:?}");
        };
        record.clone()
    }

    /// Reports, commits the proposed result, and returns the answer.
    fn finish(&mut self, grant: &LeaseGrant, outcome: Outcome) -> Answer {
        let record = self.propose(grant, outcome);
        let answered = self.commit(record.clone());
        let [Effect::Answer(answer)] = answered.as_slice() else {
            panic!("committing {record:?} gave {answered:?}");
        };
        answer.clone()
    }
}

/// Catches: a `Start` emitted when the operation is placed, before its grant is
/// committed; a second `Start` for a grant committed twice; and a `Start` for a grant
/// that expired before its commit arrived (the worker went silent meanwhile).
#[test]
fn start_is_emitted_only_once_the_grant_is_committed() {
    let mut h = Harness::new();
    h.worker("a", 8_000, 8 * GIB);
    h.submit(1, request(1));
    let [grant] = h.tick().try_into().unwrap();
    assert_eq!(grant.lease, LeaseId::new(1, 0));
    assert_eq!(grant.worker, w("a"));
    assert!(matches!(
        h.s.state(grant.operation),
        Some(OpState::Leased {
            committed: false,
            ..
        })
    ));

    let start = h.commit_and_start(&grant);
    assert_eq!(start.key, key("main", 1));
    assert_eq!(start.fence, FencePolicy::RunOn);
    assert!(h.commit(ControlRecord::Lease(grant.clone())).is_empty());

    // A grant that expires before its commit arrives never starts.
    h.submit(2, request(2));
    let [late] = h.tick().try_into().unwrap();
    h.at_secs(60).tick();
    assert_eq!(h.s.state(late.operation), Some(&OpState::Queued));
    assert!(h.commit(ControlRecord::Lease(late)).is_empty());
}

/// Catches: a `Start` for whatever lease the operation holds now when an older grant's
/// commit arrives. Lease 1.0's grant commits only after the operation was re-granted as
/// 1.1 to another live worker; 1.1 is not committed yet, so nothing may start until it is.
#[test]
fn a_superseded_grant_commit_does_not_start_the_newer_uncommitted_lease() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.worker("b", 1_000, GIB);
    h.submit(1, request(1));
    let [old] = h.tick().try_into().unwrap();
    assert_eq!(old.worker, w("a"));

    // `a` goes silent before the grant commits; at G the operation moves to `b`.
    h.at_secs(60).heartbeat("b");
    let [new] = h.tick().try_into().unwrap();
    assert_eq!(
        (new.worker.clone(), new.lease),
        (w("b"), LeaseId::new(1, 1))
    );

    let effects = h.commit(ControlRecord::Lease(old));
    assert!(
        effects.is_empty(),
        "started before its grant committed: {effects:?}"
    );
    assert!(matches!(
        h.s.state(new.operation),
        Some(OpState::Leased {
            committed: false,
            ..
        })
    ));
    // Its own commit starts it, once.
    let start = h.commit_and_start(&new);
    assert_eq!((start.worker, start.lease), (w("b"), new.lease));
}

/// Catches: an operation whose result commits while it waits in the queue (its lease
/// expired with no room elsewhere) left in the queue, so the next tick grants it again,
/// overwrites the finished state and leads to a second Start and a second Answer.
#[test]
fn an_operation_finished_while_queued_is_not_placed_again() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [grant] = h.tick().try_into().unwrap();
    h.commit_and_start(&grant);
    let result = h.propose(&grant, ok(1));

    // `a` goes silent; at G the lease expires and there is nowhere else to go.
    assert!(h.at_secs(60).tick().is_empty());
    assert_eq!(h.s.state(grant.operation), Some(&OpState::Queued));
    let answered = h.commit(result);
    let [Effect::Answer(answer)] = answered.as_slice() else {
        panic!("committing the result gave {answered:?}");
    };
    assert_eq!(answer.lease, grant.lease);
    assert_eq!(h.s.queued().count(), 0, "a finished operation still queued");

    // `a` comes back with room: the finished operation is not granted again.
    h.at_secs(61).heartbeat("a");
    assert!(h.tick().is_empty(), "a finished operation was placed again");
    assert!(h.s.state(grant.operation).is_some_and(OpState::is_done));
}

/// Catches: a result proposed from a lease that expired and was re-dispatched (a late
/// result), and a duplicate report proposed twice. Then the new lease's result is
/// the one answered.
#[test]
fn late_and_duplicate_reports_are_not_proposed() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.worker("b", 1_000, GIB);
    h.submit(1, request(1));
    let [first] = h.tick().try_into().unwrap();
    assert_eq!(first.worker, w("a"));
    h.commit_and_start(&first);

    // `a` goes silent; at G it expires and the operation moves to `b`.
    h.at_secs(59).heartbeat("b");
    assert!(h.tick().is_empty(), "re-dispatched before G");
    h.at_secs(60);
    let [second] = h.tick().try_into().unwrap();
    assert_eq!(second.worker, w("b"));
    assert!(second.lease > first.lease);
    assert_eq!(h.s.booked(&w("a")), Some(Resources::default()));
    h.commit_and_start(&second);

    assert!(h.report(&first, ok(1)).is_empty(), "late result proposed");
    assert_eq!(h.report(&second, ok(2)).len(), 1);
    assert!(h.report(&second, ok(2)).is_empty(), "duplicate proposed");
}

/// Catches: a committed result accepted from a lease the log had already superseded
/// (what a replica replaying the log must reject), a result committed twice answered
/// twice, and a superseding grant still started after the operation finished.
#[test]
fn the_log_order_decides_which_result_wins() {
    // Order 1: the result of lease 1 is proposed, then lease 2 is granted and committed
    // before the result commits. Lease 1's result loses.
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.worker("b", 1_000, GIB);
    h.submit(1, request(1));
    let [first] = h.tick().try_into().unwrap();
    h.commit_and_start(&first);
    let late = h.propose(&first, ok(1));
    h.at_secs(60).heartbeat("b");
    let [second] = h.tick().try_into().unwrap();
    h.commit_and_start(&second);
    assert!(h.commit(late).is_empty(), "superseded result accepted");
    let answer = h.finish(&second, ok(2));
    assert_eq!((answer.lease, answer.outcome), (second.lease, ok(2)));
    assert_eq!(answer.waiters, [WaiterId(1)]);
    let again = ControlRecord::Result(ResultRecord {
        lease: second.lease,
        operation: second.operation,
        outcome: ok(2),
    });
    assert!(h.commit(again).is_empty(), "answered twice");

    // Order 2: the result of lease 1 commits before lease 2's grant. Lease 1 wins, the
    // grant of lease 2 never starts, and `b`'s booking is released.
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.worker("b", 1_000, GIB);
    h.submit(1, request(1));
    let [first] = h.tick().try_into().unwrap();
    h.commit_and_start(&first);
    let early = h.propose(&first, ok(1));
    h.at_secs(60).heartbeat("b");
    let [second] = h.tick().try_into().unwrap();
    let effects = h.commit(early);
    let [Effect::Answer(answer)] = effects.as_slice() else {
        panic!("{effects:?}")
    };
    assert_eq!(answer.lease, first.lease);
    assert!(h.commit(ControlRecord::Lease(second)).is_empty());
    assert_eq!(h.s.booked(&w("b")), Some(Resources::default()));
}

/// Catches: a failure that is retried or dropped instead of answered, and a state
/// that does not record which lease failed.
#[test]
fn a_reported_failure_fails_the_operation() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [grant] = h.tick().try_into().unwrap();
    h.commit_and_start(&grant);
    assert_eq!(
        h.s.state(grant.operation),
        Some(&OpState::Running {
            lease: grant.lease,
            worker: w("a")
        })
    );
    let answer = h.finish(&grant, Outcome::Failed(Failure::Timeout));
    assert_eq!(answer.outcome, Outcome::Failed(Failure::Timeout));
    assert_eq!(
        h.s.state(grant.operation),
        Some(&OpState::Failed {
            lease: grant.lease,
            failure: Failure::Timeout
        })
    );
    assert!(h.tick().is_empty(), "a failed operation was placed again");
}

/// Catches: placement that checks CPU but not memory (or the reverse), bookings that
/// are not added up, and a worker silent for G still offered new work.
#[test]
fn first_fit_books_cpu_and_memory_on_live_workers() {
    let mut h = Harness::new();
    // `a` has CPU to spare but too little memory; `b` has both.
    h.worker("a", 16_000, GIB / 2);
    h.worker("b", 2_000, 4 * GIB);
    h.submit(1, request(1));
    let [g1] = h.tick().try_into().unwrap();
    assert_eq!(g1.worker, w("b"), "placed where memory is short");
    // The second fills `b`'s CPU; the third fits nowhere and waits.
    h.submit(2, request(2));
    h.submit(3, request(3));
    let [g2] = h.tick().try_into().unwrap();
    assert_eq!(g2.worker, w("b"));
    assert_eq!(h.s.booked(&w("b")), Some(Resources::new(2_000, 2 * GIB)));
    assert_eq!(h.s.queued().collect::<Vec<_>>(), [OperationId(2)]);

    // A worker not heard from for G gets nothing, however empty.
    h.worker("c", 64_000, 64 * GIB);
    h.at_secs(60).heartbeat("b");
    assert!(h.tick().is_empty(), "placed on a silent worker");
    h.heartbeat("c");
    let [g3] = h.tick().try_into().unwrap();
    assert_eq!(g3.worker, w("c"));
}

/// Catches: dedup that joins across instance names (another namespace's work answered
/// with this one's result), joins networked or `do_not_cache` work, misses a twin, or
/// joins a finished operation.
#[test]
fn in_flight_dedup_joins_only_the_same_instance_and_digest() {
    let mut h = Harness::new();
    h.worker("a", 64_000, 64 * GIB);
    h.submit(1, request(1));
    h.submit(2, request(1));
    h.submit(
        3,
        Request {
            key: key("other", 1),
            ..request(1)
        },
    );
    h.submit(
        4,
        Request {
            hermetic: false,
            ..request(1)
        },
    );
    h.submit(
        5,
        Request {
            do_not_cache: true,
            ..request(1)
        },
    );
    let grants = h.tick();
    assert_eq!(grants.len(), 4, "{grants:?}");
    assert_eq!(
        h.s.waiters(OperationId(0)),
        Some(&[WaiterId(1), WaiterId(2)][..])
    );
    let start = h.commit_and_start(&grants[2]);
    assert_eq!(start.fence, FencePolicy::SelfFence);

    h.commit_and_start(&grants[0]);
    let answer = h.finish(&grants[0], ok(1));
    assert_eq!(answer.waiters, [WaiterId(1), WaiterId(2)]);
    // Finished: the next caller gets a fresh operation.
    h.submit(6, request(1));
    assert_eq!(h.s.queued().collect::<Vec<_>>(), [OperationId(4)]);
}

/// Catches: a queue ordered by arrival or by the enum's declaration order instead of
/// QoS urgency, an unstable order within a level, and a more urgent caller who joins a
/// queued twin but stays behind less urgent work.
#[test]
fn qos_orders_the_queue_and_a_join_promotes() {
    let mut h = Harness::new();
    let at = |n: u8, qos: Qos| Request { qos, ..request(n) };
    h.submit(1, at(1, Qos::Batch));
    h.submit(2, at(2, Qos::Ci));
    h.submit(3, at(3, Qos::Batch));
    h.submit(4, at(4, Qos::Interactive));
    let order = |h: &Harness| h.s.queued().map(|o| o.0).collect::<Vec<_>>();
    assert_eq!(order(&h), [3, 1, 0, 2]);

    h.submit(5, at(3, Qos::Interactive));
    assert_eq!(h.s.qos(OperationId(2)), Some(&Qos::Interactive));
    assert_eq!(order(&h), [2, 3, 1, 0]);

    // Room for one: the most urgent, oldest operation goes first.
    h.worker("a", 1_000, GIB);
    let [g] = h.tick().try_into().unwrap();
    assert_eq!(g.operation, OperationId(2));
}
