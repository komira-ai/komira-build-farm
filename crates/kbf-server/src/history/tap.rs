//! What the recorder's task hands the history writer: a run record for each attempt
//! that settled in a row, a hit record for each cache hit, and a close for each record
//! that went idle. The tap mints the run id (so the row can name the run at once) and
//! offers the events to the writer's bounded queue; it never waits for the writer, the
//! object store or the state store.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use kbf_front::Tap;
use kbf_history::records::{
    Cgroup, HitHow, HitRecord, MemoryKill, Output, RunOutcome, RunRecord, Timestamps, WireDigest,
};
use kbf_history::{Event, EventQueue, InvRef, Minter};
use kbf_invocation::{
    ActionRow, Attempt, AttemptEnd, Ended, EndedKind, How, MemoryKillKind, NumaBytes, Outcome,
    RecordKey, TargetKind, WallTime,
};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, Failure};

use super::tails::{Job, Tails};

/// The invocations the tap remembers having offered a record for, at most: the writer
/// holds no more than its own bound of them (`kbf_history::Limits::invocations`), so a
/// longer list could only name invocations it has forgotten. One past it is dropped from
/// the list, and its records leave by the writer's age cut instead of at a close.
const OPEN_CAP: usize = 4096;

/// The NUMA node numbers a run record keeps memory for: a daemon reports a handful, and
/// the list is indexed by node.
const MAX_NUMA_NODES: u32 = 64;

/// The runs [`RunIndex`] remembers, at most. Run ids are time-ordered, so past it the
/// oldest is forgotten, and its run is read from history instead of the recorder.
const RUN_INDEX_CAP: usize = 1 << 16;

/// Where the live record of each recently minted run id is: its record's key and the
/// row. It is bounded ([`RUN_INDEX_CAP`], some 6 MiB), and a hint: the reader checks that
/// the row still holds the run before it answers from it.
#[derive(Debug, Default)]
pub struct RunIndex(Mutex<BTreeMap<[u8; 12], (RecordKey, usize)>>);

impl RunIndex {
    fn map(&self) -> MutexGuard<'_, BTreeMap<[u8; 12], (RecordKey, usize)>> {
        // A map; a panic elsewhere leaves it whole.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that `id` was minted for an attempt of row `row` of `key`'s record.
    pub(crate) fn note(&self, id: [u8; 12], key: RecordKey, row: usize) {
        let mut map = self.map();
        map.insert(id, (key, row));
        while map.len() > RUN_INDEX_CAP {
            map.pop_first();
        }
    }

    /// The record and row `id` was minted for, if still remembered.
    pub(crate) fn find(&self, id: &[u8; 12]) -> Option<(RecordKey, usize)> {
        self.map().get(id).cloned()
    }
}

/// The record names (`<id>` or `<id>~<n>`) an event was offered for since their last
/// close, at most [`OPEN_CAP`]. Shared with the tail workers, which close an invocation
/// again when a failed run reaches the writer after its close.
#[derive(Debug, Default)]
pub(super) struct OpenSet(Mutex<BTreeSet<String>>);

impl OpenSet {
    fn set(&self) -> MutexGuard<'_, BTreeSet<String>> {
        // A set of names; a panic elsewhere leaves it whole.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes that `name` has events in the writer, to be closed with it.
    pub(super) fn add(&self, name: &str) {
        let mut open = self.set();
        if open.len() >= OPEN_CAP && !open.contains(name) {
            open.pop_first();
        }
        open.insert(name.to_owned());
    }

    /// Forgets `name`; whether it was open.
    fn remove(&self, name: &str) -> bool {
        self.set().remove(name)
    }

    /// Whether `name` is open.
    pub(super) fn contains(&self, name: &str) -> bool {
        self.set().contains(name)
    }

    /// Forgets every name, returning them.
    fn take(&self) -> BTreeSet<String> {
        std::mem::take(&mut *self.set())
    }
}

/// The tap the recorder task calls, over the history writer's queue and the minter.
#[derive(Debug)]
pub struct HistoryTap {
    queue: Arc<EventQueue>,
    minter: Arc<Minter>,
    /// The invocations an event was offered for since their last close.
    open: Arc<OpenSet>,
    /// Where the live record of each run it minted is.
    runs: Arc<RunIndex>,
    /// The pool that reads a failed run's tails and copies its logs before its record is
    /// offered, once the server gave history a blob source ([`super::History::with_blobs`]);
    /// without it a failed run is offered at once with no tails, as every run is.
    tails: OnceLock<Tails>,
}

