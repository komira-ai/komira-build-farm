//! The intake poll: what a frontend takes in from the other frontends of its state store,
//! forgets because they took it, answers its callers from, and applies of what they set.
//! The other frontend is a handle over the same store, or a write of the test.

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Dispatch, Submission};
use kbf_proto::google::rpc;
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_types::Resources;
use tokio::sync::mpsc;
use tonic::Code;

use super::blocks_tests::{Racing, durable_over, farm_over, request, submit, unfinished};
use super::owner::{owner_key, pair_row};
use super::owner_tests::{grant, overwrite, remove, value_of};
use super::transition_tests::{answer, ending, requeue, rows, subject};
use super::*;
use crate::farm::NodeAction;

pub(super) type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// Queues operation `operation` through `durable`, as another frontend does.
fn queue_on(durable: &Durable, operation: u64) {
    let queued = durable.submitted(OperationId(operation), &request(1), &subject());
    queued.expect("submit");
}

/// A caller of `farm` that waits for the action `tag`.
pub(super) fn caller(farm: &Farm, tag: u8) -> kbf_front::Ticket {
    let submission = Submission {
        request: request(tag),
        call: None,
        described: Described::default(),
        evidence: Default::default(),
    };
    farm.submit(submission).expect("submitted")
}

/// Registers the daemon `name` with room for four actions, as a stream opening does.
pub(super) fn register(farm: &Farm, name: &str) -> Inbox {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 << 30), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    farm.register(
        &WorkerId::new(name),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    );
    inbox
}

/// How many `Start`s are waiting in `inbox`.
fn starts(inbox: &mut Inbox) -> usize {
    let messages = std::iter::from_fn(|| inbox.try_recv().ok());
    let sent = |message: &Result<ServerMessage, Status>| {
        let message = message.as_ref().expect("no error").message.as_ref();
        matches!(message, Some(server_message::Message::Start(_)))
    };
    messages.filter(sent).count()
}

/// A result of `lease` the farm accepts without a cache write.
pub(super) fn invalid(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: Some(rpc::Status {
            code: Code::InvalidArgument as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        action_result: None,
        action_digest: None,
        memory_kill: 0,
        lost: 0,
    }
}

pub(super) fn done_with(ticket: &kbf_front::Ticket) -> String {
    let Stage::Done(Finished::Failed(status)) = ticket.stage.borrow().clone() else {
        panic!("the caller was not answered");
    };
    status.message
}

/// Asserts that a farm with no state store has nothing to poll: the poll does nothing and
/// the stored epoch is not ahead.
#[tokio::test(start_paused = true)]
async fn a_farm_without_a_state_store_polls_nothing() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );

    farm.intake();

    assert_eq!(farm.epoch_ahead(), None);
}

/// Asserts that a farm a handover fenced takes nothing in, and takes in what the store
/// holds once it is put back to work.
#[tokio::test(start_paused = true)]
async fn a_farm_that_handed_its_work_over_takes_nothing_in() {
    let store = Racing::new();
    let farm = farm_over(&store);
    queue_on(&durable_over(&store), 5000);

    farm.fence();
    farm.intake();
    assert!(unfinished(&farm).is_empty(), "a fenced farm took work in");

    farm.resume();
    farm.intake();
    assert_eq!(unfinished(&farm), [OperationId(5000)]);
}

/// Asserts that a poll that cannot read a row of the store takes nothing in, and takes in
/// what is queued when the row is gone.
#[tokio::test(start_paused = true)]
async fn a_poll_that_cannot_read_the_queue_takes_nothing_in() {
    let store = Racing::new();
    let farm = farm_over(&store);
    queue_on(&durable_over(&store), 5000);
    overwrite(&store, "queue/zz", Vec::new());

    farm.intake();
    assert!(unfinished(&farm).is_empty(), "work taken in on a guess");

    remove(&store, "queue/zz");
    farm.intake();
    assert_eq!(unfinished(&farm), [OperationId(5000)]);
}

