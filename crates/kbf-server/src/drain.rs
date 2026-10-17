//! Draining the frontend: begun by a stop signal or by `POST /v1/frontend/drain`
//! ([`crate::api`]), in stages.
//!
//! A drain refuses every new Execute UNAVAILABLE and lets the open streams run until
//! their action is done (Buck2 does not recover a stream that breaks, so ending one
//! fails its action: komira-build-farm#791). It takes the server out of `/readyz` and
//! the health service, and sends GOAWAY to the REAPI listener's connections, which then
//! stop accepting. The serving future returns once the REAPI connections have closed.
//!
//! The effects are stages, each idempotent, so an update can take them apart:
//!
//! | stage | effect | undone by |
//! |---|---|---|
//! | [`Drain::refuse`] | new Execute calls refused | [`Drain::unrefuse`], until GOAWAY |
//! | [`Drain::hand_over`] | the server has given its roles away; readiness stays ready and stops asking its probes ([`Readiness::set_handed_over`]) | nothing |
//! | [`Drain::unready`] | `/readyz` and the health service answer not ready | nothing |
//! | [`Drain::goaway`] | the REAPI listener is sent GOAWAY and stops accepting | nothing |
//!
//! [`Drain::begin`] is the stop signal's path: `unready`, `refuse` and `goaway`, once.
//! The second and later calls change nothing, so a signal that arrives after the
//! endpoint (or the endpoint called twice) is a no-op. [`Drain::close`] is the
//! escalation: it ends every stream that is not done UNAVAILABLE, and is what the
//! shutdown timeout and a second signal call.

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

/// The stages that must change together, behind one lock so [`Drain::unrefuse`] cannot
/// undo a refusal that [`Drain::begin`] is making permanent.
struct Stages {
    refusing: bool,
    /// Sends GOAWAY: completes the REAPI listener's graceful shutdown. Taken by the
    /// first [`Drain::goaway`].
    goaway: Option<oneshot::Sender<()>>,
}

/// The frontend's drain: see the module.
pub struct Drain {
    closer: Closer,
    readiness: std::sync::Arc<Readiness>,
    stages: Mutex<Stages>,
    began: AtomicBool,
    /// Given by the first [`Drain::goaway`]: [`Drain::begun`] completes on it, and the
    /// listeners registered with [`Drain::begins`] are closed by it.
    started: Stop,
    closed: watch::Sender<bool>,
}

impl Drain {
    /// A drain over `closer` and `readiness`, and the receiver that completes when it
    /// sends GOAWAY, for the REAPI listener's graceful shutdown.
    #[must_use]
    pub fn new(
        closer: Closer,
        readiness: std::sync::Arc<Readiness>,
    ) -> (Self, oneshot::Receiver<()>) {
        let (goaway, begun) = oneshot::channel();
        let drain = Self {
            closer,
            readiness,
            stages: Mutex::new(Stages {
                refusing: false,
                goaway: Some(goaway),
            }),
            began: AtomicBool::new(false),
            started: Stop::new(),
            closed: watch::Sender::new(false),
        };
        (drain, begun)
    }

    fn stages(&self) -> std::sync::MutexGuard<'_, Stages> {
        self.stages.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refuses every new Execute UNAVAILABLE; open streams run on. Returns whether this
    /// call was the first.
    pub fn refuse(&self) -> bool {
        let mut stages = self.stages();
        self.closer.drain();
        !std::mem::replace(&mut stages.refusing, true)
    }

    /// Takes new Execute calls again after [`Drain::refuse`]. Returns whether it did:
    /// `false` when nothing was refused, or GOAWAY has been sent (a drain that began
    /// is not undone) or the streams were closed.
    pub fn unrefuse(&self) -> bool {
        let mut stages = self.stages();
        if self.began.load(Ordering::SeqCst) || !stages.refusing {
            return false;
        }
        stages.refusing = false;
        self.closer.undrain()
    }