impl HistoryTap {
    pub(crate) fn new(queue: Arc<EventQueue>, minter: Arc<Minter>) -> Self {
        Self {
            queue,
            minter,
            open: Arc::new(OpenSet::default()),
            runs: Arc::new(RunIndex::default()),
            tails: OnceLock::new(),
        }
    }

    /// Gives the tap its pool. The first one stays: a second call changes nothing.
    pub(super) fn install_tails(&self, tails: Tails) {
        let _already = self.tails.set(tails);
    }

    /// Waits until no failed run is queued for the tail workers or being handled; at once
    /// without a pool.
    pub(super) async fn quiesce(&self) {
        if let Some(tails) = self.tails.get() {
            tails.quiesce().await;
        }
    }

    /// Whether a pool was installed.
    pub(super) fn has_tails(&self) -> bool {
        self.tails.get().is_some()
    }

    /// The names the tail workers check an invocation's close against.
    pub(super) fn open_set(&self) -> Arc<OpenSet> {
        Arc::clone(&self.open)
    }

    /// Where the live record of each run this tap minted is.
    pub(crate) fn runs(&self) -> Arc<RunIndex> {
        Arc::clone(&self.runs)
    }

    /// Notes that `name` has events in the writer, to be closed with it.
    fn opened(&self, name: &str) {
        self.open.add(name);
    }

    /// Offers a close of every invocation offered a record and not closed since: the
    /// writer flushes them before it stops.
    pub(super) fn close_all(&self) {
        for name in self.open.take() {
            self.queue.offer(Event::Close {
                inv: InvRef::named(&name),
            });
        }
    }

    /// The run of `row`'s attempt `i`, which settled at `at`: minted, queued and its
    /// id. `None` when the row has no such attempt, or the queue dropped the record
    /// (an id that names nothing is not kept).
    fn run(&self, ended: &Ended, row: &ActionRow, i: usize) -> Option<[u8; 12]> {
        let (name, at) = (ended.key.to_string(), ended.at);
        let name = name.as_str();
        let attempt = row.attempts.get(i)?;
        let inv = InvRef::named(name);
        let minted = self.minter.mint(&inv.key, at.unix_micros() / 1000);
        let mut record = run_record(row, attempt);
        record.set_run_id(minted.id);
        let mut job = Job {
            inv,
            record: Box::new(record),
            stream_opened_ms: minted.stream_opened_ms,
        };
        // A failed run goes to the tail workers, which offer it after reading its logs.
        // A full pool drops the tails, not the run: it is offered here, with none.
        if job.record.failed()
            && let Some(tails) = self.tails.get()
        {
            match tails.submit(job) {
                Ok(()) => return self.minted(name, ended, minted.id.to_bytes()),
                Err(back) => job = back,
            }
        }
        let queued = self.queue.offer(Event::Run {
            inv: job.inv,
            record: job.record,
            failed: None,
            stream_opened_ms: job.stream_opened_ms,
        });
        if !queued {
            return None;
        }
        self.minted(name, ended, minted.id.to_bytes())
    }

    /// Notes a run whose record was handed on, and returns its id.
    fn minted(&self, name: &str, ended: &Ended, id: [u8; 12]) -> Option<[u8; 12]> {
        self.opened(name);
        self.runs.note(id, ended.key.clone(), ended.row);
        Some(id)
    }
}

impl Tap for HistoryTap {
    fn ended(&self, ended: &Ended, row: &ActionRow) -> Option<[u8; 12]> {
        let name = ended.key.to_string();
        if ended.kind == EndedKind::Hit {
            let queued = self.queue.offer(Event::Hit {
                inv: InvRef::named(&name),
                record: hit_record(row, ended.at),
            });
            if queued {
                self.opened(&name);
            }
            return None;
        }
        ended.attempt.and_then(|i| self.run(ended, row, i))
    }

    fn idle(&self, key: &RecordKey) {
        let name = key.to_string();
        if self.open.remove(&name) {
            self.queue.offer(Event::Close {
                inv: InvRef::named(&name),
            });
        }
    }
}

fn wire(d: &Digest) -> WireDigest {
    WireDigest::new(d.hash, d.size_bytes)
}

fn micros(t: Option<WallTime>) -> u64 {
    t.map_or(0, WallTime::unix_micros)
}