/// Asserts that an operation another frontend queued is taken in with the loss count the
/// store holds of it, that the ids this frontend numbers with go past it, and that a second
/// poll takes nothing more.
#[tokio::test(start_paused = true)]
async fn a_queued_operation_is_taken_in_with_the_rows_the_store_holds() {
    let store = Racing::new();
    let farm = farm_over(&store);
    queue_on(&durable_over(&store), 5000);
    overwrite(&store, "loss/0000000000001388", 2u64.to_be_bytes().to_vec());

    farm.intake();
    farm.intake();

    assert_eq!(unfinished(&farm), [OperationId(5000)]);
    let state = farm.lock();
    assert_eq!(state.sched.losses(OperationId(5000)), Some(2));
    assert!(state.sched.next_operation().0 > 5000);
}

/// Asserts that an operation another frontend granted leaves the scheduler (nothing is
/// recorded as given up), that its caller is shown it executing, and that the caller gets
/// the result the other frontend recorded, kept for the finished retention like a result
/// this frontend finished, and that the name is answered from the store's result until it
/// expires, after the callers are forgotten.
#[tokio::test(start_paused = true)]
async fn an_operation_another_frontend_took_leaves_the_scheduler_and_its_callers_follow() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let ticket = caller(&farm, 1);
    let [operation] = unfinished(&farm)[..] else {
        panic!("one operation expected");
    };
    let other = durable_over(&store);
    grant(&other, operation.0, 0, "node-b").expect("granted by another frontend");

    farm.intake();

    assert!(
        unfinished(&farm).is_empty(),
        "the scheduler kept the operation"
    );
    assert!(matches!(*ticket.stage.borrow(), Stage::Executing));
    assert_eq!(farm.lock().intake.following(), 1);
    let finished = ending("finished elsewhere");
    let result = [Effect::Answer(answer(operation.0, 0))];
    other
        .persist(&result, Some(&finished))
        .expect("finished by another frontend");

    farm.intake();

    assert_eq!(done_with(&ticket), "finished elsewhere");
    assert_eq!(farm.lock().intake.following(), 0);
    let name = ticket.name.clone();
    assert!(
        farm.wait(&name).is_some(),
        "the result was forgotten at once"
    );
    tokio::time::advance(kbf_sched::FINISHED_RETENTION + Duration::from_secs(1)).await;
    farm.intake();
    assert_eq!(
        farm.lock().machine.waiter_ids().count(),
        0,
        "the callers were kept for ever"
    );
    // The name is answered from the store's result until that goes, long after the callers.
    assert!(farm.wait(&name).is_some(), "the stored result is gone");
    store.advance(DONE_FLOOR + Duration::from_secs(1));
    assert!(farm.wait(&name).is_none(), "the result was kept for ever");
}

/// Asserts that a look at the store that fails shows the callers of an operation another
/// frontend took nothing and loses none of them, that they are shown it executing once the
/// look works, and that callers who all cancelled are no longer followed.
#[tokio::test(start_paused = true)]
async fn callers_follow_an_operation_through_a_failed_look_until_they_cancel() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let ticket = caller(&farm, 1);
    let [operation] = unfinished(&farm)[..] else {
        panic!("one operation expected");
    };
    let other = durable_over(&store);
    grant(&other, operation.0, 0, "node-b").expect("granted by another frontend");

    // The farm has no daemon, so its callers were told at the submit that no worker can run
    // the operation: that is what a look that fails must leave them showing.
    assert!(matches!(*ticket.stage.borrow(), Stage::Waiting(_)));
    store.fail_reads(Some("done/"));
    farm.intake();
    assert!(matches!(*ticket.stage.borrow(), Stage::Waiting(_)));
    assert_eq!(farm.lock().intake.following(), 1);

    store.fail_reads(None);
    farm.intake();
    assert!(matches!(*ticket.stage.borrow(), Stage::Executing));
    assert_eq!(farm.lock().intake.following(), 1);

    drop(ticket);
    farm.intake();
    assert_eq!(farm.lock().intake.following(), 0);
}

