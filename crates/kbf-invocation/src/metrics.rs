//! An invocation's aggregates (`docs/design/invocations.md`, section 4.4), computed
//! from its rows: queue and run times with approximate percentiles, reruns by reason,
//! and the usage results carry.

use std::collections::BTreeMap;

use kbf_sched::RequeueReason;
use kbf_types::Failure;

use crate::record::{AttemptEnd, How, Invocation};

/// Sub-buckets each power-of-two bucket of a [`Histogram`] is split into.
const SUB_BUCKETS: u64 = 16;

/// A histogram of non-negative values whose percentiles are approximate:
/// power-of-two buckets, each split into 16 equal sub-buckets, and a percentile
/// reported as the largest value its sub-bucket holds (but never above the largest
/// value recorded). A percentile is never below the true one and at most 1/16 above
/// it; values below 16 are exact. Min, max, sum and count are exact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Histogram {
    /// Values recorded in each sub-bucket, by [`sub_bucket`].
    counts: BTreeMap<u32, u64>,
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
}

/// The index of the sub-bucket `v` falls in: `v` itself below 16; else 16 per power of
/// two from 16 up, each covering an equal share of it.
const fn sub_bucket(v: u64) -> u32 {
    if v < SUB_BUCKETS {
        return v as u32;
    }
    let power = 63 - v.leading_zeros(); // 4 or more
    let shift = power - 4;
    let sub = (v >> shift) - SUB_BUCKETS; // 0..16
    SUB_BUCKETS as u32 + shift * SUB_BUCKETS as u32 + sub as u32
}

/// The largest value sub-bucket `i` holds.
const fn sub_bucket_top(i: u32) -> u64 {
    let n = SUB_BUCKETS as u32;
    if i < n {
        return i as u64;
    }
    let shift = (i - n) / n;
    let sub = ((i - n) % n) as u64;
    // The top sub-bucket of all ends at u64::MAX: its end, 2^64, wraps to 0.
    ((SUB_BUCKETS + sub + 1) << shift).wrapping_sub(1)
}

impl Histogram {
    /// Counts `v`.
    pub fn record(&mut self, v: u64) {
        *self.counts.entry(sub_bucket(v)).or_default() += 1;
        self.min = if self.count == 0 { v } else { self.min.min(v) };
        self.max = self.max.max(v);
        self.count += 1;
        self.sum = self.sum.saturating_add(v);
    }

    /// Values recorded.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// Their sum (saturating).
    #[must_use]
    pub const fn sum(&self) -> u64 {
        self.sum
    }

    /// The smallest, or `None` when none was recorded.
    #[must_use]
    pub const fn min(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.min)
        }
    }

    /// The largest, or `None` when none was recorded.
    #[must_use]
    pub const fn max(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.max)
        }
    }

    /// The `percent`-th percentile (1 to 100), approximate as the type says: the top
    /// of the sub-bucket holding the value of rank `ceil(percent * count / 100)`, and no
    /// more than the largest value. `None` when none was recorded.
    #[must_use]
    pub fn percentile(&self, percent: u64) -> Option<u64> {
        let rank = (percent * self.count).div_ceil(100).max(1);
        let mut seen = 0;
        self.counts.iter().find_map(|(&i, &n)| {
            seen += n;
            (seen >= rank).then(|| sub_bucket_top(i).min(self.max))
        })
    }
}

/// An invocation's aggregates (section 4.4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    /// Microseconds each lease of a `ran` row waited: queued to granted.
    pub queue: Histogram,
    /// Microseconds each lease of a `ran` row that ended held its worker: granted to
    /// ended.
    pub run: Histogram,
    /// Leases of `ran` rows given up and queued again, by [`requeue_name`].
    pub reruns: BTreeMap<&'static str, u64>,
    /// Rows whose result carries a `ResourceUsage`.
    pub usage_actions: u64,
    /// Their user and system CPU time together, in microseconds, summed (saturating).
    pub cpu_micros_sum: u64,
    /// The largest of their CPU times.
    pub cpu_micros_max: u64,
    /// Their peak memory, summed (saturating).
    pub peak_memory_sum: u64,
    /// The largest of their peak memory.
    pub peak_memory_max: u64,
}

impl Invocation {
    /// The aggregates of section 4.4 over its rows as they are now. Times count only
    /// the leases of rows the farm ran for this invocation (`how` = `ran`): a joined
    /// row's leases are its twin's, counted where the twin ran.
    #[must_use]
    pub fn metrics(&self) -> Metrics {
        let mut m = Metrics::default();
        for row in &self.rows {
            if row.how == How::Ran {
                for a in &row.attempts {
                    let granted = a.granted.unix_micros();
                    m.queue
                        .record(granted.saturating_sub(a.queued.unix_micros()));
                    if let Some(ended) = a.ended {
                        m.run.record(ended.unix_micros().saturating_sub(granted));
                    }
                    if let Some(AttemptEnd::Requeued(reason)) = &a.end {
                        *m.reruns.entry(requeue_name(reason)).or_default() += 1;
                    }
                }
            }
            if let Some(u) = row.listing.as_ref().and_then(|l| l.usage) {
                m.usage_actions += 1;
                let cpu = u.cpu_user_micros.saturating_add(u.cpu_system_micros);
                m.cpu_micros_sum = m.cpu_micros_sum.saturating_add(cpu);
                m.cpu_micros_max = m.cpu_micros_max.max(cpu);
                m.peak_memory_sum = m.peak_memory_sum.saturating_add(u.peak_memory_bytes);
                m.peak_memory_max = m.peak_memory_max.max(u.peak_memory_bytes);
            }
        }
        m
    }
}

/// Every name [`requeue_name`] gives, in the order of [`RequeueReason`]'s variants:
/// what `?requeue_reason=` takes.
pub const REQUEUE_NAMES: [&str; 10] = [
    "silent",
    "replaced",
    "reconnected",
    "not_started",
    "session_switch",
    "out_of_memory",
    "node_memory_pressure",
    "restarted",
    "lost",
    "infra",
];

/// A requeue reason's name, as the API writes it: one of [`REQUEUE_NAMES`].
#[must_use]
pub const fn requeue_name(reason: &RequeueReason) -> &'static str {
    match reason {
        RequeueReason::Silent => "silent",
        RequeueReason::Replaced => "replaced",
        RequeueReason::Reconnected => "reconnected",
        RequeueReason::NotStarted => "not_started",
        RequeueReason::SessionSwitch => "session_switch",
        RequeueReason::OutOfMemory { .. } => "out_of_memory",
        RequeueReason::NodeMemoryPressure => "node_memory_pressure",
        RequeueReason::Restarted => "restarted",
        RequeueReason::Lost(_) => "lost",
        RequeueReason::Infra => "infra",
    }
}

/// A failure class's name, as the API writes it: `timeout`, `infra`, `invalid`,
/// `out_of_memory`, `node_memory_pressure`, `missing_input` or `lost`.
#[must_use]
pub const fn failure_name(failure: Failure) -> &'static str {
    match failure {
        Failure::Timeout => "timeout",
        Failure::Infra => "infra",
        Failure::Invalid => "invalid",
        Failure::OutOfMemory => "out_of_memory",
        Failure::NodeMemoryPressure => "node_memory_pressure",
        Failure::MissingInput => "missing_input",
        Failure::Lost(_) => "lost",
    }
}

#[cfg(test)]
mod tests;
