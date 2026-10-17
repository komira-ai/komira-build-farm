//! The tap's mapping from a row's attempt to a run record, and what it offers the
//! writer. Each test names the defect it catches.

use std::sync::Arc;

use kbf_history::records::{HitHow, MemoryKill, RunOutcome};
use kbf_history::{Event, EventQueue, InvRef, Minter, QueueLimits};
use kbf_invocation::{
    ActionRow, ActionShape, Attempt, AttemptEnd, Ended, EndedKind, How, Listing, NumaBytes,
    Outcome, Output, ReapiTimes, RecordKey, Run, RunUsage, WallTime,
};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, DigestFunction, Failure, LeaseId, LostWhy, WorkerId};

use super::{HistoryTap, OPEN_CAP};
use kbf_front::Tap as _;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn at(us: u64) -> WallTime {
    WallTime::from_unix_micros(us)
}

fn attempt(end: Option<AttemptEnd>) -> Attempt {
    Attempt {
        lease: LeaseId::new(3, 9),
        worker: WorkerId::new("node-a"),
        queued: at(10_000),
        granted: at(20_000),
        started: Some(at(30_000)),
        ended: Some(at(40_000)),
        end,
        run: None,
        run_id: None,
    }
}

fn row(attempts: Vec<Attempt>, outcome: Option<Outcome>) -> ActionRow {
    ActionRow {
        action: digest(1),
        instance: "main".to_owned(),
        target: None,
        mnemonic: None,
        configuration: None,
        action_id: None,
        how: How::Ran,
        operation: Some("operations/x".to_owned()),
        attempts,
        outcome,
        listing: None,
        outputs_truncated: false,
        command: None,
        shape: None,
    }
}

fn key(id: &str) -> RecordKey {
    RecordKey::owner(kbf_invocation::InvocationId::new(id))
}

fn ended(id: &str, kind: EndedKind, attempt: Option<usize>) -> Ended {
    Ended {
        key: key(id),
        row: 0,
        attempt,
        kind,
        at: at(50_000_000),
    }
}

struct Rig {
    tap: HistoryTap,
    queue: Arc<EventQueue>,
}

fn rig(events: usize) -> Rig {
    let queue = Arc::new(EventQueue::new(QueueLimits {
        events,
        bytes: 1 << 30,
    }));
    Rig {
        tap: HistoryTap::new(Arc::clone(&queue), Arc::new(Minter::new(0))),
        queue,
    }
}

/// What `attempt` gives, ended as `kind`: the run record the tap offered.
fn record_of(
    row: &ActionRow,
    attempt: usize,
    kind: EndedKind,
) -> (kbf_history::records::RunRecord, Option<[u8; 12]>) {
    let rig = rig(16);
    let id = rig.tap.ended(&ended("b", kind, Some(attempt)), row);
    let mut events = rig.queue.drain();
    assert_eq!(events.len(), 1, "one record for one attempt");
    let Some(Event::Run {
        inv,
        record,
        failed,
        stream_opened_ms,
    }) = events.pop()
    else {
        panic!("a run event");
    };
    assert_eq!(inv, InvRef::named("b"));
    assert_eq!(failed, None, "no tails here");
    assert_eq!(stream_opened_ms, 50_000);
    (*record, id)
}

