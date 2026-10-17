//! What a test's status row says, and how a run changes it.
//!
//! Outcome rules (ui-data.md 4.3): a run that passed, failed or timed out is a result;
//! an infrastructure failure (the farm, a lost lease, a node's memory pressure) is
//! excluded from the status, `first_fail`, the transition mask and the flaky proof; a
//! kill at the action's own memory limit with no ladder rung left counts as a failure
//! for `first_fail`, the mask and the status, but not for the flaky proof (a resource
//! kill says nothing about whether the code under test is flaky); a kill followed by a
//! rung, a cancel and a run with no stated outcome are not results at all.

use crate::records::{MemoryKill, RunOutcome, RunRecord, TestStatus};
use crate::rows::{Mark, RingRun, StatusKind, StatusRow, Verdict};

/// Runs a status row keeps in its ring.
pub const RING: usize = 16;
/// Digest verdicts a status row keeps.
pub const VERDICTS: usize = 16;
/// Runs a status row's transition mask holds.
pub const MASK_BITS: u32 = 64;

/// One run, as a status row takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    /// How it ended.
    pub status: TestStatus,
    /// The run id's 12 bytes.
    pub run_id: Vec<u8>,
    /// The run's time, milliseconds.
    pub time_ms: u64,
    /// Execution time, milliseconds.
    pub execution_ms: u32,
    /// The action digest's first 8 hash bytes; 0 for none.
    pub digest16: u64,
    /// The invocation key; empty for an id-less run.
    pub inv_key: String,
}

/// How `run` ended as a test result, or `None` when it is not one.
#[must_use]
pub fn classify(run: &RunRecord) -> Option<TestStatus> {
    match run.outcome() {
        RunOutcome::Ok => Some(TestStatus::Pass),
        RunOutcome::Failed => Some(TestStatus::Fail),
        RunOutcome::TimedOut => Some(TestStatus::Timeout),
        RunOutcome::Infra | RunOutcome::Lost => Some(TestStatus::Infra),
        RunOutcome::MemoryKilled => match run.memory_kill() {
            // A rung was raised for the next attempt: this kill is not the end.
            MemoryKill::OwnLimit if run.raised_memory_bytes > 0 => None,
            MemoryKill::OwnLimit => Some(TestStatus::OomKilled),
            MemoryKill::NodePressure | MemoryKill::None => Some(TestStatus::Infra),
        },
        RunOutcome::Cancelled | RunOutcome::Unspecified => None,
    }
}

fn failure(status: TestStatus) -> bool {
    matches!(
        status,
        TestStatus::Fail | TestStatus::Timeout | TestStatus::OomKilled
    )
}

impl StatusRow {
    /// Takes run `o` in, once: a run already in the ring (a write whose acknowledgement
    /// was lost and is made again) changes nothing.
    pub fn apply(&mut self, o: &Observed) {
        if self.ring.iter().any(|r| r.run_id == o.run_id) {
            return;
        }
        self.ring.push(RingRun {
            run_id: o.run_id.clone(),
            status: o.status.into(),
            execution_ms: o.execution_ms,
            digest16: o.digest16,
        });
        if self.ring.len() > RING {
            self.ring.remove(0);
        }
        self.last_seen_ms = self.last_seen_ms.max(o.time_ms);
        let failed = failure(o.status);
        if !failed && o.status != TestStatus::Pass {
            return;
        }
        self.mask = (self.mask << 1) | u64::from(failed);
        self.masked = (self.masked + 1).min(MASK_BITS);
        let mark = Mark {
            inv_key: o.inv_key.clone(),
            time_ms: o.time_ms,
            run_id: o.run_id.clone(),
        };
        if !failed {
            self.last_pass = Some(mark.clone());
            self.first_fail = None;
        } else if !failure(self.last_outcome()) {
            self.first_fail = Some(mark.clone());
        }
        self.last_outcome = o.status.into();
        if o.status != TestStatus::OomKilled {
            self.note_digest(o.digest16, failed, mark);
        }
    }

    /// Records that digest `digest16` passed or failed, and sets the flaky proof when
    /// the digest has now done both. A run with no digest is no evidence.
    fn note_digest(&mut self, digest16: u64, failed: bool, mark: Mark) {
        if digest16 == 0 {
            return;
        }
        let at = self.verdicts.iter().position(|v| v.digest16 == digest16);
        let mut v = at.map_or_else(
            || Verdict {
                digest16,
                ..Verdict::default()
            },
            |i| self.verdicts.remove(i),
        );
        if failed {
            v.failed = true;
        } else {
            v.passed = true;
        }
        let both = v.passed && v.failed;
        self.verdicts.push(v);
        if self.verdicts.len() > VERDICTS {
            self.verdicts.remove(0);
        }
        if both && !self.proven {
            self.proven = true;
            self.proof_digest = digest16;
            self.proven_since = Some(mark);
        }
    }

    /// The status the by-status index files the row under; `None` while no run that
    /// was not infrastructure has been seen.
    #[must_use]
    pub fn kind(&self) -> Option<StatusKind> {
        match self.last_outcome() {
            TestStatus::Unspecified | TestStatus::Infra => None,
            TestStatus::Pass if self.proven => Some(StatusKind::Flaky),
            TestStatus::Pass => Some(StatusKind::Passing),
            TestStatus::OomKilled => Some(StatusKind::OomKilled),
            TestStatus::Fail | TestStatus::Timeout => Some(StatusKind::Failing),
        }
    }
}
