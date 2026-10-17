//! Draining the frontend: one idempotent action, begun by a stop signal or by
//! `POST /v1/frontend/drain` ([`crate::api`]).
//!
//! A drain refuses every new Execute UNAVAILABLE and lets the open streams run until
//! their action is done (Buck2 does not recover a stream that breaks, so ending one
//! fails its action: komira-build-farm#791). It takes the server out of `/readyz` and
//! the health service, and sends GOAWAY to the REAPI listener's connections, which then
//! stop accepting. The serving future returns once the REAPI connections have closed.
//!
//! [`Drain::begin`] does all of that, once: the second and later calls change nothing,
//! so a signal that arrives after the endpoint (or the endpoint called twice) is a
//! no-op. [`Drain::close`] is the escalation: it ends every stream that is not done
//! UNAVAILABLE, and is what the shutdown timeout and a second signal call.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use kbf_front::Closer;
use serde::Serialize;
use tokio::sync::{oneshot, watch};

use crate::handover::Stop;
use crate::health::Readiness;

/// What `GET /v1/frontend/drain` answers, and what a drain reports after it begins.
///
/// `open_executions` and `queued_operations` together tell an orchestrator which kind of
/// quiet it is looking at: both 0 is idle; a stream open with a queued operation and no
/// worker to run it is waiting, not running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DrainStatus {
    /// A drain has begun (by a signal or the endpoint) and will not be undone.
    pub draining: bool,
    /// Execute and WaitExecution operation streams open (`kbf_front_open_executions`).
    pub open_executions: u64,
    /// Operations in the scheduler's queue, not yet leased to a worker.
    pub queued_operations: u64,
}

/// The frontend's drain: see the module.
pub struct Drain {
    closer: Closer,
    readiness: std::sync::Arc<Readiness>,
    /// Sends GOAWAY: completes the REAPI listener's graceful shutdown. Taken by the
    /// first [`Drain::begin`].
    goaway: Mutex<Option<oneshot::Sender<()>>>,
    began: AtomicBool,
    /// Given by the first [`Drain::begin`]: [`Drain::begun`] completes on it, and the
    /// listeners registered with [`Drain::begins`] are closed by it.
    started: Stop,
    closed: watch::Sender<bool>,
}

impl Drain {
    /// A drain over `closer` and `readiness`, and the receiver that completes when it
    /// begins, for the REAPI listener's graceful shutdown.
    #[must_use]
    pub fn new(
        closer: Closer,
        readiness: std::sync::Arc<Readiness>,
    ) -> (Self, oneshot::Receiver<()>) {
        let (goaway, begun) = oneshot::channel();
        let drain = Self {
            closer,
            readiness,
            goaway: Mutex::new(Some(goaway)),
            began: AtomicBool::new(false),
            started: Stop::new(),
            closed: watch::Sender::new(false),
        };
        (drain, begun)
    }

    /// Begins the drain; a drain already begun is left as it is. Returns whether this
    /// call began it.
    ///
    /// In this order: `/readyz` and the health service stop saying ready (before
    /// anything stops, so a front's health check sends no new work here), new Execute
    /// calls are refused, and the REAPI listener is sent GOAWAY.
    pub fn begin(&self) -> bool {
        if self.began.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.readiness.stop();
        self.closer.drain();
        self.started.fire();
        // Taken once: the `began` swap above lets one call this far. The listener is gone
        // only if the server already stopped, and the send then has no one to tell.
        let _ = self
            .goaway
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .map(|goaway| goaway.send(()));
        true
    }

    /// Completes once the drain has begun (immediately if it has).
    pub async fn begun(&self) {
        self.started.fired().await;
    }

    /// The signal the first [`Drain::begin`] gives, for a listener to be closed by.
    pub(crate) const fn begins(&self) -> &Stop {
        &self.started
    }

