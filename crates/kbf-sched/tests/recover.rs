//! A scheduler that takes up the operations a state store holds
//! ([`Scheduler::recover`]): the queue is placed again under the store's term with ids
//! that continue, a held lease waits for its worker to register again, and the worker's
//! first heartbeat decides it.

use kbf_caps::NodeCaps;
use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{
    DaemonInstance, Event, Fate, Input, OpState, Recovered, RecoveredLease, Request, Requeue,
    RequeueReason, Scheduler,
};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseId, LeaseKind,
    MemoryKill, MemoryRun, OperationId, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
/// The term the recovering scheduler grants under.
const TERM: u64 = 7;

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn at(secs: u64) -> FarmTime {
    FarmTime::from_millis(secs * 1_000)
}

fn feed(s: &mut Scheduler, secs: u64, event: Event) -> Vec<Effect> {
    s.apply(Input::new(at(secs), event))
}

fn node(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn up(s: &mut Scheduler, secs: u64, name: &str) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let event = Event::WorkerUp {
        worker: node(name),
        instance: DaemonInstance::new("daemon"),
        predecessor_stopped: false,
        capacity: Resources::new(4_000, 8 * GIB),
        caps,
    };
    assert!(feed(s, secs, event).is_empty());
}

fn heartbeat(s: &mut Scheduler, secs: u64, name: &str, running: Vec<LeaseId>) {
    let event = Event::Heartbeat {
        worker: node(name),
        running,
    };
    assert!(feed(s, secs, event).is_empty());
}

fn queued(operation: u64, request: Request) -> Recovered {
    Recovered {
        operation: OperationId(operation),
        request,
        waiter: WaiterId(operation),
        lease: None,
        losses: 0,
        fate: None,
    }
}

fn leased(operation: u64, request: Request, seq: u64, worker: &str) -> Recovered {
    Recovered {
        lease: Some(RecoveredLease {
            lease: LeaseId::new(TERM, seq),
            worker: node(worker),
        }),
        ..queued(operation, request)
    }
}

/// A scheduler recovered over `operations`, granting leases from seq 20 and operation
/// ids from 10.
fn recovered(operations: Vec<Recovered>) -> Scheduler {
    let mut s = Scheduler::new(1).recording_requeues();
    s.recover(TERM, OperationId(10), 20, operations);
    s
}

fn committed_lease(state: Option<&OpState>) -> Option<LeaseId> {
    match state {
        Some(OpState::Leased {
            lease,
            committed: true,
            ..
        }) => Some(*lease),
        _ => None,
    }
}

/// Catches: a recovered queue that is not placed, a grant under the old scheduler's
/// term (a lease id two terms could both name) or from sequence 0 (a stale daemon's
/// result of lease 0 accepted as the new lease 0), and operation ids that restart at 0
/// or collide with a recovered one.
#[test]
fn a_recovered_queue_is_placed_under_the_new_term_and_ids_continue() {
    let mut s = recovered(vec![queued(3, request(3))]);
    assert_eq!(s.state(OperationId(3)), Some(&OpState::Queued));
    assert_eq!(s.queued_len(), 1);
    assert_eq!(s.next_operation(), OperationId(10));
    up(&mut s, 0, "node-a");
    let effects = feed(&mut s, 0, Event::Tick);
    let [Effect::Commit(ControlRecord::Lease(grant))] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    assert_eq!(grant.lease, LeaseId::new(TERM, 20));
    assert_eq!(grant.operation, OperationId(3));
    let submit = Event::Submit {
        waiter: WaiterId(99),
        request: request(9),
    };
    assert!(feed(&mut s, 0, submit).is_empty());
    assert_eq!(s.state(OperationId(10)), Some(&OpState::Queued));
    assert_eq!(s.next_operation(), OperationId(11));
}