/// Asserts that an operation whose lease the scheduler gave up, and whose requeue the store
/// has not taken, stays in the scheduler although the store has no queue row for it, and is
/// not taken in again once the row is written.
#[tokio::test(start_paused = true)]
async fn an_operation_whose_requeue_the_store_has_not_taken_is_not_handed_off() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let _inbox = register(&farm, "node-a");
    submit(&farm, 1).expect("granted at once");
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (1, 0));
    store.set_down(true);
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    store.set_down(false);

    farm.intake();

    assert_eq!(unfinished(&farm).len(), 1, "the operation was handed off");
    assert_eq!(farm.queued_operations(), 1);
    assert_eq!(farm.lock().intake.following(), 0);
    farm.tick().await;
    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (0, 1));
    farm.intake();
    assert_eq!(unfinished(&farm).len(), 1);
    assert_eq!(farm.queued_operations(), 1);
}

/// Asserts that an operation held queued whose queue row is a newer write than the one read
/// (another frontend granted it and queued it again, with a loss counted) is taken in again
/// with the rows of now, and that its old callers, who all cancelled, are not followed.
#[tokio::test(start_paused = true)]
async fn an_operation_queued_again_by_another_frontend_is_taken_in_again() {
    let store = Racing::new();
    let farm = farm_over(&store);
    queue_on(&durable_over(&store), 5000);
    farm.intake();
    assert_eq!(farm.lock().sched.losses(OperationId(5000)), Some(0));

    overwrite(&store, "queue/0000000000001388", Vec::new());
    overwrite(&store, "loss/0000000000001388", 1u64.to_be_bytes().to_vec());
    farm.intake();

    assert_eq!(unfinished(&farm), [OperationId(5000)]);
    assert_eq!(farm.lock().sched.losses(OperationId(5000)), Some(1));
    assert_eq!(farm.lock().intake.following(), 0);
    farm.intake();
    assert_eq!(farm.lock().sched.losses(OperationId(5000)), Some(1));
}

/// Asserts that a farm whose intake is closed, which holds an operation queued whose queue
/// row is then a newer write than the one read, hands the operation off and does not take it
/// in again.
#[tokio::test(start_paused = true)]
async fn a_closed_farm_does_not_take_an_operation_queued_again_back_in() {
    let store = Racing::new();
    let farm = farm_over(&store);
    queue_on(&durable_over(&store), 5000);
    farm.intake();
    assert_eq!(unfinished(&farm), [OperationId(5000)]);
    farm.close_intake();

    overwrite(&store, "queue/0000000000001388", Vec::new());
    farm.intake();

    assert!(unfinished(&farm).is_empty(), "a closed farm took it in");
    assert_eq!(rows(&store, "queue/"), 1, "the store lost the operation");
}

/// Asserts that an uncordon on a farm whose intake is closed ([`Farm::close_intake`]) does
/// not place work on the node: the node stays cordoned and is given nothing, and an uncordon
/// on a farm whose intake is open places the queued work on the node at once.
#[tokio::test(start_paused = true)]
async fn an_uncordon_on_a_closed_farm_places_nothing() {
    let store = Racing::new();
    let (open, closed) = (farm_over(&store), farm_over(&store));
    let (node_open, node_closed) = (WorkerId::new("node-open"), WorkerId::new("node-closed"));
    let mut inbox_open = register(&open, "node-open");
    let mut inbox_closed = register(&closed, "node-closed");
    for (farm, node) in [(&open, &node_open), (&closed, &node_closed)] {
        farm.place(node, NodeAction::Cordon).expect("a known node");
    }
    closed.close_intake();
    let _waiting = (caller(&open, 1), caller(&closed, 2));

    open.place(&node_open, NodeAction::Uncordon)
        .expect("a known node");
    closed
        .place(&node_closed, NodeAction::Uncordon)
        .expect("a known node");

    assert_eq!(starts(&mut inbox_open), 1, "an open farm placed nothing");
    assert_eq!(starts(&mut inbox_closed), 0, "a closed farm placed work");
    assert!(closed.lock().sched.cordon(&node_closed).is_some());
}

/// Asserts that a queued operation another frontend granted after the poll read the queue is
/// not taken in: its lease belongs to the frontend of its node, so the scheduler holds
/// nothing for it.
#[tokio::test(start_paused = true)]
async fn a_queued_operation_granted_since_the_poll_read_the_queue_is_not_taken_in() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let other = durable_over(&store);
    queue_on(&other, 5000);
    let version = other.queue_version(OperationId(5000));
    let version = version.expect("read").expect("queued");
    grant(&other, 5000, 0, "node-b").expect("granted by another frontend");

    farm.lock().take_in(OperationId(5000), version);

    assert!(
        unfinished(&farm).is_empty(),
        "a leased operation was taken in"
    );
    assert_eq!(rows(&store, "lease/"), 1);
}