    /// The server has handed its roles to a successor and keeps answering what is
    /// open: it stays ready, and `/readyz` and the health service stop asking the
    /// probes of stores that are frozen or answered through the successor. Returns
    /// whether this call was the first.
    pub fn hand_over(&self) -> bool {
        self.readiness.set_handed_over()
    }

    /// `/readyz` and the health service answer not ready. Returns whether this call
    /// was the first.
    pub fn unready(&self) -> bool {
        self.readiness.stop()
    }

    /// Sends the REAPI listener GOAWAY: it stops accepting and the serving future
    /// returns once its connections have closed. Returns whether this call was the
    /// first; [`Drain::is_draining`] is true from then on.
    pub fn goaway(&self) -> bool {
        let mut stages = self.stages();
        self.send_goaway(&mut stages)
    }

    fn send_goaway(&self, stages: &mut Stages) -> bool {
        if self.began.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.started.fire();
        // Taken once: the `began` swap above lets one call this far. The listener is gone
        // only if the server already stopped, and the send then has no one to tell.
        let _ = stages.goaway.take().map(|goaway| goaway.send(()));
        true
    }

    /// Begins the drain as a stop signal does: every stage, each idempotent, so a drain
    /// already begun is left as it is. Returns whether this call sent GOAWAY, which is
    /// what makes the drain begun.
    ///
    /// In this order: `/readyz` and the health service stop saying ready (before
    /// anything stops, so a front's health check sends no new work here), new Execute
    /// calls are refused, and the REAPI listener is sent GOAWAY.
    pub fn begin(&self) -> bool {
        let mut stages = self.stages();
        self.readiness.stop();
        self.closer.drain();
        stages.refusing = true;
        self.send_goaway(&mut stages)
    }

    /// Completes once the drain has begun (immediately if it has).
    pub async fn begun(&self) {
        self.started.fired().await;
    }

    /// The signal the first [`Drain::goaway`] gives, for a listener to be closed by.
    pub(crate) const fn begins(&self) -> &Stop {
        &self.started
    }

    /// Whether the drain has begun: GOAWAY has been sent.
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
    fn fresh() -> (
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
        let (drain, closing, readiness, mut goaway) = fresh();
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
        let (drain, closing, _readiness, mut goaway) = fresh();
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
        let (drain, _closing, _readiness, _goaway) = fresh();
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
        let (drain, closing, readiness, _goaway) = fresh();
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

    /// A probe that never answers, as a store behind a successor's closed latch does.
    struct Silent;

    impl crate::health::Probe for Silent {
        fn probe(&self) -> futures::future::BoxFuture<'_, Result<(), String>> {
            Box::pin(std::future::pending())
        }
    }

    /// A probe that fails at once.
    struct Down;

