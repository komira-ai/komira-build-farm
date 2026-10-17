//! Records built from events: how each action was answered and how it ended, its
//! attempts, joins, the row cap, squatted ids and the listing a row keeps
//! (docs/design/invocations.md, section 9, PR 2). Each test names the defect it catches.

mod support;

use kbf_invocation::{
    AttemptEnd, CallKind, Event, ExecuteAnswer, How, IDLE_AFTER, LeaseEnd, Listing, Metadata,
    OUTPUTS_PER_ROW, Outcome, Output, REASON_CAP, ROW_CAP, RecordKey, Records, State, TEXT_CAP,
    Text, WallTime,
};
use kbf_sched::RequeueReason;
use kbf_types::{Failure, LostWhy, OperationId, WorkerId};
use support::*;

fn after(at: WallTime, d: std::time::Duration) -> WallTime {
    WallTime::from_unix_micros(at.unix_micros() + u64::try_from(d.as_micros()).expect("fits"))
}

fn records(events: impl IntoIterator<Item = Event>) -> Records {
    let mut r = Records::new();
    for e in events {
        r.apply(e);
    }
    r
}

/// Catches a `how` recorded as another (a joined action shown as run, a cached Execute
/// shown as a GetActionResult hit) and a counter that counts the wrong answer.
#[test]
fn every_how_is_recorded() {
    let r = records([
        ev(call(
            "b",
            t(0),
            CallKind::GetActionResult {
                action: d(1),
                hit: Some(listing(10, 0)),
            },
        )),
        ev(call(
            "b",
            t(1),
            CallKind::Execute {
                action: d(2),
                answer: ExecuteAnswer::Cached {
                    name: "operations/cached/2".to_owned(),
                    result: Box::new(listing(20, 0)),
                },
            },
        )),
        queued("other", t(2), 3, 3),
        joined("b", t(3), 3, 3),
        queued("b", t(4), 4, 4),
        ev(call(
            "b",
            t(5),
            CallKind::GetActionResult {
                action: d(5),
                hit: None,
            },
        )),
    ]);
    let inv = r.get(&key("b")).expect("record b");
    let hows: Vec<(u8, How)> = inv
        .rows()
        .iter()
        .map(|row| (row.action.hash[0], row.how))
        .collect();
    assert_eq!(
        hows,
        [
            (1, How::CacheHit),
            (2, How::CacheHitExecute),
            (3, How::Joined),
            (4, How::Ran)
        ]
    );
    let names: Vec<Option<String>> = inv.rows().iter().map(|r| r.operation.clone()).collect();
    assert_eq!(
        names,
        [
            None,
            Some("operations/cached/2".to_owned()),
            Some(name("b", 3)),
            Some(name("b", 4))
        ]
    );
    // A hit and a cached Execute are answered at once: OK, with the cached result.
    assert_eq!(inv.rows()[0].outcome, Some(Outcome::Ok { exit_code: 0 }));
    assert_eq!(inv.rows()[1].outcome, Some(Outcome::Ok { exit_code: 0 }));
    assert_eq!(
        inv.rows()[1].listing.as_ref().and_then(|l| l.result),
        Some(d(20))
    );
    assert_eq!(inv.rows()[3].outcome, None, "op-4 is still in flight");
    let c = inv.counts();
    assert_eq!(
        (c.execute, c.cache_hit_execute, c.joined, c.ran),
        (3, 1, 1, 1)
    );
    assert_eq!((c.ac_hit, c.ac_miss, c.failed), (1, 1, 0));
}

