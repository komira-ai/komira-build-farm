//! The requeues the scheduler records for the log (issue #166): one per lease it gives
//! up, naming the operation, the lease, the worker and the reason, and none unless the
//! caller asked for them.

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::fence::{HANDOVER_GRACE, LEASE_GRACE};
use kbf_sched::{
    DaemonInstance, Event, Input, OpState, Request, Requeue, RequeueReason, Scheduler,
};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, Qos, Resources,
    StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

fn request() -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [7; 32], 7),
        },
        qos: Qos::Ci,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn at(d: Duration) -> FarmTime {
    FarmTime::from_millis(u64::try_from(d.as_millis()).expect("millis"))
}

fn feed(s: &mut Scheduler, now: Duration, event: Event) -> Vec<Effect> {
    s.apply(Input::new(at(now), event))
}

fn up(s: &mut Scheduler, now: Duration, instance: &str) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let event = Event::WorkerUp {
        worker: WorkerId::new("node-a"),
        instance: DaemonInstance::new(instance),
        capacity: Resources::new(1_000, GIB),
        caps,
    };
    assert!(feed(s, now, event).is_empty());
}

fn heartbeat(s: &mut Scheduler, now: Duration) {
    let event = Event::Heartbeat {
        worker: WorkerId::new("node-a"),
        running: Vec::new(),
    };
    assert!(feed(s, now, event).is_empty());
}

/// A scheduler with worker `node-a` (process `one`) running one operation, whose
/// `Start` went out at 0.
fn running(s: Scheduler) -> (Scheduler, LeaseGrant) {
    let mut s = s;
    up(&mut s, Duration::ZERO, "one");
    let submit = Event::Submit {
        waiter: WaiterId(1),
        request: request(),
    };
    assert!(feed(&mut s, Duration::ZERO, submit).is_empty());
    let effects = feed(&mut s, Duration::ZERO, Event::Tick);
    let [Effect::Commit(ControlRecord::Lease(grant))] = effects.as_slice() else {
        panic!("{effects:?}");
    };
    let grant = grant.clone();
    let committed = Event::Committed(ControlRecord::Lease(grant.clone()));
    assert!(matches!(
        feed(&mut s, Duration::ZERO, committed).as_slice(),
        [Effect::Start(_)]
    ));
    (s, grant)
}

fn expect_one(s: &mut Scheduler, grant: &LeaseGrant, reason: RequeueReason) {
    assert_eq!(
        s.state(grant.operation),
        Some(&OpState::Queued),
        "{reason:?}"
    );
    assert_eq!(
        s.take_requeues(),
        vec![Requeue {
            operation: grant.operation,
            lease: grant.lease,
            worker: WorkerId::new("node-a"),
            reason,
        }]
    );
    assert!(s.take_requeues().is_empty(), "a requeue taken twice");
}

/// Catches: a requeue of a silent node's lease not recorded, or recorded with another
/// operation, lease, worker or reason; one handed out twice; and grants or starts
/// recorded as requeues.
#[test]
fn a_silent_node_s_lease_is_recorded_as_silent() {
    let (mut s, grant) = running(Scheduler::new(1).recording_requeues());
    assert!(
        s.take_requeues().is_empty(),
        "a grant recorded as a requeue"
    );
    feed(&mut s, LEASE_GRACE, Event::Tick);
    expect_one(&mut s, &grant, RequeueReason::Silent);
}

/// Catches: a lease the node never listed, past the start grace, recorded with
/// another reason (the operator would look for a silent node that was not).
#[test]
fn a_lease_never_listed_is_recorded_as_not_started() {
    let (mut s, grant) = running(Scheduler::new(1).recording_requeues());
    heartbeat(&mut s, LEASE_GRACE);
    expect_one(&mut s, &grant, RequeueReason::NotStarted);
}

/// Catches: a lease the same daemon process lost across a reconnect recorded with
/// another reason.
#[test]
fn a_lease_lost_across_a_reconnect_is_recorded_as_reconnected() {
    let (mut s, grant) = running(Scheduler::new(1).recording_requeues());
    up(&mut s, Duration::from_secs(10), "one");
    heartbeat(&mut s, Duration::from_secs(11));
    expect_one(&mut s, &grant, RequeueReason::Reconnected);
}

/// Catches: a lease given up once another daemon process took over the node recorded
/// with another reason.
#[test]
fn a_lease_of_a_replaced_process_is_recorded_as_replaced() {
    let (mut s, grant) = running(Scheduler::new(1).recording_requeues());
    let later = Duration::from_secs(1);
    up(&mut s, later, "two");
    heartbeat(&mut s, HANDOVER_GRACE + later);
    expect_one(&mut s, &grant, RequeueReason::Replaced);
}

/// Catches: requeues collected by a scheduler nobody asked to record them (a
/// simulation never takes them, so they would pile up for its whole run).
#[test]
fn requeues_are_not_recorded_unless_asked_for() {
    let (mut s, grant) = running(Scheduler::new(1));
    feed(&mut s, LEASE_GRACE, Event::Tick);
    assert_eq!(s.state(grant.operation), Some(&OpState::Queued));
    assert!(s.take_requeues().is_empty());
}