/// The hit record of `row`, answered at `at`.
fn hit_record(row: &ActionRow, at: WallTime) -> HitRecord {
    let how = match row.how {
        How::CacheHit => HitHow::CacheHit,
        How::CacheHitExecute => HitHow::CacheHitExecute,
        How::Joined | How::Ran => HitHow::Unspecified,
    };
    HitRecord {
        time_ms: at.unix_micros() / 1000,
        instance: row.instance.clone(),
        action: Some(wire(&row.action)),
        how: how.into(),
        row: 0,
    }
}

/// The run record of `attempt`, which ended in `row`: what the row and the attempt hold.
/// The run id is the caller's. What the in-memory row has no source for is left at its
/// default: the input root, the wait reason, the partition, the farm reruns and the
/// presubmit time.
pub(crate) fn run_record(row: &ActionRow, attempt: &Attempt) -> RunRecord {
    let run = attempt.run.as_deref();
    let (outcome, exit_code) = outcome_of(row, attempt);
    let kill = attempt.memory_kill().unwrap_or(MemoryKillKind::None);
    let memory_kill = match kill {
        MemoryKillKind::None => MemoryKill::None,
        MemoryKillKind::OwnLimit => MemoryKill::OwnLimit,
        MemoryKillKind::NodePressure => MemoryKill::NodePressure,
    };
    let (reason, booked_from_end, raised) = match &attempt.end {
        Some(AttemptEnd::Failed(_, why)) => (why.clone(), None, 0),
        Some(AttemptEnd::Requeued(why)) => {
            let (booked, raised) = match why {
                RequeueReason::OutOfMemory { booked, raised } => (Some(*booked), *raised),
                _ => (None, 0),
            };
            (why.to_string(), booked, raised)
        }
        Some(AttemptEnd::Accepted) | None => (String::new(), None, 0),
    };
    let booked = run
        .and_then(|r| r.booked_memory_bytes)
        .or(booked_from_end)
        .unwrap_or(0);
    let memory_peak = run
        .and_then(|r| r.usage.as_ref())
        .and_then(|u| u.memory_peak_bytes)
        .unwrap_or(0);
    let killed = kill != MemoryKillKind::None;
    // The result belongs to the attempt that ended the operation: an earlier attempt
    // that was given up has no result, logs or outputs of its own.
    let final_attempt = !matches!(attempt.end, Some(AttemptEnd::Requeued(_)));
    let listing = row.listing.as_ref().filter(|_| final_attempt);
    let log = |d: Option<&Digest>| d.map(wire);
    let times = run.map(|r| r.times).unwrap_or_default();
    let first_queued = row.attempts.first().map_or(attempt.queued, |a| a.queued);
    let target = row.shape.as_deref().and_then(|s| s.target.as_ref());
    RunRecord {
        instance: row.instance.clone(),
        action: Some(wire(&row.action)),
        result: listing.and_then(|l| l.result.as_ref()).map(wire),
        command: row.command.as_ref().map(wire),
        stdout: log(listing.and_then(|l| l.stdout.as_ref())),
        stderr: log(listing.and_then(|l| l.stderr.as_ref())),
        shape: row.shape.as_deref().map_or(0, |s| s.key),
        cfg: row
            .shape
            .as_deref()
            .and_then(|s| s.cfg.clone())
            .unwrap_or_else(|| "-".to_owned()),
        test: row.shape.as_deref().is_some_and(|s| s.test),
        attempt_queued_us: micros(Some(attempt.queued)),
        started_us: micros(attempt.started),
        ended_us: micros(attempt.ended),
        target: target.map(|t| t.label.value.clone()).unwrap_or_default(),
        target_kind: target.map_or(0, |t| kind_number(t.kind)),
        node: attempt.worker.as_str().to_owned(),
        lease_epoch: attempt.lease.term,
        lease_seq: attempt.lease.seq,
        exit_code,
        outcome: outcome.into(),
        memory_kill: memory_kill.into(),
        booked_memory_bytes: booked,
        raised_memory_bytes: raised,
        cap_bytes: run.and_then(|r| r.cap_bytes).unwrap_or(0),
        timestamps: Some(Timestamps {
            queued_us: micros(times.queued.or(Some(first_queued))),
            granted_us: micros(Some(attempt.granted)),
            worker_start_us: micros(times.worker_start),
            input_fetch_start_us: micros(times.input_fetch_start),
            input_fetch_completed_us: micros(times.input_fetch_completed),
            execution_start_us: micros(times.execution_start),
            execution_completed_us: micros(times.execution_completed),
            output_upload_start_us: micros(times.output_upload_start),
            output_upload_completed_us: micros(times.output_upload_completed),
            worker_completed_us: micros(times.worker_completed),
        }),
        cgroup: run.and_then(|r| r.usage.as_ref()).map(|u| Cgroup {
            cpu_usage_us: u.cpu_usage_micros.unwrap_or(0),
            cpu_user_us: u.cpu_user_micros.unwrap_or(0),
            cpu_system_us: u.cpu_system_micros.unwrap_or(0),
            periods: u.cpu_periods.unwrap_or(0),
            throttled_periods: u.cpu_throttled_periods.unwrap_or(0),
            throttled_us: u.cpu_throttled_micros.unwrap_or(0),
            memory_peak_bytes: u.memory_peak_bytes.unwrap_or(0),
            numa_bytes: numa(&u.numa),
            wall_us: u.wall_micros,
        }),
        outputs: listing.map_or_else(Vec::new, |l| {
            l.outputs
                .iter()
                .map(|o| Output {
                    path: o.path.clone(),
                    digest: Some(wire(&o.digest)),
                    is_dir: o.directory,
                })
                .collect()
        }),
        reason,
        memory_used_bytes: if killed { memory_peak } else { 0 },
        memory_limit_bytes: if kill == MemoryKillKind::OwnLimit {
            booked
        } else {
            0
        },
        ..RunRecord::default()
    }
}

