//! The records history keeps, and the values of its `ui/` index rows, as protobuf
//! messages.
//!
//! Every record carries the `instance` it was asked under, so a read filters by the
//! instances its caller may see. Field numbers are the wire format: a field is added
//! under a new number and never renumbered.

use crate::run_id::RunId;

/// The longest tail kept of one log stream, after redaction.
pub const TAIL_BYTES: usize = 16 * 1024;

/// The longest command rendering kept, all of its strings together: the 4 KiB the
/// design sizes the failed-run section on (ui-data.md 5.3).
pub const COMMAND_BYTES: usize = 4 * 1024;

/// A digest on the wire: the SHA-256 and the size in bytes.
#[derive(Clone, PartialEq, Eq, Hash, prost::Message)]
pub struct WireDigest {
    /// The 32 hash bytes.
    #[prost(bytes = "vec", tag = "1")]
    pub hash: Vec<u8>,
    /// The size of the content.
    #[prost(uint64, tag = "2")]
    pub size: u64,
}

impl WireDigest {
    /// The digest of `size` bytes whose SHA-256 is `hash`.
    #[must_use]
    pub fn new(hash: [u8; 32], size: u64) -> WireDigest {
        WireDigest {
            hash: hash.to_vec(),
            size,
        }
    }

    /// The first 8 hash bytes as a number: the "digest16" (16 hex digits) the chunk's
    /// digest table is keyed by. A hash shorter than 8 bytes is padded with zeros.
    #[must_use]
    pub fn prefix16(&self) -> u64 {
        let mut b = [0; 8];
        let n = self.hash.len().min(8);
        b[..n].copy_from_slice(&self.hash[..n]);
        u64::from_be_bytes(b)
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum RunOutcome {
    /// Not stated (an old or broken record).
    Unspecified = 0,
    /// The command exited 0.
    Ok = 1,
    /// The command exited non-zero.
    Failed = 2,
    /// The command ran past its timeout.
    TimedOut = 3,
    /// The run was killed for memory.
    MemoryKilled = 4,
    /// The farm failed the run (infrastructure).
    Infra = 5,
    /// The lease was lost.
    Lost = 6,
    /// The client cancelled.
    Cancelled = 7,
}

/// Which memory kill ended a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum MemoryKill {
    /// No memory kill.
    None = 0,
    /// The action passed its own limit.
    OwnLimit = 1,
    /// The node ran short and killed it.
    NodePressure = 2,
}

/// The nine REAPI timestamps of a run plus the farm's grant, in microseconds after the
/// Unix epoch (0: not reported).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Timestamps {
    /// First submission, kept across requeues.
    #[prost(uint64, tag = "1")]
    pub queued_us: u64,
    /// The lease was granted.
    #[prost(uint64, tag = "2")]
    pub granted_us: u64,
    /// `worker_start_timestamp`.
    #[prost(uint64, tag = "3")]
    pub worker_start_us: u64,
    /// `input_fetch_start_timestamp`.
    #[prost(uint64, tag = "4")]
    pub input_fetch_start_us: u64,
    /// `input_fetch_completed_timestamp`.
    #[prost(uint64, tag = "5")]
    pub input_fetch_completed_us: u64,
    /// `execution_start_timestamp`.
    #[prost(uint64, tag = "6")]
    pub execution_start_us: u64,
    /// `execution_completed_timestamp`.
    #[prost(uint64, tag = "7")]
    pub execution_completed_us: u64,
    /// `output_upload_start_timestamp`.
    #[prost(uint64, tag = "8")]
    pub output_upload_start_us: u64,
    /// `output_upload_completed_timestamp`.
    #[prost(uint64, tag = "9")]
    pub output_upload_completed_us: u64,
    /// `worker_completed_timestamp`.
    #[prost(uint64, tag = "10")]
    pub worker_completed_us: u64,
}

