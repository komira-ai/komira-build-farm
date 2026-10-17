//! The bytes one action row costs once encoded (docs/design/invocations.md, section
//! 3.3), measured on an event stream shaped like a buck2 build against the farm.
//!
//! The stream is built here, deterministically, not captured from a client: nothing
//! records events until the recorder lands (section 9, PR 3). Its shape is what buck2
//! and kbf send today:
//! - one invocation, its id a UUID (buck2's trace id), and no other `RequestMetadata`
//!   (buck2 sends none);
//! - 1,000 actions queued by Execute, each under the front's per-caller operation name
//!   (`operations/<term>-<waiter>`), granted on one of eight nodes, started 5 ms later
//!   and finished from 0.1 s to 20 s after that, inside a 5-minute build;
//! - outputs per action cycling 1, 2, 1, 3, 1, 2, 4, 1 (1.875 on average), each at a
//!   buck2 output path (`buck-out/v2/gen/root/<16 hex>/<package>/__<target>__/<file>`,
//!   66 to 70 bytes);
//! - a stdout and a stderr digest on every result (the drivers store both, empty
//!   or not) and no `ResourceUsage` (no real driver fills it yet);
//! - one action in 50 requeued once (a silent node) before it ran again elsewhere.
//!
//! The test prints the measured table (`cargo test -p kbf-invocation --test
//! bytes_per_action -- --nocapture`) and holds the numbers to bands, so an encoding
//! that grows a row (hex digests, absolute times, a repeated field) goes red here.

mod support;

use kbf_invocation::{
    CallKind, Event, ExecuteAnswer, Finished, LeaseEnd, Listing, Outcome, Output, Records,
};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, DigestFunction, LeaseId, OperationId, WorkerId};
use prost::Message;
use support::{call, ev, key, t};

const ID: &str = "0b8c2f0e-5d7e-4c1a-9b3e-6f1d2a3c4b5e";
const ACTIONS: u64 = 1_000;
const OUTPUTS: [usize; 8] = [1, 2, 1, 3, 1, 2, 4, 1];

/// A digest that differs per `(kind, n)`, so no two blobs of the stream share one.
fn digest(kind: u8, n: u64, size: u64) -> Digest {
    let mut hash = [kind; 32];
    hash[..8].copy_from_slice(&n.to_le_bytes());
    Digest::new(DigestFunction::Sha256, hash, size)
}

/// The micros of `ms` milliseconds after the build's start.
fn at(ms: u64) -> kbf_invocation::WallTime {
    kbf_invocation::WallTime::from_unix_micros(t(0).unix_micros() + ms * 1_000)
}

fn output_path(n: u64, j: usize) -> String {
    format!(
        "buck-out/v2/gen/root/904931f735703749/src/mod_{:02}/__lib_{n:04}__/out{j}.o",
        n % 40
    )
}

/// What to put in each result.
#[derive(Clone, Copy)]
struct Shape {
    outputs: bool,
    logs: bool,
    reruns: bool,
}

fn listing(n: u64, shape: Shape) -> Listing {
    let count = if shape.outputs {
        OUTPUTS[usize::try_from(n % 8).expect("fits")]
    } else {
        0
    };
    Listing {
        result: Some(digest(1, n, 180 + n % 300)),
        exit_code: Some(0),
        outputs: (0..count)
            .map(|j| Output {
                path: output_path(n, j),
                digest: digest(2, n * 8 + j as u64, 4_000 + n * 37 % 900_000),
                executable: false,
                directory: false,
            })
            .collect(),
        stdout: shape.logs.then(|| digest(3, n, 0)),
        stderr: shape.logs.then(|| digest(4, n, n % 2_000)),
        usage: None,
    }
}

/// The events of a 1,000-action build of `shape`.
fn build(shape: Shape) -> Vec<Event> {
    let mut events = Vec::new();
    let mut seq = 0;
    for n in 0..ACTIONS {
        let op = OperationId(n);
        let submit = n * 300; // 1,000 actions over 300 s
        events.push(ev(call(
            ID,
            at(submit),
            CallKind::Execute {
                action: digest(0, n, 140 + n % 60),
                answer: ExecuteAnswer::Queued {
                    name: format!("operations/1-{n}"),
                    operation: op,
                },
            },
        )));
        let mut start = submit + 2;
        if shape.reruns && n % 50 == 0 {
            seq += 1;
            let lease = LeaseId::new(1, seq);
            events.push(grant(op, lease, n + 3, start));
            events.push(Event::Requeued {
                operation: op,
                lease,
                reason: RequeueReason::Silent,
                at: at(start + 15_000),
            });
            start += 15_001;
        }
        seq += 1;
        let lease = LeaseId::new(1, seq);
        events.push(grant(op, lease, n, start));
        events.push(Event::Started {
            operation: op,
            lease,
            at: at(start + 5),
        });
        let ran = 100 + (n * 7_919) % 19_900;
        events.push(Event::Finished(Finished {
            operation: op,
            lease: Some((lease, LeaseEnd::Accepted)),
            outcome: Outcome::Ok { exit_code: 0 },
            result: Some(listing(n, shape)),
            at: at(start + 5 + ran),
        }));
    }
    events
}

