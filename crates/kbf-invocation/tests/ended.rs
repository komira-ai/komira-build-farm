//! What `Records::apply` hands out for a reader that keeps history: an `Ended` for each
//! attempt that settled and each cache hit that got its row, run ids that stay with the
//! attempt they were set on, and the idle sweep. Each test names the defect it catches.

mod support;

use kbf_invocation::{
    CallKind, Ended, EndedKind, Event, ExecuteAnswer, IDLE_AFTER, LeaseEnd, Outcome, Records, Run,
    WallTime,
};
use kbf_sched::RequeueReason;
use kbf_types::{Failure, LostWhy, OperationId};
use support::*;

fn ran(op: u64, seq: u64) -> Event {
    Event::Ran {
        operation: OperationId(op),
        lease: lease(seq),
        run: Box::new(Run::default()),
    }
}

/// Applies `events` in order and gives what each ended, one list per event.
fn ended(r: &mut Records, events: impl IntoIterator<Item = Event>) -> Vec<Vec<Ended>> {
    events.into_iter().map(|e| r.apply(e)).collect()
}

fn one(key_of: &str, row: usize, attempt: Option<usize>, kind: EndedKind, at: WallTime) -> Ended {
    Ended {
        key: key(key_of),
        row,
        attempt,
        kind,
        at,
    }
}

/// Catches: an attempt that never settles (a requeued one, or the last), one that
/// settles twice (the report after a finish, or the same end on every spread), or one
/// that settles before it ended (a granted or started attempt), and a kind that does not
/// say which end it was.
#[test]
fn each_attempt_settles_once_at_its_end() {
    let mut r = Records::new();
    let got = ended(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            granted(1, 1, "node-a", t(1)),
            started(1, 1, t(1)),
            requeued(1, 1, RequeueReason::Silent, t(2)),
            granted(1, 2, "node-b", t(3)),
            started(1, 2, t(3)),
            ran(1, 2),
            finished_ok(1, 2, 7, 0, t(4)),
            // The operation is gone: a late report settles nothing.
            ran(1, 2),
        ],
    );
    let none: Vec<Ended> = Vec::new();
    assert_eq!(
        got,
        [
            none.clone(),
            none.clone(),
            none.clone(),
            vec![one("a", 0, Some(0), EndedKind::Requeued, t(2))],
            none.clone(),
            none.clone(),
            none.clone(),
            vec![one("a", 0, Some(1), EndedKind::Finished, t(4))],
            none,
        ]
    );
}

/// Catches: a requeue for a reason the worker reports (a memory kill, a lost run, an
/// infra fault) settled before its report, so the record has no run block for the very
/// run that was killed; and the report that follows not settling it.
#[test]
fn a_requeue_the_worker_reports_settles_when_its_report_arrives() {
    for reason in [
        RequeueReason::OutOfMemory {
            booked: 1,
            raised: 2,
        },
        RequeueReason::NodeMemoryPressure,
        RequeueReason::Infra,
        RequeueReason::Lost(LostWhy::Fenced),
    ] {
        let mut r = Records::new();
        let got = ended(
            &mut r,
            [
                queued("a", t(0), 1, 1),
                granted(1, 1, "node-a", t(1)),
                requeued(1, 1, reason, t(2)),
                ran(1, 1),
            ],
        );
        assert!(got[2].is_empty(), "{reason:?}: waits for the report");
        assert_eq!(
            got[3],
            [one("a", 0, Some(0), EndedKind::Ran, t(2))],
            "{reason:?}"
        );
        assert!(
            r.get(&key("a")).unwrap().rows()[0].attempts[0]
                .run
                .is_some()
        );
    }
}

/// Catches: a joined twin's row left out (its invocation would have no run record), or
/// one row reported twice, or a twin that joined after the first lease ended getting an
/// end it never saw.
#[test]
fn every_row_that_waits_on_the_operation_settles() {
    let mut r = Records::new();
    let got = ended(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            granted(1, 1, "node-a", t(1)),
            requeued(1, 1, RequeueReason::Silent, t(2)),
            joined("b", t(3), 1, 1),
            granted(1, 2, "node-a", t(4)),
            finished_ok(1, 2, 7, 0, t(5)),
        ],
    );
    assert_eq!(got[2], [one("a", 0, Some(0), EndedKind::Requeued, t(2))]);
    let mut finished = got[5].clone();
    finished.sort_by(|x, y| x.key.cmp(&y.key));
    // The twin's row lists the attempts before it joined, so its attempt 1 is lease 2.
    assert_eq!(
        finished,
        [
            one("a", 0, Some(1), EndedKind::Finished, t(5)),
            one("b", 0, Some(1), EndedKind::Finished, t(5)),
        ]
    );
}

/// Catches: an operation refused with no lease reported as an attempt that ended, and
/// a failed lease's end not reported.
#[test]
fn a_refusal_has_no_attempt_and_a_failure_has_one() {
    let mut r = Records::new();
    let got = ended(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            finished(1, None, Outcome::Refused("no node".into()), t(1)),
            queued("a", t(2), 2, 2),
            granted(2, 1, "node-a", t(3)),
            finished(
                2,
                Some((lease(1), LeaseEnd::Failed(Failure::Infra, "x".into()))),
                Outcome::Failed {
                    code: 13,
                    message: "m".into(),
                },
                t(4),
            ),
        ],
    );
    assert!(got[1].is_empty());
    assert_eq!(got[4], [one("a", 1, Some(0), EndedKind::Finished, t(4))]);
}

