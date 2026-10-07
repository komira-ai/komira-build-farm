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
        self.at_millis(secs * 1_000)
    }

    fn at_millis(&mut self, millis: u64) -> &mut Self {
        self.now = FarmTime::from_millis(millis);
        self
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    /// Registers `name` (again).
    fn worker(&mut self, name: &str, cpu_millis: u64, memory: u64) {
        let capacity = Resources::new(cpu_millis, memory);
        let worker = w(name);
        assert!(self.feed(Event::WorkerUp { worker, capacity }).is_empty());
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// A heartbeat from `name` listing nothing running.
    fn heartbeat(&mut self, name: &str) {
        self.heartbeat_running(name, &[]);
    }

    fn heartbeat_running(&mut self, name: &str, running: &[LeaseId]) {
        let running = running.to_vec();
        assert!(
            self.feed(Event::Heartbeat {
                worker: w(name),
                running,
            })
            .is_empty()
        );
    }

    /// A heartbeat from `name` listing nothing running; returns what it proposed.
    fn beat(&mut self, name: &str) -> Vec<Effect> {
        let worker = w(name);
        self.feed(Event::Heartbeat {
            worker,
            running: Vec::new(),
        })
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

    /// Whether `grant`'s operation is running under some lease.
    fn running(&self, grant: &LeaseGrant) -> bool {
        matches!(self.s.state(grant.operation), Some(OpState::Running { .. }))
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
    // `a` may still run the lost lease: its room stays booked until `a` says otherwise.
    assert_eq!(h.s.booked(&w("a")), Some(Resources::new(1_000, GIB)));
    h.commit_and_start(&second);

    assert!(h.report(&first, ok(1)).is_empty(), "late result proposed");
    assert_eq!(h.report(&second, ok(2)).len(), 1);
    assert!(h.report(&second, ok(2)).is_empty(), "duplicate proposed");
    h.heartbeat("a");
    assert_eq!(
        h.s.booked(&w("a")),
        Some(Resources::default()),
        "booking leaked"
    );
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

/// Catches: a committed lease that a worker the scheduler still hears from leaves out of
/// its running set kept for good (the booking leaks and the waiters wait forever), or
/// given up before its `Start` has been out for the Start grace, while that `Start` may
/// still be on its way (one millisecond early is too early). Once it is given up, the
/// old lease's late report is not proposed and the new lease answers.
#[test]
fn a_lease_left_out_of_the_running_set_is_requeued_after_the_start_grace() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [first] = h.tick().try_into().unwrap();
    // The Start goes out at 1 s.
    h.at_secs(1).commit_and_start(&first);

    h.at_secs(30).heartbeat_running("a", &[first.lease]);
    h.at_millis(60_999).heartbeat("a");
    assert!(
        matches!(h.s.state(first.operation), Some(OpState::Running { .. })),
        "requeued before the Start grace"
    );
    h.at_secs(61).heartbeat("a");
    assert_eq!(h.s.state(first.operation), Some(&OpState::Queued));
    assert_eq!(h.s.booked(&w("a")), Some(Resources::default()));

    let [second] = h.tick().try_into().unwrap();
    assert!(second.lease > first.lease);
    h.commit_and_start(&second);
    assert!(h.report(&first, ok(1)).is_empty(), "late result proposed");
    let answer = h.finish(&second, ok(2));
    assert_eq!(answer.lease, second.lease);
}

/// Catches: a lease requeued because the running set leaves it out while its reported
/// result is on its way to the log (a worker stops listing a lease once its result is
/// acknowledged), which would run the action again for nothing.
#[test]
fn a_lease_whose_result_was_reported_is_kept_when_left_out() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [grant] = h.tick().try_into().unwrap();
    h.commit_and_start(&grant);
    let result = h.propose(&grant, ok(1));

    h.at_secs(60).heartbeat("a");
    h.worker("a", 1_000, GIB);
    h.heartbeat("a");
    assert!(
        matches!(h.s.state(grant.operation), Some(OpState::Running { .. })),
        "requeued with its result on the way"
    );
    let answered = h.commit(result);
    assert!(matches!(answered.as_slice(), [Effect::Answer(a)] if a.lease == grant.lease));
    assert_eq!(h.s.booked(&w("a")), Some(Resources::default()));
}

/// Catches: a worker that registers again keeping, until a grace or G, a committed lease
/// its heartbeats leave out (its `Start` went to the old session, so its run is gone or
/// never began); a registration that requeues by itself, though `Hello` carries no
/// running set (the leases the daemon re-adopted would run twice); one that drops a
/// lease the worker lists as re-adopted; and one that gives up, before the Start grace,
/// a grant whose `Start` went to the new session (it may still be on its way).
#[test]
fn a_heartbeat_after_registering_again_requeues_at_once_only_what_the_worker_lost() {
    let mut h = Harness::new();
    h.worker("a", 3_000, 3 * GIB);
    h.submit(1, request(1));
    h.submit(2, request(2));
    h.submit(3, request(3));
    let [kept, lost, pending] = h.tick().try_into().unwrap();
    h.commit_and_start(&kept);
    h.commit_and_start(&lost);

    // The daemon restarts 10 s later and re-adopts `kept`. Its Hello lists nothing.
    h.at_secs(10).worker("a", 3_000, 3 * GIB);
    assert!(h.running(&kept) && h.running(&lost), "requeued on Hello");
    // `pending` is committed after the registration: its Start goes to the new session.
    let started = h.commit(ControlRecord::Lease(pending.clone()));
    assert!(matches!(started.as_slice(), [Effect::Start(s)] if s.lease == pending.lease));

    // The first heartbeat lists `kept`; it has not received `pending` yet.
    h.at_secs(11).heartbeat_running("a", &[kept.lease]);
    assert!(h.running(&kept), "a re-adopted lease requeued");
    assert_eq!(h.s.state(lost.operation), Some(&OpState::Queued));
    assert!(
        matches!(
            h.s.state(pending.operation),
            Some(OpState::Leased {
                committed: true,
                ..
            })
        ),
        "requeued before the Start grace"
    );
    assert_eq!(h.s.booked(&w("a")), Some(Resources::new(2_000, 2 * GIB)));

    let [again] = h.tick().try_into().unwrap();
    assert_eq!(again.operation, lost.operation);
    h.commit_and_start(&again);
    assert!(h.report(&lost, ok(2)).is_empty(), "late result proposed");
}

/// The record failing `grant`'s operation once its lost leases spent the infra budget.
fn infra_failure(grant: &LeaseGrant) -> ControlRecord {
    ControlRecord::Result(ResultRecord {
        lease: grant.lease,
        operation: grant.operation,
        outcome: Outcome::Failed(Failure::Infra),
    })
}

/// Catches: a lost lease not counted as an `INFRA` attempt, its retry placed on the
/// worker that lost it while one that has not has room, no retry when only workers that
/// lost it have room, and a budget off by one. RFC section 5.8: `INFRA` retries
/// elsewhere, up to three attempts, then the waiters get `INTERNAL`. The scheduler
/// commits an `INFRA` failure from the third lost lease, which answers them.
#[test]
fn lost_leases_are_retried_elsewhere_then_fail_after_three() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.worker("b", 2_000, 2 * GIB);
    h.submit(1, request(1));
    let [first] = h.tick().try_into().unwrap();
    assert_eq!(first.worker, w("a"));
    h.commit_and_start(&first);

    // `a` never lists it: at the Start grace it is lost, attempt 1. `a` has room again,
    // but the retry goes to `b`, which has not lost it.
    let lost = h.at_secs(60).beat("a");
    assert!(lost.is_empty(), "attempt 1 failed it: {lost:?}");
    h.heartbeat("b");
    let [second] = h.tick().try_into().unwrap();
    assert_eq!(second.worker, w("b"), "retried on the worker that lost it");
    h.commit_and_start(&second);

    // `b` loses it too, attempt 2. Only workers that lost it are left: first fit, `a`.
    h.at_secs(120).heartbeat("a");
    let lost = h.beat("b");
    assert!(lost.is_empty(), "attempt 2 failed it: {lost:?}");
    let [third] = h.tick().try_into().unwrap();
    assert_eq!(third.worker, w("a"));
    h.commit_and_start(&third);

    // Attempt 3 ends it: the heartbeat proposes the failure instead of a requeue.
    h.at_secs(180).heartbeat("b");
    let lost = h.beat("a");
    assert_eq!(
        lost,
        [Effect::Commit(infra_failure(&third))],
        "attempt 3 did not fail it"
    );
    let lease = third.lease;
    assert_eq!(
        h.s.state(third.operation),
        Some(&OpState::Failing { lease })
    );
    assert!(h.tick().is_empty(), "granted a fourth time");
    assert_eq!(h.s.queued().count(), 0);
    let late = h.report(&third, ok(1));
    assert!(late.is_empty(), "a lost lease's report proposed");

    let answered = h.commit(infra_failure(&third));
    let [Effect::Answer(answer)] = answered.as_slice() else {
        panic!("committing the failure gave {answered:?}");
    };
    assert_eq!(answer.lease, third.lease);
    assert_eq!(answer.outcome, Outcome::Failed(Failure::Infra));
    assert_eq!(answer.waiters, [WaiterId(1)]);
    assert_eq!(
        h.s.state(third.operation),
        Some(&OpState::Failed {
            lease: third.lease,
            failure: Failure::Infra
        })
    );
    assert_eq!(h.s.booked(&w("a")), Some(Resources::default()));
    assert_eq!(h.s.booked(&w("b")), Some(Resources::default()));
}

/// Catches: a lease lost to silence (no heartbeat for G, as when a machine reboots) not
/// counted as an `INFRA` attempt, and a grant lost before it committed counted as one
/// (no `Start` went out, so nothing ran). The tick that expires the third committed
/// lease proposes the failure.
#[test]
fn leases_lost_to_silence_count_toward_the_budget() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [uncommitted] = h.tick().try_into().unwrap();
    let expired = h.at_secs(60).feed(Event::Tick);
    assert!(expired.is_empty(), "{expired:?}");
    assert!(h.commit(ControlRecord::Lease(uncommitted)).is_empty());

    for attempt in 1..=3 {
        let t = 100 * attempt;
        // `a` reboots: it registers again, and its first heartbeat lists nothing.
        h.at_secs(t).worker("a", 1_000, GIB);
        h.heartbeat("a");
        let [grant] = h.tick().try_into().unwrap();
        h.commit_and_start(&grant);
        let expired = h.at_secs(t + 60).feed(Event::Tick);
        if attempt < 3 {
            assert!(
                expired.is_empty(),
                "attempt {attempt} failed it: {expired:?}"
            );
            assert_eq!(h.s.state(grant.operation), Some(&OpState::Queued));
        } else {
            assert_eq!(
                expired,
                [Effect::Commit(infra_failure(&grant))],
                "attempt 3 did not fail it"
            );
        }
    }
}

