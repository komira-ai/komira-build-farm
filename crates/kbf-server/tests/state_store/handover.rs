//! A farm taken out of service for a successor: what it stops doing, how it answers the
//! callers it kept, and the operator's cordon across the change.

use kbf_front::{Backend, Handed};
use kbf_server::farm::LOST_LOOKS;
use kbf_server::farm::NodeAction;
use kbf_server::fleet::PlacementView;
use kbf_state::{Cond, Txn};
use kbf_types::OperationId;

use super::*;

/// A farm over `disk` whose cache the test keeps, so it can read its metrics.
fn farm_over_disk(disk: &Disk, wait: Duration) -> (TestFarm, Backend) {
    let backend = Backend::from(Arc::new(Cache::memory()));
    let store: Arc<dyn Store> = disk.store.clone();
    let farm = Farm::new(backend.clone(), wait, kbf_sched::FINISHED_RETENTION)
        .with_state(Some(StateStore {
            store,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store");
    (farm, backend)
}

/// A request for more memory than the node has: it stays queued.
fn unplaceable(tag: u8) -> Request {
    Request {
        resources: Resources::new(1_000, 64 * GIB),
        ..request(tag)
    }
}

fn queue(farm: &TestFarm, request: Request) -> kbf_front::Ticket {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
    })
    .expect("queued")
}

fn done(ticket: &kbf_front::Ticket) -> Option<Finished> {
    match &*ticket.stage.borrow() {
        Stage::Done(finished) => Some(finished.clone()),
        _ => None,
    }
}

fn status(finished: &Finished) -> (i32, String) {
    match finished {
        Finished::Failed(status) => (status.code, status.message.clone()),
        other => panic!("{other:?}"),
    }
}

/// Catches: a fenced farm that still takes work or writes: it must queue no request
/// (UNAVAILABLE, which Buck2 asks again), grant nothing, accept no result (the daemon
/// sends it again to the successor), change nothing a heartbeat or a tick would, and
/// write nothing to the store at all (not a renewal, not a ping), since from then on the
/// successor's rows are the truth. Mutants: the fence flag not checked in `admit`, in
/// `feed`, or in the session renewal.
#[tokio::test(start_paused = true)]
async fn a_fenced_farm_queues_nothing_accepts_nothing_and_writes_nothing() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    submit(&farm, unplaceable(2)).expect("queued, never placed");
    let rows = disk.all_rows();

    farm.fence();
    let writes = disk.store.commit_stats().txns;
    let refused = submit(&farm, request(3)).expect_err("a fenced farm queued a request");
    assert_eq!(refused.code(), Code::Unavailable);
    assert!(refused.message().contains("handing its work"), "{refused}");
    farm.tick().await;
    tokio::time::advance(kbf_server::farm::SESSION_RENEWAL * 2).await;
    assert!(farm.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(!accepted(&farm, lease).await, "a fenced farm took a result");
    assert!(starts(&mut inbox).is_empty());
    assert_eq!(
        disk.store.commit_stats().txns,
        writes,
        "a fenced farm wrote"
    );
    assert_eq!(disk.all_rows(), rows);

    // Not fenced, the same result is taken: the refusal is the fence's.
    let other = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&other);
    assert!(other.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(accepted(&other, lease).await);
}

/// Catches: daemon streams left running after a handover (a daemon would go on talking
/// to a process that no longer schedules), a status that does not reach them, and a
/// count that includes a stream already gone.
#[tokio::test(start_paused = true)]
async fn the_worker_streams_are_ended_with_a_status() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let (outbound, gone) = mpsc::unbounded_channel();
    drop(gone);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    farm.register(
        &WorkerId::new("node-b"),
        DaemonInstance::new("daemon-b"),
        DaemonInstance::new(""),
        Reported::serving(Resources::new(4_000, 8 * GIB), caps),
        outbound,
        ServerMessage::default(),
    );
    let ended = farm.end_worker_streams(&Status::unavailable("handed over"));
    assert_eq!(ended, 1, "only the stream still open counts");
    let _welcome = inbox.recv().await.expect("the Welcome");
    let message = inbox.recv().await.expect("a message");
    assert_eq!(message.expect_err("a status").code(), Code::Unavailable);
}