/// The cgroup block the daemon reports for a run.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Cgroup {
    /// CPU time, microseconds.
    #[prost(uint64, tag = "1")]
    pub cpu_usage_us: u64,
    /// User CPU time, microseconds.
    #[prost(uint64, tag = "2")]
    pub cpu_user_us: u64,
    /// System CPU time, microseconds.
    #[prost(uint64, tag = "3")]
    pub cpu_system_us: u64,
    /// CFS periods.
    #[prost(uint64, tag = "4")]
    pub periods: u64,
    /// Periods throttled.
    #[prost(uint64, tag = "5")]
    pub throttled_periods: u64,
    /// Time throttled, microseconds.
    #[prost(uint64, tag = "6")]
    pub throttled_us: u64,
    /// Peak memory, bytes.
    #[prost(uint64, tag = "7")]
    pub memory_peak_bytes: u64,
    /// Memory per NUMA node, bytes, by node number.
    #[prost(uint64, repeated, tag = "8")]
    pub numa_bytes: Vec<u64>,
}

/// One output of a run.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Output {
    /// The path the action named.
    #[prost(string, tag = "1")]
    pub path: String,
    /// The file's digest, or the directory's tree digest.
    #[prost(message, optional, tag = "2")]
    pub digest: Option<WireDigest>,
    /// A directory, not a file.
    #[prost(bool, tag = "3")]
    pub is_dir: bool,
}

/// One attempt that ran (or was granted) on a node: the run record.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct RunRecord {
    /// The run id's commit time, milliseconds.
    #[prost(uint64, tag = "1")]
    pub time_ms: u64,
    /// The run id's stream.
    #[prost(uint32, tag = "2")]
    pub stream: u32,
    /// The run id's sequence in its stream.
    #[prost(uint32, tag = "3")]
    pub seq: u32,
    /// The REAPI instance name the run was asked under.
    #[prost(string, tag = "4")]
    pub instance: String,
    /// The action digest.
    #[prost(message, optional, tag = "5")]
    pub action: Option<WireDigest>,
    /// The ActionResult blob, when one was stored.
    #[prost(message, optional, tag = "6")]
    pub result: Option<WireDigest>,
    /// The Command blob.
    #[prost(message, optional, tag = "7")]
    pub command: Option<WireDigest>,
    /// The input root.
    #[prost(message, optional, tag = "8")]
    pub input_root: Option<WireDigest>,
    /// stdout, when the run has logs.
    #[prost(message, optional, tag = "9")]
    pub stdout: Option<WireDigest>,
    /// stderr, when the run has logs.
    #[prost(message, optional, tag = "10")]
    pub stderr: Option<WireDigest>,
    /// The 16-hex shape key.
    #[prost(fixed64, tag = "11")]
    pub shape: u64,
    /// The configuration hash taken out of the output paths (`-` when none).
    #[prost(string, tag = "12")]
    pub cfg: String,
    /// The node that ran it.
    #[prost(string, tag = "13")]
    pub node: String,
    /// The lease's epoch (term).
    #[prost(uint64, tag = "14")]
    pub lease_epoch: u64,
    /// The lease's sequence.
    #[prost(uint64, tag = "15")]
    pub lease_seq: u64,
    /// The command's exit code.
    #[prost(int32, tag = "16")]
    pub exit_code: i32,
    /// How the run ended.
    #[prost(enumeration = "RunOutcome", tag = "17")]
    pub outcome: i32,
    /// The memory kill that ended it, if any.
    #[prost(enumeration = "MemoryKill", tag = "18")]
    pub memory_kill: i32,
    /// The memory booked for the attempt, bytes.
    #[prost(uint64, tag = "19")]
    pub booked_memory_bytes: u64,
    /// The booking the next attempt gets after a memory kill, bytes.
    #[prost(uint64, tag = "20")]
    pub raised_memory_bytes: u64,
    /// The action's own limit at an own-limit kill, bytes.
    #[prost(uint64, tag = "21")]
    pub cap_bytes: u64,
    /// The last reason the scheduler gave while the action waited.
    #[prost(string, tag = "22")]
    pub wait_reason: String,
    /// Platform, size class and QoS of the queue it waited in.
    #[prost(string, tag = "23")]
    pub partition: String,
    /// Farm reruns used.
    #[prost(uint32, tag = "24")]
    pub farm_runs: u32,
    /// Farm reruns allowed.
    #[prost(uint32, tag = "25")]
    pub farm_budget: u32,
    /// Server time before the offer (the missing-inputs walk), microseconds.
    #[prost(uint64, tag = "26")]
    pub presubmit_us: u64,
    /// The timestamps.
    #[prost(message, optional, tag = "27")]
    pub timestamps: Option<Timestamps>,
    /// The cgroup block.
    #[prost(message, optional, tag = "28")]
    pub cgroup: Option<Cgroup>,
    /// The outputs listing.
    #[prost(message, repeated, tag = "29")]
    pub outputs: Vec<Output>,
    /// The daemon's reason text for an infrastructure end (at most 1 KiB).
    #[prost(string, tag = "30")]
    pub reason: String,
    /// The memory the run used when it was killed, bytes.
    #[prost(uint64, tag = "31")]
    pub memory_used_bytes: u64,
    /// The limit it was killed at, bytes.
    #[prost(uint64, tag = "32")]
    pub memory_limit_bytes: u64,
    /// The action is a test run (its shape names a test execution root). Added after
    /// the first chunks were written: a record without it reads `false`.
    #[prost(bool, tag = "33")]
    pub test: bool,
    /// The target label the run's shape carried (Bazel names it); empty when it had
    /// none. Added after the first chunks were written: a record without it reads empty.
    #[prost(string, tag = "34")]
    pub target: String,
}