/// Catches (issue #23, scheduler side): a lease the scheduler gave up but its worker
/// lists as running (its `Start` arrived after the Start grace) left unbooked, so new
/// work is booked into room that run still uses; that booking kept after the run leaves
/// the running set; and a listed lease the scheduler holds, or never granted, booked.
#[test]
fn a_lease_given_up_but_still_running_keeps_its_room() {
    let mut h = Harness::new();
    h.worker("a", 2_000, 2 * GIB);
    h.submit(1, request(1));
    let [late] = h.tick().try_into().unwrap();
    // The grant commits and its Start goes out, but reaches `a` only after the grace.
    assert_eq!(h.commit(ControlRecord::Lease(late.clone())).len(), 1);
    h.at_secs(60).heartbeat("a");
    let [retry] = h.tick().try_into().unwrap();
    h.commit_and_start(&retry);
    assert_eq!(h.s.booked(&w("a")), Some(Resources::new(1_000, GIB)));

    // The late Start arrives: `a` lists the given-up lease next to the retry, and a
    // lease nobody granted.
    let running = [late.lease, retry.lease, LeaseId::new(1, 99)];
    h.at_secs(61).heartbeat_running("a", &running);
    assert_eq!(
        h.s.booked(&w("a")),
        Some(Resources::new(2_000, 2 * GIB)),
        "the late run is not booked, or a lease is booked twice"
    );
    h.submit(2, request(2));
    assert!(h.tick().is_empty(), "booked into the late run's room");

    // The late run ends and leaves the running set: its room is free again.
    h.at_secs(62).heartbeat_running("a", &[retry.lease]);
    assert_eq!(h.s.booked(&w("a")), Some(Resources::new(1_000, GIB)));
    let [next] = h.tick().try_into().unwrap();
    assert_eq!(next.operation, OperationId(1));
}

