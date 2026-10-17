//! What a row keeps beyond the built fields (`docs/design/ui-data.md` 4.2, phase U1):
//! a lease's run reaching every row of its operation, the shape on the row of its
//! action, the bound held while they are admitted, and the durations a row derives.
//! Each test names the defect it catches.

mod support;

use kbf_invocation::{
    ActionShape, Event, LeaseEnd, MemoryKillKind, Outcome, Phase, ReapiTimes, Records, Run,
    RunUsage, Target, TargetKind, Text,
};
use kbf_sched::RequeueReason;
use kbf_types::{Failure, OperationId};
use support::*;

fn ran(op: u64, seq: u64, run: Run) -> Event {
    Event::Ran {
        operation: OperationId(op),
        lease: lease(seq),
        run: Box::new(run),
    }
}

fn wide_run(numa: u32) -> Run {
    Run {
        usage: Some(RunUsage {
            numa: (0..numa)
                .map(|node| kbf_invocation::NumaBytes { node, bytes: 1 })
                .collect(),
            ..RunUsage::default()
        }),
        ..Run::default()
    }
}

fn shape(inv: &str, action: u8, cfg_bytes: usize) -> Event {
    Event::Command {
        id: id(inv),
        owner: owner(""),
        action: d(action),
        command: d(99),
        shape: Some(Box::new(ActionShape {
            key: 1,
            cfg: Some("c".repeat(cfg_bytes)),
            target: Some(Target {
                kind: TargetKind::Bxl,
                label: Text::sent("l".repeat(600)),
            }),
            test: false,
        })),
    }
}

fn applied(r: &mut Records, events: impl IntoIterator<Item = Event>) {
    for e in events {
        r.apply(e);
        assert!(r.bytes() <= r.bound(), "{} over {}", r.bytes(), r.bound());
    }
}

/// Catches: a run kept on the operation only (a joined twin's row, or the row
/// itself, never sees it), set on the wrong lease, or a run for an operation or a
/// lease the records do not hold changing anything; and a shape's label kept past
/// 512 bytes or not marked cut.
#[test]
fn a_run_reaches_every_row_and_a_shape_its_own() {
    let mut r = Records::new();
    let run = Run {
        booked_memory_bytes: Some(5),
        ..Run::default()
    };
    applied(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            joined("b", t(0), 1, 1),
            granted(1, 1, "node-a", t(1)),
            requeued(1, 1, RequeueReason::Silent, t(2)),
            granted(1, 2, "node-a", t(3)),
            ran(1, 2, run.clone()),
            shape("a", 1, 16),
        ],
    );
    let before = r.clone();
    applied(&mut r, [ran(9, 1, run.clone()), ran(1, 9, run.clone())]);
    assert_eq!(r, before, "an operation or lease not held");
    for inv in ["a", "b"] {
        let row = &r.get(&key(inv)).expect("held").rows()[0];
        let runs: Vec<_> = row.attempts.iter().map(|a| a.run.as_deref()).collect();
        assert_eq!(runs, [None, Some(&run)], "{inv}");
    }
    let a = &r.get(&key("a")).expect("a").rows()[0];
    let label = &a
        .shape
        .as_ref()
        .expect("a shape")
        .target
        .as_ref()
        .expect("a target")
        .label;
    assert_eq!((label.value.len(), label.truncated), (512, true));
    assert_eq!(
        r.get(&key("b")).expect("b").rows()[0].shape,
        None,
        "b's own Command"
    );
}

/// Catches: a run or a shape admitted past the bound, or left out without the record
/// saying `complete: false`; and a run whose admission removed every record waiting
/// on its operation (a minted one) set on what is gone.
#[test]
fn a_run_or_a_shape_that_does_not_fit_is_left_out_and_marked() {
    let base = [queued("a", t(0), 1, 1), granted(1, 1, "node-a", t(1))];
    let mut sized = Records::new();
    applied(&mut sized, base.clone());
    for (what, event) in [
        ("run", ran(1, 1, wide_run(1_000))),
        ("shape", shape("a", 1, 10_000)),
    ] {
        let mut r = Records::with_bound(kbf_invocation::ROW_CAP, sized.bytes());
        applied(&mut r, base.clone());
        applied(&mut r, [event]);
        let inv = r.get(&key("a")).expect("a");
        assert!(!inv.complete(), "{what}");
        assert_eq!(inv.rows()[0].attempts[0].run, None, "{what}");
        assert_eq!(inv.rows()[0].shape, None, "{what}");
    }

    // A minted record is removed first: the run has nowhere to go.
    let Event::Call(mut call) = queued("m", t(0), 1, 1) else {
        unreachable!("a call");
    };
    call.minted = true;
    let minted = [Event::Call(call), granted(1, 1, "node-a", t(1))];
    let mut sized = Records::new();
    applied(&mut sized, minted.clone());
    let mut r = Records::with_bound(kbf_invocation::ROW_CAP, sized.bytes());
    applied(&mut r, minted);
    applied(&mut r, [ran(1, 1, wide_run(1_000))]);
    assert_eq!(r.get(&key("m")), None, "removed for the run");
}