/// Catches: a handover that goes ahead while a call is between its request and its
/// queueing, and one that never goes ahead: the farm is quiet only when no such call is
/// in flight.
#[tokio::test]
async fn a_farm_is_quiet_when_no_call_is_between_its_request_and_its_queueing() {
    let disk = Disk::new();
    let (farm, backend) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    assert!(farm.quiet());
    let busy = backend.metrics().execute_presubmit_in_flight.raise();
    assert!(!farm.quiet());
    drop(busy);
    assert!(farm.quiet());
}

/// Catches: the callers a fenced farm kept left to wait for ever, or answered wrongly,
/// whatever the successor does: it finishes an operation (its `done/` row is the answer),
/// leaves one queued (nothing changes), or leaves no trace of one (answered UNAVAILABLE at
/// the third look in a row, counted in `kbf_frontend_relay_lost_total`; a look that finds
/// the operation again starts the count over, so a successor between the two transactions
/// of a finish is not taken for one that lost the operation). Each follow returns how many
/// callers still wait. Mutants: a vanished operation answered at the first look; the look
/// count not reset by a look that finds the operation; the `done/` row ignored.
#[tokio::test(start_paused = true)]
async fn a_fenced_farm_answers_its_callers_from_what_the_successor_wrote() {
    assert_eq!(LOST_LOOKS, 3, "this test counts looks");
    let disk = Disk::new();
    let (old, backend) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let (_, mut inbox) = register(&old);
    let ran = queue(&old, request(1));
    let queued = queue(&old, unplaceable(2));
    let vanishes = queue(&old, request(3));
    let flickers = queue(&old, request(4));
    let [lease, _, _] = starts(&mut inbox)[..] else {
        panic!("three leases expected");
    };
    old.fence();
    assert_eq!(old.follow().expect("follow"), 4, "all four wait");

    // The successor: the same store, the epoch kept; the daemon reconnects to it and
    // lists its lease.
    let (new, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let (stream, _inbox) = register(&new);
    assert!(new.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(
        accepted(&new, lease).await,
        "the successor takes the result"
    );
    assert_eq!(old.follow().expect("follow"), 3);
    let finished = done(&ran).expect("answered from the done row");
    assert_eq!(
        status(&finished),
        (Code::InvalidArgument as i32, "test".to_owned())
    );
    assert!(done(&queued).is_none() && done(&vanishes).is_none());

    // Two operations lose their rows: one for good, one for two looks.
    let key = |prefix: &str, op: u64| format!("{prefix}{op:016x}");
    let flicker_row = disk
        .store
        .get(&key("op/", 3))
        .expect("store")
        .expect("its row");
    let erase = |op: u64| {
        let txn = Txn::new()
            .delete(key("op/", op), Cond::Any)
            .delete(key("queue/", op), Cond::Any);
        disk.store.txn(txn).expect("store").expect("erased");
    };
    erase(2);
    erase(3);
    for _ in 0..2 {
        assert_eq!(old.follow().expect("follow"), 3);
    }
    assert_eq!(backend.metrics().frontend_relay_lost.get(), 0);
    let back = Txn::new().put(key("op/", 3), flicker_row.value, Cond::Any);
    disk.store.txn(back).expect("store").expect("back");
    assert_eq!(
        old.follow().expect("follow"),
        2,
        "the vanished one is answered"
    );
    assert_eq!(backend.metrics().frontend_relay_lost.get(), 1);
    let (code, why) = status(&done(&vanishes).expect("answered"));
    assert_eq!(code, Code::Unavailable as i32);
    assert!(why.contains("lost in a frontend handover"), "{why}");

    // It vanishes again: two looks do not answer it, the earlier ones do not count.
    erase(3);
    for _ in 0..2 {
        assert_eq!(old.follow().expect("follow"), 2);
    }
    assert!(done(&flickers).is_none());
    assert_eq!(backend.metrics().frontend_relay_lost.get(), 1);
}

/// Catches: a farm with no state store that panics or claims callers waiting when told
/// to follow a successor that has no rows to read.
#[tokio::test]
async fn a_farm_with_no_state_store_follows_no_one() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    farm.fence();
    assert_eq!(farm.follow().expect("follow"), 0);
}

/// Catches: the relay asking a farm that has not handed over, or one that has, the wrong
/// thing: `handed` is the fence, and nothing else.
#[tokio::test]
async fn a_farm_has_handed_over_when_it_is_fenced() {
    let disk = Disk::new();
    let (farm, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    assert!(!farm.handed());
    farm.fence();
    assert!(farm.handed());
}

/// Catches: an operation the relay cannot find, or finds wrongly, by its action: it is the
/// unfinished row naming the action and instance (the instance may be left out for a call
/// that has none, then it must be the only one; two instances running one action are
/// ambiguous and neither is guessed), and a farm with no state store finds none. Mutants:
/// the instance ignored; a finished operation (no `op/` row) still found.
#[tokio::test(start_paused = true)]
async fn an_operation_is_found_by_its_action_among_the_unfinished_rows() {
    let disk = Disk::new();
    let (old, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let (new, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let ticket = queue(&new, unplaceable(2));
    let other = |instance: &str| Request {
        key: ActionKey {
            instance: instance.to_owned(),
            ..unplaceable(2).key
        },
        ..unplaceable(2)
    };
    let action = unplaceable(2).key.action;
    let found = old.operation_of(Some("main"), &action).expect("rows");
    assert!(found.is_some(), "the unfinished operation is found");
    assert_eq!(old.operation_of(Some("other"), &action), Ok(None));
    assert_eq!(old.operation_of(None, &action), Ok(found));
    assert_eq!(
        old.operation_of(Some("main"), &request(9).key.action),
        Ok(None)
    );
    queue(&new, other("other"));
    assert_eq!(old.operation_of(None, &action), Ok(None), "ambiguous");
    assert_eq!(old.operation_of(Some("main"), &action), Ok(found));
    drop(ticket);

    let bare = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    assert_eq!(bare.operation_of(Some("main"), &action), Ok(None));
    assert_eq!(bare.look(OperationId(0)), Ok(None));
}

/// Catches: the fallback of a relayed call answering wrongly from the rows of an operation
/// this farm's scheduler never held: a queued one shows nothing, a leased one is shown
/// executing, a finished one is answered from its `done/` row, and one with no trace is
/// answered UNAVAILABLE at the third look in a row and counted in
/// `kbf_frontend_relay_lost_total`, as the follower does for the callers it kept. Mutants:
/// the lost rule not applied to a relayed operation; the lost count not added.
#[tokio::test(start_paused = true)]
async fn a_relayed_operation_is_followed_from_the_rows_as_a_kept_caller_is() {
    let disk = Disk::new();
    let (old, backend) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    old.fence();
    let (new, _) = farm_over_disk(&disk, kbf_sched::UNSERVABLE_WAIT);
    let (stream, mut inbox) = register(&new);
    queue(&new, request(1));
    queue(&new, unplaceable(2));
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    let find = |request: &Request| {
        old.operation_of(Some("main"), &request.key.action)
            .expect("rows")
            .expect("unfinished")
    };
    let (ran, waits) = (find(&request(1)), find(&unplaceable(2)));
    assert_eq!(old.look(waits), Ok(None));
    assert_eq!(old.look(ran), Ok(Some(Stage::Executing)));

    assert!(new.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(accepted(&new, lease).await);
    let Ok(Some(Stage::Done(finished))) = old.look(ran) else {
        panic!("answered from the done row");
    };
    assert_eq!(
        status(&finished),
        (Code::InvalidArgument as i32, "test".to_owned())
    );

    let txn = Txn::new()
        .delete(format!("op/{:016x}", waits.0), Cond::Any)
        .delete(format!("queue/{:016x}", waits.0), Cond::Any);
    disk.store.txn(txn).expect("store").expect("erased");
    for _ in 0..2 {
        assert_eq!(old.look(waits), Ok(None));
    }
    assert_eq!(backend.metrics().frontend_relay_lost.get(), 0);
    let Ok(Some(Stage::Done(lost))) = old.look(waits) else {
        panic!("answered lost");
    };
    let (code, why) = status(&lost);
    assert_eq!(code, Code::Unavailable as i32);
    assert!(why.contains("lost in a frontend handover"), "{why}");
    assert_eq!(backend.metrics().frontend_relay_lost.get(), 1);
}

/// What the operator did to `node-a`, as the farm shows it.
fn placement(farm: &TestFarm) -> PlacementView {
    farm.node_view(&node())
        .expect("the node registered")
        .placement
}

/// Catches: an operator's cordon or drain forgotten across a start (the successor of a
/// handover would offer leases to a node taken out of placement for an update), a drain
/// that comes back without its deadline (a node drained for an hour paused at once, or
/// never), one whose deadline had passed that does not pause at the first tick, and a
/// node returned to placement that comes back cordoned. Mutants: no row written by
/// `place`; the deadline read as a duration; the row of an uncordon not deleted.
async fn a_cordon_survives_a_restart(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1, "the node holds a lease");

    // A plain cordon.
    before.place(&node(), NodeAction::Cordon).expect("cordon");
    let after = {
        disk.crash(before);
        disk.farm(EpochPolicy::Keep)
    };
    let (_stream, mut inbox) = register(&after);
    assert_eq!(placement(&after), PlacementView::Cordoned);
    submit(&after, request(2)).expect("queued");
    assert!(
        starts(&mut inbox).is_empty(),
        "a cordoned node was offered a lease after a restart"
    );

    // A drain with an hour to run keeps its deadline.
    let wall = |view: PlacementView| match view {
        PlacementView::Draining {
            deadline_unix_ms, ..
        } => deadline_unix_ms,
        other => panic!("{other:?}"),
    };
    after
        .place(&node(), NodeAction::Drain(Duration::from_secs(3600)))
        .expect("drain");
    let drain_deadline = wall(placement(&after));
    disk.crash(after);
    let after = disk.farm(EpochPolicy::Keep);
    let (_stream, _inbox) = register(&after);
    let restored = wall(placement(&after));
    assert!(
        restored.abs_diff(drain_deadline) < 10_000,
        "the deadline moved from {drain_deadline} to {restored}"
    );

    // A drain whose deadline has passed pauses at the first tick.
    after
        .place(&node(), NodeAction::Drain(Duration::ZERO))
        .expect("drain");
    disk.crash(after);
    let after = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, Vec::new()).await);
    after.tick().await;
    assert!(
        matches!(placement(&after), PlacementView::DrainPaused { .. }),
        "{:?}",
        placement(&after)
    );

    // Back in placement: nothing is left to come back.
    after
        .place(&node(), NodeAction::Uncordon)
        .expect("uncordon");
    assert_eq!(disk.rows("cordon/"), 0);
    disk.crash(after);
    let after = disk.farm(EpochPolicy::Keep);
    let (_stream, _inbox) = register(&after);
    assert_eq!(placement(&after), PlacementView::Serving);
}

#[tokio::test(start_paused = true)]
async fn a_cordon_survives_a_restart_over_the_fake() {
    a_cordon_survives_a_restart(Backing::Fake).await;
}

#[tokio::test(start_paused = true)]
async fn a_cordon_survives_a_restart_over_sqlite() {
    a_cordon_survives_a_restart(Backing::Sqlite).await;
}