/// What a run record's `target_kind` holds for `kind`: 1 a rule's target, 2 an anonymous
/// target, 3 a BXL function.
const fn kind_number(kind: TargetKind) -> u32 {
    match kind {
        TargetKind::Target => 1,
        TargetKind::Anon => 2,
        TargetKind::Bxl => 3,
    }
}

/// How `attempt` ended, as a run record says it, with the command's exit code: an
/// accepted result is the row's answer (exit 0 is `Ok`); a failure the worker reported or
/// a lease given up is told by its kind.
fn outcome_of(row: &ActionRow, attempt: &Attempt) -> (RunOutcome, i32) {
    match &attempt.end {
        Some(AttemptEnd::Accepted) => match &row.outcome {
            Some(Outcome::Ok { exit_code: 0 }) => (RunOutcome::Ok, 0),
            Some(Outcome::Ok { exit_code }) => (RunOutcome::Failed, *exit_code),
            Some(Outcome::TimedOut) => (RunOutcome::TimedOut, 0),
            Some(Outcome::Failed { .. } | Outcome::Refused(_)) => (RunOutcome::Failed, 0),
            None => (RunOutcome::Unspecified, 0),
        },
        Some(AttemptEnd::Failed(failure, _)) => {
            let outcome = match failure {
                Failure::Timeout => RunOutcome::TimedOut,
                Failure::Infra => RunOutcome::Infra,
                Failure::Invalid | Failure::MissingInput => RunOutcome::Failed,
                Failure::OutOfMemory | Failure::NodeMemoryPressure => RunOutcome::MemoryKilled,
                Failure::Lost(_) => RunOutcome::Lost,
            };
            (outcome, 0)
        }
        Some(AttemptEnd::Requeued(reason)) => {
            let outcome = match reason {
                RequeueReason::OutOfMemory { .. } | RequeueReason::NodeMemoryPressure => {
                    RunOutcome::MemoryKilled
                }
                RequeueReason::Infra => RunOutcome::Infra,
                RequeueReason::Silent
                | RequeueReason::Replaced
                | RequeueReason::Reconnected
                | RequeueReason::NotStarted
                | RequeueReason::SessionSwitch
                | RequeueReason::Restarted
                | RequeueReason::Lost(_) => RunOutcome::Lost,
            };
            (outcome, 0)
        }
        None => (RunOutcome::Unspecified, 0),
    }
}

/// Memory per NUMA node, indexed by node number, for the nodes a run record keeps.
fn numa(nodes: &[NumaBytes]) -> Vec<u64> {
    let kept = || nodes.iter().filter(|n| n.node < MAX_NUMA_NODES);
    let Some(top) = kept().map(|n| n.node).max() else {
        return Vec::new();
    };
    let mut bytes = vec![0; top as usize + 1];
    for n in kept() {
        bytes[n.node as usize] = n.bytes;
    }
    bytes
}

#[cfg(test)]
#[path = "tap_tests.rs"]
mod tests;