    /// Whether a drain has begun.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.began.load(Ordering::SeqCst)
    }

    /// Ends every open stream that is not done UNAVAILABLE, and every one opened from
    /// now on after its first operation. Begins the drain if it has not begun. Safe to
    /// call any number of times.
    pub fn close(&self) {
        self.begin();
        self.closer.close();
        self.closed.send_replace(true);
    }

    /// Completes once [`Drain::close`] has been called.
    pub async fn closed(&self) {
        let mut closed = self.closed.subscribe();
        // The sender is `self.closed`, alive as long as `self`.
        let _ = closed.wait_for(|closed| *closed).await;
    }
}

impl std::fmt::Debug for Drain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drain")
            .field("draining", &self.is_draining())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// A drain over a fresh closer and readiness, with the `Closing` the service would
    /// hold, and the GOAWAY receiver.
    fn drain() -> (
        Drain,
        kbf_front::Closing,
        Arc<Readiness>,
        oneshot::Receiver<()>,
    ) {
        let (closer, closing) = kbf_front::closing();
        let readiness = Arc::new(Readiness::default());
        let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
        (drain, closing, readiness, goaway)
    }

    /// Catches a drain that leaves any of its four effects out: `/readyz` still
    /// ready (a front keeps sending work), new Execute calls still taken, no GOAWAY
    /// for the REAPI listener (it keeps accepting), and a drain that does not say it
    /// has begun. Mutants: each line of [`Drain::begin`]'s effects removed in turn.
    #[test]
    fn begin_does_all_of_a_drain_and_says_it_began() {
        let (drain, closing, readiness, mut goaway) = drain();
        assert!(!drain.is_draining());
        assert!(!closing.draining());
        assert!(!readiness.is_stopping());
        assert_eq!(format!("{drain:?}"), "Drain { draining: false, .. }");
        assert!(goaway.try_recv().is_err(), "GOAWAY before the drain");
        assert!(drain.begin(), "the first call begins it");
        assert!(drain.is_draining());
        assert!(closing.draining(), "new Execute calls are still taken");
        assert!(readiness.is_stopping(), "/readyz still ready");
        assert_eq!(goaway.try_recv(), Ok(()), "no GOAWAY sent");
    }

    /// Catches a second trigger (the signal after the endpoint, or the endpoint twice)
    /// that begins again: it must report that it did not, and change nothing.
    #[test]
    fn a_second_begin_changes_nothing() {
        let (drain, closing, _readiness, mut goaway) = drain();
        assert!(drain.begin());
        assert_eq!(goaway.try_recv(), Ok(()));
        assert!(!drain.begin(), "a drain already begun began again");
        assert!(drain.is_draining());
        assert!(closing.draining());
        // The receiver was consumed by the first GOAWAY; a second send would have
        // nowhere to go, and must not panic.
        assert!(!drain.begin());
    }

    /// Catches a `begun` that completes before the drain begins (a listener stopped on
    /// it would close while the server is serving) or never completes once it has (a
    /// listener that keeps accepting after GOAWAY).
    #[tokio::test(start_paused = true)]
    async fn begun_completes_when_the_drain_begins() {
        let (drain, _closing, _readiness, _goaway) = drain();
        let waiting = tokio::time::timeout(std::time::Duration::from_secs(1), drain.begun());
        assert!(waiting.await.is_err(), "begun before the drain");
        drain.begin();
        drain.begun().await;
    }

    /// Catches a `close` that does not announce itself (the shutdown timeout's wait
    /// would sit out its whole time after a second signal), one that does not drain
    /// (a close with no drain would leave `/readyz` ready), and a `closed` that
    /// completes before any close.
    #[tokio::test(start_paused = true)]
    async fn close_drains_and_announces_itself() {
        let (drain, closing, readiness, _goaway) = drain();
        let waiting = tokio::time::timeout(std::time::Duration::from_secs(1), drain.closed());
        assert!(waiting.await.is_err(), "closed before any close");
        drain.close();
        tokio::time::timeout(std::time::Duration::from_secs(60), drain.closed())
            .await
            .expect("closed once closed");
        assert!(drain.is_draining());
        assert!(closing.draining());
        assert!(readiness.is_stopping());
        drain.close();
    }
}
