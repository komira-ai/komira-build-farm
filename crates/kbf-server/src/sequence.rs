//! The drain of a frontend over a state store: the order that takes it out of a load
//! balancer, whichever load balancer it is, without dropping a call. `POST
//! /v1/frontend/drain` and a stop signal (SIGTERM) begin the same sequence
//! (`docs/api.md`, `docs/deploy/kbf-units.md`):
//!
//! 1. `/readyz` and the health service answer 503 at once ([`Drain::announce`]), while
//!    everything is still served: unary calls, ByteStream, the streams that are open and
//!    new Execute calls. [`Sequence::begin`] does this before it returns, so the answer to
//!    the `POST` already says `lb_wait`.
//! 2. The frontend waits [`DrainTiming::lb_wait`] (`--drain-lb-wait`, 10 s by default, 0
//!    allowed) so that the load balancer's health check sees the 503. Unary calls have no
//!    client retry, so they must still be served in this wait.
//! 3. It refuses new Execute calls UNAVAILABLE ([`Drain::begin`]), closes its intake
//!    ([`crate::farm::Farm::close_intake`]: no queued work taken from the shared store, none
//!    placed on its nodes, so the other frontends take what it holds queued), and sends GOAWAY on
//!    every listener: REAPI, worker and operator API. The REAPI and worker listeners stop
//!    accepting; the operator API goes on answering so that the drain can be read.
//! 4. It waits until no execution is open, at most [`DrainTiming::deadline`]
//!    (`--drain-deadline`, 15 minutes by default). An execution that runs on a node of this
//!    frontend finishes here: its result is taken, and its caller is answered. An execution
//!    whose operation another frontend took is followed in the state store.
//! 5. It stands down as a retire does ([`Stand::release`]: the farm is fenced, the worker
//!    streams that are left end UNAVAILABLE and their daemons dial another frontend, whose
//!    owner rows keep their leases), closes the client streams that are left
//!    ([`Drain::close`]) and, after a short grace for the clients to take their status when
//!    there was a stream to close, ends the drain ([`Drain::sequence_ended`]); the serving
//!    future returns and the process exits 0.
//!
//! A second stop signal ([`Drain::close`]) skips the waits of steps 2 and 4. The drain is
//! not undone: the process exits, and a new one is started in its place.
//!
//! A frontend without a state store has no other frontend to hand its work to and keeps the
//! plain drain of [`crate::drain`]: [`stopped`] and the endpoint begin it at once.
//!
//! Retire (`POST /v1/frontend/retire`, [`crate::retire`]) is the same fence without the
//! waits and without the exit: it stays a mode of its own, since an updater uses it to stand
//! a frontend down and keep it answering the callers it holds, which a drain that ends the
//! process cannot do. Both end in [`Stand::release`], so the fence is one piece of code.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::drain::Drain;
use crate::retire::Retire;
use crate::serve::{CLOSE_GRACE, drain_and_stop};
use crate::stand::Stand;
use crate::stop::Stop;

/// How often step 4 looks at the executions that are open.
const OPEN_LOOK: Duration = Duration::from_millis(50);

/// The two waits of a frontend's drain (`--drain-lb-wait` and `--drain-deadline`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrainTiming {
    /// How long `/readyz` answers 503 while everything is still served, so that a load
    /// balancer's health check sees it before anything is refused: at least its check
    /// interval times its unhealthy threshold, and the time its routes take to change. 0
    /// goes on at once.
    pub lb_wait: Duration,
    /// The longest the drain waits for the open executions after it refuses new ones; the
    /// ones still open then are ended UNAVAILABLE.
    pub deadline: Duration,
}

impl Default for DrainTiming {
    /// 10 seconds of load balancer wait and 15 minutes of deadline.
    fn default() -> Self {
        Self {
            lb_wait: Duration::from_secs(10),
            deadline: Duration::from_secs(15 * 60),
        }
    }
}