/// Asserts that a result the store refuses as stale (the lease row is gone) does not leave
/// its callers waiting on this frontend: they are answered with the result another frontend
/// recorded.
#[tokio::test(start_paused = true)]
async fn a_result_the_store_refuses_as_stale_is_answered_from_the_store() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let _inbox = register(&farm, "node-a");
    let ticket = caller(&farm, 1);
    let views = farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let (lease, operation) = (view.lease, view.operation);
    remove(&store, &lease_key(lease));

    farm.report(&WorkerId::new("node-a"), invalid(lease)).await;

    assert_eq!(farm.dropped_writes(), 1);
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    assert_eq!(farm.lock().intake.following(), 1);
    remove(&store, &op_key(operation));
    let finished = ending("finished elsewhere").finished;
    overwrite(&store, &done_key(operation), finished.to_row());

    farm.intake();

    assert_eq!(done_with(&ticket), "finished elsewhere");
    assert_eq!(farm.lock().intake.following(), 0);
}

/// Asserts that a result the store refuses because the daemon said `Hello` to another
/// frontend since (the node's owner row is not this frontend's) does not leave its callers
/// waiting on this frontend: they are answered with the result the other frontend recorded.
#[tokio::test(start_paused = true)]
async fn a_result_refused_for_a_node_that_moved_is_answered_from_the_store() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let node = WorkerId::new("node-a");
    let _inbox = register(&farm, "node-a");
    let ticket = caller(&farm, 1);
    let views = farm.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let (lease, operation) = (view.lease, view.operation);
    overwrite(&store, &owner_key(&node), pair_row(9, 9).to_vec());

    farm.report(&node, invalid(lease)).await;

    assert_eq!(farm.dropped_writes(), 1);
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    assert_eq!(farm.lock().intake.following(), 1);
    remove(&store, &op_key(operation));
    let finished = ending("finished elsewhere").finished;
    overwrite(&store, &done_key(operation), finished.to_row());

    farm.intake();

    assert_eq!(done_with(&ticket), "finished elsewhere");
    assert_eq!(farm.lock().intake.following(), 0);
}

/// Asserts that a refusal the store refuses as stale (the operation is running under another
/// frontend's lease) does not leave its callers waiting on this frontend: they are answered
/// from the `done/` row that stands.
#[tokio::test(start_paused = true)]
async fn a_refusal_the_store_refuses_as_stale_is_answered_from_the_store() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let ticket = caller(&farm, 1);
    let [operation] = unfinished(&farm)[..] else {
        panic!("one operation expected");
    };
    let other = durable_over(&store);
    grant(&other, operation.0, 0, "node-b").expect("granted by another frontend");
    let wait = kbf_sched::UNSERVABLE_WAIT + Duration::from_secs(1);
    tokio::time::advance(wait).await;

    farm.tick().await;

    assert_eq!(farm.dropped_writes(), 1, "the refusal was written");
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    let finished = ending("finished elsewhere");
    let result = [Effect::Answer(answer(operation.0, 0))];
    other
        .persist(&result, Some(&finished))
        .expect("finished by another frontend");
    farm.intake();
    assert_eq!(done_with(&ticket), "finished elsewhere");
}

/// Asserts that the memory floors another frontend raised are raised here, and that the key
/// floors are read only when the learned sizes are.
#[tokio::test(start_paused = true)]
async fn the_floors_another_frontend_raised_are_raised_here() {
    let store = Racing::new();
    let farm = farm_over(&store);
    let floor = Floor {
        key: request(1).key,
        ask: 16 << 30,
        stamp: 3,
    };
    let row = floor::encode(&floor).expect("it fits");
    overwrite(&store, &floor::row_key(&floor.key), row);
    let key_floor = KeyFloor {
        key: 9,
        ask: 8 << 30,
        stamp: 4,
    };
    overwrite(
        &store,
        &floor::key_row(9),
        floor::encode_key(&key_floor, None),
    );

    farm.intake();

    assert_eq!(farm.lock().sched.memory_floor(&floor.key), Some(16 << 30));
    assert_eq!(farm.lock().sched.key_floor(9), None, "learned sizes off");
    let state = farm.lock();
    let durable = state.durable.as_ref().expect("a store");
    durable.read_size_keys.store(true, Ordering::SeqCst);
    drop(state);
    farm.intake();
    assert_eq!(farm.lock().sched.key_floor(9), Some(8 << 30));
}

