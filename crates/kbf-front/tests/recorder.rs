//! The invocation recorder's channel (docs/design/invocations.md, sections 3.4 and
//! 3.5; section 9, PR 3): sending never waits, a full channel drops the event and the
//! record it was for says `complete: false`, and what a row copies from an
//! ActionResult. Each test names the defect it catches.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use kbf_auth::AuthenticationMetadata;
use kbf_front::{CallMetadata, ExecuteCall, InvocationId as FrontId, Recorder, Text, listing};
use kbf_invocation::{
    CallKind, Event, Finished, How, InvocationId, Outcome, Output, Owner, RecordKey, Usage,
};
use kbf_proto::reapi::{
    ActionResult, Digest as ProtoDigest, ExecutedActionMetadata, OutputDirectory, OutputFile,
};
use kbf_proto::worker::ResourceUsage;
use kbf_types::{Digest, DigestFunction, OperationId};
use prost::Message;
use prost_types::Any;

/// Longer than any healthy send takes, and short enough to fail a hang quickly.
const NO_WAIT: Duration = Duration::from_secs(10);

fn metadata(id: &str) -> CallMetadata {
    CallMetadata {
        invocation_id: FrontId::parse(id),
        ..CallMetadata::default()
    }
}

fn key(id: &str) -> RecordKey {
    RecordKey::owner(InvocationId::new(id))
}

fn downloaded(recorder: &Recorder, id: &str) {
    let caller = AuthenticationMetadata::default();
    recorder.call(&metadata(id), &caller, "main", &|| CallKind::Downloaded(1));
}

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn proto(d: &Digest) -> ProtoDigest {
    kbf_front::digest_to_proto(d)
}

/// Catches a send that waits for room (plant `futures::executor::block_on(send)` in
/// place of `try_send` in `Recorder::emit`: the sending thread never returns and the
/// timeout goes red), a drop not counted, and a dropped event whose record stays
/// `complete` (or marks a record it was not for).
#[tokio::test]
async fn a_full_channel_drops_the_event_and_marks_its_record_without_waiting() {
    let (recorder, recording) = Recorder::new(1 << 20, 2);
    // Two events fill the channel: the task is not running yet.
    downloaded(&recorder, "kept");
    downloaded(&recorder, "dropped");
    let sender = recorder.clone();
    let (sent, done) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..100 {
            downloaded(&sender, "dropped");
        }
        let _ = sent.send(());
    });
    done.recv_timeout(NO_WAIT)
        .expect("100 sends on a full channel return at once");
    assert_eq!(recorder.dropped(), 100, "kbf_invocation_events_dropped");

    tokio::spawn(recording.run());
    recorder.caught_up().await;
    let dropped = recorder
        .get(&key("dropped"))
        .expect("its first call was kept");
    assert!(!dropped.complete(), "a dropped call marks its record");
    assert_eq!(
        dropped.counts().bytes_downloaded,
        1,
        "only the kept call counted"
    );
    let kept = recorder.get(&key("kept")).expect("kept");
    assert!(kept.complete(), "no event of `kept` was dropped");
}

