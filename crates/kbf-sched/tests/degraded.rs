//! A scheduler whose state store goes away and comes back, and one that counts the
//! leases an operation loses.
//!
//! The store's outage is told to the scheduler by [`Scheduler::store_unavailable`] and
//! [`Scheduler::store_available`]; the loss cap is [`Scheduler::with_loss_requeues`].

use kbf_caps::NodeCaps;
use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{
    DaemonInstance, Event, Input, LOSS_REQUEUES, OpState, Recovered, RecoveredLease, Request,
    RequeueReason, Scheduler,
};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseId, LeaseKind,
    OperationId, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const TERM: u64 = 7;
const NODE: &str = "node-a";

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

fn up(s: &mut Scheduler, secs: u64) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let event = Event::WorkerUp {
        worker: WorkerId::new(NODE),
        instance: DaemonInstance::new("daemon"),
        predecessor_stopped: false,
        capacity: Resources::new(4_000, 8 * GIB),
        caps,
    };
    assert!(feed(s, secs, event).is_empty());
}

fn heartbeat(s: &mut Scheduler, secs: u64, running: Vec<LeaseId>) -> Vec<Effect> {
    let event = Event::Heartbeat {
        worker: WorkerId::new(NODE),
        running,
    };
    feed(s, secs, event)
}

fn submit(s: &mut Scheduler, secs: u64, tag: u8) {
    let event = Event::Submit {
        waiter: WaiterId(u64::from(tag)),
        request: request(tag),
    };
    assert!(feed(s, secs, event).is_empty());
}

/// Places the one queued operation at `secs` and commits the grant: its lease, with
/// its `Start` sent.
fn start(s: &mut Scheduler, secs: u64) -> LeaseId {
    let effects = feed(s, secs, Event::Tick);
    let [Effect::Commit(record @ ControlRecord::Lease(grant))] = effects.as_slice() else {
        panic!("one grant expected: {effects:?}");
    };
    let lease = grant.lease;
    let sent = feed(s, secs, Event::Committed(record.clone()));
    assert!(matches!(sent.as_slice(), [Effect::Start(_)]), "{sent:?}");
    lease
}

/// A scheduler whose worker registered at 0 and holds the lease of operation 0, with its
/// `Start` sent, then falls silent.
fn silent_holder() -> (Scheduler, LeaseId) {
    let mut s = Scheduler::new(TERM).recording_requeues();
    up(&mut s, 0);
    submit(&mut s, 0, 1);
    let lease = start(&mut s, 0);
    (s, lease)
}

/// Catches: an outage that makes every lease look expired the moment the store returns
/// (the farm could not hear its workers meanwhile, so it requeues, and charges, all
/// running work), and one that never lets a silent worker's lease go.
#[test]
fn a_lease_unrenewed_after_the_grace_is_requeued_not_before() {
    let (mut s, lease) = silent_holder();
    let long_after = 1_000;
    s.store_unavailable();
    assert!(feed(&mut s, long_after, Event::Tick).is_empty());
    assert!(
        s.take_requeues().is_empty(),
        "given up while the store is down"
    );

    s.store_available(at(long_after));
    assert!(feed(&mut s, long_after, Event::Tick).is_empty());
    let almost = long_after + LEASE_GRACE.as_secs() - 1;
    assert!(feed(&mut s, almost, Event::Tick).is_empty());
    assert!(s.take_requeues().is_empty(), "given up inside the grace");
    assert!(matches!(
        s.state(OperationId(0)),
        Some(OpState::Leased {
            committed: true,
            ..
        })
    ));

    feed(&mut s, long_after + LEASE_GRACE.as_secs() + 1, Event::Tick);
    let given_up = s.take_requeues();
    assert_eq!(given_up.len(), 1);
    assert_eq!(given_up[0].lease, lease);
    assert_eq!(given_up[0].reason, RequeueReason::Silent);
}

/// Catches: a grace that outlives a returning worker (its lease is kept for ever, or
/// given up although the worker is heartbeating again): the worker's heartbeat inside the
/// grace is what keeps its lease past the grace.
#[test]
fn a_worker_that_returns_in_the_grace_keeps_its_lease() {
    let (mut s, lease) = silent_holder();
    let back = 1_000;
    s.store_unavailable();
    s.store_available(at(back));
    assert!(heartbeat(&mut s, back + 5, vec![lease]).is_empty());
    let after_grace = back + LEASE_GRACE.as_secs() + 1;
    assert!(feed(&mut s, after_grace, Event::Tick).is_empty());
    assert!(s.take_requeues().is_empty());
    assert!(heartbeat(&mut s, after_grace + 1, vec![lease]).is_empty());
}

/// Catches: a grace that holds back what a heartbeat says. The worker's running set is
/// heard now, whatever the outage kept the scheduler from hearing before, so a lease it
/// leaves out (here one whose `Start` went to an earlier session) is given up at once.
#[test]
fn a_heartbeat_inside_the_grace_still_reconciles() {
    let (mut s, lease) = silent_holder();
    s.store_unavailable();
    s.store_available(at(100));
    // A new session of the daemon process: the lease's `Start` went to the one before.
    up(&mut s, 101);
    assert!(heartbeat(&mut s, 102, Vec::new()).is_empty());
    let given_up = s.take_requeues();
    assert_eq!(given_up.len(), 1);
    assert_eq!(given_up[0].lease, lease);
    assert_eq!(given_up[0].reason, RequeueReason::Reconnected);
}

