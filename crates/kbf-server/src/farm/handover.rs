//! What a farm does when the other frontends of its state store take its work over (a
//! retire, `crate::retire`): it stops taking any ([`Farm::fence`]), ends the worker streams
//! so the daemons reconnect to another frontend ([`Farm::end_worker_streams`]), answers the
//! callers it still holds from what the others write to the state store ([`Farm::follow`]),
//! and keeps an operator's cordons across the change.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kbf_front::Stage;
use kbf_sched::{Event, Input};
use kbf_types::{Answer, FarmTime, OperationId, StateMachine as _, WaiterId, WorkerId};
use tokio::sync::watch;
use tonic::Status;

use super::durable::{Durable, Held, Progress, StateError, StoredCordon};
use super::{Farm, NodeAction, State};

/// How many looks in a row at the state store may find an operation neither unfinished
/// nor ended before it is logged as untraced (a WARN). A writer ends an operation in one
/// transaction (its rows removed, its result put), so a look finds one or the other; a
/// store that a release ending operations in two transactions wrote can hold neither for
/// good, and a result past its retention is also gone. Only a log line: an untraced
/// operation's callers are not answered, they keep waiting (see [`Farm::follow`]).
const UNTRACED_LOOKS: u32 = 2;

/// How often a fenced farm that was given to [`Farm::follow_every`] looks in the state
/// store for the results of the calls it kept open.
pub const FOLLOW_EVERY: Duration = Duration::from_millis(200);

/// Why a request that raced the handover is refused (UNAVAILABLE: Buck2 asks again, and
/// the successor answers).
const HANDING_OVER: &str = "this frontend is handing its work to its successor";

/// How often [`Farm::flush_unsettled`] writes again what the state store refused.
const FLUSH_LOOK: Duration = Duration::from_millis(10);

/// One `Result` being taken, counted until dropped.
pub(super) struct Taking<'a>(&'a AtomicUsize);