/// Catches: a recovered queue placed in another order than the one it was in: the
/// urgency of each operation is part of what a restart keeps (ignoring it places the
/// interactive caller behind the batch of the CI jobs that arrived first), and
/// operations of one level keep their order past a single digit of id (ids 9 and 10).
#[test]
fn a_recovered_queue_keeps_its_urgency_and_arrival_order() {
    let mut recovered_ops: Vec<Recovered> = (0..10u8)
        .map(|n| queued(u64::from(n), request(n)))
        .collect();
    let urgent = Request {
        qos: Qos::Interactive,
        ..request(10)
    };
    recovered_ops.push(queued(10, urgent));
    let mut s = recovered(recovered_ops);
    let order: Vec<u64> = s.queued().map(|id| id.0).collect();
    assert_eq!(order, [10, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    up(&mut s, 0, "node-a");
    let effects = feed(&mut s, 0, Event::Tick);
    let granted: Vec<(u64, u64)> = effects
        .iter()
        .filter_map(|e| match e {
            Effect::Commit(ControlRecord::Lease(g)) => Some((g.operation.0, g.lease.seq)),
            _ => None,
        })
        .collect();
    assert_eq!(granted, [(10, 20), (0, 21), (1, 22), (2, 23)]);
}

/// Catches: a recovered operation that a client asking for the same action again does
/// not join (the work runs twice), and one that a networked action's twin joins (those
/// runs are not interchangeable, as for any submission).
#[test]
fn a_recovered_operation_is_joined_only_where_a_submission_would_join() {
    let networked = Request {
        hermetic: false,
        ..request(4)
    };
    let mut s = recovered(vec![queued(3, request(3)), queued(4, networked.clone())]);
    let again = |waiter, request| Event::Submit {
        waiter: WaiterId(waiter),
        request,
    };
    assert!(feed(&mut s, 0, again(50, request(3))).is_empty());
    assert_eq!(
        s.waiters(OperationId(3)),
        Some(&[WaiterId(3), WaiterId(50)][..])
    );
    assert_eq!(s.operations(), 2);
    assert!(feed(&mut s, 0, again(51, networked)).is_empty());
    assert_eq!(s.operations(), 3);
}

/// Catches: a recovered lease given up as a silent worker's before its worker has had
/// the grace to register (every restart would requeue, and charge, all running work).
#[test]
fn a_recovered_lease_is_kept_until_its_worker_returns() {
    let mut s = recovered(vec![leased(3, request(3), 5, "node-a")]);
    let lease = LeaseId::new(TERM, 5);
    assert_eq!(committed_lease(s.state(OperationId(3))), Some(lease));
    assert_eq!(s.leases_held(), 1);
    let late = LEASE_GRACE.as_secs() - 1;
    assert!(feed(&mut s, late, Event::Tick).is_empty());
    assert_eq!(committed_lease(s.state(OperationId(3))), Some(lease));
    assert!(s.take_requeues().is_empty());
}

/// Catches: a worker that never returns holding its leases for ever (the operation never
/// runs again), and the give-up not charged as the silent run it may have been.
#[test]
fn a_worker_that_does_not_return_in_the_grace_loses_the_lease() {
    let mut s = recovered(vec![leased(3, request(3), 5, "node-a")]);
    feed(&mut s, LEASE_GRACE.as_secs() + 1, Event::Tick);
    assert_eq!(s.state(OperationId(3)), Some(&OpState::Queued));
    let given_up = s.take_requeues();
    assert_eq!(given_up.len(), 1);
    assert_eq!(given_up[0].reason, RequeueReason::Silent);
    assert_eq!(s.farm_runs(OperationId(3)).map(<[_]>::len), Some(1));
}

/// Catches: a run the daemon still holds that is requeued (it would run twice), or not
/// booked on the returning worker (its room offered again while it runs).
#[test]
fn a_returning_worker_that_lists_the_lease_keeps_the_run() {
    let mut s = recovered(vec![leased(3, request(3), 5, "node-a")]);
    let lease = LeaseId::new(TERM, 5);
    up(&mut s, 5, "node-a");
    heartbeat(&mut s, 6, "node-a", vec![lease]);
    assert!(feed(&mut s, 7, Event::Tick).is_empty());
    assert_eq!(committed_lease(s.state(OperationId(3))), Some(lease));
    assert_eq!(s.booked(&node("node-a")), Some(Resources::new(1_000, GIB)));
    assert_eq!(s.leases_on(&node("node-a")), vec![lease]);
    assert!(s.take_requeues().is_empty());
}

/// Catches: a grant committed whose `Start` never reached the daemon kept for ever or
/// only after the start grace, and the loss charged to the operation's farm budget
/// (the run never happened).
#[test]
fn a_returning_worker_that_leaves_the_lease_out_gets_it_requeued_at_once() {
    let mut s = recovered(vec![leased(3, request(3), 5, "node-a")]);
    let lease = LeaseId::new(TERM, 5);
    up(&mut s, 5, "node-a");
    heartbeat(&mut s, 6, "node-a", Vec::new());
    assert_eq!(s.state(OperationId(3)), Some(&OpState::Queued));
    assert_eq!(
        s.take_requeues(),
        vec![Requeue {
            operation: OperationId(3),
            lease,
            worker: node("node-a"),
            reason: RequeueReason::Reconnected,
        }]
    );
    assert_eq!(s.farm_runs(OperationId(3)).map(<[_]>::len), Some(0));
    assert_eq!(s.booked(&node("node-a")), Some(Resources::default()));
}

/// Catches: leases booked on a worker that did not register (its bookings would be
/// counted on the wrong node), and one worker's return ending another's grace.
#[test]
fn only_the_returning_workers_leases_are_booked() {
    let mut s = recovered(vec![
        leased(3, request(3), 5, "node-a"),
        leased(4, request(4), 6, "node-b"),
    ]);
    up(&mut s, 5, "node-a");
    assert_eq!(s.booked(&node("node-a")), Some(Resources::new(1_000, GIB)));
    assert_eq!(s.booked(&node("node-b")), None);
    assert_eq!(s.leases_on(&node("node-a")), vec![LeaseId::new(TERM, 5)]);
    assert!(feed(&mut s, 30, Event::Tick).is_empty());
    assert!(committed_lease(s.state(OperationId(4))).is_some());
    feed(&mut s, LEASE_GRACE.as_secs() + 1, Event::Tick);
    // The second lease was given up (node-b never came back), and its operation is placed
    // again on node-a in the same round; the first is still node-a's.
    let given_up = s.take_requeues();
    assert_eq!(given_up.len(), 1);
    assert_eq!(given_up[0].operation, OperationId(4));
    assert_eq!(given_up[0].worker, node("node-b"));
    assert_eq!(
        committed_lease(s.state(OperationId(3))),
        Some(LeaseId::new(TERM, 5))
    );
}

/// Catches: a recovered whole-machine lease, or an action at the whole host's memory,
/// booked as a share of its node (another lease placed beside it).
#[test]
fn an_exclusive_recovered_lease_keeps_its_node_to_itself() {
    let whole = Request {
        kind: LeaseKind::WholeMachine,
        ..request(3)
    };
    let host = Request {
        resources: Resources::new(1_000, 8 * GIB),
        ..request(3)
    };
    for (held, booked) in [
        (whole, Resources::new(4_000, 8 * GIB)),
        (host, Resources::new(1_000, 8 * GIB)),
    ] {
        let mut s = recovered(vec![leased(3, held, 5, "node-a")]);
        up(&mut s, 5, "node-a");
        assert_eq!(s.booked(&node("node-a")), Some(booked));
        let submit = Event::Submit {
            waiter: WaiterId(99),
            request: request(9),
        };
        assert!(feed(&mut s, 5, submit).is_empty());
        let effects = feed(&mut s, 5, Event::Tick);
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Commit(_))),
            "{effects:?}"
        );
    }
}