/// Catches: a field of the row dropped or put in another's place on its way to the
/// record: the action, result, command, logs, shape, configuration, test flag, node,
/// lease, every time, the cgroup block (its NUMA bytes by node) and the outputs; and the
/// id the tap returns not the record's.
#[test]
fn a_finished_attempt_keeps_what_the_row_and_the_attempt_hold() {
    let mut a = attempt(Some(AttemptEnd::Accepted));
    a.run = Some(Box::new(Run {
        times: ReapiTimes {
            queued: Some(at(11_000)),
            worker_start: Some(at(21_000)),
            input_fetch_start: Some(at(22_000)),
            input_fetch_completed: Some(at(23_000)),
            execution_start: Some(at(24_000)),
            execution_completed: Some(at(25_000)),
            output_upload_start: Some(at(26_000)),
            output_upload_completed: Some(at(27_000)),
            worker_completed: Some(at(28_000)),
        },
        usage: Some(RunUsage {
            wall_micros: 1,
            cpu_usage_micros: Some(2),
            cpu_user_micros: Some(3),
            cpu_system_micros: Some(4),
            cpu_periods: Some(5),
            cpu_throttled_periods: Some(6),
            cpu_throttled_micros: Some(7),
            memory_peak_bytes: Some(8),
            numa: vec![
                NumaBytes { node: 1, bytes: 90 },
                NumaBytes { node: 0, bytes: 80 },
                // Past the nodes a record keeps.
                NumaBytes {
                    node: 64,
                    bytes: 70,
                },
            ],
        }),
        booked_memory_bytes: Some(1 << 30),
        cap_bytes: None,
    }));
    let mut r = row(
        vec![
            attempt(Some(AttemptEnd::Requeued(RequeueReason::Silent))),
            a,
        ],
        Some(Outcome::Ok { exit_code: 0 }),
    );
    r.command = Some(digest(2));
    r.listing = Some(Listing {
        result: Some(digest(3)),
        exit_code: Some(0),
        outputs: vec![
            Output {
                path: "out/a.o".to_owned(),
                digest: digest(4),
                executable: false,
                directory: false,
            },
            Output {
                path: "out/dir".to_owned(),
                digest: digest(5),
                executable: false,
                directory: true,
            },
        ],
        stdout: Some(digest(6)),
        stderr: None,
        usage: None,
    });
    r.shape = Some(Box::new(ActionShape {
        key: 0x0123_4567_89ab_cdef,
        cfg: Some("7a3c1e9f0b2d4c68".to_owned()),
        target: None,
        test: true,
    }));
    let (rec, id) = record_of(&r, 1, EndedKind::Finished);
    assert_eq!(id, Some(rec.run_id().to_bytes()));
    assert_eq!(rec.instance, "main");
    let hash =
        |d: &Option<kbf_history::records::WireDigest>| d.as_ref().map(|d| (d.hash[0], d.size));
    assert_eq!(hash(&rec.action), Some((1, 1)));
    assert_eq!(hash(&rec.command), Some((2, 2)));
    assert_eq!(hash(&rec.result), Some((3, 3)));
    assert_eq!(hash(&rec.stdout), Some((6, 6)));
    assert_eq!(hash(&rec.stderr), None);
    assert_eq!(hash(&rec.input_root), None, "the row has no input root");
    assert_eq!(rec.shape, 0x0123_4567_89ab_cdef);
    assert_eq!(rec.cfg, "7a3c1e9f0b2d4c68");
    assert!(rec.test);
    assert_eq!(rec.node, "node-a");
    assert_eq!((rec.lease_epoch, rec.lease_seq), (3, 9));
    assert_eq!((rec.outcome(), rec.exit_code), (RunOutcome::Ok, 0));
    assert_eq!(rec.memory_kill(), MemoryKill::None);
    assert_eq!(rec.booked_memory_bytes, 1 << 30);
    assert_eq!(
        (rec.memory_used_bytes, rec.memory_limit_bytes),
        (0, 0),
        "no memory kill"
    );
    let t = rec.timestamps.expect("times");
    assert_eq!(
        [
            t.queued_us,
            t.granted_us,
            t.worker_start_us,
            t.input_fetch_start_us,
            t.input_fetch_completed_us,
            t.execution_start_us,
            t.execution_completed_us,
            t.output_upload_start_us,
            t.output_upload_completed_us,
            t.worker_completed_us,
        ],
        [
            11_000, 20_000, 21_000, 22_000, 23_000, 24_000, 25_000, 26_000, 27_000, 28_000
        ]
    );
    let c = rec.cgroup.expect("a cgroup block");
    assert_eq!(
        [
            c.cpu_usage_us,
            c.cpu_user_us,
            c.cpu_system_us,
            c.periods,
            c.throttled_periods,
            c.throttled_us,
            c.memory_peak_bytes
        ],
        [2, 3, 4, 5, 6, 7, 8]
    );
    assert_eq!(c.numa_bytes, [80, 90], "by node, the node past 63 left out");
    let outputs: Vec<_> = rec
        .outputs
        .iter()
        .map(|o| {
            (
                o.path.as_str(),
                o.digest.as_ref().map(|d| d.hash[0]),
                o.is_dir,
            )
        })
        .collect();
    assert_eq!(
        outputs,
        [("out/a.o", Some(4), false), ("out/dir", Some(5), true)]
    );
}

