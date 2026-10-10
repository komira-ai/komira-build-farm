//! The scheduler's rules, one scenario each, driven through its public inputs.

use kbf_caps::NodeCaps;
use kbf_sched::fence::HANDOVER_GRACE;
use std::time::Duration;

use kbf_sched::{DaemonInstance, Event, FINISHED_RETENTION, Input, OpState, Request, Scheduler};
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
        kind: kbf_types::LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

/// A Linux x86-64 node, as its report describes it.
fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
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
        Self::with(Scheduler::new(1))
    }

    fn with(s: Scheduler) -> Self {
        Self {
            s,
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

    /// Registers `name` (again), by one daemon process named after it.
    fn worker(&mut self, name: &str, cpu_millis: u64, memory: u64) {
        self.worker_by(name, name, cpu_millis, memory);
    }

    /// Registers `name` (again), by the daemon process `instance`.
    fn worker_by(&mut self, name: &str, instance: &str, cpu_millis: u64, memory: u64) {
        let capacity = Resources::new(cpu_millis, memory);
        let worker = w(name);
        assert!(
            self.feed(Event::WorkerUp {
                worker,
                instance: DaemonInstance::new(instance),
                capacity,
                caps: caps(),
            })
            .is_empty()
        );
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

    /// Ticks and returns the grants proposed. Work queued while no worker is live waits
    /// with a reason; that is checked in `platform.rs`, and skipped here.
    fn tick(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|e| match e {
                Effect::Commit(ControlRecord::Lease(g)) => Some(g),
                Effect::Waiting(_) => None,
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

/// Catches: a daemon that registers again on a new stream keeping, until a grace or G, a
/// committed lease its heartbeats leave out (its `Start` went to the old session, so its
/// run is gone or never began); a registration that requeues by itself, though `Hello`
/// carries no running set (the leases the daemon still runs would run twice); one that
/// drops a lease the daemon lists; and one that gives up, before the Start grace, a
/// grant whose `Start` went to the new session (it may still be on its way).
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

    // The daemon opens a new stream 10 s later; it still runs `kept`. Its Hello lists
    // nothing.
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

/// Catches (issue #140): a lease whose `Start` went to a replaced daemon process requeued
/// on the first heartbeat of another process registered as the same node (the replaced
/// one, when two daemons hold one certificate, runs it until it fences: twice at once),
/// or kept past the handover grace; the grace counted from the new registration instead
/// of from when the node was last heard before it, or restarted by a reconnect of the
/// same process; a process that comes back after another taken for the one its earlier
/// leases went to; and registrations without an instance id taken for one process.
#[test]
fn a_lease_of_a_replaced_daemon_process_is_kept_until_that_process_has_fenced() {
    let mut h = Harness::new();
    h.worker_by("a", "one", 4_000, 4 * GIB);
    for n in 1..=4 {
        let networked = Request {
            hermetic: false,
            ..request(n)
        };
        h.submit(u64::from(n), networked);
    }
    let [first, second, third, fourth] = h.tick().try_into().unwrap();
    h.commit_and_start(&first);

    // Process `one` is last heard at 20 s; process `two` registers as `a` at 30 s and
    // lists nothing.
    h.at_secs(20).heartbeat_running("a", &[first.lease]);
    h.at_secs(30).worker_by("a", "two", 4_000, 4 * GIB);
    h.at_secs(31).heartbeat("a");
    assert!(h.running(&first), "requeued on another process's heartbeat");
    // `second` goes to process `two`, which then reconnects without it.
    h.commit_and_start(&second);
    let handover = FarmTime::from_millis(20_000).saturating_add(HANDOVER_GRACE);
    let just_before = handover.as_millis() - 1;
    h.at_millis(just_before).heartbeat("a");
    assert!(
        h.running(&first),
        "requeued before the replaced process fenced"
    );
    h.worker_by("a", "two", 4_000, 4 * GIB);
    h.heartbeat("a");
    assert_eq!(
        h.s.state(second.operation),
        Some(&OpState::Queued),
        "a lease the same process lost kept"
    );
    assert!(h.running(&first), "a reconnect ended the handover grace");
    h.at_millis(handover.as_millis()).heartbeat("a");
    assert_eq!(h.s.state(first.operation), Some(&OpState::Queued));

    // Process `one` comes back. `third`, started on process `two`, is kept although
    // `one` registered before `two` did.
    h.commit_and_start(&third);
    h.at_secs(80).worker_by("a", "one", 4_000, 4 * GIB);
    h.at_secs(81).heartbeat("a");
    assert!(
        h.running(&third),
        "process two's lease requeued by process one"
    );

    // Without an instance id, each registration is another process's.
    h.at_secs(140).worker_by("a", "", 4_000, 4 * GIB);
    h.commit_and_start(&fourth);
    h.at_secs(141)
        .heartbeat_running("a", &[third.lease, fourth.lease]);
    h.at_secs(142).worker_by("a", "", 4_000, 4 * GIB);
    h.at_secs(143).heartbeat_running("a", &[third.lease]);
    assert!(h.running(&fourth), "two unnamed processes taken for one");
}

/// Catches (issue #25): a resent `Hello` treated as a registration, which would count
/// every `Start` already sent as sent to an earlier session and requeue it on the next
/// heartbeat that has not listed it yet (an unfenced second run for self-fenced work);
/// a capacity change that placement does not see; a resend that does not count as
/// hearing from the worker; and a capacity change that registers an unknown worker.
#[test]
fn a_capacity_change_resizes_the_worker_and_opens_no_session() {
    let mut h = Harness::new();
    h.worker("a", 1_000, 4 * GIB);
    h.submit(1, request(1));
    h.submit(2, request(2));
    let [first] = h.tick().try_into().unwrap();
    h.commit_and_start(&first);
    assert!(h.tick().is_empty(), "placed beyond the capacity");

    // The node report changes 50 s in: twice the CPUs.
    let capacity = Resources::new(2_000, 4 * GIB);
    let resend = Event::Capacity {
        worker: w("a"),
        capacity,
        caps: caps(),
    };
    assert!(h.at_secs(50).feed(resend).is_empty());
    // A heartbeat that has not listed `first` yet keeps it: same session, inside G.
    h.at_secs(51).heartbeat("a");
    assert!(h.running(&first), "a capacity change opened a session");
    // Heard at 51 s at the latest: alive at 100 s, and roomier.
    let [second] = h.at_secs(100).tick().try_into().unwrap();
    assert_eq!(second.worker, w("a"));
    assert_eq!(h.s.booked(&w("a")), Some(Resources::new(2_000, 2 * GIB)));

    let unknown = Event::Capacity {
        worker: w("z"),
        capacity,
        caps: caps(),
    };
    assert!(h.feed(unknown).is_empty());
    assert_eq!(
        h.s.booked(&w("z")),
        None,
        "a capacity change registered a worker"
    );
}

/// Catches: a capacity change that does not count as hearing from the worker, so a
/// worker whose daemon resends `Hello` but whose heartbeats are delayed is taken for
/// silent and gets no work.
#[test]
fn a_capacity_change_counts_as_hearing_from_the_worker() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    let resend = Event::Capacity {
        worker: w("a"),
        capacity: Resources::new(1_000, GIB),
        caps: caps(),
    };
    assert!(h.at_secs(50).feed(resend).is_empty());
    h.submit(1, request(1));
    let [grant] = h.at_secs(100).tick().try_into().unwrap();
    assert_eq!(grant.worker, w("a"), "a resend did not count as hearing");
}

/// Catches (issue #23): a lease the worker lists that the scheduler gave up, granted
/// to another worker, or finished, not named for cancelling (its run goes on beside
/// the retry); a lease the worker holds named (its only run would be killed), even
/// while its retry's grant is not committed yet; and a lease this scheduler never
/// granted named, whether a later sequence number of its term or another term's (a
/// newer leader's grant is not this scheduler's to cancel).
#[test]
fn not_held_names_the_listed_leases_given_up_on_the_worker() {
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    h.worker("b", 1_000, GIB);
    h.submit(1, request(1));
    h.submit(2, request(2));
    let [on_a, on_b] = h.tick().try_into().unwrap();
    assert_eq!((&on_a.worker, &on_b.worker), (&w("a"), &w("b")));
    h.at_secs(1).commit_and_start(&on_a);
    h.commit_and_start(&on_b);
    let not_held = |h: &Harness, worker: &str, running: &[LeaseId]| -> Vec<LeaseId> {
        h.s.not_held(&w(worker), running).collect()
    };
    let never = [LeaseId::new(1, 99), LeaseId::new(2, 0), LeaseId::new(0, 0)];
    let mut listed = vec![on_a.lease, on_b.lease];
    listed.extend(never);
    assert_eq!(not_held(&h, "a", &listed), [on_b.lease]);
    assert_eq!(not_held(&h, "b", &listed), [on_a.lease]);

    // `on_a` is given up: its Start has been out for the grace and `a` leaves it out.
    h.at_secs(61).heartbeat("a");
    h.heartbeat_running("b", &[on_b.lease]);
    assert_eq!(not_held(&h, "a", &[on_a.lease]), [on_a.lease]);
    let [retry] = h.tick().try_into().unwrap();
    assert_eq!(retry.worker, w("a"));
    assert_eq!(not_held(&h, "a", &[on_a.lease, retry.lease]), [on_a.lease]);
    h.commit_and_start(&retry);
    assert_eq!(not_held(&h, "a", &[on_a.lease, retry.lease]), [on_a.lease]);

    // A finished operation's lease is held nowhere.
    h.finish(&on_b, ok(2));
    assert_eq!(not_held(&h, "b", &[on_b.lease]), [on_b.lease]);
}

/// Runs one operation on `a` from submit to answer at the harness's time, with a
/// heartbeat first so `a` stays live. Returns its grant.
fn run_one(h: &mut Harness, waiter: u64, n: u8) -> LeaseGrant {
    h.heartbeat("a");
    h.submit(waiter, request(n));
    let [grant] = h.tick().try_into().unwrap();
    h.commit_and_start(&grant);
    h.finish(&grant, ok(n));
    grant
}

/// Catches (issue #165): finished operations kept forever, so the scheduler's memory
/// grows with every operation it ever ran; one dropped before the retention is up, so
/// a WaitExecution right after a broken stream finds nothing; and a drop that takes
/// only one expired operation per input, which falls behind a burst.
///
/// One operation finishes each second for 100 s. The operations held are exactly
/// those finished in the last retention (60 s), never more; one input at the end of
/// the retention drops all the rest at once.
#[test]
fn a_finished_operation_is_dropped_once_the_retention_is_up() {
    let retention = FINISHED_RETENTION.as_secs();
    let mut h = Harness::new();
    h.worker("a", 1_000, GIB);
    let mut grants = Vec::new();
    for i in 0..100u64 {
        h.at_secs(i);
        grants.push(run_one(&mut h, i, u8::try_from(i).unwrap()));
        let kept = (i + 1).min(retention);
        assert_eq!(u64::try_from(h.s.operations()), Ok(kept), "at {i} s");
        // The newest finished operations are still there, the older ones gone.
        let oldest_kept = i + 1 - kept;
        for (j, grant) in (0u64..).zip(&grants) {
            let held = h.s.state(grant.operation).is_some();
            assert_eq!(held, j >= oldest_kept, "operation {j} at {i} s");
        }
    }
    let last = grants.last().unwrap();

    // Within the retention the last one is still there, finished, with its waiter.
    h.at_millis((99 + retention) * 1_000 - 1).heartbeat("a");
    assert!(h.s.state(last.operation).is_some_and(OpState::is_done));
    assert_eq!(h.s.waiters(last.operation), Some(&[WaiterId(99)][..]));

    // One input at the end of the retention drops every one left.
    h.at_secs(99 + retention).heartbeat("a");
    assert_eq!(h.s.operations(), 0);
    assert_eq!(h.s.state(last.operation), None);
    assert_eq!(h.s.waiters(last.operation), None);
}

/// Catches: a dropped operation that a late input brings back or acts on. A stale
/// result or grant commit, and a late report or start for it, are dropped as for an
/// unknown operation; its lease, still listed by the worker, is named not held (so
/// the worker is told to cancel it); and the same action again is a new operation.
#[test]
fn late_inputs_for_a_dropped_operation_are_dropped() {
    let retention = Duration::from_secs(5);
    let mut h = Harness::with(Scheduler::new(1).with_finished_retention(retention));
    h.worker("a", 1_000, GIB);
    let grant = run_one(&mut h, 1, 1);
    let record = ControlRecord::Result(ResultRecord {
        lease: grant.lease,
        operation: grant.operation,
        outcome: ok(1),
    });
    h.at_secs(5).heartbeat("a");
    assert_eq!(h.s.state(grant.operation), None);

    assert!(h.commit(record).is_empty(), "a dropped operation answered");
    assert!(h.commit(ControlRecord::Lease(grant.clone())).is_empty());
    assert!(h.report(&grant, ok(1)).is_empty());
    let started = Event::Started {
        operation: grant.operation,
        lease: grant.lease,
    };
    assert!(h.feed(started).is_empty());
    assert_eq!(h.s.state(grant.operation), None, "brought back");
    let listed = [grant.lease];
    let not_held: Vec<LeaseId> = h.s.not_held(&w("a"), &listed).collect();
    assert_eq!(not_held, listed);

    h.submit(2, request(1));
    let [again] = h.tick().try_into().unwrap();
    assert_ne!(again.operation, grant.operation);
    assert_eq!(h.s.operations(), 1);
}

/// Catches: a retention other than the configured one, and a refused operation that
/// is never dropped (only answered ones are). A zero retention drops an answered
/// operation in the input that answers it; the answer still names its waiters.
#[test]
fn the_retention_is_configurable_and_holds_for_refusals_too() {
    let mut h = Harness::with(Scheduler::new(1).with_finished_retention(Duration::ZERO));
    h.worker("a", 1_000, GIB);
    h.submit(1, request(1));
    let [grant] = h.tick().try_into().unwrap();
    h.commit_and_start(&grant);
    let answer = h.finish(&grant, ok(1));
    assert_eq!(answer.waiters, [WaiterId(1)]);
    assert_eq!(h.s.operations(), 0, "kept with a zero retention");

    let s = Scheduler::new(1)
        .with_unservable_wait(Duration::from_secs(1))
        .with_finished_retention(Duration::from_secs(10));
    let mut h = Harness::with(s);
    h.worker("a", 1_000, GIB);
    let mut big = request(1);
    big.resources = Resources::new(2_000, GIB);
    h.submit(1, big);
    h.feed(Event::Tick);
    let record = h
        .at_secs(1)
        .feed(Event::Tick)
        .into_iter()
        .find_map(|e| match e {
            Effect::Commit(record @ ControlRecord::Refusal(_)) => Some(record),
            _ => None,
        })
        .expect("a refusal proposed");
    h.commit(record);
    let id = OperationId(0);
    assert!(matches!(h.s.state(id), Some(OpState::Refused { .. })));
    h.at_millis(10_999).heartbeat("a");
    assert!(h.s.state(id).is_some(), "dropped within the retention");
    h.at_secs(11).heartbeat("a");
    assert_eq!(h.s.state(id), None, "a refused operation kept");
    assert_eq!(h.s.operations(), 0);
}