/// Catches: a lease expired to silence after its worker reported a result counted as an
/// `INFRA` attempt. Its run finished; on the last attempt that proposes a second result
/// record for the lease, and if the appends are reordered that record reaches the log
/// first and a completed action is answered `INFRA`. The lease is still given up, as a
/// silent worker's is, and its result wins if it commits before a new grant.
#[test]
fn a_reported_lease_lost_to_silence_is_not_an_attempt() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.worker("b", 1_000, GIB);
    h.submit(1, request(1));
    // Attempts 1 and 2: `a`, then `b`, never list their lease.
    let [first] = h.tick().try_into().unwrap();
    h.commit_and_start(&first);
    h.at_secs(60).heartbeat("b");
    h.heartbeat("a");
    let [second] = h.tick().try_into().unwrap();
    assert_eq!(second.worker, w("b"));
    h.commit_and_start(&second);
    h.at_secs(120).heartbeat("a");
    h.heartbeat("b");
    let [third] = h.tick().try_into().unwrap();
    h.commit_and_start(&third);

    // The third lease's worker reports success, then goes silent for G before the
    // result commits.
    let result = h.propose(&third, ok(1));
    h.at_secs(180).heartbeat("a");
    h.heartbeat("b");
    let expired = h.at_secs(240).feed(Event::Tick);
    assert!(expired.is_empty(), "a reported lease counted: {expired:?}");
    assert_eq!(h.s.state(third.operation), Some(&OpState::Queued));

    let answered = h.commit(result);
    let [Effect::Answer(answer)] = answered.as_slice() else {
        panic!("committing the result gave {answered:?}");
    };
    assert_eq!((answer.lease, answer.outcome), (third.lease, ok(1)));
    assert_eq!(h.s.booked(&third.worker), Some(Resources::default()));
}