    impl crate::health::Probe for Down {
        fn probe(&self) -> futures::future::BoxFuture<'_, Result<(), String>> {
            Box::pin(async { Err("down".to_owned()) })
        }
    }

    /// Everything a stop signal's drain changes, as one line: draining, refusing new
    /// Execute, not ready, handed over, GOAWAY sent.
    fn effects(
        drain: &Drain,
        closing: &kbf_front::Closing,
        readiness: &Readiness,
        goaway: &mut oneshot::Receiver<()>,
    ) -> String {
        format!(
            "draining={} refusing={} unready={} handed_over={} goaway={}",
            drain.is_draining(),
            closing.draining(),
            readiness.is_stopping(),
            readiness.is_handed_over(),
            goaway.try_recv().is_ok(),
        )
    }

    /// The golden of the stop signal: what `begin` changes, and that it changes the same
    /// whichever stages ran before it (a signal that arrives mid-update), and nothing a
    /// second time. Catches a split of `begin` that loses an effect or adds one (such
    /// as `handed_over`, which the signal's path must not set). Mutants: `begin` not
    /// calling `unready`, `refuse` or `goaway`; `begin` setting `hand_over`; `begin`
    /// returning early once `goaway` has been sent.
    #[test]
    fn the_stop_signal_changes_exactly_what_it_did_before_the_split() {
        const GOLDEN: &str =
            "draining=true refusing=true unready=true handed_over=false goaway=true";
        let (drain, closing, readiness, mut goaway) = fresh();
        assert_eq!(
            effects(&drain, &closing, &readiness, &mut goaway),
            "draining=false refusing=false unready=false handed_over=false goaway=false"
        );
        assert!(drain.begin());
        assert_eq!(effects(&drain, &closing, &readiness, &mut goaway), GOLDEN);

        // After each stage alone, `begin` still ends in the golden state.
        type Stage = fn(&Drain);
        let stages: [Stage; 3] = [
            |d| {
                d.unready();
            },
            |d| {
                d.refuse();
            },
            |d| {
                d.goaway();
            },
        ];
        for stage in stages {
            let (drain, closing, readiness, mut goaway) = fresh();
            stage(&drain);
            drain.begin();
            assert_eq!(effects(&drain, &closing, &readiness, &mut goaway), GOLDEN);
        }
        // After a hand-over the same, with the pin kept.
        let (drain, closing, readiness, mut goaway) = fresh();
        drain.hand_over();
        drain.begin();
        assert_eq!(
            effects(&drain, &closing, &readiness, &mut goaway),
            "draining=true refusing=true unready=true handed_over=true goaway=true"
        );
    }

    /// Catches `refuse` that is not reversible (an update that fails its prepare would
    /// leave the old process refusing calls), an `unrefuse` that touches readiness or
    /// sends GOAWAY, and an `unrefuse` that undoes a drain that has begun or closed.
    /// Mutants: `unrefuse` without the `began` check; `refuse` also calling `unready`;
    /// `unrefuse` returning `true` when nothing was refused.
    #[test]
    fn refuse_is_reversible_until_the_drain_begins() {
        let (drain, closing, readiness, mut goaway) = fresh();
        assert!(!drain.unrefuse(), "unrefused what was never refused");
        assert!(drain.refuse());
        assert!(!drain.refuse(), "a second refuse was a first");
        assert!(closing.draining());
        assert!(!readiness.is_stopping(), "refusing flipped readiness");
        assert!(!drain.is_draining(), "refusing is not a begun drain");
        assert!(goaway.try_recv().is_err(), "refusing sent GOAWAY");
        assert!(drain.unrefuse());
        assert!(!closing.draining(), "still refusing after unrefuse");
        assert!(!drain.unrefuse(), "a second unrefuse was a first");
        assert!(drain.refuse(), "refuse again after unrefuse");

        assert!(drain.begin());
        assert!(!drain.unrefuse(), "a drain that began was undone");
        assert!(closing.draining());

        let (drain, closing, _readiness, _goaway) = fresh();
        drain.refuse();
        drain.close();
        assert!(!drain.unrefuse(), "a closed drain was undone");
        assert!(closing.draining());
    }

    /// `hand_over` alone: catches readiness flipped by it (Envoy would stop choosing the
    /// old process before the successor is seen healthy), a store probe still asked
    /// after it (a probe through a frozen or forwarding store can wait out its timeout
    /// or fail while the old process still answers calls), the same through the gRPC
    /// health service, and `unready` that does not flip both. The probe here never
    /// answers, which is longer than any store-probe timeout. Mutants: `hand_over`
    /// calling `unready`; `Checks::failing` asking the probes after a hand-over;
    /// `unready` not reaching the health service.
    #[tokio::test(start_paused = true)]
    async fn hand_over_leaves_readiness_and_health_ready_and_unready_flips_both() {
        use kbf_proto::grpc::health::v1::HealthCheckRequest;
        use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
        use kbf_proto::grpc::health::v1::health_server::Health;

        let guard = std::time::Duration::from_secs(60);
        let (drain, closing, readiness, mut goaway) = fresh();
        let checks = Arc::new(
            crate::health::Checks::new(Arc::clone(&readiness)).probing("store", Arc::new(Silent)),
        );
        let health = crate::grpc_health::HealthService::of(
            Arc::clone(&checks),
            std::time::Duration::from_secs(1),
        );
        let serving = || async {
            tokio::time::timeout(
                guard,
                health.check(tonic::Request::new(HealthCheckRequest::default())),
            )
            .await
            .map(|r| r.expect("a known service").into_inner().status)
        };
        // Control: before the hand-over the silent probe is asked, and never answers.
        assert!(
            tokio::time::timeout(guard, checks.failing()).await.is_err(),
            "the silent probe was not asked before the hand-over"
        );

        assert!(drain.hand_over());
        assert!(!drain.hand_over(), "a second hand_over was a first");
        let failing = tokio::time::timeout(guard, checks.failing())
            .await
            .expect("the probe was asked after the hand-over");
        assert!(failing.is_empty(), "not ready after a hand-over");
        assert_eq!(serving().await, Ok(ServingStatus::Serving as i32));
        assert!(!readiness.is_stopping());
        assert!(!closing.draining(), "hand_over refused calls");
        assert!(!drain.is_draining());
        assert!(goaway.try_recv().is_err(), "hand_over sent GOAWAY");

        // Only `stopping` and `leader` are evaluated after it.
        readiness.set_leader(false);
        let failing = checks.failing().await;
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].check, "leader");
        readiness.set_leader(true);

        assert!(drain.unready());
        assert!(!drain.unready(), "a second unready was a first");
        let failing = checks.failing().await;
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].check, "stopping");
        assert_eq!(serving().await, Ok(ServingStatus::NotServing as i32));
        assert!(!closing.draining(), "unready refused calls");
        assert!(goaway.try_recv().is_err(), "unready sent GOAWAY");
    }

    /// Catches a hand-over pin that hides a failing probe from a server that has not
    /// handed over (the pin must be the hand-over's alone). Mutant: `is_handed_over`
    /// true from the start.
    #[tokio::test]
    async fn a_failing_probe_still_fails_readiness_before_the_hand_over() {
        let (drain, _closing, readiness, _goaway) = fresh();
        let checks =
            crate::health::Checks::new(Arc::clone(&readiness)).probing("store", Arc::new(Down));
        assert_eq!(checks.failing().await.len(), 1);
        drain.hand_over();
        assert!(checks.failing().await.is_empty());
    }

    /// `goaway` alone: catches one that does not say the drain began (the listener
    /// would keep accepting), one that flips readiness or refuses calls, and a second
    /// call that is a first. Mutants: `goaway` not firing `begins`; `goaway` calling
    /// `refuse`.
    #[tokio::test(start_paused = true)]
    async fn goaway_alone_is_one_way_and_touches_nothing_else() {
        let (drain, closing, readiness, mut goaway) = fresh();
        assert!(drain.goaway());
        assert!(!drain.goaway(), "a second goaway was a first");
        assert_eq!(goaway.try_recv(), Ok(()));
        assert!(drain.is_draining());
        drain.begun().await;
        assert!(!closing.draining());
        assert!(!readiness.is_stopping());
        // A stop signal after it still takes the server out of readiness and refuses.
        assert!(!drain.begin(), "GOAWAY was sent twice");
        assert!(closing.draining());
        assert!(readiness.is_stopping());
    }

    /// Catches a `stop` or `set_handed_over` that reports "first" every time, and one
    /// that wakes health watchers when nothing changed. Mutant: the `first` guard
    /// removed from either.
    #[test]
    fn readiness_stages_announce_only_a_change() {
        let readiness = Readiness::default();
        let mut changed = readiness.subscribe();
        assert!(!changed.has_changed().expect("sender alive"));
        assert!(readiness.stop());
        assert!(changed.has_changed().expect("sender alive"));
        changed.mark_unchanged();
        assert!(!readiness.stop());
        assert!(!changed.has_changed().expect("sender alive"));
        assert!(readiness.set_handed_over());
        assert!(changed.has_changed().expect("sender alive"));
        changed.mark_unchanged();
        assert!(!readiness.set_handed_over());
        assert!(!changed.has_changed().expect("sender alive"));
    }
}