/// Asserts that a floor row, or a key floor row, the scheduler did not write fails the poll
/// (nothing is taken in on a guess), and that the poll works once the row is gone.
#[tokio::test(start_paused = true)]
async fn a_poll_that_cannot_read_a_floor_takes_nothing_in() {
    let store = Racing::new();
    let farm = farm_over(&store);
    queue_on(&durable_over(&store), 5000);
    let state = farm.lock();
    let durable = state.durable.as_ref().expect("a store");
    durable.read_size_keys.store(true, Ordering::SeqCst);
    drop(state);

    overwrite(&store, "floor/zz", vec![9]);
    farm.intake();
    assert!(unfinished(&farm).is_empty(), "work taken in on a guess");
    remove(&store, "floor/zz");

    overwrite(&store, "keyfloor/zz", vec![9]);
    farm.intake();
    assert!(unfinished(&farm).is_empty(), "work taken in on a guess");
    remove(&store, "keyfloor/zz");

    farm.intake();
    assert_eq!(unfinished(&farm), [OperationId(5000)]);
}

/// Asserts that a requeue that raises a floor writes the larger of its ask and the row's,
/// keeps its own stamp, and loses to a write of another frontend between its read of the row
/// and its write (the requeue is then written again from the row as it stands).
#[test]
fn a_floor_row_keeps_the_larger_ask_and_loses_to_a_write_in_between() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let key = request(1).key;
    let row_key = floor::row_key(&key);
    let held = |ask: u64, stamp: u64| Floor {
        key: key.clone(),
        ask,
        stamp,
    };
    overwrite(
        &store,
        &row_key,
        floor::encode(&held(16 << 30, 1)).expect("it fits"),
    );
    let raise = |ask: u64| FloorChange {
        raised: Some(held(ask, 9)),
        ..FloorChange::default()
    };
    let stored = || floor::decode(&value_of(&store, &row_key)).expect("a floor");

    durable
        .requeued(&requeue(4, 0), None, None, &raise(8 << 30))
        .expect("written");
    assert_eq!((stored().ask, stored().stamp), (16 << 30, 9));
    durable
        .requeued(&requeue(4, 0), None, None, &raise(32 << 30))
        .expect("written");
    assert_eq!(stored().ask, 32 << 30);

    let other = floor::encode(&held(64 << 30, 2)).expect("it fits");
    store.land(&row_key, Txn::new().put(row_key.clone(), other, Cond::Any));
    let lost = durable.requeued(&requeue(4, 0), None, None, &raise(48 << 30));
    assert!(matches!(lost, Err(StateError::Conflict(_))), "{lost:?}");
    assert_eq!(stored().ask, 64 << 30);
}

/// Asserts that a requeue that raises a key floor writes the larger of its ask and the
/// row's, keeps its own stamp, and keeps the name of the size summary the row already had.
#[test]
fn a_key_floor_row_keeps_the_larger_ask_and_its_name() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let row_key = floor::key_row(9);
    let held = |ask: u64, stamp: u64| KeyFloor { key: 9, ask, stamp };
    overwrite(
        &store,
        &row_key,
        floor::encode_key(&held(16 << 30, 1), Some("est/x")),
    );
    let raise = |ask: u64| FloorChange {
        key_raised: Some(held(ask, 9)),
        ..FloorChange::default()
    };

    durable
        .requeued(&requeue(4, 0), None, None, &raise(8 << 30))
        .expect("written");
    let (stored, name) = floor::decode_key(&value_of(&store, &row_key)).expect("a key floor");
    assert_eq!((stored.ask, stored.stamp), (16 << 30, 9));
    assert_eq!(name.as_deref(), Some("est/x"));
    durable
        .requeued(&requeue(4, 0), None, None, &raise(32 << 30))
        .expect("written");
    let (stored, _) = floor::decode_key(&value_of(&store, &row_key)).expect("a key floor");
    assert_eq!(stored.ask, 32 << 30);
}