/// Catches a drop whose record does not exist yet left unmarked (the record its
/// caller's next call makes must start incomplete), and a dropped lease event that
/// marks no record (plant: the `Event::Granted` arm of `Shared::drop_event` leaves
/// no mark): `op`, whose call and Command were kept, stays complete.
#[tokio::test]
async fn drops_mark_the_record_the_event_was_for() {
    let (recorder, recording) = Recorder::new(1 << 20, 2);
    let caller = AuthenticationMetadata::default();
    let queued = ExecuteCall::new(&recorder, &metadata("op"), &caller, "main", d(1), d(2))
        .expect("an id and a recorder on")
        .answered("operations/1-0".to_owned(), OperationId(7), false);
    for event in queued {
        recorder.emit(event);
    }
    // The channel is full: these are dropped.
    recorder.emit(Event::Granted {
        operation: OperationId(7),
        lease: kbf_types::LeaseId::new(1, 1),
        worker: kbf_types::WorkerId::new("node-a"),
        at: kbf_front::wall_now(),
    });
    downloaded(&recorder, "unborn");
    // A dropped Command and a dropped finish mark the same records again.
    recorder.emit(Event::Command {
        id: InvocationId::new("unborn"),
        owner: Owner::default(),
        action: d(3),
        command: d(4),
        shape: None,
    });
    recorder.emit(Event::Finished(Finished {
        operation: OperationId(8),
        lease: None,
        outcome: Outcome::Refused("no worker".to_owned()),
        result: None,
        at: kbf_front::wall_now(),
    }));
    assert_eq!(recorder.dropped(), 4);
    tokio::spawn(recording.run());
    recorder.caught_up().await;
    let op = recorder.get(&key("op")).expect("op");
    assert_eq!(op.rows()[0].how, How::Ran);
    assert_eq!(
        op.rows()[0].command,
        Some(d(2)),
        "the Command event was kept"
    );
    assert!(!op.complete(), "its grant was dropped");
    downloaded(&recorder, "unborn");
    recorder.caught_up().await;
    let unborn = recorder.get(&key("unborn")).expect("unborn");
    assert!(!unborn.complete(), "its first call was dropped");
}

/// Catches more drop marks kept than the cap (memory without end) and the overflow
/// lost: past the cap, every record is marked incomplete, even one that lost nothing.
#[tokio::test]
async fn past_the_mark_cap_every_record_is_marked() {
    let (recorder, recording) = Recorder::new(1 << 20, 1);
    downloaded(&recorder, "lost-nothing");
    for n in 0..1100 {
        downloaded(&recorder, &format!("dropped-{n}"));
    }
    tokio::spawn(recording.run());
    recorder.caught_up().await;
    let record = recorder.get(&key("lost-nothing")).expect("held");
    assert!(!record.complete(), "the overflow marks every record");
}

/// Catches an off recorder that records (or allocates a task), and a send to a
/// stopped task that panics or counts a drop.
#[tokio::test]
async fn an_off_recorder_and_a_stopped_one_record_nothing() {
    let off = Recorder::off();
    assert!(!off.is_on());
    downloaded(&off, "x");
    off.emit(Event::Started {
        operation: OperationId(1),
        lease: kbf_types::LeaseId::new(1, 1),
        at: kbf_front::wall_now(),
    });
    off.caught_up().await;
    assert_eq!(
        (off.get(&key("x")), off.dropped(), off.bytes()),
        (None, 0, (0, 0))
    );
    let caller = AuthenticationMetadata::default();
    assert!(ExecuteCall::new(&off, &metadata("x"), &caller, "main", d(1), d(2)).is_none());

    let (stopped, recording) = Recorder::new(1 << 20, 4);
    drop(recording);
    downloaded(&stopped, "x");
    assert_eq!(stopped.dropped(), 0, "a stopped task is not a full channel");
    assert_eq!(stopped.bytes(), (0, 1 << 20));
}