/// Catches: the first attempt of a requeued action given the result, logs and outputs
/// of the attempt that ended it; the time of first submission not kept across attempts
/// when the run reports none of its own; a missing shape, run block or cgroup block not
/// read as none (a `-` configuration, zero times, no cgroup).
#[test]
fn a_given_up_attempt_has_no_result_and_the_first_queue_time() {
    let mut second = attempt(Some(AttemptEnd::Accepted));
    second.queued = at(35_000);
    let mut r = row(
        vec![
            attempt(Some(AttemptEnd::Requeued(RequeueReason::Silent))),
            second,
        ],
        Some(Outcome::Ok { exit_code: 0 }),
    );
    r.listing = Some(Listing {
        result: Some(digest(3)),
        exit_code: Some(0),
        outputs: vec![Output {
            path: "o".to_owned(),
            digest: digest(4),
            executable: false,
            directory: false,
        }],
        stdout: Some(digest(6)),
        stderr: Some(digest(7)),
        usage: None,
    });
    let (first, _) = record_of(&r, 0, EndedKind::Requeued);
    assert!(first.result.is_none() && first.stdout.is_none() && first.stderr.is_none());
    assert!(first.outputs.is_empty());
    assert_eq!(first.outcome(), RunOutcome::Lost);
    assert_eq!(first.cfg, "-");
    assert!(!first.test);
    assert_eq!(first.shape, 0);
    assert!(first.cgroup.is_none());
    let t = first.timestamps.expect("times");
    assert_eq!(
        (t.queued_us, t.granted_us, t.worker_start_us),
        (10_000, 20_000, 0)
    );
    let (last, _) = record_of(&r, 1, EndedKind::Finished);
    assert!(last.result.is_some() && last.stdout.is_some() && last.stderr.is_some());
    assert_eq!(last.outputs.len(), 1);
    assert_eq!(
        last.timestamps.expect("times").queued_us,
        10_000,
        "the first submission, kept across the requeue"
    );
}

fn outcome_of(end: AttemptEnd, answered: Option<Outcome>) -> (RunOutcome, i32) {
    let r = row(vec![attempt(Some(end))], answered);
    let (rec, _) = record_of(&r, 0, EndedKind::Finished);
    (rec.outcome(), rec.exit_code)
}

/// Catches: an accepted result with a non-zero exit read as OK, a timeout, a memory
/// kill, an infrastructure failure or a lost run told as another, a refusal or failure
/// answer read as OK, and an attempt with no end read as one that ended well.
#[test]
fn every_end_has_its_outcome() {
    use RunOutcome::{Failed, Infra, Lost, MemoryKilled, TimedOut, Unspecified};
    let accepted = |answer| outcome_of(AttemptEnd::Accepted, answer);
    assert_eq!(
        accepted(Some(Outcome::Ok { exit_code: 0 })),
        (RunOutcome::Ok, 0)
    );
    assert_eq!(accepted(Some(Outcome::Ok { exit_code: 2 })), (Failed, 2));
    assert_eq!(accepted(Some(Outcome::TimedOut)), (TimedOut, 0));
    let failure = Outcome::Failed {
        code: 13,
        message: "m".to_owned(),
    };
    assert_eq!(accepted(Some(failure)), (Failed, 0));
    assert_eq!(
        accepted(Some(Outcome::Refused("r".to_owned()))),
        (Failed, 0)
    );
    assert_eq!(accepted(None), (Unspecified, 0));
    for (failure, want) in [
        (Failure::Timeout, TimedOut),
        (Failure::Infra, Infra),
        (Failure::Invalid, Failed),
        (Failure::MissingInput, Failed),
        (Failure::OutOfMemory, MemoryKilled),
        (Failure::NodeMemoryPressure, MemoryKilled),
        (Failure::Lost(LostWhy::Fenced), Lost),
    ] {
        let end = AttemptEnd::Failed(failure, "why".to_owned());
        assert_eq!(outcome_of(end, None), (want, 0), "{failure:?}");
    }
    for (reason, want) in [
        (RequeueReason::Silent, Lost),
        (RequeueReason::Replaced, Lost),
        (RequeueReason::Reconnected, Lost),
        (RequeueReason::NotStarted, Lost),
        (RequeueReason::SessionSwitch, Lost),
        (RequeueReason::Restarted, Lost),
        (RequeueReason::Lost(LostWhy::DaemonRestarted), Lost),
        (RequeueReason::Infra, Infra),
        (RequeueReason::NodeMemoryPressure, MemoryKilled),
        (
            RequeueReason::OutOfMemory {
                booked: 1,
                raised: 2,
            },
            MemoryKilled,
        ),
    ] {
        let end = AttemptEnd::Requeued(reason);
        assert_eq!(outcome_of(end, None), (want, 0), "{reason:?}");
    }
    let held = row(vec![attempt(None)], None);
    let (rec, _) = record_of(&held, 0, EndedKind::Finished);
    assert_eq!(rec.outcome(), Unspecified);
}

