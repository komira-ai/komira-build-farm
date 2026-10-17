//! What history counts, for `GET /metrics`: the records it dropped by stage, and the
//! size of its queue and of what the writer holds.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kbf_history::{Drops, EventQueue};

/// The counters and gauges of one history writer. The queue's are read live; the
/// writer's are published after each of its steps.
#[derive(Debug)]
pub struct Stats {
    queue: Arc<EventQueue>,
    buffer: AtomicU64,
    put: AtomicU64,
    index: AtomicU64,
    invocations: AtomicU64,
    pending_rows: AtomicUsize,
    open_invocations: AtomicUsize,
}

impl Stats {
    pub(super) fn new(queue: Arc<EventQueue>) -> Self {
        Self {
            queue,
            buffer: AtomicU64::new(0),
            put: AtomicU64::new(0),
            index: AtomicU64::new(0),
            invocations: AtomicU64::new(0),
            pending_rows: AtomicUsize::new(0),
            open_invocations: AtomicUsize::new(0),
        }
    }

    /// Takes what the writer holds after a step.
    pub(super) fn publish(&self, drops: Drops, pending_rows: usize, open_invocations: usize) {
        self.buffer.store(drops.buffer, Ordering::Relaxed);
        self.put.store(drops.put, Ordering::Relaxed);
        self.index.store(drops.index, Ordering::Relaxed);
        self.invocations.store(drops.invocations, Ordering::Relaxed);
        self.pending_rows.store(pending_rows, Ordering::Relaxed);
        self.open_invocations
            .store(open_invocations, Ordering::Relaxed);
    }

    /// Records dropped, by stage: `queue`, `buffer`, `put`, `index` and `invocations`.
    fn stages(&self) -> [(&'static str, u64); 5] {
        [
            ("queue", self.queue.dropped()),
            ("buffer", self.buffer.load(Ordering::Relaxed)),
            ("put", self.put.load(Ordering::Relaxed)),
            ("index", self.index.load(Ordering::Relaxed)),
            ("invocations", self.invocations.load(Ordering::Relaxed)),
        ]
    }

    /// Records dropped at `stage` (one of `queue`, `buffer`, `put`, `index` and
    /// `invocations`; 0 for any other name).
    #[must_use]
    pub fn dropped(&self, stage: &str) -> u64 {
        self.stages()
            .into_iter()
            .find(|(name, _)| *name == stage)
            .map_or(0, |(_, dropped)| dropped)
    }

    /// Appends the families to `out`, in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "# HELP kbf_history_dropped_total Records history dropped instead of keeping, \
             by stage\n# TYPE kbf_history_dropped_total counter"
        );
        for (stage, dropped) in self.stages() {
            let _ = writeln!(
                out,
                "kbf_history_dropped_total{{stage=\"{stage}\"}} {dropped}"
            );
        }
        let gauges = [
            (
                "kbf_history_queue_events",
                "Events waiting for the history writer",
                self.queue.len() as u64,
            ),
            (
                "kbf_history_queue_bytes",
                "Bytes of the events waiting for the history writer",
                self.queue.bytes() as u64,
            ),
            (
                "kbf_history_pending_rows",
                "Index rows the history writer has not applied yet",
                self.pending_rows.load(Ordering::Relaxed) as u64,
            ),
            (
                "kbf_history_open_invocations",
                "Invocations the history writer holds state for",
                self.open_invocations.load(Ordering::Relaxed) as u64,
            ),
        ];
        for (name, help, value) in gauges {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
            );
        }
    }
}
