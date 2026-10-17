//! What an operation's memory kills leave in the state store, and what a farm that takes
//! the store over (after a handover, or after the old process died) books and holds
//! because of them. Two farms run over one store: the old one holds writer generation
//! 1, the successor takes generation 2.

use kbf_proto::worker::MemoryKill;
use kbf_proto::worker::server_message::Message;
use kbf_server::farm::NodeAction;

use super::*;

/// A node with room for the whole ladder (64 GiB), so a kill at 4 GiB is raised to 16.
fn register_big(farm: &TestFarm) -> (StreamId, Inbox) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 64 * GIB), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = farm.register(
        &node(),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    );
    (stream, inbox)
}

/// The leases of the `Start`s waiting in `inbox`, each with the memory it books.
fn booked(inbox: &mut Inbox) -> Vec<(LeaseId, u64)> {
    let mut starts = Vec::new();
    while let Ok(message) = inbox.try_recv() {
        if let Some(Message::Start(start)) = message.expect("no error").message {
            let lease = start.lease_id.expect("a lease id");
            starts.push((LeaseId::new(lease.term, lease.seq), start.memory_bytes));
        }
    }
    starts
}

/// A result of `lease`: killed for passing its own memory limit.
fn out_of_memory(lease: LeaseId) -> worker::Result {
    worker::Result {
        status: Some(rpc::Status {
            code: Code::ResourceExhausted as i32,
            message: "oom".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: MemoryKill::OwnLimit as i32,
        ..invalid(lease)
    }
}

/// An action booked at 4 GiB, killed for memory on a cordoned node: the farm raises its
/// booking to 16 GiB and holds it queued, with one memory run. Returns the farm and what
/// it holds of the kill.
async fn killed_once(
    disk: &Disk,
    farm: &TestFarm,
) -> (LeaseId, BTreeMap<OperationId, Vec<MemoryRun>>) {
    let (_, mut inbox) = register_big(farm);
    let request = Request {
        resources: Resources::new(1_000, 4 * GIB),
        ..request(1)
    };
    submit(farm, request).expect("submitted");
    let [(lease, memory)] = booked(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert_eq!(memory, 4 * GIB);
    // Cordoned, the node is not offered the requeued operation: it stays queued at its
    // raised booking, which is what a handover finds.
    farm.place(&node(), NodeAction::Cordon).expect("cordoned");
    let ack = farm.report(&node(), out_of_memory(lease)).await;
    assert!(ack.expect("an acknowledgement").accepted);
    assert!(booked(&mut inbox).is_empty(), "a cordoned node was granted");
    assert_eq!((disk.rows("op/"), disk.rows("queue/")), (1, 1));
    let runs = farm.memory_runs();
    let [(_, kills)] = runs.iter().collect::<Vec<_>>()[..] else {
        panic!("one operation with a kill expected: {runs:?}");
    };
    assert_eq!(kills.len(), 1);
    assert_eq!(kills[0].booked, 4 * GIB);
    (lease, runs)
}

/// The successor's node comes back to placement: the operation is granted at the booking
/// the successor holds for it.
fn granted_to(successor: &TestFarm) -> u64 {
    let (_, mut inbox) = register_big(successor);
    successor
        .place(&node(), NodeAction::Uncordon)
        .expect("uncordoned");
    let [(_, memory)] = booked(&mut inbox)[..] else {
        panic!("the successor granted nothing, or more than one lease");
    };
    memory
}

/// Catches: a booking and a memory ladder that live only in the old process. An action
/// killed at 4 GiB and raised to 16 GiB is granted again by the successor at 16 GiB, not
/// at 4 GiB (one more kill, and the ladder restarts at every handover), and the
/// successor holds the same memory runs for it, so the failure text it ends with names
/// every run. Mutant: the fate row not written.
#[tokio::test(start_paused = true)]
async fn a_raised_booking_and_its_kills_survive_a_handover() {
    let disk = Disk::new();
    let old = disk.generation_farm();
    let (_, runs) = killed_once(&disk, &old).await;

    old.fence();
    let successor = disk.generation_farm();
    assert_eq!(granted_to(&successor), 16 * GIB, "the ladder restarted");
    assert_eq!(successor.memory_runs(), runs);
}

/// Catches: the same, when the old process dies without a handover: nothing but the rows
/// it wrote before it died tells the successor.
#[tokio::test(start_paused = true)]
async fn a_raised_booking_and_its_kills_survive_a_crash() {
    let mut disk = Disk::new();
    let old = disk.generation_farm();
    let (_, runs) = killed_once(&disk, &old).await;

    disk.crash(old);
    let successor = disk.generation_farm();
    assert_eq!(granted_to(&successor), 16 * GIB, "the ladder restarted");
    assert_eq!(successor.memory_runs(), runs);
}

/// Catches: a fate row left behind when the operation ends, which would grow with every
/// kill and name an operation that no longer exists. Present while the operation is
/// unfinished after a kill; gone when it finishes.
#[tokio::test(start_paused = true)]
async fn the_fate_row_goes_when_the_operation_finishes() {
    let disk = Disk::new();
    let farm = disk.generation_farm();
    let (_, _) = killed_once(&disk, &farm).await;
    assert_eq!(disk.rows("fate/"), 1, "a kill left no fate row");

    let (_, mut inbox) = register_big(&farm);
    farm.place(&node(), NodeAction::Uncordon)
        .expect("uncordoned");
    let [(lease, _)] = booked(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert!(accepted(&farm, lease).await);
    assert_eq!(disk.rows("op/"), 0);
    assert_eq!(disk.rows("fate/"), 0, "the fate row outlived its operation");
}