/// Catches: a memory kill not told apart (own limit from node pressure), the booking
/// the next run gets lost, the booking and the cap not kept for an own-limit kill, and
/// the memory the run used and the limit it was killed at set for a kill that was not
/// the action's own.
#[test]
fn a_memory_kill_keeps_its_booking_cap_and_limit() {
    let usage = |peak| RunUsage {
        memory_peak_bytes: Some(peak),
        ..RunUsage::default()
    };
    let ran = |booked, cap, peak| {
        Some(Box::new(Run {
            usage: Some(usage(peak)),
            booked_memory_bytes: booked,
            cap_bytes: cap,
            ..Run::default()
        }))
    };
    // Given up for its own limit, then the report arrives: the run block has the booking.
    let mut own = attempt(Some(AttemptEnd::Requeued(RequeueReason::OutOfMemory {
        booked: 4,
        raised: 8,
    })));
    own.run = ran(Some(4), Some(64), 5);
    let (rec, _) = record_of(&row(vec![own], None), 0, EndedKind::Ran);
    assert_eq!(rec.memory_kill(), MemoryKill::OwnLimit);
    assert_eq!(rec.outcome(), RunOutcome::MemoryKilled);
    assert_eq!(
        (
            rec.booked_memory_bytes,
            rec.raised_memory_bytes,
            rec.cap_bytes
        ),
        (4, 8, 64)
    );
    assert_eq!((rec.memory_used_bytes, rec.memory_limit_bytes), (5, 4));
    assert!(rec.reason.contains("memory limit"), "{}", rec.reason);
    // The booking is read off the end when the run block names none.
    let bare = attempt(Some(AttemptEnd::Requeued(RequeueReason::OutOfMemory {
        booked: 4,
        raised: 8,
    })));
    let (rec, _) = record_of(&row(vec![bare], None), 0, EndedKind::Ran);
    assert_eq!((rec.booked_memory_bytes, rec.raised_memory_bytes), (4, 8));
    assert_eq!((rec.memory_used_bytes, rec.memory_limit_bytes), (0, 4));
    // The node's kill, and an operation that finished with the action's own.
    let mut node = attempt(Some(AttemptEnd::Requeued(
        RequeueReason::NodeMemoryPressure,
    )));
    node.run = ran(Some(4), None, 6);
    let (rec, _) = record_of(&row(vec![node], None), 0, EndedKind::Ran);
    assert_eq!(rec.memory_kill(), MemoryKill::NodePressure);
    assert_eq!((rec.memory_used_bytes, rec.memory_limit_bytes), (6, 0));
    let mut last = attempt(Some(AttemptEnd::Failed(
        Failure::OutOfMemory,
        "the reason the daemon gave".to_owned(),
    )));
    last.run = ran(Some(32), Some(32), 33);
    let (rec, _) = record_of(&row(vec![last], None), 0, EndedKind::Finished);
    assert_eq!(rec.memory_kill(), MemoryKill::OwnLimit);
    assert_eq!(rec.reason, "the reason the daemon gave");
    assert_eq!((rec.memory_used_bytes, rec.memory_limit_bytes), (33, 32));
}

/// Catches: NUMA bytes that do not follow node numbers, or a huge node number sizing a
/// list.
#[test]
fn numa_bytes_are_indexed_by_node_and_bounded() {
    use super::numa;
    assert_eq!(numa(&[]), Vec::<u64>::new());
    assert_eq!(
        numa(&[NumaBytes { node: 2, bytes: 7 }]),
        [0, 0, 7],
        "the nodes before it are 0"
    );
    assert_eq!(
        numa(&[NumaBytes {
            node: u32::MAX,
            bytes: 7
        }]),
        Vec::<u64>::new()
    );
}

