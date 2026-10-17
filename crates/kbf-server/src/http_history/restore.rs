//! History records back into the rows the build pages already render: one
//! [`ActionRow`] per action, its attempts from the run records of that action and its
//! `how` from the first record seen.
//!
//! History keeps less than a live record holds, so a restored row differs from the live
//! one it came from in exactly these ways:
//!
//! - **Not kept, so absent:** the client's own hints (`target`, `mnemonic`,
//!   `configuration`, `action_id` of the row), the REAPI operation name, the result's
//!   `usage` and an output's `executable` bit, and the listing of a cache hit.
//! - **Derived from the run's class:** the end of an attempt and the row's outcome (see
//!   [`end_of`]): a failure's gRPC status code and a lost run's cause are not kept, so a
//!   restored one says `INVALID_ARGUMENT`, `RESOURCE_EXHAUSTED` or `UNAVAILABLE` and a
//!   fenced node by the class alone; an attempt that is not the last of its action is a
//!   requeue.
//! - **Zero is not absent:** a cgroup counter the daemon could not read is kept as 0, and
//!   a NUMA node that was not reported is left out when its bytes are 0.
//! - **One row per action:** an action run twice in one invocation (a rerun past the
//!   cache) is one row with every attempt history holds.

use std::collections::BTreeMap;

use kbf_history::records::{HitHow, HitRecord, MemoryKill, RunOutcome, RunRecord, WireDigest};
use kbf_invocation::{
    ActionRow, ActionShape, Attempt, AttemptEnd, How, Listing, NumaBytes, Outcome, Output,
    ReapiTimes, Run, RunUsage, Target, TargetKind, Text, WallTime,
};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, DigestFunction, Failure, LeaseId, LostWhy, WorkerId};

/// gRPC `INVALID_ARGUMENT`: what a run the farm could not run as written was answered.
const INVALID_ARGUMENT: i32 = 3;
/// gRPC `RESOURCE_EXHAUSTED`: what a run killed at its own memory limit was answered.
const RESOURCE_EXHAUSTED: i32 = 8;
/// gRPC `UNAVAILABLE`: what a run the farm failed was answered.
const UNAVAILABLE: i32 = 14;

/// The rows of `runs` and `hits`, in the order the records were first seen, and how many
/// records were passed over because their action digest is not one (a broken record).
pub(crate) fn rows(runs: &[RunRecord], hits: &[HitRecord]) -> (Vec<ActionRow>, usize) {
    // Records in the order they were written: by time, a run before a hit of the same
    // millisecond, then in sequence.
    let mut order: Vec<(u64, u8, u32, usize)> = runs
        .iter()
        .enumerate()
        .map(|(i, r)| (r.time_ms, 0, r.seq, i))
        .chain(
            hits.iter()
                .enumerate()
                .map(|(i, h)| (h.time_ms, 1, h.row, i)),
        )
        .collect();
    order.sort_unstable();
    let mut rows: Vec<ActionRow> = Vec::new();
    let mut at: BTreeMap<Digest, usize> = BTreeMap::new();
    // Each row's run records, in order.
    let mut records: Vec<Vec<&RunRecord>> = Vec::new();
    let mut skipped = 0;
    for (_, kind, _, i) in order {
        let (action, instance) = match kind {
            0 => (digest(runs[i].action.as_ref()), &runs[i].instance),
            _ => (digest(hits[i].action.as_ref()), &hits[i].instance),
        };
        let Some(action) = action else {
            skipped += 1;
            continue;
        };
        let row = *at.entry(action).or_insert_with(|| {
            let how = match kind {
                0 => How::Ran,
                _ => hit_how(&hits[i]),
            };
            rows.push(new_row(action, instance, how));
            records.push(Vec::new());
            rows.len() - 1
        });
        if kind == 0 {
            records[row].push(&runs[i]);
        }
    }
    for (row, runs) in rows.iter_mut().zip(&records) {
        fill(row, runs);
    }
    (rows, skipped)
}

/// A row of `action` asked under `instance`, answered `how`, with nothing else yet. A
/// cache hit's answer is OK: a failed action is never cached.
fn new_row(action: Digest, instance: &str, how: How) -> ActionRow {
    let outcome = match how {
        How::Ran | How::Joined => None,
        How::CacheHit | How::CacheHitExecute => Some(Outcome::Ok { exit_code: 0 }),
    };
    ActionRow {
        action,
        instance: instance.to_owned(),
        target: None,
        mnemonic: None,
        configuration: None,
        action_id: None,
        how,
        operation: None,
        attempts: Vec::new(),
        outcome,
        listing: None,
        outputs_truncated: false,
        command: None,
        shape: None,
    }
}