fn grant(operation: OperationId, lease: LeaseId, node: u64, ms: u64) -> Event {
    Event::Granted {
        operation,
        lease,
        worker: WorkerId::new(format!("linux-x86-{:02}", node % 8)),
        at: at(ms),
    }
}

/// The encoded chunk of `events`' one record, in bytes per row.
fn bytes_per_row(events: Vec<Event>) -> f64 {
    let mut r = Records::new();
    for e in events {
        r.apply(e);
    }
    let inv = r.get(&key(ID)).expect("the build's record");
    let rows = inv.rows().len();
    assert_eq!(rows as u64, ACTIONS);
    inv.chunk(0).encoded_len() as f64 / rows as f64
}

/// The same 1,000 actions answered by GetActionResult hits, the row a fully cached
/// build writes.
fn hits() -> Vec<Event> {
    let full = Shape {
        outputs: true,
        logs: true,
        reruns: false,
    };
    (0..ACTIONS)
        .map(|n| {
            ev(call(
                ID,
                at(n),
                CallKind::GetActionResult {
                    action: digest(0, n, 140 + n % 60),
                    hit: Some(listing(n, full)),
                },
            ))
        })
        .collect()
}

/// Catches an encoding that makes a row grow past the design's estimate (hex digests,
/// absolute times, the listing or attempts written twice) and an encoding that loses
/// most of a row (a band floor). Prints what it measured.
#[test]
fn bytes_per_action_on_a_buck2_shaped_build() {
    let shape = |outputs, logs, reruns| Shape {
        outputs,
        logs,
        reruns,
    };
    let core = bytes_per_row(build(shape(false, false, false)));
    let with_logs = bytes_per_row(build(shape(false, true, false)));
    let with_outputs = bytes_per_row(build(shape(true, true, false)));
    let full = bytes_per_row(build(shape(true, true, true)));
    let hit = bytes_per_row(hits());

    let outputs_per_action = OUTPUTS.iter().sum::<usize>() as f64 / OUTPUTS.len() as f64;
    let per_output = (with_outputs - with_logs) / outputs_per_action;
    let path = (0..ACTIONS)
        .flat_map(|n| (0..OUTPUTS[(n % 8) as usize]).map(move |j| output_path(n, j).len()))
        .sum::<usize>() as f64
        / (ACTIONS as f64 * outputs_per_action);
    let per_rerun = (full - with_outputs) * 50.0;
    let line = |label: &str, bytes: f64| eprintln!("  {label:<58} {bytes:6.1}");
    eprintln!("bytes per action row, encoded, 1,000 actions:");
    line(
        "core (digest, operation name, one attempt, outcome, result)",
        core,
    );
    line("+ stdout and stderr digests", with_logs - core);
    line(&format!("+ one output (a {path:.1}-byte path)"), per_output);
    line("+ one rerun", per_rerun);
    line(
        &format!("the build ({outputs_per_action} outputs a row, a rerun in 50)"),
        full,
    );
    line("a GetActionResult hit with the same listing", hit);

    // Section 3.3 estimated 170 B for the core row (130 B with times as deltas, which
    // this encoding uses), 45 B plus the path per output, 90 B for the two log digests
    // and 40 B per rerun.
    assert!((100.0..=170.0).contains(&core), "core row {core}");
    assert!(
        (70.0..=90.0).contains(&(with_logs - core)),
        "logs {}",
        with_logs - core
    );
    assert!(
        (path + 30.0..=path + 45.0).contains(&per_output),
        "output {per_output}, path {path}"
    );
    assert!((30.0..=60.0).contains(&per_rerun), "rerun {per_rerun}");
    assert!((300.0..=700.0).contains(&full), "build row {full}");
    assert!(hit < full, "a hit row ({hit}) has no attempt or operation");
}