/// Catches a call without an invocation id recorded (section 2.3: only Execute gets a
/// minted id, with the link it is for), the caller's `public` metadata not taken as
/// the owner (a squatter would share the owner's record), and the metadata layer's
/// texts lost on the way to the row, or their truncation mark.
#[tokio::test]
async fn a_call_is_recorded_under_its_id_and_caller() {
    let (recorder, recording) = Recorder::new(1 << 20, 16);
    tokio::spawn(recording.run());
    let caller = AuthenticationMetadata::new(Some(serde_json::json!({"user": "a"})), None);
    let no_id = CallMetadata::default();
    recorder.call(&no_id, &caller, "main", &|| CallKind::Downloaded(1));
    recorder.call(&metadata("b"), &caller, "main", &|| CallKind::Downloaded(1));
    // The layer's texts reach the row, a truncated one marked so.
    let mut sent = metadata("b");
    sent.target_id = Text {
        value: "//pkg:cut".to_owned(),
        truncated: true,
    };
    sent.tool_name = Text {
        value: "buck2".to_owned(),
        truncated: false,
    };
    let execute = ExecuteCall::new(&recorder, &sent, &caller, "main", d(1), d(2))
        .expect("an id and a recorder on");
    for event in execute.answered("operations/1-0".to_owned(), OperationId(1), true) {
        recorder.emit(event);
    }
    recorder.caught_up().await;
    let record = recorder.get(&key("b")).expect("b");
    assert_eq!(record.owner(), &Owner(r#"{"user":"a"}"#.to_owned()));
    assert_eq!(
        record.counts().bytes_downloaded,
        1,
        "the id-less call is not"
    );
    assert_eq!(record.tool(), (Some("buck2"), None));
    let row = &record.rows()[0];
    assert_eq!(row.how, How::Joined);
    let target = row.target.as_ref().expect("the target");
    assert_eq!(
        (target.value.as_str(), target.truncated),
        ("//pkg:cut", true)
    );
    assert_eq!(row.mnemonic, None, "an empty text is absent");
    assert!(ExecuteCall::new(&recorder, &no_id, &caller, "main", d(1), d(2)).is_none());
}

/// Catches a row listing that loses an output, mixes up a file and a directory, drops
/// the executable bit, the logs or the usage, or keeps an output whose digest does
/// not parse; and a timed-out run's partial listing given the partial result's exit
/// code (an action that was stopped has none).
#[test]
fn a_listing_copies_outputs_logs_and_usage() {
    let usage = ResourceUsage {
        cpu_user_micros: 1,
        cpu_system_micros: 2,
        peak_memory_bytes: 3,
        wall_micros: 4,
        cgroup: None,
    };
    let result = ActionResult {
        output_files: vec![
            OutputFile {
                path: "bin/tool".to_owned(),
                digest: Some(proto(&d(1))),
                is_executable: true,
                ..Default::default()
            },
            OutputFile {
                path: "bad".to_owned(),
                digest: Some(ProtoDigest {
                    hash: "not hex".to_owned(),
                    size_bytes: 1,
                }),
                ..Default::default()
            },
        ],
        output_directories: vec![OutputDirectory {
            path: "out".to_owned(),
            tree_digest: Some(proto(&d(2))),
            ..Default::default()
        }],
        exit_code: 3,
        stdout_digest: Some(proto(&d(4))),
        execution_metadata: Some(ExecutedActionMetadata {
            auxiliary_metadata: vec![
                Any {
                    type_url: "type.googleapis.com/other.Message".to_owned(),
                    value: vec![1, 2],
                },
                Any {
                    type_url: "type.googleapis.com/kbf.worker.v1.ResourceUsage".to_owned(),
                    value: usage.encode_to_vec(),
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    };
    let l = listing(Some(d(9)), &result);
    assert_eq!((l.result, l.exit_code), (Some(d(9)), Some(3)));
    let partial = listing(None, &result);
    assert_eq!(
        (partial.result, partial.exit_code),
        (None, None),
        "a partial result's action was stopped: no exit code, not its 3"
    );
    assert_eq!(
        l.outputs,
        [
            Output {
                path: "bin/tool".to_owned(),
                digest: d(1),
                executable: true,
                directory: false,
            },
            Output {
                path: "out".to_owned(),
                digest: d(2),
                executable: false,
                directory: true,
            },
        ]
    );
    assert_eq!((l.stdout, l.stderr), (Some(d(4)), None));
    assert_eq!(
        l.usage,
        Some(Usage {
            cpu_user_micros: 1,
            cpu_system_micros: 2,
            peak_memory_bytes: 3,
            wall_micros: 4,
        })
    );
    let bare = listing(Some(d(9)), &ActionResult::default());
    assert_eq!((bare.outputs.len(), bare.usage), (0, None));
}

/// Catches work done for a call that is not recorded (the verifier's probe: an
/// id-less GetActionResult hit cloned its listing): plant `kind()` before the id
/// check in `Recorder::call`, and the count goes to 1. With the recorder off, or on
/// and the call without an id, the kind is never built and nothing is sent.
#[tokio::test]
async fn a_call_that_is_not_recorded_builds_nothing() {
    let built = AtomicU32::new(0);
    let kind = || {
        built.fetch_add(1, Ordering::Relaxed);
        CallKind::Downloaded(1)
    };
    let caller = AuthenticationMetadata::default();
    let (on, _recording) = Recorder::new(1 << 20, 16);
    on.call(&CallMetadata::default(), &caller, "main", &kind);
    Recorder::off().call(&metadata("x"), &caller, "main", &kind);
    assert_eq!(built.load(Ordering::Relaxed), 0, "no kind built");
    assert_eq!(on.sent(), 0, "nothing sent");
    on.call(&metadata("x"), &caller, "main", &kind);
    assert_eq!((built.load(Ordering::Relaxed), on.sent()), (1, 1));
}

/// A finish whose result lists `n` outputs with 60-byte paths.
fn finished_listing(n: usize) -> Event {
    let mut l = listing(Some(d(9)), &ActionResult::default());
    l.outputs = (0..n)
        .map(|k| Output {
            path: format!("buck-out/v2/gen/root/0123456789abcdef/pkg/__lib__/{k:08}.o"),
            digest: d(1),
            executable: false,
            directory: false,
        })
        .collect();
    Event::Finished(Finished {
        operation: OperationId(1),
        lease: None,
        outcome: Outcome::Ok { exit_code: 0 },
        result: Some(l),
        at: kbf_front::wall_now(),
    })
}

/// Catches a channel bounded in events only (plant: no byte check in
/// `Recorder::emit`): events waiting to be applied, together, hold no more than the
/// memory bound, so a burst of finishes with wide listings is dropped, counted and
/// marked like a full channel, and once the task has applied what was queued there
/// is room again.
#[tokio::test]
async fn events_past_the_channel_bytes_are_dropped() {
    let (recorder, recording) = Recorder::new(200_000, 16);
    // Over 100 kB each: the first fits, the second does not.
    recorder.emit(finished_listing(800));
    recorder.emit(finished_listing(800));
    assert_eq!((recorder.sent(), recorder.dropped()), (1, 1));
    let task = tokio::spawn(recording.run());
    tokio::time::timeout(NO_WAIT, recorder.caught_up())
        .await
        .expect("the task applies the queued event");
    recorder.emit(finished_listing(800));
    assert_eq!((recorder.sent(), recorder.dropped()), (2, 1), "room again");
    task.abort();
}

/// Catches the bytes of an event dropped on a full channel kept in the channel's
/// byte count (plant: no `fetch_sub` on `TrySendError::Full` in `Recorder::emit`):
/// with one event's place, small events dropped for want of a place would fill the
/// byte count, and once the task has applied the one queued, a wide finish that
/// fits the empty channel would be dropped for bytes.
#[tokio::test]
async fn an_event_dropped_on_a_full_channel_gives_its_bytes_back() {
    let wide = || finished_listing(800);
    let (recorder, recording) = Recorder::new(2 * wide().bytes(), 1);
    for _ in 0..1000 {
        recorder.emit(finished_listing(1));
    }
    assert_eq!((recorder.sent(), recorder.dropped()), (1, 999), "one place");
    let task = tokio::spawn(recording.run());
    tokio::time::timeout(NO_WAIT, recorder.caught_up())
        .await
        .expect("the task applies the queued event");
    recorder.emit(wide());
    assert_eq!(
        (recorder.sent(), recorder.dropped()),
        (2, 999),
        "the channel is empty"
    );
    task.abort();
}

/// Catches an event listing sent uncut (plant `outputs.collect()` without `take`): a
/// result with 1,500 output files and 10 directories lists one output past the row's
/// 1,000, so the row still says `outputs_truncated`, files first.
#[test]
fn a_listing_keeps_one_output_past_the_row_limit() {
    let file = |k: usize| OutputFile {
        path: format!("f{k}"),
        digest: Some(proto(&d(1))),
        ..Default::default()
    };
    let result = ActionResult {
        output_files: (0..1500).map(file).collect(),
        output_directories: (0..10)
            .map(|k| OutputDirectory {
                path: format!("d{k}"),
                tree_digest: Some(proto(&d(2))),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let l = listing(Some(d(9)), &result);
    assert_eq!(l.outputs.len(), kbf_invocation::OUTPUTS_PER_ROW + 1);
    assert_eq!(l.outputs.last().map(|o| o.path.as_str()), Some("f1000"));
}