/// Catches (issue #23): a lease lost to silence left unbooked although its worker may
/// still be running it. When the worker registers again, a tick before its first
/// heartbeat places new work into that room. The room is freed once a heartbeat leaves
/// the lease out.
#[test]
fn a_lease_lost_to_silence_keeps_its_room_until_a_heartbeat_leaves_it_out() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [lost] = h.tick().try_into().unwrap();
    h.commit_and_start(&lost);

    assert!(h.at_secs(60).tick().is_empty());
    assert_eq!(h.s.state(lost.operation), Some(&OpState::Queued));
    assert_eq!(
        h.s.booked(&w("a")),
        Some(Resources::new(1_000, GIB)),
        "a lease lost to silence is not booked"
    );

    // `a` comes back (a partition healed, the run went on) and registers again.
    h.at_secs(70).worker("a", 1_000, GIB);
    assert!(
        h.tick().is_empty(),
        "placed into the room of a run that may go on"
    );
    h.heartbeat_running("a", &[lost.lease]);
    assert_eq!(h.s.booked(&w("a")), Some(Resources::new(1_000, GIB)));
    assert!(h.tick().is_empty(), "placed into the late run's room");

    // The run ends and leaves the running set.
    h.at_secs(71).heartbeat("a");
    assert_eq!(h.s.booked(&w("a")), Some(Resources::default()));
    let [retry] = h.tick().try_into().unwrap();
    assert_eq!(retry.operation, lost.operation);
}