/// Catches: a hit with no event, a second hit of the same action (no new row) reported
/// again, and an Execute answered from the action cache not counted as a hit.
#[test]
fn a_hit_is_reported_when_it_gets_its_row() {
    let mut r = Records::new();
    let hit = |at, action| {
        ev(call(
            "a",
            at,
            CallKind::GetActionResult {
                action: d(action),
                hit: Some(listing(5, 0)),
            },
        ))
    };
    let cached = ev(call(
        "a",
        t(3),
        CallKind::Execute {
            action: d(2),
            answer: ExecuteAnswer::Cached {
                name: "operations/cached/2".into(),
                result: Box::new(listing(6, 0)),
            },
        },
    ));
    let miss = ev(call(
        "a",
        t(4),
        CallKind::GetActionResult {
            action: d(9),
            hit: None,
        },
    ));
    let got = ended(&mut r, [hit(t(1), 1), hit(t(2), 1), cached, miss]);
    assert_eq!(got[0], [one("a", 0, None, EndedKind::Hit, t(1))]);
    assert!(got[1].is_empty());
    assert_eq!(got[2], [one("a", 1, None, EndedKind::Hit, t(3))]);
    assert!(got[3].is_empty());
}

/// Catches: a run id lost when the operation's attempts are copied into the row again
/// (the next lease's grant), set on the wrong lease or row, or accepted for a record,
/// row or lease that is not there.
#[test]
fn a_run_id_stays_with_its_attempt() {
    let mut r = Records::new();
    ended(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            granted(1, 1, "node-a", t(1)),
            requeued(1, 1, RequeueReason::Silent, t(2)),
        ],
    );
    let id = [7; 12];
    assert!(r.set_run_id(&key("a"), 0, lease(1), id));
    ended(&mut r, [granted(1, 2, "node-a", t(3)), started(1, 2, t(3))]);
    let attempts = &r.get(&key("a")).unwrap().rows()[0].attempts;
    assert_eq!(
        attempts.iter().map(|a| a.run_id).collect::<Vec<_>>(),
        [Some(id), None]
    );
    assert!(!r.set_run_id(&key("a"), 0, lease(9), id), "no such lease");
    assert!(!r.set_run_id(&key("a"), 5, lease(1), id), "no such row");
    assert!(!r.set_run_id(&key("z"), 0, lease(1), id), "no such record");
    assert_eq!(
        r.get(&key("a")).unwrap().rows()[0].attempts[1].run_id,
        None,
        "the other lease keeps none"
    );
}

/// Catches: a record reported idle before `IDLE_AFTER`, one with an operation in flight
/// reported, an idle record reported on every sweep, and one that woke and went quiet
/// again not reported again.
#[test]
fn an_idle_record_is_reported_once_per_quiet_spell() {
    let mut r = Records::new();
    ended(
        &mut r,
        [
            queued("busy", t(0), 1, 1),
            granted(1, 1, "node-a", t(0)),
            ev(call("quiet", t(0), CallKind::Uploaded(5))),
        ],
    );
    let quiet_for = |secs: u64| t(secs);
    let soon = quiet_for(IDLE_AFTER.as_secs() - 1);
    let later = quiet_for(IDLE_AFTER.as_secs() + 1);
    assert!(r.idle_since(soon).is_empty(), "not idle yet");
    assert_eq!(
        r.idle_since(later),
        [key("quiet")],
        "busy has an operation in flight"
    );
    assert!(r.idle_since(later).is_empty(), "once");
    // It wakes and goes quiet again.
    r.apply(ev(call(
        "quiet",
        t(IDLE_AFTER.as_secs() + 5),
        CallKind::Uploaded(1),
    )));
    assert!(r.idle_since(later).is_empty(), "awake");
    let again = quiet_for(2 * IDLE_AFTER.as_secs() + 10);
    assert_eq!(r.idle_since(again), [key("quiet")]);
    // The busy one finishes and goes quiet.
    r.apply(finished_ok(1, 1, 7, 0, t(2 * IDLE_AFTER.as_secs() + 11)));
    let end = quiet_for(4 * IDLE_AFTER.as_secs());
    assert_eq!(r.idle_since(end), [key("busy")]);
}

/// Catches: an end reported for a row that no longer shows the attempt. A later Execute
/// of the same action replaced the row; the first operation's events do not reach it, so
/// its attempt ends in no row (and the history of the build holds a record of the
/// attempt the row of the second Execute never listed).
#[test]
fn an_end_is_not_reported_for_a_row_a_later_execute_replaced() {
    let mut r = Records::new();
    let got = ended(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            granted(1, 1, "node-a", t(1)),
            // The same action again, as a new operation: the row now shows it.
            queued("a", t(2), 1, 2),
            requeued(1, 1, RequeueReason::Silent, t(3)),
            granted(2, 1, "node-b", t(4)),
            finished_ok(2, 1, 7, 0, t(5)),
        ],
    );
    assert!(got[3].is_empty(), "operation 1's row was replaced");
    assert_eq!(got[5], [one("a", 0, Some(0), EndedKind::Finished, t(5))]);
}

/// Catches: an end reported for an attempt the row does not hold because it did not fit
/// within the memory bound (an index past the row's attempts, or a record of an attempt
/// the page cannot show).
#[test]
fn an_attempt_the_bound_left_out_settles_nothing() {
    let mut sized = Records::new();
    sized.apply(queued("a", t(0), 1, 1));
    let mut r = Records::with_bound(kbf_invocation::ROW_CAP, sized.bytes());
    let got = ended(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            granted(1, 1, "node-a", t(1)),
            finished_ok(1, 1, 7, 0, t(2)),
        ],
    );
    assert!(
        r.get(&key("a")).unwrap().rows()[0].attempts.is_empty(),
        "the attempt did not fit"
    );
    assert!(got.iter().all(Vec::is_empty), "{got:?}");
}