/// Catches an outcome stored as another, a status message or refusal reason lost, and
/// the `failed` counter missing a non-zero exit, a timeout or a refusal, or counting an
/// exit 0.
#[test]
fn every_outcome_is_recorded() {
    let mut events = Vec::new();
    for (i, op) in [0, 1, 2, 3, 4].into_iter().enumerate() {
        events.push(queued("b", t(0), 10 + i as u8, op));
    }
    events.push(finished_ok(0, 9, 50, 0, t(1)));
    events.push(finished_ok(1, 9, 51, 3, t(1)));
    events.push(finished(
        2,
        None,
        Outcome::Failed {
            code: 9,
            message: "missing input".to_owned(),
        },
        t(1),
    ));
    events.push(finished(3, None, Outcome::TimedOut, t(1)));
    events.push(finished(
        4,
        None,
        Outcome::Refused("no node runs it".to_owned()),
        t(1),
    ));
    let r = records(events);
    let inv = r.get(&key("b")).expect("record b");
    let outcomes: Vec<Option<Outcome>> = inv.rows().iter().map(|r| r.outcome.clone()).collect();
    assert_eq!(
        outcomes,
        [
            Some(Outcome::Ok { exit_code: 0 }),
            Some(Outcome::Ok { exit_code: 3 }),
            Some(Outcome::Failed {
                code: 9,
                message: "missing input".to_owned()
            }),
            Some(Outcome::TimedOut),
            Some(Outcome::Refused("no node runs it".to_owned())),
        ]
    );
    assert_eq!(inv.counts().failed, 4);
    assert_eq!(
        inv.rows()[1].listing.as_ref().map(|l| l.exit_code),
        Some(Some(3))
    );
}

/// Catches attempts out of grant order, a requeue that does not end its lease or names
/// the wrong reason, the second lease's queued time taken from the submit instead of
/// the requeue, and a start or end time lost.
#[test]
fn a_requeued_then_finished_action_has_two_attempts_in_order() {
    let r = records([
        queued("b", t(0), 1, 1),
        granted(1, 1, "node-a", t(1)),
        started(1, 1, t(2)),
        requeued(1, 1, RequeueReason::Silent, t(30)),
        granted(1, 2, "node-b", t(31)),
        started(1, 2, t(32)),
        finished_ok(1, 2, 50, 0, t(40)),
    ]);
    let row = &r.get(&key("b")).expect("record b").rows()[0];
    let a = &row.attempts;
    assert_eq!(a.len(), 2);
    assert_eq!((a[0].lease, a[1].lease), (lease(1), lease(2)));
    assert_eq!(
        (&a[0].worker, &a[1].worker),
        (&WorkerId::new("node-a"), &WorkerId::new("node-b"))
    );
    assert_eq!(
        (a[0].queued, a[0].granted, a[0].started, a[0].ended),
        (t(0), t(1), Some(t(2)), Some(t(30)))
    );
    assert_eq!(a[0].end, Some(AttemptEnd::Requeued(RequeueReason::Silent)));
    assert_eq!(
        (a[1].queued, a[1].granted, a[1].started, a[1].ended),
        (t(30), t(31), Some(t(32)), Some(t(40)))
    );
    assert_eq!(a[1].end, Some(AttemptEnd::Accepted));
    assert_eq!(row.outcome, Some(Outcome::Ok { exit_code: 0 }));
}

/// Catches a joined action that starts with no attempts (it would show only the leases
/// granted after it joined), one that stops receiving its twin's later attempts or
/// outcome, and a join counted as no Execute.
#[test]
fn a_joined_action_copies_its_twins_attempts() {
    let mut r = records([
        queued("first", t(0), 1, 1),
        granted(1, 1, "node-a", t(1)),
        requeued(1, 1, RequeueReason::NotStarted, t(20)),
        joined("second", t(21), 1, 1),
    ]);
    let rows = |r: &Records, k: &str| r.get(&key(k)).expect("record").rows()[0].clone();
    // At the join, before any later event: the twin's attempt so far.
    assert_eq!(rows(&r, "second").attempts, rows(&r, "first").attempts);
    assert_eq!(rows(&r, "second").attempts.len(), 1);
    r.apply(granted(1, 2, "node-b", t(22)));
    r.apply(finished_ok(1, 2, 50, 0, t(30)));
    let (first, second) = (rows(&r, "first"), rows(&r, "second"));
    assert_eq!(second.how, How::Joined);
    assert_eq!(second.operation, Some(name("second", 1)));
    assert_eq!(second.attempts.len(), 2, "{:?}", second.attempts);
    assert_eq!(second.attempts, first.attempts);
    assert_eq!(second.attempts[0].lease, lease(1));
    assert_eq!(second.outcome, first.outcome);
    assert_eq!(second.listing, first.listing);
    // The twin's lease events are the second record's events too.
    assert_eq!(r.get(&key("second")).map(|i| i.last_seen()), Some(t(30)));
}

