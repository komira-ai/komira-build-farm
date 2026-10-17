//! Execute and WaitExecution metrics: where the time of an Execute goes at the front,
//! and how many such streams are open, queued or not. Rendered with the cache's
//! families ([`super::CacheMetrics::render`]).
//!
//! The front can see four moments of an Execute: its arrival, the call to the
//! scheduler ([`crate::Dispatch::submit`]), the scheduler's acceptance, and the first
//! update that is not `QUEUED` (`EXECUTING`, or the done operation when the stream
//! missed the `EXECUTING` one because the stage changed twice between two polls). It
//! cannot see inside the scheduler or the worker, so everything after that first
//! update is one number, [`ExecuteMetrics::total`].
//!
//! Recording takes no lock and allocates nothing: a stream holds one `Arc` of these
//! metrics and each observation is a few relaxed atomic adds, like the CAS metrics.

use std::sync::Arc;
use std::time::Instant;

use super::{Counter, Families, Gauge, Histogram, Unit};

/// Everything the front measures about Execute and WaitExecution.
#[derive(Debug)]
pub struct ExecuteMetrics {
    /// Execute calls ended: answered, failed or dropped by their caller.
    pub requests: Counter,
    /// Execute calls that ended in an error status, or were dropped before the done
    /// operation. A call whose operation finished with a failed `ExecuteResponse` is
    /// not one: its gRPC call succeeded.
    pub errors: Counter,
    /// Execute and WaitExecution streams open: begun and not yet ended.
    pub in_flight: Gauge,
    /// Operation streams alive: one per Execute or WaitExecution whose call became a
    /// stream of operations ([`OpenExecute::streaming`]), from the moment it is made
    /// (before its first item is read) until it ends or its caller drops it. Unlike
    /// `in_flight` it holds no call that is still being authorized or prepared and none
    /// answered from the action cache, so it is the count a drain waits to see reach 0.
    pub open_executions: Gauge,
    /// Execute calls the front took (not refused for a drain), a cache hit included.
    /// A rollout that sees it flat while `open_executions` is 0 sees a frontend no
    /// client is using.
    pub started: Counter,
    /// Execute streams the scheduler accepted whose operation has not left `QUEUED`
    /// yet. A stream that joined a running twin counts only until its first update.
    pub queued: Gauge,
    /// From the arrival of an Execute to the call of [`crate::Dispatch::submit`]:
    /// authorization, the action cache lookup, the reads of the action, its command and
    /// its input tree, the check that every input is held, and the booking. Observed
    /// only for an Execute that reaches the scheduler.
    pub prepare: Histogram,
    /// The call of [`crate::Dispatch::submit`] itself, from the call to its return
    /// (the scheduler joining or queueing the request).
    pub dispatch: Histogram,
    /// From the scheduler's acceptance to this stream's first update that is not
    /// `QUEUED`. It is read by the stream as it polls its operation, so it can run a
    /// little over the scheduler's own time; a call that joined an operation already
    /// executing observes near zero.
    pub queued_seconds: Histogram,
    /// An Execute from its arrival to its last response (or its error, or its caller
    /// leaving). Cache hits are in it.
    pub total: Histogram,
}

impl Default for ExecuteMetrics {
    fn default() -> Self {
        Self {
            requests: Counter::default(),
            errors: Counter::default(),
            in_flight: Gauge::default(),
            open_executions: Gauge::default(),
            started: Counter::default(),
            queued: Gauge::default(),
            prepare: Histogram::new(Unit::Seconds),
            dispatch: Histogram::new(Unit::Seconds),
            queued_seconds: Histogram::new(Unit::Seconds),
            total: Histogram::new(Unit::Seconds),
        }
    }
}

impl ExecuteMetrics {
    /// Starts timing an Execute. It counts as an error unless [`OpenExecute::ok`] is
    /// called before the guard is dropped.
    #[must_use]
    pub fn execute(self: &Arc<Self>) -> OpenExecute {
        self.in_flight.add_one();
        let now = Instant::now();
        OpenExecute {
            metrics: Arc::clone(self),
            started: now,
            phase: now,
            counted: true,
            ok: false,
            queued: false,
            streaming: false,
        }
    }

    /// Counts a WaitExecution stream as open until the guard is dropped; it feeds no
    /// other family.
    #[must_use]
    pub fn wait(self: &Arc<Self>) -> OpenExecute {
        self.in_flight.add_one();
        let now = Instant::now();
        OpenExecute {
            metrics: Arc::clone(self),
            started: now,
            phase: now,
            counted: false,
            ok: false,
            queued: false,
            streaming: false,
        }
    }