impl RunRecord {
    /// The run's id.
    #[must_use]
    pub fn run_id(&self) -> RunId {
        RunId::new(self.time_ms, crate::run_id::StreamId(self.stream), self.seq)
    }

    /// Sets the id fields from `id`.
    pub fn set_run_id(&mut self, id: RunId) {
        self.time_ms = id.time_ms();
        self.stream = id.stream().0;
        self.seq = id.seq();
    }

    /// Whether the run failed (anything but [`RunOutcome::Ok`]).
    #[must_use]
    pub fn failed(&self) -> bool {
        self.outcome() != RunOutcome::Ok
    }

    /// Execution time (execution start to completion), microseconds; 0 when either
    /// timestamp is missing.
    #[must_use]
    pub fn execution_us(&self) -> u64 {
        self.timestamps.as_ref().map_or(0, |t| {
            t.execution_completed_us
                .saturating_sub(t.execution_start_us)
        })
    }

    /// Wall time (first queued to worker completed), microseconds; 0 when either
    /// timestamp is missing.
    #[must_use]
    pub fn wall_us(&self) -> u64 {
        self.timestamps
            .as_ref()
            .map_or(0, |t| t.worker_completed_us.saturating_sub(t.queued_us))
    }
}

/// How a cache hit was served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum HitHow {
    /// Not stated.
    Unspecified = 0,
    /// `GetActionResult` found it.
    CacheHit = 1,
    /// `Execute` found it in the action cache.
    CacheHitExecute = 2,
}

/// A cache hit: no run, but the build used the result.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct HitRecord {
    /// When the answer was sent, milliseconds.
    #[prost(uint64, tag = "1")]
    pub time_ms: u64,
    /// The instance asked.
    #[prost(string, tag = "2")]
    pub instance: String,
    /// The action digest.
    #[prost(message, optional, tag = "3")]
    pub action: Option<WireDigest>,
    /// How it was served.
    #[prost(enumeration = "HitHow", tag = "4")]
    pub how: i32,
    /// Its position among the chunk's hits (set by the writer).
    #[prost(uint32, tag = "5")]
    pub row: u32,
}

/// Why a tail is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum TailOmitted {
    /// The tail is present (possibly empty).
    No = 0,
    /// The log was too large to read for a tail.
    TooLarge = 1,
    /// The log could not be read.
    Unreadable = 2,
}

/// The last bytes of one log stream of a failed run, after redaction.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Tail {
    /// At most [`TAIL_BYTES`] bytes: the end of the log.
    #[prost(bytes = "vec", tag = "1")]
    pub bytes: Vec<u8>,
    /// The whole log's length.
    #[prost(uint64, tag = "2")]
    pub total_bytes: u64,
    /// Why there is no tail, if there is none.
    #[prost(enumeration = "TailOmitted", tag = "3")]
    pub omitted: i32,
}