/// Catches: placement while the store is down (a grant that cannot be written is
/// refused and its `Start` dropped for each round), and a placement that does not resume
/// when the store returns.
#[test]
fn nothing_is_placed_while_the_store_is_down() {
    let mut s = Scheduler::new(TERM).recording_requeues();
    up(&mut s, 0);
    submit(&mut s, 0, 1);
    s.store_unavailable();
    assert!(feed(&mut s, 1, Event::Tick).is_empty());
    assert_eq!(s.queued_len(), 1);
    s.store_available(at(2));
    let effects = feed(&mut s, 2, Event::Tick);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Commit(ControlRecord::Lease(_))]
        ),
        "{effects:?}"
    );
}

/// Loses `lease` as a daemon that reconnected without it does: a new session of the
/// daemon process, and a heartbeat that does not list it.
fn lose(s: &mut Scheduler, lease: LeaseId, secs: u64) {
    up(s, secs);
    assert!(heartbeat(s, secs + 1, Vec::new()).is_empty());
    let given_up = s.take_requeues();
    assert_eq!(given_up.len(), 1, "{given_up:?}");
    assert_eq!(given_up[0].lease, lease);
    assert_eq!(given_up[0].reason, RequeueReason::Reconnected);
}

fn capped() -> Scheduler {
    Scheduler::new(TERM)
        .recording_requeues()
        .with_loss_requeues(LOSS_REQUEUES)
}

/// Catches: an action whose lease is lost again and again that loops for ever instead of
/// failing with a reason (a poison action keeps killing its daemon); a cap that spends
/// the farm's rerun budget instead of its own (they are separate: a lease lost to a
/// reconnect charges no farm run); a count that is off by one (the third loss refuses,
/// the second does not); and a refusal that names no cause or leaves its caller
/// unanswered.
#[test]
fn an_operation_whose_lease_is_lost_three_times_is_refused_with_a_reason() {
    assert_eq!(LOSS_REQUEUES, 3);
    let mut s = capped();
    up(&mut s, 0);
    submit(&mut s, 0, 1);
    let mut secs = 1;
    for lost in 1..=LOSS_REQUEUES {
        let lease = start(&mut s, secs);
        lose(&mut s, lease, secs + 1);
        assert_eq!(s.losses(OperationId(0)), Some(lost));
        if lost < LOSS_REQUEUES {
            assert_eq!(s.state(OperationId(0)), Some(&OpState::Queued));
        }
        secs += 5;
    }
    let effects = feed(&mut s, secs, Event::Tick);
    let [Effect::Commit(record @ ControlRecord::Refusal(refusal))] = effects.as_slice() else {
        panic!("the third loss refuses: {effects:?}");
    };
    assert_eq!(refusal.operation, OperationId(0));
    assert!(refusal.reason.contains("lost"), "{}", refusal.reason);
    assert!(refusal.reason.contains('3'), "{}", refusal.reason);
    let done = feed(&mut s, secs, Event::Committed(record.clone()));
    assert!(
        matches!(done.as_slice(), [Effect::Refuse(r)] if r.waiters == [WaiterId(1)]),
        "{done:?}"
    );
    assert!(matches!(
        s.state(OperationId(0)),
        Some(OpState::Refused { .. })
    ));
    assert_eq!(s.farm_runs(OperationId(0)).map(<[_]>::len), Some(0));
    assert_eq!(s.queued_len(), 0);
}

/// Pins (it passes without the cap): a scheduler not given a cap requeues for ever, as
/// the simulations and every earlier caller expect.
#[test]
fn without_a_cap_a_lease_lost_again_is_requeued() {
    let mut s = Scheduler::new(TERM).recording_requeues();
    up(&mut s, 0);
    submit(&mut s, 0, 1);
    let mut secs = 1;
    for _ in 0..(LOSS_REQUEUES + 2) {
        let lease = start(&mut s, secs);
        lose(&mut s, lease, secs + 1);
        secs += 5;
    }
    assert_eq!(s.state(OperationId(0)), Some(&OpState::Queued));
}

/// Catches: a count a restart forgets (an action that kills the scheduler's host starts
/// from none each time, and loops): the losses the store holds count toward the cap, and
/// the lease the restarted scheduler gives up is one more.
#[test]
fn losses_the_store_holds_count_toward_the_cap() {
    for (before, refused) in [(1, false), (2, true)] {
        let mut s = capped();
        let held = Recovered {
            operation: OperationId(3),
            request: request(3),
            waiter: WaiterId(3),
            lease: Some(RecoveredLease {
                lease: LeaseId::new(TERM, 5),
                worker: WorkerId::new(NODE),
            }),
            losses: before,
            fate: None,
        };
        s.recover(TERM, OperationId(10), 20, vec![held]);
        assert_eq!(s.losses(OperationId(3)), Some(before));
        up(&mut s, 5);
        assert!(heartbeat(&mut s, 6, Vec::new()).is_empty());
        assert_eq!(s.losses(OperationId(3)), Some(before + 1));
        let effects = feed(&mut s, 7, Event::Tick);
        let record = matches!(
            effects.as_slice(),
            [Effect::Commit(ControlRecord::Refusal(_))]
        );
        assert_eq!(record, refused, "after {before} losses: {effects:?}");
    }
}