    /// Appends the families to `out`.
    pub(super) fn render(&self, out: &mut Families) {
        out.counter(
            "kbf_execute_requests_total",
            "Execute calls ended: answered, failed or dropped by their caller",
            self.requests.get(),
        );
        out.counter(
            "kbf_execute_errors_total",
            "Execute calls that ended in an error status or were dropped before their done operation",
            self.errors.get(),
        );
        out.gauge(
            "kbf_execute_in_flight",
            "Execute and WaitExecution streams open",
            self.in_flight.get(),
        );
        out.gauge(
            "kbf_front_open_executions",
            "Execute and WaitExecution operation streams alive, from the stream's making to its end",
            self.open_executions.get(),
        );
        out.counter(
            "kbf_front_executes_started_total",
            "Execute calls the front took, not refused for a drain, a cache hit included",
            self.started.get(),
        );
        out.gauge(
            "kbf_execute_queued",
            "Execute streams accepted by the scheduler whose operation has not left QUEUED",
            self.queued.get(),
        );
        let histograms = [
            (
                "kbf_execute_prepare_seconds",
                "An Execute from its arrival to the call of the scheduler: authorization, action cache lookup, reading the action and its inputs, the held-inputs check and the booking",
                &self.prepare,
            ),
            (
                "kbf_execute_dispatch_seconds",
                "The scheduler's submit call, from the call to its return",
                &self.dispatch,
            ),
            (
                "kbf_execute_queued_seconds",
                "From the scheduler's acceptance to the stream's first update that is not QUEUED",
                &self.queued_seconds,
            ),
            (
                "kbf_execute_total_seconds",
                "An Execute from its arrival to its last response, error or departure of its caller",
                &self.total,
            ),
        ];
        for (name, help, histogram) in histograms {
            out.head(name, "histogram", help);
            histogram.render(&mut out.0, name, "");
        }
    }
}

/// One open Execute or WaitExecution stream, recorded when dropped. It is owned, so a
/// response stream can carry it; a stream dropped by its caller records an error.
#[derive(Debug)]
pub struct OpenExecute {
    metrics: Arc<ExecuteMetrics>,
    started: Instant,
    /// When the current phase began.
    phase: Instant,
    /// Whether this is an Execute (feeds every family) or a WaitExecution (only
    /// `in_flight`).
    counted: bool,
    ok: bool,
    /// Whether this stream is in the `queued` gauge.
    queued: bool,
    /// Whether this stream is in the `open_executions` gauge.
    streaming: bool,
}

impl OpenExecute {
    /// The call reaches the scheduler: observes the prepare phase.
    pub fn dispatching(&mut self) {
        let now = Instant::now();
        self.metrics
            .prepare
            .observe_duration(now.duration_since(self.started));
        self.phase = now;
    }

    /// The scheduler returned: observes its call, and counts the stream as queued
    /// until [`Self::update`] sees it leave `QUEUED`. Called with the scheduler's
    /// answer either way; only an accepted call is queued.
    pub fn dispatched(&mut self, accepted: bool) {
        let now = Instant::now();
        self.metrics
            .dispatch
            .observe_duration(now.duration_since(self.phase));
        self.phase = now;
        if accepted {
            self.metrics.queued.add_one();
            self.queued = true;
        }
    }

    /// An update was read from the stream's operation: `queued` says whether it is
    /// still `QUEUED`, `done` whether it is the done operation (which marks the call
    /// answered).
    pub fn update(&mut self, queued: bool, done: bool) {
        if self.queued && !queued {
            self.queued = false;
            self.metrics.queued.sub_one();
            self.metrics.queued_seconds.since(self.phase);
        }
        if done {
            self.ok = true;
        }
    }

    /// The call became a stream of operations: counts it in `open_executions` until
    /// the guard is dropped. Once only; a second call changes nothing.
    pub fn streaming(&mut self) {
        if !self.streaming {
            self.streaming = true;
            self.metrics.open_executions.add_one();
        }
    }

    /// Marks the call answered without error.
    pub const fn ok(&mut self) {
        self.ok = true;
    }
}

impl Drop for OpenExecute {
    fn drop(&mut self) {
        if self.queued {
            self.metrics.queued.sub_one();
        }
        if self.streaming {
            self.metrics.open_executions.sub_one();
        }
        self.metrics.in_flight.sub_one();
        if self.counted {
            self.metrics.requests.add(1);
            if !self.ok {
                self.metrics.errors.add(1);
            }
            self.metrics.total.since(self.started);
        }
    }
}