/// Catches a record that rows past its cap, one that stops counting the actions it
/// does not row, a capped action whose finish is lost (its record would stay open for
/// ever), and a default cap other than the 1,000,000 of section 3.3.
#[test]
fn the_row_cap_counts_actions_it_does_not_row() {
    let mut r = Records::with_row_cap(2);
    for (i, op) in [1, 2, 3].into_iter().enumerate() {
        r.apply(queued("b", t(0), 1 + i as u8, op));
    }
    r.apply(ev(call(
        "b",
        t(0),
        CallKind::GetActionResult {
            action: d(9),
            hit: Some(listing(90, 0)),
        },
    )));
    r.apply(ev(call(
        "b",
        t(0),
        CallKind::Execute {
            action: d(8),
            answer: ExecuteAnswer::Cached {
                name: "operations/cached/8".to_owned(),
                result: Box::new(listing(80, 0)),
            },
        },
    )));
    let inv = r.get(&key("b")).expect("record b");
    assert_eq!(inv.rows().len(), 2);
    assert!(inv.rows_capped());
    assert_eq!((inv.counts().execute, inv.counts().ac_hit), (4, 1));
    assert_eq!(inv.counts().cache_hit_execute, 1);
    assert_eq!(inv.state(after(t(0), IDLE_AFTER)), State::Open);
    for op in [1, 2, 3] {
        r.apply(finished(op, None, Outcome::TimedOut, t(1)));
    }
    let inv = r.get(&key("b")).expect("record b");
    assert_eq!(
        inv.counts().failed,
        3,
        "the unrowed action's answer is counted"
    );
    assert_eq!(inv.state(after(t(1), IDLE_AFTER)), State::Idle);

    assert_eq!(ROW_CAP, 1_000_000);
    assert_eq!(Records::new(), Records::with_row_cap(ROW_CAP));
    assert_eq!(Records::default(), Records::new());
}