fn hit_how(hit: &HitRecord) -> How {
    match hit.how() {
        HitHow::CacheHitExecute => How::CacheHitExecute,
        HitHow::CacheHit | HitHow::Unspecified => How::CacheHit,
    }
}

/// Adds what the run records `runs` of one action say to `row`: its attempts, and from
/// the last one the shape, command, listing and outcome.
fn fill(row: &mut ActionRow, runs: &[&RunRecord]) {
    let Some(last) = runs.last() else {
        return;
    };
    for (i, r) in runs.iter().enumerate() {
        let (end, _) = end_of(r, i + 1 == runs.len());
        let mut attempt = attempt(r);
        attempt.end = end;
        row.attempts.push(attempt);
    }
    let (_, outcome) = end_of(last, true);
    row.outcome = outcome;
    row.command = digest(last.command.as_ref());
    row.shape = shape(last);
    row.listing = listing(last);
}

/// How the attempt of `r` ended, and, when it is the last of its action, what its callers
/// were answered. Not kept, so derived from the run's class: see the module's text.
fn end_of(r: &RunRecord, last: bool) -> (Option<AttemptEnd>, Option<Outcome>) {
    let reason = r.reason.clone();
    let kill = r.memory_kill();
    if !last {
        // An attempt another followed was given up and its operation queued again.
        return match r.outcome() {
            RunOutcome::Unspecified | RunOutcome::Cancelled => (None, None),
            RunOutcome::MemoryKilled if kill == MemoryKill::OwnLimit => (
                Some(AttemptEnd::Requeued(RequeueReason::OutOfMemory {
                    booked: r.booked_memory_bytes,
                    raised: r.raised_memory_bytes,
                })),
                None,
            ),
            RunOutcome::MemoryKilled if kill == MemoryKill::NodePressure => (
                Some(AttemptEnd::Requeued(RequeueReason::NodeMemoryPressure)),
                None,
            ),
            RunOutcome::Infra => (Some(AttemptEnd::Requeued(RequeueReason::Infra)), None),
            _ => (Some(AttemptEnd::Requeued(RequeueReason::Silent)), None),
        };
    }
    let failed = |failure: Failure, code: i32| {
        (
            Some(AttemptEnd::Failed(failure, reason.clone())),
            Some(Outcome::Failed {
                code,
                message: reason.clone(),
            }),
        )
    };
    match r.outcome() {
        RunOutcome::Unspecified | RunOutcome::Cancelled => (None, None),
        RunOutcome::Ok => (
            Some(AttemptEnd::Accepted),
            Some(Outcome::Ok { exit_code: 0 }),
        ),
        RunOutcome::Failed if r.exit_code != 0 => (
            Some(AttemptEnd::Accepted),
            Some(Outcome::Ok {
                exit_code: r.exit_code,
            }),
        ),
        RunOutcome::Failed => failed(Failure::Invalid, INVALID_ARGUMENT),
        RunOutcome::TimedOut => (
            Some(AttemptEnd::Failed(Failure::Timeout, reason)),
            Some(Outcome::TimedOut),
        ),
        RunOutcome::MemoryKilled if kill == MemoryKill::OwnLimit => {
            failed(Failure::OutOfMemory, RESOURCE_EXHAUSTED)
        }
        RunOutcome::MemoryKilled => failed(Failure::NodeMemoryPressure, UNAVAILABLE),
        RunOutcome::Infra => failed(Failure::Infra, UNAVAILABLE),
        RunOutcome::Lost => failed(Failure::Lost(LostWhy::Fenced), UNAVAILABLE),
    }
}

/// The attempt `r` is the record of, with no end yet.
fn attempt(r: &RunRecord) -> Attempt {
    let t = r.timestamps.clone().unwrap_or_default();
    // A record written before the attempt's own queue time was kept has none: the first
    // submission is the nearest answer.
    let queued = if r.attempt_queued_us == 0 {
        t.queued_us
    } else {
        r.attempt_queued_us
    };
    Attempt {
        lease: LeaseId::new(r.lease_epoch, r.lease_seq),
        worker: WorkerId::new(&r.node),
        queued: WallTime::from_unix_micros(queued),
        granted: WallTime::from_unix_micros(t.granted_us),
        started: wall(r.started_us),
        ended: wall(r.ended_us),
        end: None,
        run: run(r),
        run_id: Some(r.run_id().to_bytes()),
    }
}

