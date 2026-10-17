//! What a row keeps beyond the built fields of section 3.2 (`docs/design/ui-data.md`
//! 4.2, phase U1): the action's shape and the target its own paths name, and per
//! attempt what its lease's report measured. In memory only: `kbf.invocation.v1`
//! does not carry them yet.

use kbf_sched::RequeueReason;
use kbf_types::Failure;

use crate::event::WallTime;
use crate::record::{ActionRow, Attempt, AttemptEnd, Text};

/// What the server read from an action's `Command` that stays the same across builds:
/// its shape key, the configuration hash beside it and the target its own `buck-out`
/// paths name (issue #796). The client's `RequestMetadata` never enters it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionShape {
    /// The shape key: 8 bytes, big-endian.
    pub key: u64,
    /// The configuration hash an output path named.
    pub cfg: Option<String>,
    /// The target the action's paths name; `None` when they name none.
    pub target: Option<Target>,
    /// Whether the action is a test run: it writes under `buck-out/<v>/test/execution/`.
    pub test: bool,
}

impl ActionShape {
    /// The key as the server's logs write it: 16 lowercase hex digits.
    #[must_use]
    pub fn key_text(&self) -> String {
        format!("{:016x}", self.key)
    }

    /// What it holds beyond itself: its texts.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.cfg.as_ref().map_or(0, String::len)
            + self.target.as_ref().map_or(0, |t| t.label.value.len())
    }
}

/// The owner an action's paths name: `cell//pkg:name`, and its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Which kind of owner.
    pub kind: TargetKind,
    /// Its label, capped as a `RequestMetadata` text is.
    pub label: Text,
}

/// Whose outputs a `buck-out` path holds, as buck2's roots tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// A rule's target (`art`, `gen`).
    Target,
    /// An anonymous target (`art-anon`, `gen-anon`).
    Anon,
    /// A BXL function (`art-bxl`, `gen-bxl`).
    Bxl,
}

impl TargetKind {
    /// The kind's name, as the server's logs and the read API write it: `target`,
    /// `anon`, `bxl`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Anon => "anon",
            Self::Bxl => "bxl",
        }
    }
}

/// What one lease's report measured, as the server took it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Run {
    /// The nine `ExecutedActionMetadata` timestamps.
    pub times: ReapiTimes,
    /// The `ResourceUsage` the report carried, if any.
    pub usage: Option<RunUsage>,
    /// For a run killed for memory: its memory booking, in bytes.
    pub booked_memory_bytes: Option<u64>,
    /// For a run killed at its own limit: the memory of the largest node that could
    /// run it when the kill was committed, in bytes.
    pub cap_bytes: Option<u64>,
}

impl Run {
    /// What it holds beyond itself.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.usage
            .as_ref()
            .map_or(0, |u| u.numa.len() * size_of::<NumaBytes>())
    }
}

/// REAPI's `ExecutedActionMetadata` times of one run; `None` for one not reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct ReapiTimes {
    pub queued: Option<WallTime>,
    pub worker_start: Option<WallTime>,
    pub input_fetch_start: Option<WallTime>,
    pub input_fetch_completed: Option<WallTime>,
    pub execution_start: Option<WallTime>,
    pub execution_completed: Option<WallTime>,
    pub output_upload_start: Option<WallTime>,
    pub output_upload_completed: Option<WallTime>,
    pub worker_completed: Option<WallTime>,
}

impl ReapiTimes {
    /// The times by their REAPI names (without `_timestamp`), in the order REAPI lists
    /// them.
    #[must_use]
    pub const fn named(&self) -> [(&'static str, Option<WallTime>); 9] {
        [
            ("queued", self.queued),
            ("worker_start", self.worker_start),
            ("input_fetch_start", self.input_fetch_start),
            ("input_fetch_completed", self.input_fetch_completed),
            ("execution_start", self.execution_start),
            ("execution_completed", self.execution_completed),
            ("output_upload_start", self.output_upload_start),
            ("output_upload_completed", self.output_upload_completed),
            ("worker_completed", self.worker_completed),
        ]
    }
}

/// `kbf.worker.v1.ResourceUsage` with its cgroup block, whole. A cgroup field that is
/// `None` was not readable: absent is not zero.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct RunUsage {
    /// Wall time from the action's start to its exit, in microseconds.
    pub wall_micros: u64,
    pub cpu_usage_micros: Option<u64>,
    pub cpu_user_micros: Option<u64>,
    pub cpu_system_micros: Option<u64>,
    pub cpu_periods: Option<u64>,
    pub cpu_throttled_periods: Option<u64>,
    pub cpu_throttled_micros: Option<u64>,
    pub memory_peak_bytes: Option<u64>,
    /// Bytes per NUMA node, as the daemon reported them.
    pub numa: Vec<NumaBytes>,
}

impl RunUsage {
    /// CPU time over wall time: how many CPUs the run kept busy on average. `None`
    /// without a cgroup CPU time or with no wall time.
    #[must_use]
    pub fn parallelism(&self) -> Option<f64> {
        let cpu = self.cpu_usage_micros?;
        (self.wall_micros > 0).then(|| cpu as f64 / self.wall_micros as f64)
    }
}

/// Bytes of one NUMA node.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NumaBytes {
    /// The node.
    pub node: u32,
    /// Its bytes.
    pub bytes: u64,
}

