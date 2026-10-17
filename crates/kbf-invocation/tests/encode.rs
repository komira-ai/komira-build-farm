//! A record as `kbf.invocation.v1` messages: every field of the header and of a row
//! reaches the encoding, each enum value maps to its own proto value, and times are
//! signed offsets from `first_seen`. Each test names the defect it catches.

mod support;

use std::collections::BTreeSet;

use kbf_invocation::proto::{self, attempt_end::End, outcome::Outcome as P};
use kbf_invocation::{
    CallKind, Event, ExecuteAnswer, LeaseEnd, Metadata, Outcome, Records, Text, Usage,
};
use kbf_sched::RequeueReason;
use kbf_types::{Failure, LostWhy, OperationId};
use prost::Message;
use support::*;

fn pd(n: u8) -> Option<proto::Digest> {
    Some(proto::Digest {
        sha256: vec![n; 32],
        size_bytes: u64::from(n),
    })
}

/// Through bytes, so what is checked is what a chunk blob would hold.
fn round_trip<M: Message + Default>(m: &M) -> M {
    M::decode(m.encode_to_vec().as_slice()).expect("decode")
}

/// Catches a header field left out of the encoding or written from the wrong value
/// (`last_seen` absolute instead of after `first_seen`, `incomplete` inverted, the
/// collisions or instances lost).
#[test]
fn the_header_carries_every_field() {
    let mut first = call(
        "build",
        t(0),
        CallKind::FindMissingBlobs {
            asked: 5,
            missing: 2,
        },
    );
    first.metadata = Metadata {
        tool: Some("buck2".to_owned()),
        tool_version: Some("0.1.0".to_owned()),
        correlated_invocations_id: Some("corr".to_owned()),
        ..Metadata::default()
    };
    first.minted = true;
    let mut squat = call("build", t(1), CallKind::Uploaded(1));
    squat.owner = owner(r#"{"user":"b"}"#);
    let mut r = Records::with_row_cap(0);
    for e in [
        ev(first),
        ev(squat),
        ev(call("build", t(2), CallKind::Uploaded(11))),
        ev(call("build", t(3), CallKind::Downloaded(12))),
        queued("build", t(4), 1, 1),
        joined("build", t(4), 2, 2),
        ev(call(
            "build",
            t(4),
            CallKind::GetActionResult {
                action: d(3),
                hit: None,
            },
        )),
        finished(1, None, Outcome::TimedOut, t(9)),
    ] {
        r.apply(e);
    }
    let h = round_trip(&r.get(&key("build")).expect("record").header());
    assert_eq!(h.key, "build");
    assert_eq!(
        (h.tool.as_str(), h.tool_version.as_str()),
        ("buck2", "0.1.0")
    );
    assert_eq!(h.correlated_invocations_id, "corr");
    assert_eq!(h.owner, "");
    assert_eq!(h.instances, ["main"]);
    assert_eq!(h.first_seen_micros, t(0).unix_micros());
    assert_eq!(h.last_seen_micros, 9_000_000);
    assert!(h.minted);
    assert!(!h.incomplete);
    assert!(h.rows_capped);
    assert_eq!(h.collisions, ["build~2"]);
    assert_eq!(
        h.counts,
        Some(proto::Counts {
            execute: 2,
            cache_hit_execute: 0,
            joined: 1,
            ran: 1,
            failed: 1,
            ac_hit: 0,
            ac_miss: 1,
            blobs_asked: 5,
            blobs_missing: 2,
            bytes_uploaded: 11,
            bytes_downloaded: 12,
        })
    );
    r.get_mut(&key("build")).expect("record").mark_incomplete();
    let h = r.get(&key("build")).expect("record").header();
    assert!(h.incomplete);
    let squatter = r.get(&kbf_invocation::RecordKey::collision(id("build"), 2));
    let h = squatter.expect("the squatter's record").header();
    assert_eq!(
        (h.key.as_str(), h.owner.as_str()),
        ("build~2", r#"{"user":"b"}"#)
    );
    assert!(h.collisions.is_empty());

    // A record with no tool, no correlated id and no owner leaves them empty.
    let mut r = Records::new();
    r.apply(ev(call("plain", t(0), CallKind::Uploaded(1))));
    let h = r.get(&key("plain")).expect("record").header();
    assert_eq!(
        (h.tool, h.tool_version, h.correlated_invocations_id, h.owner),
        (String::new(), String::new(), String::new(), String::new())
    );
}

/// Catches a row field left out of the encoding: the action, instance, metadata texts,
/// how, operation, outcome, result, command, outputs (paths, digests, bits), stdout and
/// stderr digests, usage; and times written absolute or unsigned (a joined action's
/// attempt that began before the record did would wrap).
#[test]
fn a_row_carries_every_field() {
    let mut c = call(
        "late",
        t(10),
        CallKind::Execute {
            action: d(1),
            answer: ExecuteAnswer::Joined {
                name: name("late", 1),
                operation: OperationId(1),
            },
        },
    );
    c.metadata = Metadata {
        target: Some(Text::sent("//pkg:lib")),
        mnemonic: Some(Text::sent("CxxCompile")),
        configuration: Some(Text::sent("cfg")),
        action_id: Some(Text::sent("a-1")),
        ..Metadata::default()
    };
    let mut result = listing(40, 0);
    result.outputs.push(kbf_invocation::Output {
        path: "out/dir".to_owned(),
        digest: d(45),
        executable: false,
        directory: true,
    });
    result.outputs[0].executable = true;
    result.usage = Some(Usage {
        cpu_user_micros: 1,
        cpu_system_micros: 2,
        peak_memory_bytes: 3,
        wall_micros: 4,
    });
    let mut done = finished_ok(1, 1, 40, 0, t(20));
    if let Event::Finished(f) = &mut done {
        f.result = Some(result);
    }
    let mut r = Records::new();
    for e in [
        queued("early", t(0), 1, 1),
        granted(1, 1, "node-a", t(2)),
        started(1, 1, t(3)),
        ev(c),
        done,
        Event::Command {
            id: id("late"),
            owner: owner(""),
            action: d(1),
            command: d(50),
            shape: None,
        },
    ] {
        r.apply(e);
    }
    let chunk = round_trip(&r.get(&key("late")).expect("record").chunk(0));
    let row = &chunk.actions[0];
    assert_eq!(row.action, pd(1));
    assert_eq!(row.instance, "main");
    let text = |v: &str| {
        Some(proto::Text {
            value: v.to_owned(),
            truncated: false,
        })
    };
    assert_eq!(row.target, text("//pkg:lib"));
    assert_eq!(row.mnemonic, text("CxxCompile"));
    assert_eq!(row.configuration, text("cfg"));
    assert_eq!(row.action_id, text("a-1"));
    assert_eq!(row.how(), proto::How::Joined);
    assert_eq!(row.operation, name("late", 1));
    assert_eq!(
        row.attempts,
        [proto::Attempt {
            lease_term: 1,
            lease_seq: 1,
            worker: "node-a".to_owned(),
            queued: -10_000_000,
            granted: -8_000_000,
            started: Some(-7_000_000),
            ended: Some(10_000_000),
            end: Some(proto::AttemptEnd {
                end: Some(End::Result(proto::Accepted {}))
            }),
        }]
    );
    assert_eq!(
        row.outcome,
        Some(proto::Outcome {
            outcome: Some(P::Ok(proto::Exited { exit_code: 0 }))
        })
    );
    assert_eq!((row.result.clone(), row.command.clone()), (pd(40), pd(50)));
    assert_eq!(
        row.outputs,
        [
            proto::Output {
                path: "out/40.o".to_owned(),
                digest: pd(41),
                executable: true,
                directory: false,
            },
            proto::Output {
                path: "out/dir".to_owned(),
                digest: pd(45),
                executable: false,
                directory: true,
            },
        ]
    );
    assert!(!row.outputs_truncated);
    assert_eq!((row.stdout.clone(), row.stderr.clone()), (pd(42), pd(43)));
    assert_eq!(
        row.usage,
        Some(proto::Usage {
            cpu_user_micros: 1,
            cpu_system_micros: 2,
            peak_memory_bytes: 3,
            wall_micros: 4,
        })
    );

    // A row in flight has no outcome, result, logs or usage, and an attempt not yet
    // started or ended has no times for them.
    let early = r.get(&key("early")).expect("record").chunk(0);
    assert_eq!(early.actions[0].attempts[0].queued, 0);
    let mut open = Records::new();
    open.apply(queued("b", t(0), 1, 1));
    open.apply(granted(1, 1, "node-a", t(1)));
    let row = &open.get(&key("b")).expect("record").chunk(0).actions[0];
    assert_eq!(row.how(), proto::How::Ran);
    assert_eq!((row.outcome.clone(), row.result.clone()), (None, None));
    assert_eq!((row.stdout.clone(), row.usage), (None, None));
    assert!(row.outputs.is_empty());
    let a = &row.attempts[0];
    assert_eq!((a.started, a.ended, a.end.clone()), (None, None, None));
}

/// Catches `chunk(from)` returning rows before `from` (a flush would write them twice)
/// or dropping the one at `from`, and a cache hit's or cached Execute's `how` or an
/// absent text encoded as present.
#[test]
fn a_chunk_holds_the_rows_from_its_start() {
    let mut r = Records::new();
    r.apply(ev(call(
        "b",
        t(0),
        CallKind::GetActionResult {
            action: d(1),
            hit: Some(listing(10, 0)),
        },
    )));
    r.apply(ev(call(
        "b",
        t(0),
        CallKind::Execute {
            action: d(2),
            answer: ExecuteAnswer::Cached {
                name: "operations/cached/2".to_owned(),
                result: Box::new(listing(20, 0)),
            },
        },
    )));
    r.apply(queued("b", t(0), 3, 3));
    let inv = r.get(&key("b")).expect("record");
    let hows =
        |from| -> Vec<proto::How> { inv.chunk(from).actions.iter().map(|a| a.how()).collect() };
    assert_eq!(
        hows(0),
        [
            proto::How::CacheHit,
            proto::How::CacheHitExecute,
            proto::How::Ran
        ]
    );
    assert_eq!(hows(2), [proto::How::Ran]);
    assert!(hows(3).is_empty());
    let first = &inv.chunk(0).actions[0];
    assert_eq!(first.operation, "");
    assert_eq!(first.target, None);
    assert_eq!(first.command, None);
}

/// Catches two requeue reasons or failure classes encoded as one value (a page could
/// not tell a silent node from a restarted daemon), the unspecified value written, and
/// an out-of-memory requeue's bookings lost.
#[test]
fn each_requeue_reason_and_failure_class_has_its_own_value() {
    let reasons = [
        RequeueReason::Silent,
        RequeueReason::Replaced,
        RequeueReason::Reconnected,
        RequeueReason::NotStarted,
        RequeueReason::SessionSwitch,
        RequeueReason::OutOfMemory {
            booked: 1 << 30,
            raised: 2 << 30,
        },
        RequeueReason::NodeMemoryPressure,
        RequeueReason::Restarted,
        RequeueReason::Lost(LostWhy::DaemonRestarted),
        RequeueReason::Lost(LostWhy::Fenced),
        RequeueReason::Infra,
    ];
    let failures = [
        Failure::Timeout,
        Failure::Infra,
        Failure::Invalid,
        Failure::OutOfMemory,
        Failure::NodeMemoryPressure,
        Failure::MissingInput,
        Failure::Lost(LostWhy::DaemonRestarted),
        Failure::Lost(LostWhy::Fenced),
    ];
    let mut r = Records::new();
    r.apply(queued("b", t(0), 1, 100));
    for (i, reason) in reasons.iter().enumerate() {
        r.apply(granted(100, i as u64, "node-a", t(1)));
        r.apply(requeued(100, i as u64, *reason, t(2)));
    }
    for (i, f) in failures.iter().enumerate() {
        let op = 200 + i as u64;
        r.apply(queued("b", t(0), 10 + i as u8, op));
        r.apply(granted(op, 1, "node-a", t(1)));
        r.apply(finished(
            op,
            Some((lease(1), LeaseEnd::Failed(*f, format!("why {i}")))),
            Outcome::TimedOut,
            t(2),
        ));
    }
    let chunk = round_trip(&r.get(&key("b")).expect("record").chunk(0));
    let requeues: Vec<proto::Requeue> = chunk.actions[0]
        .attempts
        .iter()
        .map(|a| match a.end.clone().and_then(|e| e.end) {
            Some(End::Requeue(q)) => q,
            other => panic!("not a requeue: {other:?}"),
        })
        .collect();
    let values: BTreeSet<i32> = requeues.iter().map(|q| q.reason).collect();
    assert_eq!(values.len(), reasons.len());
    assert!(!values.contains(&(proto::RequeueReason::Unspecified as i32)));
    assert_eq!(requeues[0].reason(), proto::RequeueReason::Silent);
    assert_eq!(requeues[9].reason(), proto::RequeueReason::LostFenced);
    assert_eq!(requeues[10].reason(), proto::RequeueReason::Infra);
    assert_eq!(
        (requeues[5].reason(), requeues[5].booked, requeues[5].raised),
        (proto::RequeueReason::OutOfMemory, 1 << 30, 2 << 30)
    );
    assert_eq!((requeues[0].booked, requeues[0].raised), (0, 0));

    let classes: Vec<(i32, String)> = chunk.actions[1..]
        .iter()
        .map(
            |row| match row.attempts[0].end.clone().and_then(|e| e.end) {
                Some(End::Failed(f)) => (f.class, f.reason),
                other => panic!("not a failure: {other:?}"),
            },
        )
        .collect();
    let values: BTreeSet<i32> = classes.iter().map(|c| c.0).collect();
    assert_eq!(values.len(), failures.len());
    assert!(!values.contains(&(proto::FailureClass::Unspecified as i32)));
    assert_eq!(
        classes[0],
        (proto::FailureClass::Timeout as i32, "why 0".to_owned())
    );
    assert_eq!(classes[5].0, proto::FailureClass::MissingInput as i32);
}

/// Catches an outcome encoded as another, and a status message or refusal reason lost.
#[test]
fn each_outcome_has_its_own_encoding() {
    let outcomes = [
        Outcome::Ok { exit_code: 3 },
        Outcome::Failed {
            code: 9,
            message: "m".to_owned(),
        },
        Outcome::TimedOut,
        Outcome::Refused("r".to_owned()),
    ];
    let mut r = Records::new();
    for (i, o) in outcomes.iter().enumerate() {
        let op = i as u64;
        r.apply(queued("b", t(0), 1 + i as u8, op));
        r.apply(finished(op, None, o.clone(), t(1)));
    }
    let chunk = round_trip(&r.get(&key("b")).expect("record").chunk(0));
    let got: Vec<Option<P>> = chunk
        .actions
        .iter()
        .map(|a| a.outcome.clone().and_then(|o| o.outcome))
        .collect();
    assert_eq!(
        got,
        [
            Some(P::Ok(proto::Exited { exit_code: 3 })),
            Some(P::Failed(proto::Status {
                code: 9,
                message: "m".to_owned()
            })),
            Some(P::TimedOut(proto::TimedOut {})),
            Some(P::Refused("r".to_owned())),
        ]
    );
}

/// Catches a row cut to 1,000 outputs whose encoding does not say it was cut.
#[test]
fn a_cut_listing_is_marked_in_the_encoding() {
    let mut result = listing(40, 0);
    result.outputs = vec![result.outputs[0].clone(); kbf_invocation::OUTPUTS_PER_ROW + 1];
    let mut done = finished_ok(1, 1, 40, 0, t(1));
    if let Event::Finished(f) = &mut done {
        f.result = Some(result);
    }
    let mut r = Records::new();
    r.apply(queued("b", t(0), 1, 1));
    r.apply(done);
    let row = &r.get(&key("b")).expect("record").chunk(0).actions[0];
    assert!(row.outputs_truncated);
    assert_eq!(row.outputs.len(), kbf_invocation::OUTPUTS_PER_ROW);
}
