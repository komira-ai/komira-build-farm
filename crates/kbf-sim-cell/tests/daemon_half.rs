//! The daemon half of an all-in-one process in the cell (`FakeDaemon::in_process`):
//! it stores each output before it reports, and dies and starts again with the
//! server's process. Here, not in the crate's unit tests, so that the build the cell's
//! sweeps run is the one these inputs reach.

use kbf_sim::{Event, NodeId, NodeInput};
use kbf_sim_cell::{FakeDaemon, Msg, action, output_of};
use kbf_types::{FarmTime, LeaseId, OperationId, Resources, StateMachine as _};

fn input(at: u64, event: Event<Msg>) -> NodeInput<Msg> {
    NodeInput {
        now: FarmTime::from_millis(at),
        entropy: 1,
        event,
    }
}

fn message(at: u64, msg: Msg) -> NodeInput<Msg> {
    let from = NodeId::from("server");
    input(at, Event::Message { from, msg })
}

fn start(stream: u64, seq: u64) -> Msg {
    Msg::Start {
        stream,
        lease: LeaseId::new(1, seq),
        operation: OperationId(seq),
        action: action(seq),
    }
}

fn welcome(stream: u64) -> Msg {
    Msg::Welcome {
        stream,
        epoch: 7,
        interval_ms: 5_000,
    }
}

fn runs(d: &FakeDaemon) -> Vec<LeaseId> {
    d.unfenced_runs(FarmTime::from_millis(10))
        .map(|(l, _)| l)
        .collect()
}

/// Catches a daemon half that outlives its process: a run, a staged output or an
/// unacknowledged result kept across a death, an input taken while dead, a second
/// death counted twice, or a revived process that reconnects as the dead instance
/// or on one of its streams; and an in-process run reported before its output is
/// stored.
#[test]
fn the_daemon_half_dies_with_its_process_and_starts_again_as_a_new_instance() {
    let mut d =
        FakeDaemon::in_process(NodeId::from("server"), "daemon-0", Resources::new(1_000, 1));
    let lease = |seq| LeaseId::new(1, seq);
    d.apply(input(0, Event::Start));
    d.apply(message(1, welcome(1)));
    d.apply(message(2, start(1, 1)));
    d.apply(message(3, start(1, 2)));
    // A resent `Start` of a running lease runs nothing more.
    d.apply(message(4, start(1, 1)));
    assert_eq!(runs(&d), [lease(1), lease(2)]);
    d.take_outputs();
    // Both runs end; each output is uploaded, and only a stored one reports. (No
    // heartbeat was acknowledged, so the poll also opens stream 2.)
    d.apply(input(
        30_000,
        Event::Timer {
            tag: FakeDaemon::POLL,
        },
    ));
    let uploads = format!("{:?}", d.take_outputs());
    let first = output_of(&action(1));
    let upload = format!("Upload {{ digest: {first:?} }}");
    assert!(uploads.contains(&upload), "{uploads}");
    assert!(
        d.reported().is_empty(),
        "reported before its output was stored"
    );
    d.apply(message(30_000, welcome(2)));
    // Neither a staged lease, nor one whose result is unacknowledged, nor one
    // acknowledged, runs again; nor does any once contact is lost.
    d.apply(message(30_001, start(2, 2)));
    d.apply(message(30_002, Msg::Uploaded { digest: first }));
    assert_eq!(d.reported().iter().copied().collect::<Vec<_>>(), [lease(1)]);
    d.apply(message(30_003, start(2, 1)));
    let acked = Msg::ResultAck {
        lease: lease(1),
        accepted: true,
    };
    d.apply(message(30_004, acked));
    d.apply(message(30_005, start(2, 1)));
    assert!(runs(&d).is_empty(), "a finished lease ran again");
    d.apply(message(80_000, start(2, 9)));
    assert!(runs(&d).is_empty(), "a Start taken without contact");
    d.die();
    d.die();
    assert_eq!(d.lost(), 1, "the one staged output, counted once");
    d.apply(message(80_001, start(2, 3)));
    assert!(runs(&d).is_empty(), "a dead process took a Start");
    d.take_outputs();
    d.apply(input(
        81_000,
        Event::Timer {
            tag: FakeDaemon::POLL,
        },
    ));
    let quiet = format!("{:?}", d.take_outputs());
    assert!(!quiet.contains("Send"), "a dead process sent: {quiet}");
    d.revive();
    d.revive();
    d.apply(input(
        82_000,
        Event::Timer {
            tag: FakeDaemon::POLL,
        },
    ));
    // Later than every stream of the dead process.
    let sent = format!("{:?}", d.take_outputs());
    let hello = r#"Hello { stream: 3, instance: "daemon-0-2""#;
    assert!(sent.contains(hello), "{sent}");
    d.apply(message(82_001, welcome(2)));
    d.apply(message(82_002, start(2, 4)));
    assert!(
        runs(&d).is_empty(),
        "a message of the dead process's stream was taken"
    );
}