/// Asserts that a requeue that raises a floor, and one that raises a key floor, write over
/// a row they cannot read: it counts for nothing, and the row holds the floor raised.
#[test]
fn a_floor_row_that_cannot_be_read_is_written_over_by_a_raise() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let key = request(1).key;
    let row_key = floor::row_key(&key);
    let key_row = floor::key_row(9);
    overwrite(&store, &row_key, vec![9]);
    overwrite(&store, &key_row, vec![9]);
    let change = FloorChange {
        raised: Some(Floor {
            key,
            ask: 8 << 30,
            stamp: 3,
        }),
        key_raised: Some(KeyFloor {
            key: 9,
            ask: 4 << 30,
            stamp: 4,
        }),
        ..FloorChange::default()
    };

    durable
        .requeued(&requeue(4, 0), None, None, &change)
        .expect("written");

    let stored = floor::decode(&value_of(&store, &row_key)).expect("a floor");
    assert_eq!((stored.ask, stored.stamp), (8 << 30, 3));
    let (stored, _) = floor::decode_key(&value_of(&store, &key_row)).expect("a key floor");
    assert_eq!((stored.ask, stored.stamp), (4 << 30, 4));
}

/// Asserts that floors carrying one stamp, which frontends that numbered their raises
/// alone write, load in key order with distinct stamps, and that floors with stamps of
/// their own keep them.
#[test]
fn floors_that_share_a_stamp_load_in_order_with_distinct_stamps() {
    let store = Racing::new();
    let durable = durable_over(&store);
    let floor_of = |tag: u8, stamp: u64| Floor {
        key: request(tag).key,
        ask: u64::from(tag) << 30,
        stamp,
    };
    let key_floor_of = |key: u64, stamp: u64| KeyFloor {
        key,
        ask: key << 30,
        stamp,
    };
    for floor in [floor_of(1, 5), floor_of(2, 5), floor_of(3, 9)] {
        let row = floor::encode(&floor).expect("it fits");
        overwrite(&store, &floor::row_key(&floor.key), row);
    }
    for floor in [key_floor_of(1, 2), key_floor_of(2, 2), key_floor_of(3, 2)] {
        let row = floor::encode_key(&floor, None);
        overwrite(&store, &floor::key_row(floor.key), row);
    }

    let stored = durable.load().expect("load");

    let stamps: Vec<u64> = stored.floors.iter().map(|floor| floor.stamp).collect();
    let ascending = stamps.windows(2).all(|pair| pair[0] < pair[1]);
    assert_eq!(stamps.len(), 3);
    assert!(ascending, "{stamps:?}");
    assert_eq!(stamps.last(), Some(&9), "a stamp of its own moved");
    let key_stamps: Vec<u64> = stored.key_floors.iter().map(|floor| floor.stamp).collect();
    assert_eq!(key_stamps, [2, 3, 4]);
}

/// Asserts that the poll runs every [`INTAKE_POLL`]: an operation queued after the first
/// look is taken in by a later one.
#[tokio::test(start_paused = true)]
async fn the_poll_looks_at_the_store_every_interval() {
    let store = Racing::new();
    let farm = Arc::new(farm_over(&store));
    let polling = Arc::clone(&farm);
    let running = tokio::spawn(async move { polling.poll_intake().await });
    queue_on(&durable_over(&store), 5000);

    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(unfinished(&farm), [OperationId(5000)]);
    queue_on(&durable_over(&store), 6000);
    assert_eq!(
        unfinished(&farm),
        [OperationId(5000)],
        "taken before its time"
    );
    tokio::time::advance(INTAKE_POLL).await;
    tokio::task::yield_now().await;

    assert_eq!(unfinished(&farm), [OperationId(5000), OperationId(6000)]);
    running.abort();
}

