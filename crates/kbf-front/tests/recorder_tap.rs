//! The recorder task's tap (`kbf_front::Tap`): what it hears, in what order, the id it
//! returns landing in the record, and the idle sweep. Each test names the defect it
//! catches.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_auth::AuthenticationMetadata;
use kbf_front::{CallMetadata, ExecuteCall, InvocationId as FrontId, Recorder, Tap, wall_now};
use kbf_invocation::{
    ActionRow, Call, CallKind, Ended, EndedKind, Event, Finished, InvocationId, LeaseEnd, Metadata,
    Outcome, Owner, RecordKey, WallTime,
};
use kbf_types::{Digest, DigestFunction, LeaseId, OperationId, WorkerId};

fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

fn key(id: &str) -> RecordKey {
    RecordKey::owner(InvocationId::new(id))
}

/// What the tap heard of an end: the record, its kind, its attempt and its row's instance.
type Heard = (RecordKey, EndedKind, Option<usize>, String);

/// A tap that notes what it hears, and mints the ids `[1; 12]`, `[2; 12]` and so on
/// for attempts (none for a hit), unless told to mint none.
#[derive(Debug, Default)]
struct Noted {
    ended: Mutex<Vec<Heard>>,
    idle: Mutex<Vec<RecordKey>>,
    minted: AtomicU8,
    mint_none: bool,
}

impl Tap for Noted {
    fn ended(&self, ended: &Ended, row: &ActionRow) -> Option<[u8; 12]> {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                ended.key.clone(),
                ended.kind,
                ended.attempt,
                row.instance.clone(),
            ));
        (ended.attempt.is_some() && !self.mint_none)
            .then(|| [self.minted.fetch_add(1, Ordering::SeqCst) + 1; 12])
    }

    fn idle(&self, key: &RecordKey) {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key.clone());
    }
}

fn queue(recorder: &Recorder, id: &str, action: u8, operation: u64) {
    let metadata = CallMetadata {
        invocation_id: FrontId::parse(id),
        ..CallMetadata::default()
    };
    let call = ExecuteCall::new(
        recorder,
        &metadata,
        &AuthenticationMetadata::default(),
        "main",
        d(action),
        d(99),
    )
    .expect("an id and a recorder on");
    for event in call.answered(
        format!("operations/{id}-{operation}"),
        OperationId(operation),
        false,
    ) {
        recorder.emit(event);
    }
}

fn granted(operation: u64, seq: u64) -> Event {
    Event::Granted {
        operation: OperationId(operation),
        lease: LeaseId::new(1, seq),
        worker: WorkerId::new("node-a"),
        at: wall_now(),
    }
}

fn finished_ok(operation: u64, seq: u64) -> Event {
    Event::Finished(Finished {
        operation: OperationId(operation),
        lease: Some((LeaseId::new(1, seq), LeaseEnd::Accepted)),
        outcome: Outcome::Ok { exit_code: 0 },
        result: None,
        at: wall_now(),
    })
}

/// Catches: a tap that is never called, called before the event is applied (the row
/// would not show the end yet), or called once for every event; an id the tap minted
/// that is not written into the attempt it was minted for (the row would not name the
/// run); and an id written on the wrong attempt.
#[tokio::test]
async fn the_tap_hears_each_settled_attempt_and_its_id_lands_in_the_record() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    let tap = Arc::new(Noted::default());
    tokio::spawn(recording.with_tap(tap.clone()).run());
    queue(&recorder, "build", 1, 7);
    recorder.emit(granted(7, 1));
    recorder.emit(Event::Requeued {
        operation: OperationId(7),
        lease: LeaseId::new(1, 1),
        reason: kbf_sched::RequeueReason::Silent,
        at: wall_now(),
    });
    recorder.emit(granted(7, 2));
    recorder.emit(finished_ok(7, 2));
    recorder.caught_up().await;

    let heard = tap.ended.lock().unwrap().clone();
    assert_eq!(
        heard,
        [
            (
                key("build"),
                EndedKind::Requeued,
                Some(0),
                "main".to_owned()
            ),
            (
                key("build"),
                EndedKind::Finished,
                Some(1),
                "main".to_owned()
            ),
        ]
    );
    let record = recorder.get(&key("build")).expect("a record");
    let ids: Vec<_> = record.rows()[0].attempts.iter().map(|a| a.run_id).collect();
    assert_eq!(ids, [Some([1; 12]), Some([2; 12])]);
}

/// Catches: an id written for a tap that minted none (a stale or zero id on the
/// attempt), and a hit given an attempt to put an id on.
#[tokio::test]
async fn a_tap_that_mints_no_id_leaves_the_attempt_without_one() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    let tap = Arc::new(Noted {
        mint_none: true,
        ..Noted::default()
    });
    tokio::spawn(recording.with_tap(tap.clone()).run());
    queue(&recorder, "build", 1, 7);
    recorder.emit(granted(7, 1));
    recorder.emit(finished_ok(7, 1));
    recorder.caught_up().await;
    assert_eq!(tap.ended.lock().unwrap().len(), 1);
    let record = recorder.get(&key("build")).expect("a record");
    assert_eq!(record.rows()[0].attempts[0].run_id, None);
}

/// Catches: an idle record never reported (history would hold its chunk open for ever),
/// one reported before it is idle, and one reported on every sweep rather than once.
/// The records' times are the wall clock's; the sweep's cadence is the paused clock's.
#[tokio::test(start_paused = true)]
async fn an_idle_record_is_reported_once_and_a_fresh_one_not_at_all() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    let tap = Arc::new(Noted::default());
    tokio::spawn(recording.with_tap(tap.clone()).run());
    let call = |id: &str, at: WallTime| {
        Event::Call(Box::new(Call {
            id: InvocationId::new(id),
            minted: false,
            owner: Owner::default(),
            instance: "main".to_owned(),
            at,
            metadata: Metadata::default(),
            kind: CallKind::Uploaded(1),
        }))
    };
    let now = wall_now().unix_micros();
    let old = WallTime::from_unix_micros(now - 11 * 60 * 1_000_000);
    recorder.emit(call("old", old));
    recorder.emit(call("fresh", WallTime::from_unix_micros(now)));
    recorder.caught_up().await;
    assert!(
        tap.idle.lock().unwrap().is_empty(),
        "no sweep has run before its first tick"
    );
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_secs(11)).await;
    }
    assert_eq!(*tap.idle.lock().unwrap(), [key("old")]);
}

/// Catches: a recorder task started without a tap panicking in its loop (the sweep
/// branch's future is built even when the branch is off), which would take down every
/// server run without the history flag. Without a tap, events apply as before.
#[tokio::test(start_paused = true)]
async fn a_recorder_without_a_tap_applies_events_and_does_not_panic() {
    let (recorder, recording) = Recorder::new(1 << 20, 64);
    let task = tokio::spawn(recording.run());
    queue(&recorder, "build", 1, 7);
    recorder.emit(granted(7, 1));
    recorder.emit(finished_ok(7, 1));
    // A dead task never catches up: bound the wait so the defect is a failure, not a hang.
    tokio::time::timeout(Duration::from_secs(60), recorder.caught_up())
        .await
        .expect("the recorder task applies its events");
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert!(!task.is_finished(), "the recorder task died");
    let record = recorder.get(&key("build")).expect("a record");
    assert_eq!(record.rows()[0].attempts.len(), 1);
}
