//! Retire mode of a frontend (`POST /v1/frontend/retire`, [`crate::api`]): the frontend
//! stops taking work and leaves its callers to the others, and is restarted afterwards.
//!
//! Where several frontends share one state store, the operator takes one out of service to
//! update it. A drain alone does not do that: it lets the worker streams and the queue go
//! on. A retire does, in this order:
//!
//! 1. new Execute calls are refused `UNAVAILABLE` ([`crate::drain::Drain::refuse`]);
//! 2. the frontend waits, for a bounded time, for the calls between their request and their
//!    queueing, fences its farm (no intake from the store, no grant, no claim), writes the
//!    requeues the store refused, stops the task that expires the store's rows, stops the
//!    worker listener, ends the worker streams `UNAVAILABLE` (the daemons dial another
//!    frontend) and goes not ready ([`Stand::release`]);
//! 3. it answers the calls it still holds from the `done/` rows the others write
//!    ([`crate::farm::Farm::follow_every`]), for as long as the process lives.
//!
//! The REAPI and operator API listeners keep serving, so the open Execute and WaitExecution
//! streams are answered. A retire is not undone: the unit is restarted.
//!
//! The frontend's drain ([`crate::sequence`], `POST /v1/frontend/drain`) ends in the same
//! stand-down ([`Stand::release`]) and then exits. It is the mode of this struct too, since
//! it needs the same pieces and exists on the same frontends (those over a state store); a
//! retire is the shorter one, which an updater uses and which keeps the process answering.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::farm::FOLLOW_EVERY;
use crate::sequence::Sequence;
use crate::stand::Stand;

/// What `GET /v1/frontend/retire` answers, and what a retire answers after it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RetireStatus {
    /// The retire has completed (the worker streams are ended and the follower runs) and
    /// will not be undone.
    pub retired: bool,
    /// Execute and WaitExecution streams open now: the callers this frontend still
    /// answers.
    pub open_streams: u64,
    /// Worker sessions the frontend holds ([`crate::farm::Farm::open_sessions`]).
    pub open_sessions: usize,
}

/// The retire mode of one frontend: what it stops, and whether it has.
pub struct Retire {
    stand: Stand,
    /// The frontend's drain, over the same pieces ([`Retire::drain`]).
    sequence: Sequence,
    /// The once flag of the follower ([`crate::farm::Farm::follow_every`]): set when the
    /// retire has run, and so what [`Retire::status`] reports.
    retired: Arc<AtomicBool>,
    /// One [`Retire::begin`] runs at a time, so a second call waits for the first and
    /// finds the frontend retired.
    moving: Arc<tokio::sync::Mutex<()>>,
}

impl Retire {
    /// The retire mode of the frontend `stand` gives up, and its drain `sequence`.
    pub(crate) fn new(stand: Stand, sequence: Sequence) -> Self {
        Self {
            stand,
            sequence,
            retired: Arc::new(AtomicBool::new(false)),
            moving: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Begins the frontend's drain ([`crate::sequence`]) unless it has begun, and returns
    /// whether this call began it. Call it inside a Tokio runtime.
    pub(crate) fn drain(&self) -> bool {
        self.sequence.begin()
    }

    /// Retires the frontend: the steps in the module docs. Every step is idempotent, and a
    /// call on a frontend already retired changes nothing. Returns whether this call
    /// retired it.
    ///
    /// The steps run in a task of their own, so a caller that goes away (its request is
    /// dropped) does not leave the frontend fenced and not retired.
    pub async fn begin(&self) -> bool {
        let stand = self.stand.clone();
        let retired = Arc::clone(&self.retired);
        let moving = Arc::clone(&self.moving);
        let steps = tokio::spawn(async move {
            let _moving = moving.lock().await;
            if retired.load(Ordering::SeqCst) {
                return false;
            }
            stand.drain.refuse();
            stand.release().await;
            stand.farm.follow_every(&retired, FOLLOW_EVERY)
        });
        // A task that panicked retired nothing: the status says so.
        steps.await.unwrap_or(false)
    }

    /// Whether the frontend is retired, and what it still holds.
    #[must_use]
    pub fn status(&self) -> RetireStatus {
        RetireStatus {
            retired: self.retired.load(Ordering::SeqCst),
            open_streams: self.stand.farm.open_executions(),
            open_sessions: self.stand.farm.open_sessions(),
        }
    }
}
