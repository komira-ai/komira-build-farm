//! Builders for the tests' events.

#![allow(dead_code, reason = "each test binary uses some of the builders")]

use kbf_invocation::{
    Call, CallKind, Event, ExecuteAnswer, Finished, InvocationId, LeaseEnd, Listing, Metadata,
    Outcome, Output, Owner, RecordKey, WallTime,
};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, DigestFunction, LeaseId, OperationId, WorkerId};

/// A digest whose hash is `n` repeated and whose size is `n`.
pub fn d(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// `s` seconds after an arbitrary epoch, in wall time.
pub fn t(s: u64) -> WallTime {
    WallTime::from_unix_micros(1_800_000_000_000_000 + s * 1_000_000)
}

pub fn id(s: &str) -> InvocationId {
    InvocationId::new(s)
}

pub fn key(s: &str) -> RecordKey {
    RecordKey::owner(id(s))
}

pub fn owner(s: &str) -> Owner {
    Owner(s.to_owned())
}

/// A call of invocation `inv` by the owner `""`, on instance `main`, at `at`.
pub fn call(inv: &str, at: WallTime, kind: CallKind) -> Call {
    Call {
        id: id(inv),
        minted: false,
        owner: Owner::default(),
        instance: "main".to_owned(),
        at,
        metadata: Metadata::default(),
        kind,
    }
}

pub fn ev(c: Call) -> Event {
    Event::Call(Box::new(c))
}

/// The REAPI name the front gives invocation `inv`'s call that waits on `op`: one per
/// caller, as the front names them.
pub fn name(inv: &str, op: u64) -> String {
    format!("operations/{inv}-{op}")
}

/// An Execute of `d(action)` by `inv` that queued the scheduler operation `op`.
pub fn queued(inv: &str, at: WallTime, action: u8, op: u64) -> Event {
    ev(call(
        inv,
        at,
        CallKind::Execute {
            action: d(action),
            answer: ExecuteAnswer::Queued {
                name: name(inv, op),
                operation: OperationId(op),
            },
        },
    ))
}

/// An Execute of `d(action)` by `inv` that joined the scheduler operation `op`.
pub fn joined(inv: &str, at: WallTime, action: u8, op: u64) -> Event {
    ev(call(
        inv,
        at,
        CallKind::Execute {
            action: d(action),
            answer: ExecuteAnswer::Joined {
                name: name(inv, op),
                operation: OperationId(op),
            },
        },
    ))
}

/// A listing with result `d(result)`, exit code `exit_code`, one output file and both
/// logs.
pub fn listing(result: u8, exit_code: i32) -> Listing {
    Listing {
        result: Some(d(result)),
        exit_code: Some(exit_code),
        outputs: vec![Output {
            path: format!("out/{result}.o"),
            digest: d(result.wrapping_add(1)),
            executable: false,
            directory: false,
        }],
        stdout: Some(d(result.wrapping_add(2))),
        stderr: Some(d(result.wrapping_add(3))),
        usage: None,
    }
}

pub fn lease(seq: u64) -> LeaseId {
    LeaseId::new(1, seq)
}

pub fn granted(op: u64, seq: u64, worker: &str, at: WallTime) -> Event {
    Event::Granted {
        operation: OperationId(op),
        lease: lease(seq),
        worker: WorkerId::new(worker),
        at,
    }
}

pub fn started(op: u64, seq: u64, at: WallTime) -> Event {
    Event::Started {
        operation: OperationId(op),
        lease: lease(seq),
        at,
    }
}

pub fn requeued(op: u64, seq: u64, reason: RequeueReason, at: WallTime) -> Event {
    Event::Requeued {
        operation: OperationId(op),
        lease: lease(seq),
        reason,
        at,
    }
}

/// `op` finished by lease `seq`'s accepted result, OK with `exit_code`.
pub fn finished_ok(op: u64, seq: u64, result: u8, exit_code: i32, at: WallTime) -> Event {
    Event::Finished(Finished {
        operation: OperationId(op),
        lease: Some((lease(seq), LeaseEnd::Accepted)),
        outcome: Outcome::Ok { exit_code },
        result: Some(listing(result, exit_code)),
        at,
    })
}

/// `op` finished with `outcome` and no result.
pub fn finished(
    op: u64,
    lease: Option<(LeaseId, LeaseEnd)>,
    outcome: Outcome,
    at: WallTime,
) -> Event {
    Event::Finished(Finished {
        operation: OperationId(op),
        lease,
        outcome,
        result: None,
        at,
    })
}