impl Tail {
    /// The tail of `log`: its last [`TAIL_BYTES`] bytes.
    #[must_use]
    pub fn of(log: &[u8]) -> Tail {
        let from = log.len().saturating_sub(TAIL_BYTES);
        Tail {
            bytes: log[from..].to_vec(),
            total_bytes: log.len() as u64,
            omitted: TailOmitted::No.into(),
        }
    }
}

/// A decoded, redacted rendering of a run's Command: arguments as sent after redaction,
/// environment variable names only (never values).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct CommandRendering {
    /// The arguments.
    #[prost(string, repeated, tag = "1")]
    pub argv: Vec<String>,
    /// The environment variable names.
    #[prost(string, repeated, tag = "2")]
    pub env_names: Vec<String>,
    /// The working directory.
    #[prost(string, tag = "3")]
    pub working_directory: String,
    /// The output paths.
    #[prost(string, repeated, tag = "4")]
    pub output_paths: Vec<String>,
    /// Strings were dropped to keep the rendering within [`COMMAND_BYTES`].
    #[prost(bool, tag = "5")]
    pub truncated: bool,
}

impl CommandRendering {
    /// Drops strings from the end (output paths, then environment names, then
    /// arguments) until the rendering's strings fit [`COMMAND_BYTES`], and marks it
    /// truncated if any were dropped.
    pub fn bound(&mut self) {
        let mut size: usize = self.working_directory.len()
            + self.argv.iter().map(String::len).sum::<usize>()
            + self.env_names.iter().map(String::len).sum::<usize>()
            + self.output_paths.iter().map(String::len).sum::<usize>();
        for list in [&mut self.output_paths, &mut self.env_names, &mut self.argv] {
            while size > COMMAND_BYTES {
                let Some(s) = list.pop() else { break };
                size -= s.len();
                self.truncated = true;
            }
        }
        if size > COMMAND_BYTES {
            self.working_directory.clear();
            self.truncated = true;
        }
    }
}

/// The failed-run section of one run: its tails and command rendering, kept in its
/// own section of the chunk so a list of failures reads only those blocks.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct FailedRun {
    /// The run's sequence in its stream (the chunk holds one stream).
    #[prost(uint32, tag = "1")]
    pub seq: u32,
    /// The instance the run was asked under.
    #[prost(string, tag = "2")]
    pub instance: String,
    /// The stdout tail.
    #[prost(message, optional, tag = "3")]
    pub stdout: Option<Tail>,
    /// The stderr tail.
    #[prost(message, optional, tag = "4")]
    pub stderr: Option<Tail>,
    /// The command rendering.
    #[prost(message, optional, tag = "5")]
    pub command: Option<CommandRendering>,
}

/// A test outcome's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum TestStatus {
    /// Not stated.
    Unspecified = 0,
    /// Passed.
    Pass = 1,
    /// Failed.
    Fail = 2,
    /// Timed out.
    Timeout = 3,
    /// Killed at its own memory limit.
    OomKilled = 4,
    /// The farm failed it.
    Infra = 5,
}

/// One run of a test shape: what the 400-day outcome history keeps after the run record
/// is gone. Its key is (instance, shape, cfg, branch, time).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct OutcomeRecord {
    /// The instance.
    #[prost(string, tag = "1")]
    pub instance: String,
    /// The shape key.
    #[prost(fixed64, tag = "2")]
    pub shape: u64,
    /// The configuration hash.
    #[prost(string, tag = "3")]
    pub cfg: String,
    /// The branch, when a report or header named it.
    #[prost(string, tag = "4")]
    pub branch: String,
    /// The run's commit time, milliseconds.
    #[prost(uint64, tag = "5")]
    pub time_ms: u64,
    /// The run id's 12 bytes.
    #[prost(bytes = "vec", tag = "6")]
    pub run_id: Vec<u8>,
    /// The action digest (the flake proof: same digest, different status).
    #[prost(message, optional, tag = "7")]
    pub action: Option<WireDigest>,
    /// The commit, when the farm knows it.
    #[prost(string, tag = "8")]
    pub commit: String,
    /// The status.
    #[prost(enumeration = "TestStatus", tag = "9")]
    pub status: i32,
    /// The invocation key; empty for an id-less run.
    #[prost(string, tag = "10")]
    pub inv_key: String,
}