/// Asserts that the poll reports a stored epoch past the one this frontend serves, and
/// reports none when they are the same.
#[tokio::test(start_paused = true)]
async fn a_stored_epoch_past_the_served_one_is_reported() {
    let store = Racing::new();
    let farm = farm_over(&store);
    farm.intake();
    assert_eq!(farm.epoch_ahead(), None);

    let bumped = farm.bump_epoch().expect("a bump").expect("a store");
    farm.intake();
    farm.intake();

    assert_eq!(farm.epoch_ahead(), Some(bumped.epoch));
    assert_eq!(farm.lease_epoch(), bumped.previous);
}

/// Asserts that the lease of a node no frontend hears is requeued by a poll, only after the
/// wait, and runs on a daemon of the frontend that polls once it is taken in.
#[tokio::test(start_paused = true)]
async fn a_poll_requeues_the_lease_of_a_node_no_frontend_hears() {
    let store = Racing::new();
    let wait = Duration::from_secs(30);
    let farm = farm_over(&store).with_orphan_wait(wait);
    let other = durable_over(&store);
    queue_on(&other, 5000);
    grant(&other, 5000, 0, "node-z").expect("granted to a node of another frontend");

    farm.intake();
    assert_eq!(farm.lock().intake.orphans_seen(), 1);
    store.advance(wait);
    farm.intake();

    assert_eq!((rows(&store, "lease/"), rows(&store, "queue/")), (0, 1));
    let mut inbox = register(&farm, "node-a");
    farm.intake();
    farm.tick().await;
    assert_eq!(starts(&mut inbox), 1, "the requeued operation did not run");
}

/// Asserts that a farm whose intake is closed ([`Farm::close_intake`]) takes in no operation
/// another frontend queued, and that the store still holds the operation for the others.
#[tokio::test(start_paused = true)]
async fn a_farm_whose_intake_is_closed_takes_no_queued_operation_in() {
    let store = Racing::new();
    let farm = farm_over(&store);
    queue_on(&durable_over(&store), 5000);
    farm.close_intake();

    farm.intake();

    assert!(unfinished(&farm).is_empty(), "a closed farm took work in");
    assert_eq!(rows(&store, "queue/"), 1, "the store lost the operation");
}

/// Asserts that a farm whose intake is closed places nothing on its nodes (those it has and
/// one that registers later) and leaves the operation it holds queued, which another
/// frontend takes in and runs, and that it takes the result of the lease it holds, so that
/// an execution that is running finishes where it started. The other frontend's nodes are
/// placed work as before.
#[tokio::test(start_paused = true)]
async fn a_farm_whose_intake_is_closed_places_nothing_and_finishes_what_it_holds() {
    let store = Racing::new();
    let (a, b) = (farm_over(&store), farm_over(&store));
    let node = WorkerId::new("node-a");
    let mut inbox_a = register(&a, "node-a");
    let mut inbox_b = register(&b, "node-b");
    let running = caller(&a, 1);
    a.tick().await;
    assert_eq!(starts(&mut inbox_a), 1, "the first operation did not start");
    let views = a.lease_views(None);
    let [view] = &views.leases[..] else {
        panic!("one lease expected");
    };
    let lease = view.lease;
    a.close_intake();
    let mut late = register(&a, "node-late");
    let waiting = caller(&a, 2);
    a.tick().await;
    assert_eq!(starts(&mut inbox_a), 0, "a node was placed work");
    assert_eq!(starts(&mut late), 0, "a later node was placed work");
    assert_eq!(a.queued_operations(), 1);
    assert!(!matches!(*waiting.stage.borrow(), Stage::Done(_)));

    b.intake();
    b.tick().await;
    assert_eq!(starts(&mut inbox_b), 1, "the other frontend ran nothing");
    a.report(&node, invalid(lease)).await;
    assert_eq!(done_with(&running), "test");
}

/// Asserts that a farm with a daemon connected does not requeue the lease of that daemon
/// whatever the store says of its session, however short the wait.
#[tokio::test(start_paused = true)]
async fn a_connected_daemons_lease_is_never_swept() {
    let store = Racing::new();
    let farm = farm_over(&store).with_orphan_wait(Duration::ZERO);
    let _inbox = register(&farm, "node-a");
    submit(&farm, 1).expect("granted at once");
    remove(&store, "session/node-a");

    farm.intake();
    farm.intake();

    assert_eq!(rows(&store, "lease/"), 1);
    assert_eq!(farm.lock().intake.orphans_seen(), 0);
}