/// Catches: a row's durations read from the wrong leases (wall not from the first
/// lease's queueing to the last's end, queue not summed over every lease), a phase
/// read from the wrong pair of REAPI times or from the first lease's run, a phase
/// whose end is before its start given a huge value, and a memory kill read wrong
/// from how a lease ended.
#[test]
fn a_row_derives_its_durations_and_its_memory_kills() {
    let at = |s: u64| Some(t(s));
    let last = Run {
        times: ReapiTimes {
            input_fetch_start: at(10),
            input_fetch_completed: at(12),
            execution_start: at(12),
            execution_completed: at(17),
            output_upload_start: at(18),
            output_upload_completed: at(17),
            ..ReapiTimes::default()
        },
        ..Run::default()
    };
    let mut r = Records::new();
    applied(
        &mut r,
        [
            queued("a", t(0), 1, 1),
            granted(1, 1, "node-a", t(2)),
            ran(1, 1, Run::default()),
            requeued(
                1,
                1,
                RequeueReason::OutOfMemory {
                    booked: 1,
                    raised: 2,
                },
                t(5),
            ),
            granted(1, 2, "node-a", t(8)),
            ran(1, 2, last),
            requeued(1, 2, RequeueReason::NodeMemoryPressure, t(19)),
            granted(1, 3, "node-a", t(20)),
            finished(
                1,
                Some((
                    lease(3),
                    LeaseEnd::Failed(Failure::OutOfMemory, String::new()),
                )),
                Outcome::Failed {
                    code: 9,
                    message: String::new(),
                },
                t(25),
            ),
        ],
    );
    let row = &r.get(&key("a")).expect("a").rows()[0];
    let d = row.durations();
    let s = |n: u64| Some(n * 1_000_000);
    assert_eq!(d.wall, s(25));
    assert_eq!(d.queue, s(2 + 3 + 1));
    assert_eq!(
        (d.execution, d.input_fetch, d.output_upload),
        (None, None, None),
        "the last lease's run has none"
    );
    let kills: Vec<_> = row
        .attempts
        .iter()
        .map(kbf_invocation::Attempt::memory_kill)
        .collect();
    assert_eq!(
        kills,
        [
            Some(MemoryKillKind::OwnLimit),
            Some(MemoryKillKind::NodePressure),
            Some(MemoryKillKind::OwnLimit)
        ]
    );
    // Ended by its second lease instead: the phases are that lease's run's.
    let mut middle = row.clone();
    middle.attempts.truncate(2);
    middle.attempts[1].end = Some(kbf_invocation::AttemptEnd::Accepted);
    let d = middle.durations();
    assert_eq!(d.wall, s(19));
    assert_eq!(
        (d.input_fetch, d.execution, d.output_upload),
        (s(2), s(5), None)
    );
    let mut held = middle.clone();
    held.attempts[1].ended = None;
    held.attempts[1].end = None;
    assert_eq!(
        held.durations(),
        kbf_invocation::Durations {
            queue: s(2 + 3),
            ..kbf_invocation::Durations::default()
        },
        "its last lease is held: only the queueing so far"
    );
    assert_eq!(held.attempts[1].memory_kill(), None, "held");
    for (phase, name) in Phase::NAMED {
        assert_eq!(
            d.of(phase),
            match phase {
                Phase::Wall => d.wall,
                Phase::Execution => d.execution,
                Phase::Queue => d.queue,
                Phase::InputFetch => d.input_fetch,
                Phase::OutputUpload => d.output_upload,
            }
        );
        assert_eq!(phase.name(), name);
    }
    assert_eq!(
        MemoryKillKind::NAMED.map(|(k, _)| k.name()),
        ["none", "own_limit", "node_pressure"]
    );
    assert_eq!(
        [TargetKind::Target, TargetKind::Anon, TargetKind::Bxl].map(TargetKind::name),
        ["target", "anon", "bxl"]
    );
    let empty = r.get(&key("a")).expect("a").rows()[0].clone();
    let mut none = empty;
    none.attempts.clear();
    assert_eq!(none.durations(), kbf_invocation::Durations::default());
}

/// Catches (review of ui-data 4.2): a row waiting to be granted again after a
/// requeue given a wall time (first queueing to the requeue, as if final) or the
/// requeued lease's phases, so a list sorted by either ranks it on a number that
/// will change; and the queueing so far dropped with them.
#[test]
fn a_requeued_row_waiting_again_has_no_wall_or_phases() {
    let at = |s: u64| Some(t(s));
    let run = Run {
        times: ReapiTimes {
            execution_start: at(3),
            execution_completed: at(4),
            ..ReapiTimes::default()
        },
        ..Run::default()
    };
    let mut r = Records::new();
    applied(
        &mut r,
        [
            queued("w", t(0), 1, 1),
            granted(1, 1, "node-a", t(2)),
            ran(1, 1, run),
            requeued(
                1,
                1,
                RequeueReason::OutOfMemory {
                    booked: 1,
                    raised: 2,
                },
                t(5),
            ),
        ],
    );
    let row = &r.get(&key("w")).expect("w").rows()[0];
    assert!(
        row.attempts[0].ended.is_some(),
        "the requeue ended its lease"
    );
    assert_eq!(
        row.durations(),
        kbf_invocation::Durations {
            queue: Some(2_000_000),
            ..kbf_invocation::Durations::default()
        }
    );
}