/// One record of an invocation chunk.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChunkRecord {
    /// The record.
    #[prost(oneof = "ChunkBody", tags = "1, 2, 3")]
    pub body: Option<ChunkBody>,
}

/// What a [`ChunkRecord`] holds.
// A run record is larger than a hit; these are transient decode values, read a block
// at a time, so boxing would cost an allocation per record for no memory saved.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq, Eq, prost::Oneof)]
pub enum ChunkBody {
    /// A run.
    #[prost(message, tag = "1")]
    Run(RunRecord),
    /// A failed run's section.
    #[prost(message, tag = "2")]
    Failed(FailedRun),
    /// A hit.
    #[prost(message, tag = "3")]
    Hit(HitRecord),
}

/// The section byte of a run's key in a chunk.
pub const SECTION_RUN: u8 = 0;
/// The section byte of a failed-run section's key.
pub const SECTION_FAILED: u8 = 1;
/// The section byte of a hit's key.
pub const SECTION_HIT: u8 = 2;

/// The sort key of a record in a chunk: its section byte, then the run's sequence (or
/// the hit's row), big-endian. Blocks are cut in key order, so the block index finds
/// a run by its sequence and the failed-run section is a run of adjacent blocks.
#[must_use]
pub fn chunk_key(section: u8, n: u32) -> [u8; 5] {
    let mut k = [section, 0, 0, 0, 0];
    k[1..].copy_from_slice(&n.to_be_bytes());
    k
}

impl ChunkBody {
    /// The record's sort key.
    #[must_use]
    pub fn key(&self) -> [u8; 5] {
        match self {
            ChunkBody::Run(r) => chunk_key(SECTION_RUN, r.seq),
            ChunkBody::Failed(f) => chunk_key(SECTION_FAILED, f.seq),
            ChunkBody::Hit(h) => chunk_key(SECTION_HIT, h.row),
        }
    }

    /// The instance the record was asked under.
    #[must_use]
    pub fn instance(&self) -> &str {
        match self {
            ChunkBody::Run(r) => &r.instance,
            ChunkBody::Failed(f) => &f.instance,
            ChunkBody::Hit(h) => &h.instance,
        }
    }
}

/// Whether an invocation is still recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum InvState {
    /// Records may still come.
    Open = 0,
    /// The writer flushed its last chunk after the invocation turned idle.
    Closed = 1,
}

/// Instances an [`InvocationHeader`] lists at most. A build uses one or a few; the
/// bound keeps a header row (and the reader's all-must-allow check) small whatever
/// names a client sends.
pub const MAX_INSTANCES: usize = 64;

/// The `ui/invid/<inv-key>` row: an invocation's header. The newest chunk's footer
/// carries the same header, so the row can be restored from the object.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct InvocationHeader {
    /// The client's invocation id; empty for an id-less pseudo-invocation.
    #[prost(string, tag = "1")]
    pub record_id: String,
    /// An id-less pseudo-invocation (listed nowhere).
    #[prost(bool, tag = "2")]
    pub idless: bool,
    /// The instances the invocation's records used, sorted: the first
    /// [`MAX_INSTANCES`] by name. The names are the client's, so the list is bounded.
    #[prost(string, repeated, tag = "3")]
    pub instances: Vec<String>,
    /// Run records written.
    #[prost(uint64, tag = "4")]
    pub runs: u64,
    /// Hit records written.
    #[prost(uint64, tag = "5")]
    pub hits: u64,
    /// Failed runs written.
    #[prost(uint64, tag = "6")]
    pub failed: u64,
    /// The first record's time, milliseconds.
    #[prost(uint64, tag = "7")]
    pub first_seen_ms: u64,
    /// The last record's time, milliseconds.
    #[prost(uint64, tag = "8")]
    pub last_seen_ms: u64,
    /// When the last chunk was written: the gap starts here when the invocation was
    /// left open by a crash.
    #[prost(uint64, tag = "9")]
    pub last_chunk_ms: u64,
    /// Chunks written.
    #[prost(uint32, tag = "10")]
    pub chunks: u32,
    /// Open or closed.
    #[prost(enumeration = "InvState", tag = "11")]
    pub state: i32,
    /// Records the writer dropped (full buffers): the history has a hole.
    #[prost(uint64, tag = "12")]
    pub dropped: u64,
    /// The records used more than [`MAX_INSTANCES`] instances, so `instances` does not
    /// list them all. A reader cannot check every instance, so it answers no caller.
    #[prost(bool, tag = "13")]
    pub instances_truncated: bool,
    /// The time the invocation's `ui/inv` list entries are keyed by: its first record's
    /// time when the writer first listed it, never moved, so a list has one entry per
    /// instance. An entry whose key time is not this one is stale and is not shown.
    #[prost(uint64, tag = "14")]
    pub list_ms: u64,
}