/// Ends the drain sequence ([`Drain::sequence_ended`]) when it is dropped: the sequence runs
/// in a task nobody joins, so a step that panics must not leave the stop waiting for an end
/// that never comes.
struct Ends(Arc<Drain>);

impl Drop for Ends {
    fn drop(&mut self) {
        self.0.sequence_ended();
    }
}

/// The drain sequence of one frontend: the pieces it stops, and its two waits.
#[derive(Clone)]
pub(crate) struct Sequence {
    stand: Stand,
    /// Sends the operator API's connections GOAWAY ([`crate::serve::serve_api_goaway`]).
    api: Stop,
    timing: DrainTiming,
}

impl Sequence {
    /// The sequence of the frontend `stand` gives up, which sends the operator API GOAWAY
    /// with `api`.
    pub(crate) fn new(stand: Stand, api: Stop, timing: DrainTiming) -> Self {
        Self { stand, api, timing }
    }

    /// Begins the drain unless it has begun: the first step is taken before this returns,
    /// and the rest run in a task of their own (a caller that goes away leaves nothing
    /// half done). Returns whether this call began it. Call it inside a Tokio runtime.
    pub(crate) fn begin(&self) -> bool {
        let drain = &self.stand.drain;
        if !drain.sequence_began() {
            return false;
        }
        drain.announce();
        tokio::spawn(self.clone().run());
        true
    }

    /// Steps 2 to 5, after [`Sequence::begin`] took the first. The drain ends when this
    /// returns, and when a step panics.
    async fn run(self) {
        let drain = &self.stand.drain;
        let _ends = Ends(Arc::clone(drain));
        let timing = self.timing;
        tracing::info!(lb_wait = ?timing.lb_wait, "draining: not ready, serving on");
        tokio::select! {
            () = tokio::time::sleep(timing.lb_wait) => {}
            () = drain.closed() => {}
        }

        tracing::info!("draining: refusing new executions, taking no more work, sending GOAWAY");
        self.stand.farm.close_intake();
        drain.begin();
        self.stand.worker.fire();
        self.api.fire();
        let idle = tokio::time::timeout(timing.deadline, async {
            tokio::select! {
                () = self.until_idle() => {}
                () = drain.closed() => {}
            }
        });
        if idle.await.is_err() {
            let open = self.stand.farm.open_executions();
            tracing::warn!(
                open,
                deadline = ?timing.deadline,
                "the drain deadline passed with executions open: closing their streams"
            );
        }

        tracing::info!("draining: standing down, closing the streams that are left");
        let farm = &self.stand.farm;
        let sessions = u64::try_from(farm.open_sessions()).unwrap_or(u64::MAX);
        let streams = farm.open_executions().saturating_add(sessions);
        self.stand.release().await;
        drain.close();
        // The clients that were ended get a moment to take their status before the process
        // goes; with nothing to end, it goes at once.
        if streams > 0 {
            tokio::time::sleep(CLOSE_GRACE).await;
        }
    }

    /// Completes once no execution is open.
    async fn until_idle(&self) {
        while self.stand.farm.open_executions() > 0 {
            tokio::time::sleep(OPEN_LOOK).await;
        }
    }
}

/// What a frontend does once its stop signal has come: begins the drain sequence if it has
/// a state store to run one over (`retire` is set, [`crate::retire`]) and returns when it is
/// over, or when `timeout` (`--shutdown-timeout-secs`) runs out, which closes the streams
/// that are left and ends the sequence at once. A frontend with no state store keeps the
/// plain drain ([`drain_and_stop`]).
pub(crate) async fn stopped(drain: &Arc<Drain>, retire: &OnceLock<Retire>, timeout: Duration) {
    let Some(retire) = retire.get() else {
        drain_and_stop(drain, timeout).await;
        return;
    };
    retire.drain();
    tokio::select! {
        () = drain.sequence_finished() => {}
        () = tokio::time::sleep(timeout) => {
            drain.close();
            drain.sequence_finished().await;
        }
    }
}

#[cfg(test)]
mod tests;