/// How an attempt was killed for memory, if it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKillKind {
    /// Not killed for memory.
    None,
    /// The action passed its own memory limit.
    OwnLimit,
    /// The node killed it under memory pressure, below its own limit.
    NodePressure,
}

impl MemoryKillKind {
    /// Every kind, by the name the API writes: `none`, `own_limit`, `node_pressure`.
    pub const NAMED: [(Self, &'static str); 3] = [
        (Self::None, "none"),
        (Self::OwnLimit, "own_limit"),
        (Self::NodePressure, "node_pressure"),
    ];

    /// The kind's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        Self::NAMED
            .iter()
            .find(|(k, _)| *k == self)
            .map_or("", |(_, n)| n)
    }
}

impl Attempt {
    /// How it was killed for memory, read from how it ended; `None` while it is held.
    #[must_use]
    pub const fn memory_kill(&self) -> Option<MemoryKillKind> {
        Some(match &self.end {
            None => return None,
            Some(
                AttemptEnd::Requeued(RequeueReason::OutOfMemory { .. })
                | AttemptEnd::Failed(Failure::OutOfMemory, _),
            ) => MemoryKillKind::OwnLimit,
            Some(
                AttemptEnd::Requeued(RequeueReason::NodeMemoryPressure)
                | AttemptEnd::Failed(Failure::NodeMemoryPressure, _),
            ) => MemoryKillKind::NodePressure,
            Some(_) => MemoryKillKind::None,
        })
    }
}

/// How long an action's phases took, in microseconds; `None` for one not measured.
///
/// Every phase but `queue` is measured only once a lease has ended the operation
/// (its end accepted or failed): until then a requeued lease's end is not the last
/// one, and its run not the one that counts. So these four never change once set;
/// `queue` only grows, by each lease granted after a requeue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Durations {
    /// From its first lease's queueing to the end of the lease that ended the
    /// operation: the whole time the farm held it, requeues included. `None` until a
    /// lease ends it, and for an operation that ended with no lease ending it (given up
    /// while queued again).
    pub wall: Option<u64>,
    /// Queued to granted, summed over its leases so far. `None` without a lease.
    pub queue: Option<u64>,
    /// `execution_start` to `execution_completed` of the run of the lease that ended
    /// the operation.
    pub execution: Option<u64>,
    /// `input_fetch_start` to `input_fetch_completed` of that run.
    pub input_fetch: Option<u64>,
    /// `output_upload_start` to `output_upload_completed` of that run.
    pub output_upload: Option<u64>,
}

/// A phase [`Durations`] measures, by which a list is sorted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// [`Durations::wall`].
    Wall,
    /// [`Durations::execution`].
    Execution,
    /// [`Durations::queue`].
    Queue,
    /// [`Durations::input_fetch`].
    InputFetch,
    /// [`Durations::output_upload`].
    OutputUpload,
}

impl Phase {
    /// Every phase, by the name the API writes its milliseconds under.
    pub const NAMED: [(Self, &'static str); 5] = [
        (Self::Wall, "wall_ms"),
        (Self::Execution, "execution_ms"),
        (Self::Queue, "queue_ms"),
        (Self::InputFetch, "input_fetch_ms"),
        (Self::OutputUpload, "output_upload_ms"),
    ];

    /// The phase's name.
    #[must_use]
    pub fn name(self) -> &'static str {
        Self::NAMED
            .iter()
            .find(|(p, _)| *p == self)
            .map_or("", |(_, n)| n)
    }
}

impl Durations {
    /// The duration of `phase`.
    #[must_use]
    pub const fn of(&self, phase: Phase) -> Option<u64> {
        match phase {
            Phase::Wall => self.wall,
            Phase::Execution => self.execution,
            Phase::Queue => self.queue,
            Phase::InputFetch => self.input_fetch,
            Phase::OutputUpload => self.output_upload,
        }
    }
}

/// `to - from`, when both were measured and `to` is not before `from`.
fn between(from: Option<WallTime>, to: Option<WallTime>) -> Option<u64> {
    to?.unix_micros().checked_sub(from?.unix_micros())
}

impl ActionRow {
    /// How long its phases took (see [`Durations`]).
    #[must_use]
    pub fn durations(&self) -> Durations {
        let (Some(first), Some(last)) = (self.attempts.first(), self.attempts.last()) else {
            return Durations::default();
        };
        let queue = self
            .attempts
            .iter()
            .map(|a| {
                a.granted
                    .unix_micros()
                    .saturating_sub(a.queued.unix_micros())
            })
            .fold(0_u64, u64::saturating_add);
        let ended = matches!(
            last.end,
            Some(AttemptEnd::Accepted | AttemptEnd::Failed(..))
        );
        let Some(ended_at) = last.ended.filter(|_| ended) else {
            return Durations {
                queue: Some(queue),
                ..Durations::default()
            };
        };
        let times = last.run.as_ref().map(|r| r.times).unwrap_or_default();
        Durations {
            wall: between(Some(first.queued), Some(ended_at)),
            queue: Some(queue),
            execution: between(times.execution_start, times.execution_completed),
            input_fetch: between(times.input_fetch_start, times.input_fetch_completed),
            output_upload: between(times.output_upload_start, times.output_upload_completed),
        }
    }
}