/// Catches: a fate a scheduler ignores (the operation books its submitted memory again
/// and the ladder starts over), one that lowers a larger booking, and memory runs that are
/// not held again or not given back by `fate()` for the next write.
#[test]
fn a_fate_restores_the_booking_and_the_memory_runs() {
    let fate = Fate {
        booked_bytes: 16 * GIB,
        memory_runs: vec![MemoryRun {
            worker: node("node-a"),
            booked: GIB,
            kill: MemoryKill::NodePressure,
        }],
    };
    let mut small = queued(3, request(3));
    small.fate = Some(fate.clone());
    let mut big = queued(
        4,
        Request {
            resources: Resources::new(1_000, 32 * GIB),
            ..request(4)
        },
    );
    big.fate = Some(fate.clone());
    let s = recovered(vec![small, big, queued(5, request(5))]);
    assert_eq!(s.fate(OperationId(3)), Some(fate.clone()));
    assert_eq!(
        s.fate(OperationId(4)).map(|f| f.booked_bytes),
        Some(32 * GIB),
        "a booking fell"
    );
    assert_eq!(s.fate(OperationId(5)), None);
    assert_eq!(
        s.memory_runs(OperationId(3)),
        Some(fate.memory_runs.as_slice())
    );
}

/// Catches: `fate()` answering for an operation the scheduler does not hold, and a fate
/// with no runs that books memory it should not or that leaves a phantom entry of runs
/// behind (a raised booking is kept; no kills are held).
#[test]
fn a_fate_without_runs_raises_the_booking_and_holds_no_kills() {
    let mut raised = queued(6, request(6));
    raised.fate = Some(Fate {
        booked_bytes: 8 * GIB,
        memory_runs: Vec::new(),
    });
    let s = recovered(vec![raised]);
    assert_eq!(s.fate(OperationId(99)), None, "an operation not held");
    assert_eq!(s.fate(OperationId(6)), None, "no kills to hand on");
    assert!(s.memory_state().runs_by_operation().is_empty());
}