/// Catches a squatter's calls landing in the owner's record (any caller could then
/// write into another build's page), a collision numbered from 1 instead of 2, the
/// owner's record not listing its collisions, and the owner's later calls routed to a
/// collision record.
#[test]
fn a_squatted_id_goes_to_its_own_record_from_tilde_2() {
    let by = |who: &str, at, action| {
        let mut c = call(
            "build",
            at,
            CallKind::Execute {
                action: d(action),
                answer: ExecuteAnswer::Queued {
                    name: format!("operations/{who}-{action}"),
                    operation: OperationId(u64::from(action)),
                },
            },
        );
        c.owner = owner(who);
        ev(c)
    };
    let r = records([
        by(r#"{"user":"a"}"#, t(0), 1),
        by(r#"{"user":"b"}"#, t(1), 2),
        by(r#"{"user":"c"}"#, t(2), 3),
        by(r#"{"user":"a"}"#, t(3), 4),
        by(r#"{"user":"b"}"#, t(4), 5),
    ]);
    let tilde2 = RecordKey::collision(id("build"), 2);
    let tilde3 = RecordKey::collision(id("build"), 3);
    assert_eq!(tilde2.to_string(), "build~2");
    assert_eq!(tilde3.to_string(), "build~3");
    assert_eq!(key("build").to_string(), "build");
    assert_eq!(tilde2.id(), &id("build"));

    let actions = |k: &RecordKey| -> Vec<u8> {
        r.get(k)
            .expect("record")
            .rows()
            .iter()
            .map(|row| row.action.hash[0])
            .collect()
    };
    assert_eq!(actions(&key("build")), [1, 4]);
    assert_eq!(actions(&tilde2), [2, 5]);
    assert_eq!(actions(&tilde3), [3]);
    let owners = r.get(&key("build")).expect("owner's record");
    assert_eq!(owners.owner(), &owner(r#"{"user":"a"}"#));
    assert_eq!(owners.collisions(), [tilde2.clone(), tilde3]);
    assert_eq!(r.get(&tilde2).map(|i| i.collisions().len()), Some(0));
    assert_eq!(r.get(&tilde2).map(|i| i.key()), Some(&tilde2));
}

fn many_outputs(n: usize) -> Listing {
    let mut l = listing(10, 0);
    l.outputs = (0..n)
        .map(|i| Output {
            path: format!("buck-out/v2/gen/root/{i}/lib.a"),
            digest: d(u8::try_from(i % 200).expect("fits")),
            executable: i % 2 == 1,
            directory: i % 3 == 2,
        })
        .collect();
    l
}

/// Catches a row that drops its result's outputs, their digests or executable and
/// directory bits, or the stdout and stderr digests: the page could not list them once
/// the ActionResult blob is collected (section 3.2). Run, hit and cached-Execute rows
/// each copy the listing.
#[test]
fn the_row_keeps_output_paths_digests_and_the_log_digests() {
    let mut finish = finished_ok(1, 1, 10, 0, t(2));
    if let Event::Finished(f) = &mut finish {
        f.result = Some(many_outputs(3));
    }
    let r = records([
        queued("b", t(0), 1, 1),
        granted(1, 1, "node-a", t(1)),
        finish,
        ev(call(
            "b",
            t(3),
            CallKind::GetActionResult {
                action: d(2),
                hit: Some(many_outputs(3)),
            },
        )),
        ev(call(
            "b",
            t(4),
            CallKind::Execute {
                action: d(3),
                answer: ExecuteAnswer::Cached {
                    name: "operations/cached/3".to_owned(),
                    result: Box::new(many_outputs(3)),
                },
            },
        )),
    ]);
    let inv = r.get(&key("b")).expect("record b");
    assert_eq!(inv.rows().len(), 3);
    for row in inv.rows() {
        let l = row.listing.as_ref().expect("the row copies the listing");
        assert_eq!(l, &many_outputs(3), "{:?}", row.how);
        assert_eq!(l.outputs[1].path, "buck-out/v2/gen/root/1/lib.a");
        assert_eq!(
            (
                l.outputs[1].digest,
                l.outputs[1].executable,
                l.outputs[2].directory
            ),
            (d(1), true, true)
        );
        assert_eq!((l.stdout, l.stderr), (Some(d(12)), Some(d(13))));
        assert!(!row.outputs_truncated);
    }
}

/// Catches a row that keeps more outputs than section 3.2 allows (a huge result would
/// blow the memory bound) or fewer, and a cut listing not marked truncated.
#[test]
fn a_row_keeps_the_first_thousand_outputs_and_says_so() {
    let mut finish = finished_ok(1, 1, 10, 0, t(2));
    if let Event::Finished(f) = &mut finish {
        f.result = Some(many_outputs(OUTPUTS_PER_ROW + 1));
    }
    let mut cut = finished_ok(2, 1, 10, 0, t(2));
    if let Event::Finished(f) = &mut cut {
        f.result = Some(many_outputs(OUTPUTS_PER_ROW));
    }
    let r = records([
        queued("b", t(0), 1, 1),
        queued("b", t(0), 2, 2),
        finish,
        cut,
    ]);
    let rows = r.get(&key("b")).expect("record b").rows();
    assert_eq!(OUTPUTS_PER_ROW, 1_000);
    let kept = &rows[0].listing.as_ref().expect("listing").outputs;
    assert_eq!(kept.len(), OUTPUTS_PER_ROW);
    assert_eq!(kept[..], many_outputs(OUTPUTS_PER_ROW).outputs[..]);
    assert!(rows[0].outputs_truncated);
    assert!(!rows[1].outputs_truncated, "exactly 1,000 is not truncated");
}

/// Catches a metadata text kept past 512 bytes, cut inside a character (a panic or
/// invalid UTF-8), not marked truncated, or an empty one stored as present; and a
/// reason or status message kept past 1 KiB.
#[test]
fn texts_and_reasons_are_capped() {
    // 511 ASCII bytes then a 3-byte character: byte 512 is inside it.
    let long = format!("{}\u{20ac}tail", "t".repeat(TEXT_CAP - 1));
    let mut c = call(
        "b",
        t(0),
        CallKind::Execute {
            action: d(1),
            answer: ExecuteAnswer::Queued {
                name: name("b", 1),
                operation: OperationId(1),
            },
        },
    );
    c.metadata = Metadata {
        target: Some(Text::sent(long)),
        mnemonic: Some(Text::sent("CxxCompile")),
        configuration: Some(Text::sent(String::new())),
        action_id: Some(Text::sent("x".repeat(TEXT_CAP))),
        ..Metadata::default()
    };
    let reason = "r".repeat(REASON_CAP + 10);
    let r = records([
        ev(c),
        queued("b", t(0), 2, 2),
        queued("b", t(0), 3, 3),
        granted(1, 1, "node-a", t(1)),
        finished(
            1,
            Some((lease(1), LeaseEnd::Failed(Failure::Infra, reason.clone()))),
            Outcome::Failed {
                code: 14,
                message: reason.clone(),
            },
            t(2),
        ),
        finished(2, None, Outcome::Refused(reason.clone()), t(2)),
        finished(
            3,
            None,
            Outcome::Failed {
                code: 13,
                message: "short".to_owned(),
            },
            t(2),
        ),
    ]);
    let rows = r.get(&key("b")).expect("record b").rows();
    let row = &rows[0];
    let target = row.target.as_ref().expect("target");
    assert_eq!(target.value, "t".repeat(TEXT_CAP - 1));
    assert!(target.truncated);
    let mnemonic = row.mnemonic.as_ref().expect("mnemonic");
    assert_eq!(
        (mnemonic.value.as_str(), mnemonic.truncated),
        ("CxxCompile", false)
    );
    assert_eq!(row.configuration, None, "an empty text is absent");
    let action_id = row.action_id.as_ref().expect("action id");
    assert_eq!(
        (action_id.value.len(), action_id.truncated),
        (TEXT_CAP, false)
    );

    let kept = "r".repeat(REASON_CAP);
    assert_eq!(
        row.attempts[0].end,
        Some(AttemptEnd::Failed(Failure::Infra, kept.clone()))
    );
    assert_eq!(
        row.outcome,
        Some(Outcome::Failed {
            code: 14,
            message: kept.clone()
        })
    );
    assert_eq!(rows[1].outcome, Some(Outcome::Refused(kept)));
    assert_eq!(
        rows[2].outcome,
        Some(Outcome::Failed {
            code: 13,
            message: "short".to_owned()
        })
    );
}

/// Catches header fields taken from the wrong call (the tool of a later call, an empty
/// correlated id kept over a later real one), an instance set that drops or repeats a
/// name, and a `last_seen` that ignores the farm's events for the record's operations.
#[test]
fn the_header_takes_the_first_values_and_the_last_event() {
    let with = |at, m: Metadata, instance: &str| {
        let mut c = call("b", at, CallKind::Uploaded(100));
        c.metadata = m;
        c.instance = instance.to_owned();
        ev(c)
    };
    let tool = |name: &str, v: &str| Metadata {
        tool: Some(name.to_owned()),
        tool_version: Some(v.to_owned()),
        ..Metadata::default()
    };
    let r = records([
        with(t(0), Metadata::default(), "main"),
        with(
            t(1),
            Metadata {
                correlated_invocations_id: Some(String::new()),
                ..tool("buck2", "0.1.0")
            },
            "other",
        ),
        with(
            t(2),
            Metadata {
                correlated_invocations_id: Some("corr-1".to_owned()),
                ..tool("bazel", "8.0.0")
            },
            "main",
        ),
        with(
            t(3),
            Metadata {
                correlated_invocations_id: Some("corr-2".to_owned()),
                ..Metadata::default()
            },
            "main",
        ),
        ev(call("b", t(4), CallKind::Downloaded(7))),
        ev(call(
            "b",
            t(4),
            CallKind::FindMissingBlobs {
                asked: 10,
                missing: 3,
            },
        )),
        queued("b", t(5), 1, 1),
        granted(1, 1, "node-a", t(6)),
        started(1, 1, t(7)),
        requeued(1, 1, RequeueReason::Restarted, t(8)),
        granted(1, 2, "node-a", t(9)),
        finished_ok(1, 2, 20, 0, t(10)),
    ]);
    let inv = r.get(&key("b")).expect("record b");
    assert_eq!(inv.tool(), (Some("buck2"), Some("0.1.0")));
    assert_eq!(inv.correlated_invocations_id(), Some("corr-1"));
    let instances: Vec<&str> = inv.instances().iter().map(String::as_str).collect();
    assert_eq!(instances, ["main", "other"]);
    assert_eq!((inv.first_seen(), inv.last_seen()), (t(0), t(10)));
    let c = inv.counts();
    assert_eq!((c.bytes_uploaded, c.bytes_downloaded), (400, 7));
    assert_eq!((c.blobs_asked, c.blobs_missing), (10, 3));
    assert!(!inv.minted());
    assert!(inv.complete());
}

/// Catches each lease event's touch of `last_seen` lost on its own (an action that
/// only runs would let its record go idle under it).
#[test]
fn each_lease_event_moves_last_seen() {
    let mut r = records([queued("b", t(0), 1, 1)]);
    let steps = [
        granted(1, 1, "node-a", t(1)),
        started(1, 1, t(2)),
        requeued(1, 1, RequeueReason::Silent, t(3)),
        finished(1, None, Outcome::TimedOut, t(4)),
    ];
    for (i, e) in steps.into_iter().enumerate() {
        r.apply(e);
        let last = r.get(&key("b")).map(|inv| inv.last_seen());
        assert_eq!(last, Some(t(i as u64 + 1)));
    }
}

/// Catches an invocation called idle while one of its operations is in flight (a long
/// action's build would be evicted), or before 10 minutes without an event, or never.
#[test]
fn idle_needs_no_operation_in_flight_and_ten_quiet_minutes() {
    assert_eq!(IDLE_AFTER, std::time::Duration::from_secs(600));
    let mut r = records([queued("b", t(0), 1, 1)]);
    let state = |r: &Records, at| r.get(&key("b")).expect("record b").state(at);
    assert_eq!(state(&r, after(t(0), IDLE_AFTER * 6)), State::Open);
    r.apply(finished_ok(1, 1, 10, 0, t(5)));
    assert_eq!(state(&r, t(6)), State::Open);
    let just_before = WallTime::from_unix_micros(after(t(5), IDLE_AFTER).unix_micros() - 1);
    assert_eq!(state(&r, just_before), State::Open);
    assert_eq!(state(&r, after(t(5), IDLE_AFTER)), State::Idle);
    // A clock behind the last event is not idle.
    assert_eq!(state(&r, t(0)), State::Open);
}

/// Catches a Command digest set on the wrong row or the wrong caller's record, and an
/// event for an id no call made creating a record.
#[test]
fn the_command_digest_lands_on_its_row() {
    let command = |who: &str, inv: &str, action| Event::Command {
        id: id(inv),
        owner: owner(who),
        action: d(action),
        command: d(77),
        shape: None,
    };
    let r = records([
        queued("b", t(0), 1, 1),
        queued("b", t(0), 2, 2),
        command("", "b", 2),
        command("", "b", 3),
        command("someone else", "b", 1),
        command("", "nobody", 1),
    ]);
    let rows = r.get(&key("b")).expect("record b").rows();
    assert_eq!((rows[0].command, rows[1].command), (None, Some(d(77))));
    assert!(r.get(&key("nobody")).is_none());
}

/// Catches lease events for an operation no record waits on, or for a lease the
/// operation does not hold, changing a record (a stale or foreign report would rewrite
/// another build's attempts).
#[test]
fn events_for_unknown_operations_and_leases_change_nothing() {
    let base = records([queued("b", t(0), 1, 1), granted(1, 1, "node-a", t(1))]);
    let mut r = base.clone();
    for e in [
        granted(9, 1, "node-a", t(2)),
        started(9, 1, t(2)),
        requeued(9, 1, RequeueReason::Silent, t(2)),
        finished_ok(9, 1, 10, 0, t(2)),
    ] {
        r.apply(e);
    }
    assert_eq!(r, base);
    r.apply(started(1, 7, t(3)));
    r.apply(requeued(1, 8, RequeueReason::Replaced, t(4)));
    r.apply(finished_ok(1, 9, 10, 0, t(5)));
    let row = &r.get(&key("b")).expect("record b").rows()[0];
    assert_eq!(row.attempts.len(), 1);
    assert_eq!(
        (row.attempts[0].started, row.attempts[0].end.clone()),
        (None, None)
    );
    assert_eq!(row.outcome, Some(Outcome::Ok { exit_code: 0 }));
}

/// Catches a second Execute of an action adding a second row (rows are one per digest),
/// and the first operation's late events overwriting the row the second one now owns.
#[test]
fn a_later_execute_of_an_action_replaces_its_row() {
    let r = records([
        ev(call(
            "b",
            t(0),
            CallKind::GetActionResult {
                action: d(1),
                hit: Some(listing(10, 0)),
            },
        )),
        queued("b", t(1), 1, 1),
        queued("b", t(2), 1, 2),
        granted(1, 1, "node-a", t(3)),
        finished(
            1,
            None,
            Outcome::Failed {
                code: 2,
                message: "first".to_owned(),
            },
            t(4),
        ),
        ev(call(
            "b",
            t(5),
            CallKind::GetActionResult {
                action: d(1),
                hit: Some(listing(30, 0)),
            },
        )),
    ]);
    let inv = r.get(&key("b")).expect("record b");
    assert_eq!(inv.rows().len(), 1);
    let row = &inv.rows()[0];
    assert_eq!(
        (row.how, row.operation.as_deref()),
        (How::Ran, Some(name("b", 2).as_str()))
    );
    assert!(row.attempts.is_empty(), "{:?}", row.attempts);
    assert_eq!((row.outcome.clone(), row.listing.clone()), (None, None));
    assert_eq!(
        inv.counts().failed,
        1,
        "op-1's answer was still a failed Execute"
    );
    assert_eq!(inv.counts().ac_hit, 2);
}

/// Catches a dropped event not marking its record, or marking another.
#[test]
fn mark_incomplete_marks_one_record() {
    let mut r = records([queued("a", t(0), 1, 1), queued("b", t(0), 1, 1)]);
    r.get_mut(&key("a")).expect("record a").mark_incomplete();
    assert!(!r.get(&key("a")).expect("a").complete());
    assert!(r.get(&key("b")).expect("b").complete());
    assert!(r.get_mut(&key("c")).is_none());
}

/// Catches a minted id not marked as minted (section 2.3 keeps those in memory only).
#[test]
fn a_minted_invocation_says_so() {
    let mut c = call("kbf-1", t(0), CallKind::Uploaded(1));
    c.minted = true;
    let r = records([ev(c)]);
    assert!(r.get(&key("kbf-1")).expect("record").minted());
}

/// Catches every requeue reason and failure class the farm reports not surviving into
/// the attempt (one mapped to another).
#[test]
fn attempts_keep_each_requeue_reason_and_failure_class() {
    let reasons = [
        RequeueReason::OutOfMemory {
            booked: 1 << 30,
            raised: 2 << 30,
        },
        RequeueReason::Lost(LostWhy::Fenced),
    ];
    let mut events = vec![queued("b", t(0), 1, 1)];
    for (i, reason) in reasons.iter().enumerate() {
        events.push(granted(1, i as u64, "node-a", t(1)));
        events.push(requeued(1, i as u64, *reason, t(2)));
    }
    events.push(granted(1, 9, "node-a", t(3)));
    events.push(finished(
        1,
        Some((
            lease(9),
            LeaseEnd::Failed(Failure::OutOfMemory, "oom".to_owned()),
        )),
        Outcome::Failed {
            code: 8,
            message: "out of memory".to_owned(),
        },
        t(4),
    ));
    let r = records(events);
    let ends: Vec<Option<AttemptEnd>> = r.get(&key("b")).expect("record b").rows()[0]
        .attempts
        .iter()
        .map(|a| a.end.clone())
        .collect();
    assert_eq!(
        ends,
        [
            Some(AttemptEnd::Requeued(reasons[0])),
            Some(AttemptEnd::Requeued(reasons[1])),
            Some(AttemptEnd::Failed(Failure::OutOfMemory, "oom".to_owned())),
        ]
    );
}

/// Catches the metadata layer's truncation lost on the way in (plant `Text::kept`
/// marking only what it cuts itself): the layer already cut `target` to the same
/// cap, so the row sees no excess, yet the client sent more.
#[test]
fn a_text_the_metadata_layer_cut_stays_marked_truncated() {
    let mut c = call(
        "a",
        t(0),
        CallKind::Execute {
            action: d(1),
            answer: ExecuteAnswer::Queued {
                name: name("a", 1),
                operation: OperationId(1),
            },
        },
    );
    c.metadata.target = Some(Text {
        value: "//cut".to_owned(),
        truncated: true,
    });
    c.metadata.mnemonic = Some(Text::sent("CxxCompile"));
    let r = records([ev(c)]);
    let row = &r.get(&key("a")).expect("a").rows()[0];
    let target = row.target.as_ref().expect("target kept");
    assert_eq!((target.value.as_str(), target.truncated), ("//cut", true));
    assert!(!row.mnemonic.as_ref().expect("mnemonic kept").truncated);
}
