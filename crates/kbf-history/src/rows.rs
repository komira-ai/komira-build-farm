//! The values of the shape catalog and status rows, and the by-status names.
//!
//! Field numbers are the wire format: a field is added under a new number and never
//! renumbered.

use crate::records::TestStatus;

/// What a shape is to the farm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum ShapeKind {
    /// Not a test.
    Other = 0,
    /// A test execution.
    Test = 1,
}

/// The `ui/shape/<instance>/<shape>/<cfg>` row: the slim catalog entry, written when the
/// writer first sees the key (a stored row is never rewritten by it).
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ShapeRow {
    /// Test or not.
    #[prost(enumeration = "ShapeKind", tag = "1")]
    pub kind: i32,
    /// The time of the first run seen, milliseconds.
    #[prost(uint64, tag = "2")]
    pub first_seen_ms: u64,
    /// The time of the same run: the row is not rewritten, so this is the first sight
    /// too, until a later fold updates it.
    #[prost(uint64, tag = "3")]
    pub last_seen_ms: u64,
    /// The target label, when the run's shape carried one; empty otherwise.
    #[prost(string, tag = "4")]
    pub target: String,
}

/// A run named from a status row: where to find it.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Mark {
    /// The invocation key; empty for a run of the id-less pseudo-invocation.
    #[prost(string, tag = "1")]
    pub inv_key: String,
    /// The run's time, milliseconds.
    #[prost(uint64, tag = "2")]
    pub time_ms: u64,
    /// The run id's 12 bytes.
    #[prost(bytes = "vec", tag = "3")]
    pub run_id: Vec<u8>,
}

/// What one action digest has done in this key's recent runs.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct Verdict {
    /// The digest's first 8 hash bytes.
    #[prost(fixed64, tag = "1")]
    pub digest16: u64,
    /// A run of it passed.
    #[prost(bool, tag = "2")]
    pub passed: bool,
    /// A run of it failed or timed out.
    #[prost(bool, tag = "3")]
    pub failed: bool,
}

/// One of the last runs of a key.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct RingRun {
    /// The run id's 12 bytes.
    #[prost(bytes = "vec", tag = "1")]
    pub run_id: Vec<u8>,
    /// How it ended.
    #[prost(enumeration = "TestStatus", tag = "2")]
    pub status: i32,
    /// Execution time, milliseconds.
    #[prost(uint32, tag = "3")]
    pub execution_ms: u32,
    /// Its action digest's first 8 hash bytes; 0 when it had none.
    #[prost(fixed64, tag = "4")]
    pub digest16: u64,
}

/// The `ui/status/<instance>/<shape>/<cfg>/_` row of a test: where it stands, from the
/// runs the farm has seen.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct StatusRow {
    /// How the last run that was not infrastructure ended; the status is told from it.
    #[prost(enumeration = "TestStatus", tag = "1")]
    pub last_outcome: i32,
    /// The newest pass.
    #[prost(message, optional, tag = "2")]
    pub last_pass: Option<Mark>,
    /// The first failure since the last pass; none while the test passes.
    #[prost(message, optional, tag = "3")]
    pub first_fail: Option<Mark>,
    /// One bit per run that was not infrastructure, the newest in bit 0: 1 is a failure.
    #[prost(fixed64, tag = "4")]
    pub mask: u64,
    /// How many bits of `mask` are runs (at most 64).
    #[prost(uint32, tag = "5")]
    pub masked: u32,
    /// The verdicts of the last 16 digests, the one seen last at the end.
    #[prost(message, repeated, tag = "6")]
    pub verdicts: Vec<Verdict>,
    /// One digest was seen both passing and failing: flaky, and it stays set.
    #[prost(bool, tag = "7")]
    pub proven: bool,
    /// The digest that proved it.
    #[prost(fixed64, tag = "8")]
    pub proof_digest: u64,
    /// The run that completed the proof.
    #[prost(message, optional, tag = "9")]
    pub proven_since: Option<Mark>,
    /// The last 16 runs, infrastructure ones too, the newest at the end.
    #[prost(message, repeated, tag = "10")]
    pub ring: Vec<RingRun>,
    /// The time of the newest run, milliseconds.
    #[prost(uint64, tag = "11")]
    pub last_seen_ms: u64,
}

/// A test's status, as the by-status index names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StatusKind {
    /// The last run passed.
    Passing,
    /// The last run failed or timed out.
    Failing,
    /// The last run passed and one digest has been seen both ways.
    Flaky,
    /// The last run was killed at its own memory limit.
    OomKilled,
}

impl StatusKind {
    /// Every status the index holds.
    pub const ALL: [StatusKind; 4] = [
        StatusKind::Passing,
        StatusKind::Failing,
        StatusKind::Flaky,
        StatusKind::OomKilled,
    ];

    /// The name in a `ui/statusx/` key and in the API's `status=` filter.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            StatusKind::Passing => "passing",
            StatusKind::Failing => "failing",
            StatusKind::Flaky => "flaky",
            StatusKind::OomKilled => "oom_killed",
        }
    }

    /// The status a name stands for.
    #[must_use]
    pub fn parse(name: &str) -> Option<StatusKind> {
        StatusKind::ALL.into_iter().find(|k| k.name() == name)
    }
}