/// `us` microseconds after the epoch; `None` for 0, which a record keeps for a time that
/// was not reported.
fn wall(us: u64) -> Option<WallTime> {
    if us == 0 {
        None
    } else {
        Some(WallTime::from_unix_micros(us))
    }
}

/// What the attempt's lease measured, if the record has anything but the queue time: a
/// lease the scheduler took no report for has none.
fn run(r: &RunRecord) -> Option<Box<Run>> {
    let t = r.timestamps.clone().unwrap_or_default();
    let times = ReapiTimes {
        queued: wall(t.queued_us),
        worker_start: wall(t.worker_start_us),
        input_fetch_start: wall(t.input_fetch_start_us),
        input_fetch_completed: wall(t.input_fetch_completed_us),
        execution_start: wall(t.execution_start_us),
        execution_completed: wall(t.execution_completed_us),
        output_upload_start: wall(t.output_upload_start_us),
        output_upload_completed: wall(t.output_upload_completed_us),
        worker_completed: wall(t.worker_completed_us),
    };
    let measured = times.named().iter().skip(1).any(|(_, t)| t.is_some());
    let bytes = |b: u64| (b != 0).then_some(b);
    let usage = r.cgroup.as_ref().map(|c| RunUsage {
        wall_micros: c.wall_us,
        cpu_usage_micros: Some(c.cpu_usage_us),
        cpu_user_micros: Some(c.cpu_user_us),
        cpu_system_micros: Some(c.cpu_system_us),
        cpu_periods: Some(c.periods),
        cpu_throttled_periods: Some(c.throttled_periods),
        cpu_throttled_micros: Some(c.throttled_us),
        memory_peak_bytes: Some(c.memory_peak_bytes),
        numa: c
            .numa_bytes
            .iter()
            .zip(0..)
            .filter(|(b, _)| **b != 0)
            .map(|(b, node)| NumaBytes { node, bytes: *b })
            .collect(),
    });
    let booked = bytes(r.booked_memory_bytes);
    let cap = bytes(r.cap_bytes);
    (measured || usage.is_some() || booked.is_some() || cap.is_some()).then(|| {
        Box::new(Run {
            times,
            usage,
            booked_memory_bytes: booked,
            cap_bytes: cap,
        })
    })
}

/// The shape `r` names; `None` when it names none (no key, no configuration, no target).
fn shape(r: &RunRecord) -> Option<Box<ActionShape>> {
    let cfg = (r.cfg != "-" && !r.cfg.is_empty()).then(|| r.cfg.clone());
    let kind = match r.target_kind {
        1 => Some(TargetKind::Target),
        2 => Some(TargetKind::Anon),
        3 => Some(TargetKind::Bxl),
        _ => None,
    };
    let target = kind.filter(|_| !r.target.is_empty()).map(|kind| Target {
        kind,
        label: Text::sent(r.target.clone()),
    });
    (r.shape != 0 || cfg.is_some() || target.is_some() || r.test).then(|| {
        Box::new(ActionShape {
            key: r.shape,
            cfg,
            target,
            test: r.test,
        })
    })
}

/// The result, logs and outputs of the run `r` that ended its action, if it kept any.
fn listing(r: &RunRecord) -> Option<Listing> {
    let (result, stdout, stderr) = (
        digest(r.result.as_ref()),
        digest(r.stdout.as_ref()),
        digest(r.stderr.as_ref()),
    );
    let outputs: Vec<Output> = r
        .outputs
        .iter()
        .filter_map(|o| {
            Some(Output {
                path: o.path.clone(),
                digest: digest(o.digest.as_ref())?,
                executable: false,
                directory: o.is_dir,
            })
        })
        .collect();
    let any = result.is_some() || stdout.is_some() || stderr.is_some() || !outputs.is_empty();
    any.then(|| Listing {
        result,
        exit_code: (r.outcome() == RunOutcome::Ok || r.outcome() == RunOutcome::Failed)
            .then_some(r.exit_code),
        outputs,
        stdout,
        stderr,
        usage: None,
    })
}

/// The SHA-256 digest a record holds; `None` for none, or a hash that is not 32 bytes.
fn digest(d: Option<&WireDigest>) -> Option<Digest> {
    let d = d?;
    let hash = <[u8; 32]>::try_from(d.hash.as_slice()).ok()?;
    Some(Digest::new(DigestFunction::Sha256, hash, d.size))
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