/// The `ui/ichunk/<inv-key>/<writer>/<seq>` row: where one chunk is and what it holds.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChunkRow {
    /// The `yyyymmdd` day in the object's key.
    #[prost(uint32, tag = "1")]
    pub day: u32,
    /// The object's length.
    #[prost(uint64, tag = "2")]
    pub bytes: u64,
    /// The stream of its runs (meaningful only when it has runs).
    #[prost(uint32, tag = "3")]
    pub stream: u32,
    /// Its first run's sequence.
    #[prost(uint32, tag = "4")]
    pub first_seq: u32,
    /// Its last run's sequence.
    #[prost(uint32, tag = "5")]
    pub last_seq: u32,
    /// Run records in it.
    #[prost(uint32, tag = "6")]
    pub runs: u32,
    /// Hit records in it.
    #[prost(uint32, tag = "7")]
    pub hits: u32,
    /// Failed runs in it.
    #[prost(uint32, tag = "8")]
    pub failed: u32,
    /// The earliest record time, milliseconds.
    #[prost(uint64, tag = "9")]
    pub first_ms: u64,
    /// The latest record time, milliseconds.
    #[prost(uint64, tag = "10")]
    pub last_ms: u64,
    /// Where the footer (with the trailer) starts.
    #[prost(uint64, tag = "11")]
    pub footer_offset: u64,
    /// The footer's length with the trailer.
    #[prost(uint32, tag = "12")]
    pub footer_len: u32,
}

impl ChunkRow {
    /// Whether the chunk holds run `seq` of `stream`.
    #[must_use]
    pub fn holds(&self, stream: u32, seq: u32) -> bool {
        self.runs > 0 && self.stream == stream && (self.first_seq..=self.last_seq).contains(&seq)
    }
}

/// The `ui/stream/<day>/<stream>/<first ms>` row: which invocation and writer a stream
/// belongs to.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct StreamRow {
    /// The invocation key.
    #[prost(string, tag = "1")]
    pub inv_key: String,
    /// The writer.
    #[prost(uint64, tag = "2")]
    pub writer: u64,
}

/// A run among the slowest of an invocation.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Slow {
    /// The run id's 12 bytes.
    #[prost(bytes = "vec", tag = "1")]
    pub run_id: Vec<u8>,
    /// The time it is ranked by, microseconds.
    #[prost(uint64, tag = "2")]
    pub us: u64,
}

/// An entry of a chunk's digest table.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct DigestRow {
    /// The digest's first 8 hash bytes.
    #[prost(fixed64, tag = "1")]
    pub prefix: u64,
    /// The record's key in the chunk.
    #[prost(bytes = "vec", tag = "2")]
    pub key: Vec<u8>,
}