/// Catches: a hit with no record, a hit read as a run, a hit counted as served another
/// way (an Execute answered from the action cache is another kind), and a hit that gets
/// an id.
#[test]
fn a_hit_is_offered_as_a_hit_with_no_id() {
    let rig = rig(16);
    for (how, want) in [
        (How::CacheHit, HitHow::CacheHit),
        (How::CacheHitExecute, HitHow::CacheHitExecute),
        (How::Joined, HitHow::Unspecified),
    ] {
        let mut r = row(Vec::new(), None);
        r.how = how;
        assert_eq!(rig.tap.ended(&ended("b", EndedKind::Hit, None), &r), None);
        let Some(Event::Hit { inv, record }) = rig.queue.drain().pop() else {
            panic!("a hit event");
        };
        assert_eq!(inv, InvRef::named("b"));
        assert_eq!(record.how(), want);
        assert_eq!(record.time_ms, 50_000);
        assert_eq!(record.instance, "main");
        assert_eq!(record.action.expect("an action").hash[0], 1);
    }
}

/// Catches: an end with no attempt (or an attempt the row does not list) that panics
/// or mints an id for nothing; and an id kept for a record the full queue dropped.
#[test]
fn no_id_is_kept_for_a_record_that_was_not_offered() {
    let rig = rig(1);
    let r = row(vec![attempt(Some(AttemptEnd::Accepted))], None);
    assert_eq!(
        rig.tap.ended(&ended("b", EndedKind::Finished, Some(5)), &r),
        None
    );
    assert_eq!(
        rig.tap.ended(&ended("b", EndedKind::Finished, None), &r),
        None
    );
    assert!(rig.queue.is_empty());
    let first = rig.tap.ended(&ended("b", EndedKind::Finished, Some(0)), &r);
    assert!(first.is_some());
    let second = rig.tap.ended(&ended("b", EndedKind::Finished, Some(0)), &r);
    assert_eq!(second, None, "the queue holds one event");
    assert_eq!(rig.queue.dropped(), 1);
    // A hit the full queue drops is not remembered as open.
    assert_eq!(rig.tap.ended(&ended("c", EndedKind::Hit, None), &r), None);
    assert_eq!(rig.queue.dropped(), 2);
    rig.queue.drain();
    rig.tap.idle(&key("c"));
    assert!(
        rig.queue.is_empty(),
        "nothing of c was offered, so nothing closes"
    );
}

/// Catches: a close for an invocation the writer was never told of (a header read for
/// nothing), no close for one it was told of (a chunk held open to the age cut), a close
/// sent twice, and the stop's flush missing an open invocation.
#[test]
fn an_idle_record_is_closed_once_and_a_stop_closes_what_is_open() {
    let rig = rig(16);
    let r = row(vec![attempt(Some(AttemptEnd::Accepted))], None);
    for id in ["b", "c"] {
        rig.tap.ended(&ended(id, EndedKind::Finished, Some(0)), &r);
    }
    rig.queue.drain();
    rig.tap.idle(&key("never"));
    assert!(rig.queue.is_empty());
    rig.tap.idle(&key("b"));
    let closes = |events: Vec<Event>| -> Vec<InvRef> {
        events
            .into_iter()
            .map(|e| match e {
                Event::Close { inv } => inv,
                other => panic!("a close, not {other:?}"),
            })
            .collect()
    };
    assert_eq!(closes(rig.queue.drain()), [InvRef::named("b")]);
    rig.tap.idle(&key("b"));
    assert!(rig.queue.is_empty(), "closed once");
    rig.tap.close_all();
    assert_eq!(closes(rig.queue.drain()), [InvRef::named("c")]);
    rig.tap.close_all();
    assert!(rig.queue.is_empty());
}

/// Catches: a list of open invocations that grows with every invocation a long-lived
/// server sees (records evicted for the memory bound never report idle).
#[test]
fn the_open_list_is_bounded() {
    let rig = rig(3 * OPEN_CAP);
    let r = row(Vec::new(), None);
    for n in 0..=OPEN_CAP {
        rig.tap
            .ended(&ended(&format!("b{n:05}"), EndedKind::Hit, None), &r);
    }
    // A name already listed is not a new one: nothing is dropped for it.
    rig.tap
        .ended(&ended(&format!("b{OPEN_CAP:05}"), EndedKind::Hit, None), &r);
    rig.queue.drain();
    rig.tap.close_all();
    let closed: Vec<Option<String>> = rig
        .queue
        .drain()
        .into_iter()
        .map(|e| match e {
            Event::Close { inv } => inv.record_id,
            other => panic!("a close, not {other:?}"),
        })
        .collect();
    assert_eq!(closed.len(), OPEN_CAP);
    assert!(
        !closed.contains(&Some("b00000".to_owned())),
        "the first went to make room"
    );
    assert!(closed.contains(&Some("b00001".to_owned())));
    assert!(closed.contains(&Some(format!("b{OPEN_CAP:05}"))));
}