impl Drop for Taking<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl NodeAction {
    /// How the cordon row keeps this action, `at` milliseconds on the state store's clock:
    /// `None` when the node returns to placement.
    pub(super) fn stored(self, at: u64) -> Option<StoredCordon> {
        match self {
            Self::Cordon => Some(StoredCordon::Cordoned),
            Self::Drain(within) => Some(StoredCordon::Draining {
                deadline_unix_ms: at.saturating_add(millis(within)),
            }),
            Self::Uncordon => None,
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl Farm {
    /// Counts a `Result` as being taken until the guard drops.
    pub(super) fn in_flight(&self) -> Taking<'_> {
        self.reports.fetch_add(1, Ordering::SeqCst);
        Taking(&self.reports)
    }

    /// Whether nothing is half done that a handover should not cut: no `Result` is being
    /// taken and no Execute is between its request and its queueing (the input check).
    #[must_use]
    pub fn quiet(&self) -> bool {
        let presubmit = self.cache.metrics().execute_presubmit_in_flight.get();
        let taking = u64::try_from(self.reports.load(Ordering::SeqCst)).unwrap_or(u64::MAX);
        presubmit + taking == 0
    }

    /// Takes this farm out of service for good, for a successor to take over: from now
    /// on it feeds the scheduler no input (a `Result` is refused, nothing is granted or
    /// given up, a heartbeat changes nothing), queues no request, and writes nothing a
    /// successor's work could depend on. What it already holds stays as it was, for
    /// [`Farm::follow`] to answer.
    pub fn fence(&self) {
        self.lock().fenced = true;
    }

    /// Lets a farm that [`Farm::fence`] stopped for a takeover that did not happen feed the
    /// scheduler again. Nothing was fed while it was fenced, so the farm's memory is the
    /// state store's as of the fence; the caller checks first that the store is still at
    /// the generation this process holds.
    pub fn resume(&self) {
        self.lock().fenced = false;
    }

    /// Closes this farm's intake, the step of a drain (`crate::sequence`) before the fence:
    /// from now on it takes in no operation another frontend queued, and places none on its
    /// nodes (each is cordoned in the scheduler, with no row written to the state store, so
    /// no other frontend sees it), so the operations it holds queued are taken by the other
    /// frontends. It goes on with everything else: a `Result` and a heartbeat of a lease it
    /// holds are taken, and its callers are answered, from its own nodes and from what the
    /// other frontends write ([`Farm::intake`]). Unlike [`Farm::fence`] it refuses nothing a
    /// node reports, so the executions that are running finish here. Nothing undoes it.
    pub fn close_intake(&self) {
        self.lock().closed = true;
    }

    /// Whether the state store has refused a write of this farm because a newer writer
    /// holds it.
    #[must_use]
    pub fn superseded(&self) -> bool {
        self.lock()
            .durable
            .as_ref()
            .is_some_and(Durable::superseded)
    }

    /// Worker streams open now.
    #[must_use]
    pub fn open_sessions(&self) -> usize {
        self.lock().links.len()
    }

    /// Writes, to the state store, the requeues it refused (a lease the scheduler gave
    /// up while the store did not take the write), so that the successor recovers each
    /// operation queued and not under the lease this process gave up. Works on a fenced
    /// farm, which feeds the scheduler nothing and so would never write them again.
    /// Tries again every [`FLUSH_LOOK`] until none is left, and stops at once when the
    /// store says a newer writer holds it (nothing may be written then), or at the
    /// `deadline`; whatever is left is logged at ERROR, naming the operations. Returns
    /// the operations whose requeues were not written.
    pub async fn flush_unsettled(&self, deadline: Duration) -> Vec<OperationId> {
        let until = tokio::time::Instant::now() + deadline;
        let (left, why) = loop {
            // The lock is not held across the sleep.
            let step = self.lock().flush_step();
            match step {
                Flushed::Settled => return Vec::new(),
                Flushed::Superseded(left) => break (left, "the state store has a newer writer"),
                Flushed::Refused(left) => {
                    if tokio::time::Instant::now() >= until {
                        break (
                            left,
                            "the state store kept refusing them until the deadline",
                        );
                    }
                    tokio::time::sleep(FLUSH_LOOK).await;
                }
            }
        };
        let operations: Vec<String> = left.iter().map(|op| format!("{:016x}", op.0)).collect();
        tracing::error!(
            count = left.len(),
            operations = %operations.join(","),
            why,
            "requeues not written to the state store: the successor recovers these operations \
             with the lease they held"
        );
        left
    }

    /// Ends every worker stream with `status`, so each daemon dials again (to the
    /// successor, once this process no longer accepts). Returns how many streams were
    /// still open.
    pub fn end_worker_streams(&self, status: &Status) -> usize {
        let state = self.lock();
        state
            .links
            .values()
            .filter(|link| link.outbound.send(Err(status.clone())).is_ok())
            .count()
    }

    /// Answers, from the state store, the callers this fenced farm still holds, and the
    /// clients that asked it for an operation by name ([`State::follow_watched`]): an
    /// operation the successor finished gets its result from the `done/` row, one it has
    /// leased is shown executing. A caller waits until its operation finishes, it cancels
    /// (its stream is dropped), or this process is killed: no look count or timer ends
    /// it. An operation the store has no trace of keeps its callers waiting; it is
    /// logged at WARN once the looks in a row that found no trace reach `UNTRACED_LOOKS`,
    /// so an operator sees it. Call it every so often; returns how many callers are still
    /// waiting.
    ///
    /// # Errors
    /// The state store cannot be read.
    pub fn follow(&self) -> Result<usize, StateError> {
        self.lock().follow()
    }

    /// Runs [`Farm::follow`] every `every`, for as long as the process lives, so the
    /// callers this fenced farm holds are answered when the state store has their results.
    /// `started` is the caller's once flag: the first call that finds it clear sets it and
    /// starts the follower, and no later call with the same flag starts another (calls with
    /// flags of their own each start one). Returns whether this call started it. Call it
    /// inside a Tokio runtime.
    pub fn follow_every(self: &Arc<Self>, started: &AtomicBool, every: Duration) -> bool {
        if started.swap(true, Ordering::SeqCst) {
            return false;
        }
        let farm = Arc::clone(self);
        tokio::spawn(async move {
            let mut look = tokio::time::interval(every);
            loop {
                look.tick().await;
                // A store that cannot be read is looked at again at the next look.
                let _ = farm.follow();
            }
        });
        true
    }
}

/// How an attempt to write the requeues the state store refused ended.
pub(super) enum Flushed {
    /// Every requeue is written.
    Settled,
    /// A newer writer holds the store: these operations' requeues will not be written.
    Superseded(Vec<OperationId>),
    /// The store refused again: these operations' requeues are still unwritten.
    Refused(Vec<OperationId>),
}

impl State {
    /// Cordons, in the scheduler alone, every node this farm has heard from once its intake
    /// is closed ([`Farm::close_intake`]), so that placement gives none of them work. Before
    /// every input: a node that registers after the intake closed is cordoned before the
    /// placement round that follows its registration. A farm whose intake is open does
    /// nothing. A node that is cordoned already (by this or by an operator) is left as it
    /// is.
    pub(super) fn hold_placements(&mut self, now: FarmTime) {
        if !self.closed {
            return;
        }
        let open = |worker: &&WorkerId| self.sched.cordon(worker).is_none();
        let nodes: Vec<WorkerId> = self.machine.nodes().filter(open).cloned().collect();
        for worker in nodes {
            let none = self.sched.apply(Input::new(now, Event::Cordon { worker }));
            debug_assert!(none.is_empty(), "a cordon has no effect: {none:?}");
        }
    }

    /// The input to feed in place of `event`: on a farm whose intake is closed an uncordon
    /// is a tick, since the scheduler places work on the node inside the uncordon itself,
    /// which would give a node this farm was told not to use work. Any other event, and
    /// every event of a farm whose intake is open, is returned as it is.
    pub(super) fn held_event(&self, event: Event) -> Event {
        match event {
            Event::Uncordon { .. } if self.closed => Event::Tick,
            event => event,
        }
    }

    /// Writes the requeues the state store has not taken, and the ones the scheduler has
    /// given up since the last feed, once. Works on a fenced farm: this is the one thing
    /// a fenced farm writes, because the successor's recovery depends on it (see
    /// [`Farm::flush_unsettled`]).
    pub(super) fn flush_step(&mut self) -> Flushed {
        let superseded = |state: &Self| state.durable.as_ref().is_some_and(Durable::superseded);
        if !superseded(self) {
            self.log_requeues();
        }
        let left: Vec<OperationId> = self
            .unsettled
            .iter()
            .map(|(requeue, ..)| requeue.operation)
            .collect();
        match (left.is_empty(), superseded(self)) {
            (true, _) => Flushed::Settled,
            (false, true) => Flushed::Superseded(left),
            (false, false) => Flushed::Refused(left),
        }
    }

    /// Feeds `event` to the scheduler (see [`State::carry_out`]), unless the farm is
    /// fenced: then nothing is fed and there are no answers.
    pub(super) fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Answer> {
        if self.fenced {
            return Vec::new();
        }
        self.carry_out(now, event)
    }

    /// Writes `request`, about to be queued, to the state store (see
    /// [`State::queue`]), unless the farm is fenced, and says where it is held. `now` is
    /// farm time.
    ///
    /// # Errors
    /// UNAVAILABLE if the farm is fenced or the store did not take it.
    pub(super) fn admit(
        &mut self,
        request: &kbf_sched::Request,
        subject: &crate::action_usage::Subject,
        size_key: Option<u64>,
        now: FarmTime,
    ) -> Result<Held, Status> {
        if self.fenced {
            return Err(Status::unavailable(HANDING_OVER));
        }
        self.queue(request, subject, size_key, now)
    }

    /// The callers of `operation` still waiting: not answered, and with a stream open
    /// (a caller that cancelled has dropped its end of the channel).
    fn callers_waiting(&self, operation: OperationId) -> Vec<WaiterId> {
        let callers = self.sched.waiters(operation).unwrap_or_default();
        self.waiting_among(callers)
    }

    /// The callers among `callers` still waiting: not answered, and with a stream open.
    pub(super) fn waiting_among(&self, callers: &[WaiterId]) -> Vec<WaiterId> {
        let waiting = |caller: &WaiterId| {
            let stage = self.stages.get(caller);
            stage.is_some_and(|stage| {
                !stage.is_closed() && !matches!(*stage.borrow(), Stage::Done(_))
            })
        };
        callers.iter().copied().filter(waiting).collect()
    }

    /// [`Farm::follow`] under the lock: how many callers still wait.
    pub(super) fn follow(&mut self) -> Result<usize, StateError> {
        let Some(durable) = &self.durable else {
            return Ok(0);
        };
        let mut looked = Vec::new();
        for operation in self.sched.unfinished() {
            let callers = self.callers_waiting(operation);
            if !callers.is_empty() {
                looked.push((operation, callers, durable.progress(operation)?));
            }
        }
        // An operation nobody waits on any more (its callers cancelled) has nothing to
        // report.
        self.untraced_kept
            .retain(|operation, _| looked.iter().any(|(looked, ..)| looked == operation));
        let mut waiting = 0;
        for (operation, callers, progress) in looked {
            let stage = note(&mut self.untraced_kept, operation, &progress);
            let ended = matches!(stage, Some(Stage::Done(_)));
            waiting += callers.len() * usize::from(!ended);
            if let Some(stage) = stage {
                for channel in callers.iter().filter_map(|caller| self.stages.get(caller)) {
                    show(channel, stage.clone());
                }
            }
        }
        // The operations clients ask for by name that this farm does not hold are looked at
        // too: a fenced farm takes no intake poll, and answers them from here.
        self.follow_watched();
        Ok(waiting)
    }

    /// Takes up the cordons of the nodes an operator placed before this process started,
    /// as the store holds them: a drain keeps its deadline on the store's clock, `at` now
    /// (already past: paused at the first tick). The rows are remembered as the intake
    /// poll's last look at them.
    pub(super) fn restore_cordons(&mut self, cordons: Vec<(WorkerId, StoredCordon)>, at: u64) {
        let now = FarmTime::default();
        for (worker, cordon) in cordons {
            self.intake.cordons.insert(worker.clone(), cordon);
            let none = self
                .sched
                .apply(Input::new(now, cordon_event(worker, cordon, now, at)));
            debug_assert!(none.is_empty(), "a cordon has no effect: {none:?}");
        }
    }
}

/// The scheduler input that places a node as its `cordon/` row says, at farm time `now`
/// when the store's clock reads `at` milliseconds: a drain ends as far from `now` as its
/// deadline is from `at`, so frontends whose own clocks differ agree on when it ends.
pub(super) fn cordon_event(
    worker: WorkerId,
    cordon: StoredCordon,
    now: FarmTime,
    at: u64,
) -> Event {
    match cordon {
        StoredCordon::Cordoned => Event::Cordon { worker },
        StoredCordon::Draining { deadline_unix_ms } => Event::Drain {
            worker,
            deadline: now
                .saturating_add(Duration::from_millis(deadline_unix_ms.saturating_sub(at))),
        },
    }
}

impl Progress {
    /// What the callers of an operation at this point are shown: `None` leaves them as
    /// they are (queued, or no trace: they wait).
    pub(super) fn stage(&self) -> Option<Stage> {
        match self {
            Self::Ended(finished) => Some(Stage::Done(finished.clone())),
            Self::Running => Some(Stage::Executing),
            Self::Queued | Self::Gone => None,
        }
    }
}

/// One look at `operation`, whose callers wait: what they are shown now (`None` leaves
/// them as they are). An operation the store has no trace of is shown nothing: it is
/// counted in `untraced`, logged once it has been untraced for [`UNTRACED_LOOKS`] looks
/// in a row, and its callers go on waiting. A look that finds it starts the count over.
pub(super) fn note(
    untraced: &mut BTreeMap<OperationId, u32>,
    operation: OperationId,
    progress: &Progress,
) -> Option<Stage> {
    if progress.is_gone() {
        let looks = untraced.entry(operation).or_default();
        *looks = looks.saturating_add(1);
        if *looks == UNTRACED_LOOKS {
            tracing::warn!(%operation, "no trace in the state store: callers keep waiting");
        }
    } else {
        untraced.remove(&operation);
    }
    progress.stage()
}

/// Shows `stage` on `channel`, waking its watchers only if it changed.
pub(super) fn show(channel: &watch::Sender<Stage>, stage: Stage) {
    channel.send_if_modified(|now| {
        let changed = *now != stage;
        if changed {
            *now = stage;
        }
        changed
    });
}