/// The family summary of an invocation chunk's footer: everything its index rows hold,
/// so a row lost after the PUT is restored from the object with no LIST, and the
/// invocation's cumulative summary up to this chunk.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ChunkSummary {
    /// The invocation key.
    #[prost(string, tag = "1")]
    pub inv_key: String,
    /// The writer.
    #[prost(uint64, tag = "2")]
    pub writer: u64,
    /// The chunk's sequence among every chunk its writer wrote in that life (one count
    /// across invocations, so a writer never makes a key twice).
    #[prost(uint32, tag = "3")]
    pub seq: u32,
    /// What the chunk's `ichunk` row holds, but for the object's length and footer
    /// position, which are zero here: the footer cannot hold its own position. A
    /// restore takes them from the footer range it read.
    #[prost(message, optional, tag = "4")]
    pub row: Option<ChunkRow>,
    /// The open time of the chunk's stream (its first id's time), the `ui/stream` row's
    /// key.
    #[prost(uint64, tag = "5")]
    pub stream_first_ms: u64,
    /// The invocation's header as of this chunk.
    #[prost(message, optional, tag = "6")]
    pub header: Option<InvocationHeader>,
    /// The invocation's slowest runs by execution time so far, slowest first.
    #[prost(message, repeated, tag = "7")]
    pub slowest_execution: Vec<Slow>,
    /// The invocation's slowest runs by wall time so far, slowest first.
    #[prost(message, repeated, tag = "8")]
    pub slowest_wall: Vec<Slow>,
    /// This chunk's action digests to record keys, sorted by prefix.
    #[prost(message, repeated, tag = "9")]
    pub digests: Vec<DigestRow>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_id::StreamId;

    #[test]
    fn the_test_flag_is_field_33_and_older_records_read_false() {
        // Catches: a flag renumbered or dropped from the wire (a chunk written by this
        // version would read back as no test), and a record written before the flag
        // existed failing to decode or reading true.
        use prost::Message as _;
        let test = RunRecord {
            test: true,
            ..RunRecord::default()
        };
        // Tag 33, varint: key 264 (0x88 0x02), value 1.
        assert_eq!(test.encode_to_vec(), [0x88, 0x02, 0x01]);
        assert!(
            RunRecord::decode([0x88, 0x02, 0x01].as_slice())
                .unwrap()
                .test
        );
        assert!(!RunRecord::default().test);
        assert!(RunRecord::default().encode_to_vec().is_empty());
        let older = RunRecord {
            seq: 3,
            ..RunRecord::default()
        };
        assert!(
            !RunRecord::decode(older.encode_to_vec().as_slice())
                .unwrap()
                .test
        );
    }

    #[test]
    fn a_digest_prefix_is_its_first_eight_bytes() {
        // Catches: a little-endian prefix, which would not match the 16 hex digits the
        // digest is shown with.
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(WireDigest::new(h, 9).prefix16(), 0x0102_0304_0506_0708);
        let short = WireDigest {
            hash: vec![0xAB],
            size: 1,
        };
        assert_eq!(short.prefix16(), 0xAB00_0000_0000_0000);
    }

    #[test]
    fn a_run_record_carries_its_id_and_times() {
        // Catches: id fields written to the wrong parts, a non-ok run counted as ok, or
        // a time computed from the wrong timestamps.
        let mut r = RunRecord::default();
        let id = RunId::new(42, StreamId(7), 3);
        r.set_run_id(id);
        assert_eq!(r.run_id(), id);
        assert!(r.failed(), "unspecified is not ok");
        r.outcome = RunOutcome::Ok.into();
        assert!(!r.failed());
        assert_eq!((r.execution_us(), r.wall_us()), (0, 0));
        r.timestamps = Some(Timestamps {
            queued_us: 10,
            execution_start_us: 20,
            execution_completed_us: 50,
            worker_completed_us: 70,
            ..Timestamps::default()
        });
        assert_eq!((r.execution_us(), r.wall_us()), (30, 60));
    }

    #[test]
    fn a_tail_keeps_the_end_of_the_log() {
        // Catches: the head kept instead of the tail, or the length of the tail stored
        // as the log's length.
        let log: Vec<u8> = (0..TAIL_BYTES + 10).map(|i| (i % 251) as u8).collect();
        let t = Tail::of(&log);
        assert_eq!(t.bytes.len(), TAIL_BYTES);
        assert_eq!(t.bytes[..], log[10..]);
        assert_eq!(t.total_bytes, log.len() as u64);
        assert_eq!(t.omitted(), TailOmitted::No);
        assert_eq!(Tail::of(b"short").bytes, b"short");
    }

    #[test]
    fn a_command_rendering_is_bounded_at_four_kib() {
        // Catches: a bound looser than the design's 4 KiB rendering (ui-data.md 5.3 and
        // 4.2 size the failed-run section and its cost on it).
        let mut c = CommandRendering {
            argv: vec!["x".repeat(4 * 1024 + 1)],
            ..CommandRendering::default()
        };
        c.bound();
        assert!(c.truncated);
        assert!(c.argv.is_empty());
        let mut fits = CommandRendering {
            argv: vec!["x".repeat(4 * 1024)],
            ..CommandRendering::default()
        };
        fits.bound();
        assert!(!fits.truncated);
    }

    #[test]
    fn a_command_rendering_is_bounded_from_the_end() {
        // Catches: an unbounded rendering, arguments dropped before output paths, or a
        // cut rendering not marked truncated.
        let big = "x".repeat(COMMAND_BYTES / 4);
        let mut c = CommandRendering {
            argv: vec!["cc".into(), big.clone()],
            env_names: vec![big.clone(), "PATH".into()],
            working_directory: "w".into(),
            output_paths: vec![big.clone(), big.clone()],
            truncated: false,
        };
        c.bound();
        assert!(c.truncated);
        assert_eq!(c.argv.len(), 2);
        assert_eq!(c.env_names.len(), 2);
        assert_eq!(c.output_paths.len(), 1, "only the last output path went");
        let mut fits = CommandRendering {
            argv: vec!["cc".into()],
            ..CommandRendering::default()
        };
        fits.bound();
        assert!(!fits.truncated);
        assert_eq!(fits.argv, ["cc"]);
        // A working directory alone over the bound is cleared.
        let mut wd = CommandRendering {
            working_directory: "d".repeat(COMMAND_BYTES + 1),
            argv: vec!["a".into()],
            ..CommandRendering::default()
        };
        wd.bound();
        assert!(wd.truncated);
        assert!(wd.working_directory.is_empty());
        assert!(wd.argv.is_empty());
    }

    #[test]
    fn chunk_keys_sort_runs_then_failures_then_hits() {
        // Catches: a key that sorts a failure among the runs (the failed section would
        // not be adjacent blocks) or a little-endian sequence (run 256 before run 1).
        let run = |seq| {
            ChunkBody::Run(RunRecord {
                seq,
                instance: "a".into(),
                ..RunRecord::default()
            })
        };
        let failed = ChunkBody::Failed(FailedRun {
            seq: 0,
            instance: "b".into(),
            ..FailedRun::default()
        });
        let hit = ChunkBody::Hit(HitRecord {
            row: 0,
            instance: "c".into(),
            ..HitRecord::default()
        });
        assert!(run(1).key() < run(256).key());
        assert!(run(u32::MAX).key() < failed.key());
        assert!(failed.key() < hit.key());
        let instances: Vec<_> = [run(0), failed, hit]
            .iter()
            .map(|b| b.instance().to_owned())
            .collect();
        assert_eq!(instances, ["a", "b", "c"]);
    }

    #[test]
    fn a_chunk_row_holds_only_its_stream_and_range() {
        // Catches: a row matched on sequence alone (another stream's run would resolve
        // to this chunk) or a hits-only chunk taken as holding a run.
        let row = ChunkRow {
            stream: 5,
            first_seq: 10,
            last_seq: 20,
            runs: 11,
            ..ChunkRow::default()
        };
        assert!(row.holds(5, 10));
        assert!(row.holds(5, 20));
        assert!(!row.holds(5, 9));
        assert!(!row.holds(5, 21));
        assert!(!row.holds(6, 15));
        let hits_only = ChunkRow {
            stream: 0,
            ..ChunkRow::default()
        };
        assert!(!hits_only.holds(0, 0));
    }

    #[test]
    fn the_target_and_the_list_time_have_their_own_field_numbers() {
        // Catches: a field renumbered or dropped from the wire, and a record written
        // before the fields existed failing to decode or reading them set.
        use prost::Message as _;
        let run = RunRecord {
            target: "//a:t".into(),
            ..RunRecord::default()
        };
        // Tag 34, length-delimited: key 274 (0x92 0x02).
        assert_eq!(
            run.encode_to_vec(),
            [0x92, 0x02, 5, b'/', b'/', b'a', b':', b't']
        );
        let header = InvocationHeader {
            list_ms: 5,
            ..InvocationHeader::default()
        };
        // Tag 14, varint: key 112.
        assert_eq!(header.encode_to_vec(), [112, 5]);
        assert_eq!(RunRecord::decode(&[][..]).unwrap().target, "");
        assert_eq!(InvocationHeader::decode(&[][..]).unwrap().list_ms, 0);
    }
}
