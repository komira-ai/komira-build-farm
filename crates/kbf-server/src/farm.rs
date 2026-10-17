//! The farm core of one server: the scheduler, the operations callers wait on, the
//! worker streams, and the action-cache write for accepted results.
//!
//! Everything that changes scheduler state happens under one lock, in the order inputs
//! arrive; the scheduler's effects are carried out under the same lock (`Commit`,
//! `Start`, the offer that precedes a `Start`) or right after it (`Answer`, which reads
//! and writes the cache).
//!
//! **Single node.** The control log is this process: a record the scheduler asks to
//! commit is committed as soon as it is appended, and fed straight back (see
//! [`State::feed`]). The replicated log replaces exactly that step. Since the leases
//! die with the process, each process grants them under its own term, so a lease id
//! names one lease ever and an earlier process's `Result` is refused (issue #137).
//! Operations die with it too, so the term is part of every operation name (issue
//! #154). The term, and the lease epoch `Welcome` names, come from the metadata log
//! ([`MetaLog::leadership`]):
//!
//! - Under the in-memory metadata log (tests only) the log is this process's, so the term is a new
//!   [`process_term`] and the epoch is the same number: every start names another
//!   epoch, and a daemon kills the runs of the leases it held before.
//! - Under the Raft metadata log the term is the Raft term the log elected this server in,
//!   greater at every start, and the epoch is the log id, the same at every start over
//!   one `--meta-dir`. A daemon that reconnects after a restart therefore keeps the
//!   runs of its old leases, which this process never granted: until the scheduler's
//!   state is in the log they are orphans, whose results are refused and which no
//!   heartbeat cancels (docs/design/worker-protocol.md#server-restarts-and-the-lease-epoch
//!   bounds the work booked beside them).
//!
//! **Placement in the log** (issue #166): each grant at debug level, and each lease
//! the scheduler gives up at info, with the operation, the lease, the node and why.
//! An accepted result carries the node that ran it and when it was queued
//! ([`crate::stamp`]).

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::BuildHasher;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::{Backend, Dispatch, Finished, Recorder, Stage, Submission, Ticket};
use kbf_meta::ActionRecord;
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    self, LeaseOffer, NodeStatus, ResultAck, ServerMessage, Start, server_message,
};
use kbf_sched::fence::START_VALIDITY;
use kbf_sched::{
    Cordon, DaemonInstance, Event, FARM_RERUNS, Fate, Input, LOSS_REQUEUES, MemoryLadder, OpState,
    Requeue, Scheduler,
};
use kbf_types::{
    Answer, ControlRecord, Digest, Effect, Failure, FarmRun, FarmTime, LeaseGrant, LeaseId,
    MemoryRun, OperationId, Outcome, Refusal, StartLease, StateMachine, WaiterId, Waiting,
    WorkerId,
};
use tokio::sync::{mpsc, watch};
use tonic::{Code, Status};

mod beat;
mod durable;
mod handover;
mod phase;
mod record;

pub use durable::{DONE_FLOOR, EpochPolicy, SESSION_RENEWAL, StateError};
pub use handover::LOST_LOOKS;
pub use phase::Reported;

use self::durable::{Durable, Undone};
use crate::action_usage::{self, Measured, Shapes};
use crate::fault;
use crate::fleet::{
    NodeView, NodesView, PhaseView, PlacementView, SoftwareView, attention_changes, needs_attention,
};
use crate::machine::{FarmMachine, Sent, Waiter};
use crate::memory;
use crate::missing;
use crate::run_record;
use crate::stamp::Stamp;

/// The scheduler term of a new single-node server process: the wall-clock time of its
/// start in milliseconds since the Unix epoch, times 2^16, plus 16 random bits.
///
/// Every lease id a process grants carries its term, and a single node's leases live
/// in its process alone, so a term two processes share would let a lease id name two
/// leases: an old run's `Result` could be taken for another operation's (issue #137).
/// While the wall clock does not step back across a restart, each term is greater
/// than every earlier process's, as the scheduler's lease order expects of a newer
/// leader's. If it does step back, the new term still differs from an earlier one
/// unless the restart lands on that one's very millisecond and the random bits match
/// (one chance in 65 536). Under the Raft metadata log the Raft term replaces it.
#[must_use]
pub fn process_term() -> u64 {
    let start = unix_ms();
    // A `RandomState` is keyed from the operating system's random source.
    let random = RandomState::new().hash_one(start) & 0xffff;
    (start << 16) | random
}

/// Where the server sends a worker's messages: the outbound half of its stream.
pub type Outbound = mpsc::UnboundedSender<Result<ServerMessage, Status>>;

/// Names one worker stream, so messages from a replaced stream can be told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StreamId(u64);

/// The farm core. Cheap to share: every method takes `&self`.
#[derive(Debug)]
pub struct Farm {
    cache: Backend,
    /// This process's scheduler term: every lease it grants carries it, and every
    /// operation name it gives out names it.
    term: u64,
    /// The lease epoch `Welcome` names: the term itself, or the Raft log's id.
    lease_epoch: u64,
    /// When this farm was built, on tokio's clock (so a test that pauses it moves farm
    /// time with `advance`).
    epoch: tokio::time::Instant,
    /// The wall-clock time of `epoch`, in milliseconds since the Unix epoch: how farm
    /// times are shown to operators.
    epoch_unix_ms: u64,
    /// The action shapes this process has described ([`action_usage`]).
    shapes: Shapes,
    /// `Result`s being taken now ([`Farm::report`]): a handover waits for none to be.
    reports: AtomicUsize,
    state: Mutex<State>,
}

/// An operator action named a node that never registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no node {0} has registered")]
pub struct UnknownNode(pub String);

/// What an operator does to a node's placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAction {
    /// Offer it no new lease; its leases run on.
    Cordon,
    /// Cordon it and wait this long for its leases to end; then the drain pauses.
    Drain(Duration),
    /// Return it to placement, ending any drain.
    Uncordon,
}

/// A worker's newest stream.
#[derive(Debug)]
struct Link {
    stream: StreamId,
    /// The daemon process that opened it.
    instance: DaemonInstance,
    outbound: Outbound,
    /// The seq of the newest heartbeat taken on the stream; 0 before the first. Each
    /// `Start` names it, and the daemon acts on the `Start` only within
    /// [`START_VALIDITY`] of having sent that heartbeat (or, for 0, its `Hello`).
    newest_beat: u64,
}

/// An OK result and its action-cache record, from the report to the answer.
#[derive(Debug)]
struct Pending {
    result: ActionResult,
    record: ActionRecord,
}

/// What a report carries beyond its scheduler outcome, from the report to the answer.
#[derive(Debug)]
enum Detail {
    /// An OK result to answer with and maybe cache.
    Ran(Box<Pending>),
    /// The daemon's reason the action is invalid, passed to the callers.
    Invalid(String),
    /// The input blobs the worker could not fetch, which the callers are asked to
    /// upload again (`crate::missing`).
    Missing(Vec<Digest>),
    /// A timed-out action's partial result ([`crate::timeout`]), answered, never cached.
    TimedOut(Box<ActionResult>),
    /// The daemon's reason for an infrastructure failure (a missing image, a full
    /// disk), or why an OK result could not be accepted: the callers are told only
    /// that the farm could not run the action, and the invocation record keeps it.
    Infra(String),
}

impl Detail {
    /// The failure's reason text a lease's attempt records, if the report gave one.
    fn reason(&self) -> Option<String> {
        match self {
            Self::Invalid(why) | Self::Infra(why) => Some(why.clone()),
            Self::Ran(_) | Self::Missing(_) | Self::TimedOut(_) => None,
        }
    }
}

#[derive(Debug)]
struct State {
    sched: Scheduler,
    /// The callers' records, the finished operations whose callers are kept, the
    /// leases whose `Start` was sent, and the node registry.
    machine: FarmMachine,
    /// How long the scheduler keeps a finished operation; the state store keeps its
    /// `done/` row at least as long.
    finished_retention: Duration,
    /// The channel of each caller the machine keeps, forgotten with its record.
    stages: BTreeMap<WaiterId, watch::Sender<Stage>>,
    next_stream: u64,
    /// The newest stream of each node the machine has registered.
    links: BTreeMap<WorkerId, Link>,
    /// Nodes whose drain has paused, once logged.
    paused: BTreeSet<WorkerId>,
    /// Where the invocation record's lease and finish events go ([`record`]).
    recorder: Recorder,
    /// The operations a recorded call waits on: the only ones [`record`] tells the
    /// recorder of. Each leaves at its end.
    recorded: BTreeSet<OperationId>,
    /// The state store every change is written through before it is acknowledged, if
    /// the farm has one ([`Farm::with_state`]).
    durable: Option<Durable>,
    /// Leases given up that the state store has not yet been told of, oldest first.
    /// Until it has, it still holds the lease and no queue row for the operation, so
    /// every one is written again before anything that follows it is
    /// ([`State::log_requeues`]).
    unsettled: Vec<(Requeue, Option<u32>, Option<Fate>)>,
    /// The effects of committed records whose rows the state store has not taken, one
    /// batch per record, oldest first. A batch leaves only when the store has taken every
    /// row of it, in order, and nothing is carried out (no `Start`, no answer) before its
    /// row is written, so a store that refuses holds the farm's later effects behind the
    /// earliest refused one ([`State::release_unwritten`]).
    unwritten: VecDeque<VecDeque<Effect>>,
    /// The detail of a report whose answer waits in `unwritten`, by lease, for the answer
    /// to be settled with it when the store takes the answer's rows.
    parked: BTreeMap<LeaseId, Detail>,
    /// The `done/` rows the state store has not taken, oldest first
    /// ([`State::retry_results`]).
    undone: Vec<Undone>,
    /// The held effect whose last try was refused as unreachable: a "row is gone" for
    /// this effect and no other may be its own applied work
    /// ([`State::release_unwritten`]).
    retrying: Option<Effect>,
    /// Writes given up on for good: dropped as stale or fenced, past their retention,
    /// or over the cap ([`Farm::dropped_writes`]).
    write_drops: u64,
    /// The most committed records `unwritten` holds before a new one is dropped.
    held_cap: usize,
    /// The scheduler was told the state store is unavailable ([`State::watch_store`]).
    store_down: bool,
    /// When each node's session row in the state store was last written.
    renewed: BTreeMap<WorkerId, FarmTime>,
    /// A handover took the work from this farm ([`Farm::fence`]): it feeds the scheduler
    /// nothing and queues nothing.
    fenced: bool,
    /// For each operation a successor is followed on ([`Farm::follow`]), how many looks
    /// in a row found it neither unfinished nor ended.
    looks: BTreeMap<OperationId, u8>,
}

/// How many committed records the farm holds for a state store that refuses them.
const HELD_CAP: usize = 4096;

/// What a finished operation tells its callers, and the action-cache entry to write
/// first, if any.
struct Settled {
    finished: Finished,
    write: Option<(Digest, ActionRecord)>,
    stages: Vec<watch::Sender<Stage>>,
}

impl Farm {
    /// A farm over `cache`, with an empty scheduler of a new term that refuses queued work once no live worker has been able to run it for
    /// `unservable_wait`, and keeps a finished operation, which WaitExecution still
    /// answers, for `finished_retention`.
    ///
    /// The term and the lease epoch are those of the cache's metadata log
    /// ([`MetaLog::leadership`]: its leader's term, and its id) if it has them, and a
    /// new [`process_term`] for both otherwise (see the module docs).
    #[must_use]
    pub fn new(
        cache: impl Into<Backend>,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        let cache = cache.into();
        let (term, lease_epoch) = cache.leadership().map_or_else(
            || {
                let term = process_term();
                (term, term)
            },
            |log| (log.term, log.log_id.get()),
        );
        let sched = Scheduler::new(term)
            .with_unservable_wait(unservable_wait)
            .with_finished_retention(finished_retention)
            .with_loss_requeues(LOSS_REQUEUES)
            .recording_requeues();
        Self {
            cache,
            term,
            lease_epoch,
            epoch: tokio::time::Instant::now(),
            epoch_unix_ms: unix_ms(),
            shapes: Shapes::default(),
            reports: AtomicUsize::new(0),
            state: Mutex::new(State {
                sched,
                machine: FarmMachine::new(term),
                finished_retention,
                stages: BTreeMap::new(),
                next_stream: 0,
                links: BTreeMap::new(),
                paused: BTreeSet::new(),
                recorder: Recorder::off(),
                recorded: BTreeSet::new(),
                durable: None,
                unsettled: Vec::new(),
                unwritten: VecDeque::new(),
                parked: BTreeMap::new(),
                undone: Vec::new(),
                retrying: None,
                write_drops: 0,
                held_cap: HELD_CAP,
                store_down: false,
                renewed: BTreeMap::new(),
                fenced: false,
                looks: BTreeMap::new(),
            }),
        }
    }

    /// This farm, whose scheduler climbs `ladder` for an action that passes its memory limit
    /// (instead of [`kbf_sched::MemoryLadder::default`]).
    #[must_use]
    pub fn with_memory_ladder(mut self, ladder: MemoryLadder) -> Self {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        state.sched.set_memory_ladder(ladder);
        self
    }

    /// This farm, each Execute's queued or joined answer, each lease granted, started
    /// and given up, and each operation's end sent to `recorder`
    /// (`docs/design/invocations.md`, section 3.4).
    #[must_use]
    pub fn recording(mut self, recorder: Recorder) -> Self {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        state.recorder = recorder;
        self
    }

    /// This process's scheduler term, which every lease it grants carries.
    #[must_use]
    pub const fn term(&self) -> u64 {
        self.term
    }

    /// The lease epoch `Welcome` names (see the module docs).
    #[must_use]
    pub const fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }

    /// Farm time: milliseconds since this farm was built.
    fn now(&self) -> FarmTime {
        let millis = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        FarmTime::from_millis(millis)
    }

    /// Sets the scheduler gauges of `/metrics` from `state`: called where the lock is
    /// already held after a submit and on each tick, so a scrape sees the queue and the
    /// leases at most a tick old.
    fn publish(&self, state: &State) {
        let metrics = self.cache.metrics();
        metrics.sched_queued.set(state.sched.queued_len() as u64);
        metrics.sched_leases.set(state.sched.leases_held() as u64);
    }

    /// Operations waiting in the scheduler's queue for a lease, now (read under the
    /// scheduler's lock, not the gauge `/metrics` serves, which is a tick old).
    #[must_use]
    pub fn queued_operations(&self) -> u64 {
        self.lock().sched.queued_len() as u64
    }

    /// Writes to the state store the farm gave up on for good: held effects dropped as
    /// stale or on a fence, `done/` rows past their retention, records over the cap.
    #[must_use]
    pub fn dropped_writes(&self) -> u64 {
        self.lock().write_drops
    }

    /// Held-back committed records the farm keeps for a refusing state store at most.
    #[doc(hidden)]
    #[must_use]
    pub fn with_held_cap(self, cap: usize) -> Self {
        self.lock().held_cap = cap;
        self
    }

    /// The memory runs of every operation this farm holds that was killed for memory,
    /// oldest first ([`kbf_sched::MemoryState::runs_by_operation`]): what a successor
    /// farm over the same state store must hold too.
    #[must_use]
    pub fn memory_runs(&self) -> BTreeMap<OperationId, Vec<MemoryRun>> {
        self.lock().sched.memory_state().runs_by_operation()
    }

    /// Execute and WaitExecution operation streams open now
    /// (`kbf_front_open_executions`).
    #[must_use]
    pub fn open_executions(&self) -> u64 {
        self.cache.metrics().execute.open_executions.get()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing under the lock panics midway through a change.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The first `Hello` of a stream, from the daemon process `instance`: queues
    /// `welcome` on `outbound`, makes this the worker's stream for every `Start` from now
    /// on, and registers the worker (a new session) with what it `reported`. Its phase
    /// is taken before any placement, and may send a `Start` again or a `Cancel` on this
    /// stream ([`Self::resize`] says when). Messages still arriving on its earlier
    /// stream are ignored from now on. A stream of another process than the earlier
    /// stream's is logged: leases that process ran are kept until it has fenced (issue
    /// #140), unless it is proven stopped. It is when the new process names it as its
    /// `predecessor` (whose runs the new daemon's drivers killed before its `Hello`) and
    /// the earlier stream has closed: a process still connected may be another
    /// machine's, with a copy of this one's disk (issue #22).
    pub fn register(
        &self,
        worker: &WorkerId,
        instance: DaemonInstance,
        predecessor: DaemonInstance,
        reported: Reported,
        outbound: Outbound,
        welcome: ServerMessage,
    ) -> StreamId {
        let now = self.now();
        let mut state = self.lock();
        let stream = StreamId(state.next_stream);
        state.next_stream += 1;
        // The receiver is the stream's own response, alive until the stream ends.
        let _ = outbound.send(Ok(welcome));
        let link = Link {
            stream,
            instance: instance.clone(),
            outbound,
            newest_beat: 0,
        };
        let replaced = state.links.insert(worker.clone(), link);
        state.machine.register(worker);
        let stopped = replaced
            .as_ref()
            .is_some_and(|old| old.instance.same_as(&predecessor) && old.outbound.is_closed());
        if stopped {
            // Its runs were killed: its leases go at once, as farm faults.
            let predecessor = predecessor.as_str();
            tracing::info!(%worker, predecessor, "the daemon restarted and names its predecessor stopped");
        } else if replaced.is_some_and(|old| !old.instance.same_as(&instance)) {
            // The earlier process's leases are kept until it has fenced.
            let cause = "the daemon restarted, or two daemons hold this node's certificate";
            tracing::warn!(%worker, cause, "another daemon process registered as this node");
        }
        state.record_session(now, worker, &instance, stopped, &reported);
        let event = Event::WorkerUp {
            worker: worker.clone(),
            instance,
            predecessor_stopped: stopped,
            capacity: reported.capacity,
            caps: reported.caps,
        };
        // No placement between the registration and the phase: a quarantined node
        // must not be offered a lease in between.
        let none = state.sched.apply(Input::new(now, event));
        debug_assert!(none.is_empty(), "a registration has no effect: {none:?}");
        state.phase(now, worker, reported.phase);
        stream
    }

    /// A `Hello` resent on `stream`: the node report or the node's phase changed.
    /// Changes the worker's capacity, capabilities and phase, and opens no session;
    /// ignored if `stream` was replaced.
    ///
    /// A `Preparing` or `Restoring` phase naming a lease the scheduler holds on the
    /// worker, whose `Start` went to an earlier session, sends that `Start` again (the
    /// daemon restarted for the session switch); one naming a lease of this process it
    /// does not hold there is sent a `Cancel`, so the daemon cleans up after it. A
    /// change into or out of quarantine is logged as an attention item.
    pub fn resize(&self, worker: &WorkerId, stream: StreamId, reported: Reported) {
        let now = self.now();
        let mut state = self.lock();
        if state.is_current(worker, stream) {
            let event = Event::Capacity {
                worker: worker.clone(),
                capacity: reported.capacity,
                caps: reported.caps,
            };
            let none = state.sched.apply(Input::new(now, event));
            debug_assert!(none.is_empty(), "a capacity change has no effect: {none:?}");
            state.phase(now, worker, reported.phase);
        }
    }

    /// A `NodeStatus` on `stream`: kept as the worker's newest, unless `stream` was
    /// replaced (a newer stream sends its own after its `Welcome`). Each attention item
    /// it raises or clears against the node's previous status, from any stream, is
    /// logged once; an Xcode it lists not surveyed yet keeps its previous item
    /// (`crate::fleet`). Returns those lines, `true` for each raised.
    pub fn node_status(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        status: NodeStatus,
    ) -> Vec<(bool, String)> {
        let received = unix_ms();
        let mut state = self.lock();
        if !state.is_current(worker, stream) {
            return Vec::new();
        }
        let mut view = SoftwareView::new(status, received);
        let node = state.machine.node(worker);
        let before = node
            .and_then(|n| n.status.as_ref())
            .map_or(&[][..], |s| &s.xcodes);
        // An Xcode not surveyed yet (a restarted daemon's first status) keeps the item
        // it had: neither cleared now nor raised again by the survey (`crate::fleet`).
        view.hold_unsurveyed(before);
        let changes = attention_changes(worker.as_str(), before, &view.xcodes);
        for (raise, line) in &changes {
            if *raise {
                tracing::warn!(target: "kbf_server::attention", "{line}");
            } else {
                tracing::info!(target: "kbf_server::attention", "{line}");
            }
        }
        state.machine.report_status(worker, view);
        changes
    }

    /// This build, and every node registered since this farm started, in node-id order.
    pub fn nodes(&self) -> NodesView {
        let mut state = self.lock();
        state.note_closed_streams();
        let nodes = state
            .machine
            .nodes()
            .map(|worker| self.node(&state, worker))
            .collect();
        NodesView::of_this_build(nodes)
    }

    /// `worker` as `GET /v1/nodes` lists it, if it has registered.
    pub fn node_view(&self, worker: &WorkerId) -> Option<NodeView> {
        let mut state = self.lock();
        state.note_closed_streams();
        state
            .machine
            .node(worker)
            .is_some()
            .then(|| self.node(&state, worker))
    }

    /// Cordons, drains or uncordons `worker` (see `kbf_sched::Cordon`), and returns
    /// the node as it is now. A drain's deadline counts from now.
    ///
    /// # Errors
    /// `worker` has never registered.
    pub fn place(&self, worker: &WorkerId, action: NodeAction) -> Result<NodeView, UnknownNode> {
        let now = self.now();
        let mut state = self.lock();
        state.note_closed_streams();
        if state.machine.node(worker).is_none() {
            return Err(UnknownNode(worker.as_str().to_owned()));
        }
        let name = worker.clone();
        let event = match action {
            NodeAction::Cordon => Event::Cordon { worker: name },
            NodeAction::Drain(within) => Event::Drain {
                worker: name,
                deadline: now.saturating_add(within),
            },
            NodeAction::Uncordon => Event::Uncordon { worker: name },
        };
        tracing::info!(%worker, ?action, "operator action");
        state.feed_quiet(now, event);
        let at = self.epoch_unix_ms.saturating_add(now.as_millis());
        state.durably("a cordon", |d| d.cordoned(worker, action.stored(at)));
        Ok(self.node(&state, worker))
    }

    /// One node's view; `worker` is registered, and every closed stream noted
    /// ([`State::note_closed_streams`]).
    fn node(&self, state: &State, worker: &WorkerId) -> NodeView {
        let wall = |at: FarmTime| self.epoch_unix_ms.saturating_add(at.as_millis());
        let leases = || {
            let held = state.sched.leases_on(worker);
            held.iter().map(ToString::to_string).collect()
        };
        let placement = match state.sched.cordon(worker) {
            None => PlacementView::Serving,
            Some(Cordon::Cordoned) => PlacementView::Cordoned,
            Some(Cordon::Draining { deadline }) => PlacementView::Draining {
                deadline_unix_ms: wall(*deadline),
                leases: leases(),
            },
            Some(Cordon::Drained) => PlacementView::Drained,
            Some(Cordon::Paused { deadline }) => PlacementView::DrainPaused {
                deadline_unix_ms: wall(*deadline),
                leases: leases(),
            },
        };
        let node = state.machine.node(worker);
        let software = node.and_then(|n| n.status.clone());
        let phase = state.sched.phase(worker).cloned().unwrap_or_default();
        NodeView {
            node_id: worker.as_str().to_owned(),
            connected: node.is_some_and(|n| n.connected),
            needs_attention: needs_attention(software.as_ref(), &phase),
            software,
            placement,
            phase: PhaseView::new(&phase),
        }
    }

    /// Expires leases of silent workers and places queued work. A silent lease lost
    /// past its operation's farm reruns finishes the operation: its callers are
    /// answered (nothing is cached), as a heartbeat's are. Logs each drain that has
    /// paused at its deadline, once.
    pub async fn tick(&self) {
        let now = self.now();
        let settled = {
            let mut state = self.lock();
            let answers = state.feed(now, Event::Tick);
            self.publish(&state);
            let paused: BTreeSet<WorkerId> = state
                .machine
                .nodes()
                .filter(|w| matches!(state.sched.cordon(w), Some(Cordon::Paused { .. })))
                .cloned()
                .collect();
            for worker in paused.difference(&state.paused) {
                let leases = state.sched.leases_on(worker).len();
                tracing::warn!(%worker, leases, "drain paused at its deadline; leases run on");
            }
            state.paused = paused;
            answers
                .iter()
                .map(|a| state.settle(a, None))
                .collect::<Vec<Settled>>()
        };
        for settled in settled {
            self.deliver(settled).await;
        }
    }

    /// A `Result` from `worker`. Accepted only if `worker` holds the operation's
    /// current lease, which this process granted, and the `Result` names no action but
    /// the one that lease runs; an accepted OK result is written to the action cache
    /// (unless the action is `do_not_cache` or exited non-zero) before its callers are
    /// answered. Before it is checked, its `ActionResult` gets the node id and the
    /// server's times ([`Stamp::apply`]), so callers and the cache see them. Returns
    /// the acknowledgement, or `None` for a `Result` without a lease id.
    pub async fn report(&self, worker: &WorkerId, result: worker::Result) -> Option<ResultAck> {
        let wire_lease = result.lease_id?;
        let _taking = self.in_flight();
        let lease = LeaseId::new(wire_lease.term, wire_lease.seq);
        let refused = ResultAck {
            lease_id: Some(wire_lease),
            accepted: false,
        };
        let (holding, recorded) = {
            let state = self.lock();
            let holding = state.holder(lease, worker);
            let recorded = holding
                .as_ref()
                .is_some_and(|(operation, ..)| state.is_recorded(*operation));
            (holding, recorded)
        };
        let Some((operation, action, stamp)) = holding else {
            tracing::warn!(%worker, %lease, "result refused: not the holder of the current lease");
            return Some(refused);
        };
        // The lease id is this process's, so the run it reports was started for this
        // operation; a daemon that says it ran another action is refused all the same
        // (issue #137).
        if let Some(named) = &result.action_digest
            && *named != kbf_front::digest_to_proto(&action)
        {
            tracing::warn!(%worker, %lease, %action, "result refused: it names another action");
            return Some(refused);
        }
        let ran_on = (worker, stamp);
        // Read before `outcome`, which drops a failed run's result.
        let measured = Measured::of(&result);
        let run = recorded.then(|| {
            let usage = measured.as_ref().map(Measured::usage);
            run_record::of(&result, usage, stamp, SystemTime::now())
        });
        let (outcome, mut detail) = self.outcome(lease, ran_on, result).await;

        let now = self.now();
        let (answers, accepted) = {
            let mut state = self.lock();
            let report = Report {
                lease,
                outcome,
                measured,
                run,
            };
            let (answers, accepted) = state.take_report(now, worker, operation, report);
            // The detail is this lease's: an answer the store held back earlier, released
            // by this input, is settled with its own.
            let settled: Vec<Settled> = answers
                .iter()
                .map(|a| {
                    state.settle(
                        a,
                        if a.lease == lease {
                            detail.take()
                        } else {
                            None
                        },
                    )
                })
                .collect();
            if let Some(detail) = detail {
                state.park(lease, detail);
            }
            (settled, accepted)
        };
        for settled in answers {
            self.deliver(settled).await;
        }
        Some(ResultAck {
            lease_id: Some(wire_lease),
            accepted,
        })
    }

    /// The scheduler's outcome for a `Result`, with the cache record of an OK one, the
    /// reason for an INVALID_ARGUMENT one, the partial result of a DEADLINE_EXCEEDED one
    /// ([`crate::timeout`], stamped as an OK result is), or the blobs of a missing-input
    /// one ([`missing::inputs`]). An OK result whose outputs are not all stored is an
    /// infrastructure failure: accepting it would answer callers with files nobody can
    /// fetch. An OK result gets the node it `ran_on` and the server's times
    /// ([`Stamp::apply`]) before it is checked and its cache record made.
    async fn outcome(
        &self,
        lease: LeaseId,
        ran_on: (&WorkerId, Stamp),
        result: worker::Result,
    ) -> (Outcome, Option<Detail>) {
        if let Some(why) = fault::lost(&result) {
            let node = ran_on.0;
            tracing::warn!(%lease, %node, %why, "the farm lost the run");
            return (Outcome::Failed(Failure::Lost(why)), None);
        }
        if let Some(kill) = memory::killed(&result) {
            tracing::info!(%lease, ?kill, "lease killed for memory");
            return (Outcome::Failed(kill), None);
        }
        let code = result.status.as_ref().map_or(Code::Ok as i32, |s| s.code);
        match (Code::from_i32(code), result.action_result) {
            (Code::Ok, Some(mut result)) => {
                let (node, stamp) = ran_on;
                stamp.apply(&mut result, node, SystemTime::now());
                match self.cache.prepare_action_result(&result).await {
                    Ok(record) => {
                        let outcome = Outcome::Completed {
                            action_result: record.result,
                        };
                        (
                            outcome,
                            Some(Detail::Ran(Box::new(Pending { result, record }))),
                        )
                    }
                    Err(e) => {
                        tracing::warn!(%lease, error = %e, "an OK result whose outputs are not stored");
                        let why = format!("an OK result whose outputs are not stored: {e}");
                        (Outcome::Failed(Failure::Infra), Some(Detail::Infra(why)))
                    }
                }
            }
            (Code::DeadlineExceeded, reported) => {
                let partial = match reported {
                    Some(reported) => crate::timeout::partial(&self.cache, reported).await,
                    None => None,
                };
                let detail = partial.map(|mut partial| {
                    let (node, stamp) = ran_on;
                    stamp.apply(&mut partial, node, SystemTime::now());
                    Detail::TimedOut(Box::new(partial))
                });
                (Outcome::Failed(Failure::Timeout), detail)
            }
            (Code::InvalidArgument, _) => {
                let why = result.status.map(|s| s.message).unwrap_or_default();
                (
                    Outcome::Failed(Failure::Invalid),
                    Some(Detail::Invalid(why)),
                )
            }
            (code, _) => match result.status.as_ref().and_then(missing::inputs) {
                Some(blobs) => {
                    tracing::warn!(%lease, blobs = blobs.len(), "input blobs missing at the worker");
                    (
                        Outcome::Failed(Failure::MissingInput),
                        Some(Detail::Missing(blobs)),
                    )
                }
                None => {
                    // The client is told only that the farm could not run the action: the
                    // daemon's reason (a missing image, a full disk) is in this line and
                    // in the invocation record.
                    let why = result.status.map(|s| s.message).unwrap_or_default();
                    tracing::warn!(%lease, node = %ran_on.0, ?code, reason = %why, "lease failed");
                    (Outcome::Failed(Failure::Infra), Some(Detail::Infra(why)))
                }
            },
        }
    }

    /// Writes the action-cache entry, if any, then answers the callers.
    async fn deliver(&self, settled: Settled) {
        if let Some((action, record)) = settled.write
            && let Err(e) = self.cache.commit_action_record(action, record).await
        {
            tracing::error!(%action, error = %e, "action-cache write of an accepted result failed");
        }
        for stage in settled.stages {
            stage.send_replace(Stage::Done(settled.finished.clone()));
        }
    }
}

impl Dispatch for Farm {
    fn submit(&self, mut submission: Submission) -> Result<Ticket, Status> {
        // Outside the lock: hashes the action's shape and drops its argv and paths.
        let usage = self
            .shapes
            .subject(std::mem::take(&mut submission.described));
        let shape = submission.call.as_ref().map(|_| usage.recorded());
        let now = self.now();
        let waiting = std::time::Instant::now();
        let mut state = self.lock();
        self.cache.metrics().sched_lock_wait.since(waiting);
        state.admit(&submission.request)?;
        let action = submission.request.key.action;
        let instance = submission.request.key.instance.clone();
        let waiter = state.machine.submit(Waiter {
            key: submission.request.key.clone(),
            kind: submission.request.kind,
            do_not_cache: submission.request.do_not_cache,
            queued: SystemTime::now(),
            usage,
        });
        let name = state.machine.name(waiter);
        if let Some((call, shape)) = submission.call.take().zip(shape) {
            state.record_submit(call.with_shape(shape), &name, &submission.request);
        }
        let (stage, receiver) = watch::channel(Stage::Queued);
        state.stages.insert(waiter, stage);
        state.feed_quiet(
            now,
            Event::Submit {
                waiter,
                request: submission.request,
            },
        );
        // A caller that joined a twin already started sees it executing.
        let joined_started = state.machine.started_operations().any(|operation| {
            state
                .sched
                .waiters(operation)
                .is_some_and(|w| w.contains(&waiter))
        });
        if let Some(stage) = state.stages.get(&waiter).filter(|_| joined_started) {
            stage.send_replace(Stage::Executing);
        }
        self.publish(&state);
        Ok(Ticket {
            name,
            instance,
            action,
            stage: receiver,
        })
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        let state = self.lock();
        let (id, waiter) = state.machine.named(name)?;
        let stage = state.stages.get(&id)?;
        Some(Ticket {
            name: name.to_owned(),
            instance: waiter.key.instance.clone(),
            action: waiter.key.action,
            stage: stage.subscribe(),
        })
    }
}

impl State {
    /// How many runs of `operation` the scheduler has recorded as killed for memory or
    /// as the farm's fault.
    fn runs_recorded(&self, operation: OperationId) -> usize {
        let memory = self.sched.memory_runs(operation).map_or(0, <[_]>::len);
        let farm = self.sched.farm_runs(operation).map_or(0, <[_]>::len);
        memory + farm
    }

    /// Feeds `worker`'s report that `lease` of `operation` ended with `outcome`, and
    /// returns the answers it led to and whether the scheduler took it. A busy node's
    /// memory kill the scheduler took is logged for operators (`crate::memory`), and a
    /// report the scheduler took that `measured` what the lease used has its `action
    /// usage:` line ([`action_usage`]).
    ///
    /// The in-process log commits its record at once: an accepted result is answered
    /// here, and a refused one (its lease given up while its outputs were checked)
    /// never is. A memory kill or lost run the scheduler takes is answered, or records
    /// a run and runs the operation again. The replicated log will answer once the
    /// record commits, carrying the result with it.
    ///
    /// A recorded operation's report the scheduler took has its measurement sent to
    /// the invocation recorder, with the booking of a memory kill.
    ///
    /// Not generic, unlike `Farm::report`, so every test binary's runs count toward the
    /// same function's coverage.
    fn take_report(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        operation: OperationId,
        report: Report,
    ) -> (Vec<Answer>, bool) {
        let Report {
            lease,
            outcome,
            measured,
            run,
        } = report;
        // Taken first: a report that finishes the operation may drop its callers.
        let usage = measured.and_then(|measured| {
            let (_, waiter) = self.first_waiter(operation)?;
            Some((waiter.usage.clone(), measured))
        });
        let recorded_before = self.runs_recorded(operation);
        let name = self.operation_name(operation);
        let event = Event::Report {
            operation,
            lease,
            outcome,
        };
        let answers = self.feed(now, event);
        let rerun = self.runs_recorded(operation) > recorded_before;
        let accepted = rerun || answers.iter().any(|a| a.lease == lease);
        if accepted && let Some((subject, measured)) = &usage {
            action_usage::log(operation, subject, measured, outcome, lease, worker);
        }
        if accepted && let Some(run) = run {
            let killed = match outcome {
                Outcome::Failed(Failure::OutOfMemory | Failure::NodeMemoryPressure) => answers
                    .iter()
                    .find(|a| a.lease == lease)
                    .map(|a| a.memory_runs.as_slice())
                    .or_else(|| self.sched.memory_runs(operation))
                    .and_then(<[_]>::last)
                    .filter(|r| r.worker == *worker),
                _ => None,
            };
            let run = match killed {
                Some(killed) => run_record::killed(run, killed),
                None => run,
            };
            self.record_ran(operation, lease, run);
        }
        let (operation, node) = (name, worker);
        match outcome {
            Outcome::Completed { .. } => {
                tracing::info!(%operation, %lease, %node, accepted, "lease finished ok");
            }
            Outcome::Failed(failure) => {
                tracing::warn!(%operation, %lease, %node, ?failure, accepted, "lease finished failed");
            }
        }
        if accepted && outcome == Outcome::Failed(Failure::NodeMemoryPressure) {
            memory::pressure(worker, self.sched.memory_pressure(worker));
        }
        (answers, accepted)
    }

    fn is_current(&self, worker: &WorkerId, stream: StreamId) -> bool {
        self.links.get(worker).is_some_and(|l| l.stream == stream)
    }

    /// Tells the machine of each node whose newest stream has closed (its response
    /// side is gone), so its registry says the node is not connected.
    fn note_closed_streams(&mut self) {
        for (worker, link) in &self.links {
            if link.outbound.is_closed() {
                self.machine.disconnect(worker);
            }
        }
    }

    /// The operation whose current lease is `lease`, the action its `Start` named,
    /// and the server's times for the run, if `worker` holds it. Only this process's
    /// leases are in the machine's started table, so a lease of an earlier process
    /// (another term) is never one.
    fn holder(&self, lease: LeaseId, worker: &WorkerId) -> Option<(OperationId, Digest, Stamp)> {
        let sent = self.machine.sent(lease)?;
        let operation = sent.operation;
        let current = match self.sched.state(operation)? {
            OpState::Leased {
                lease: held,
                worker: holder,
                committed: true,
            }
            | OpState::Running {
                lease: held,
                worker: holder,
            } => *held == lease && holder == worker,
            _ => false,
        };
        current.then_some((operation, sent.action, sent.stamp))
    }

    /// Feeds an input that cannot lead to an answer: only a `Report`, a heartbeat (a
    /// lease lost to a proven restart or a replacement) and a tick (a silent lease)
    /// propose a result, and only a committed result answers; those three deliver their
    /// answers themselves.
    fn feed_quiet(&mut self, now: FarmTime, event: Event) {
        let answers = self.feed(now, event);
        debug_assert!(
            answers.is_empty(),
            "an answer without a report: {answers:?}"
        );
    }

    /// [`State::feed`] on a farm that is not fenced: feeds `event`, then a tick (so
    /// placement follows at once), and carries out the effects in order. Returns the
    /// answers, for the caller to deliver once the lock is released.
    fn carry_out(&mut self, now: FarmTime, event: Event) -> Vec<Answer> {
        self.watch_store(now);
        // Only the inputs whose callers deliver answers ([`State::feed_quiet`]) release
        // effects an earlier refusal held back: one of them may be an answer.
        let answering = matches!(
            event,
            Event::Tick | Event::Heartbeat { .. } | Event::Report { .. }
        );
        let mut effects: VecDeque<Effect> = self.sched.apply(Input::new(now, event)).into();
        effects.extend(self.sched.apply(Input::new(now, Event::Tick)));
        self.log_requeues();
        if answering {
            // Before this input's own effects: they follow what was held back.
            for effect in self.release_unwritten().into_iter().rev() {
                effects.push_front(effect);
            }
        }
        self.retry_results();
        let mut answers = Vec::new();
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(record) => {
                    if let ControlRecord::Lease(grant) = &record {
                        self.offer(grant);
                    }
                    // Appended is committed; the effects of the commit (the `Start`, the
                    // answer) are carried out only once the state store has taken what
                    // they settle. What it refuses is kept and written again, in order,
                    // until it takes it or a newer writer fences it.
                    let committed = Event::Committed(record);
                    let next = self.sched.apply(Input::new(now, committed));
                    if self.unwritten.len() >= self.held_cap {
                        self.write_drops += 1;
                        tracing::error!(cap = self.held_cap, "records over the cap: one dropped");
                        continue;
                    }
                    self.unwritten.push_back(next.into());
                    if answering || self.unwritten.len() == 1 {
                        effects.extend(self.release_unwritten());
                    }
                }
                Effect::Start(start) => self.start(start),
                Effect::Answer(answer) => {
                    self.log_failed_answer(&answer);
                    answers.push(answer);
                }
                Effect::Waiting(waiting) => self.waiting(&waiting),
                Effect::Refuse(refusal) => self.refuse(&refusal),
            }
        }
        self.log_requeues();
        self.forget_dropped();
        answers
    }

    /// Logs, as one WARN line, an operation the scheduler finishes failed: the failure
    /// kind, and how much of the farm-fault budget and the memory ladder its runs used
    /// (`farm_runs` at `farm_budget` is the farm budget spent, so no rerun is left).
    /// Operations that complete are logged where their lease's result is taken
    /// ([`State::take_report`]).
    fn log_failed_answer(&self, answer: &Answer) {
        if let Outcome::Failed(failure) = answer.outcome {
            let operation = self.operation_name(answer.operation);
            tracing::warn!(
                %operation,
                lease = %answer.lease,
                ?failure,
                farm_runs = answer.farm_runs.len(),
                farm_budget = FARM_RERUNS,
                memory_runs = answer.memory_runs.len(),
                "operation finished failed"
            );
        }
    }

    /// Logs the leases the scheduler has given up since the last call, one INFO line
    /// each, naming the operation by its first caller's name. Called before any
    /// effect is carried out (an effect can finish an operation, and with a zero
    /// finished retention drop it and forget its callers) and again at the end of a
    /// feed. A requeue only names an operation the scheduler held when it was made;
    /// one dropped since is logged with an empty name, never looked up by index.
    fn log_requeues(&mut self) {
        let given_up = self.sched.take_requeues();
        self.settle_requeues(&given_up);
        for requeue in given_up {
            self.record_requeue(&requeue);
            let Requeue {
                operation,
                lease,
                worker: node,
                reason,
            } = requeue;
            let farm_runs = self.sched.farm_runs(operation).map_or(0, <[_]>::len);
            let operation = self.operation_name(operation);
            // One line: the coverage of a logged field counts only where it is logged.
            // `farm_runs` is the farm-fault budget spent so far, of `farm_budget` reruns
            // (a lease not started has a budget of its own, in the scheduler).
            tracing::info!(
                %operation, %lease, %node, %reason, farm_runs, farm_budget = FARM_RERUNS,
                "lease given up; requeued"
            );
        }
    }

    /// Keeps the callers of `operation`, which has just finished, until the scheduler
    /// drops it.
    fn keep_finished(
        &mut self,
        operation: OperationId,
        waiters: Vec<WaiterId>,
        finished: &Finished,
    ) {
        // After the finish is durable and before any caller is answered from this
        // process's memory alone: a process that kept streams open across a handover
        // reads the result here. A store that does not take it is logged and the callers
        // here are answered all the same.
        let retention = self.finished_retention;
        self.keep_result(operation, finished, retention);
        self.machine.keep_finished(operation, waiters);
        self.forget_dropped();
    }

    /// Forgets the callers, and their channels, of every finished operation the
    /// scheduler has dropped ([`FarmMachine::forget_dropped`]): a WaitExecution on one
    /// is NOT_FOUND from now on. Every input ends here, so debug builds check here
    /// that the callers with a channel are exactly the callers the machine keeps: the
    /// same ids, not only as many.
    fn forget_dropped(&mut self) {
        let sched = &self.sched;
        let forgotten = self
            .machine
            .forget_dropped(|operation| sched.state(operation).is_some());
        for id in forgotten {
            self.stages.remove(&id);
        }
        debug_assert!(
            self.stages.keys().copied().eq(self.machine.waiter_ids()),
            "a caller's channel outlives its record, or a record has no channel"
        );
    }

    /// The REAPI name of `operation`, its first waiter's, as the log names it; empty
    /// for an operation without one.
    fn operation_name(&self, operation: OperationId) -> String {
        self.first_waiter(operation)
            .map_or_else(String::new, |(id, _)| self.machine.name(id))
    }

    /// The first waiter of `operation`: its key and lease kind are the operation's.
    fn first_waiter(&self, operation: OperationId) -> Option<(WaiterId, &Waiter)> {
        let first = *self.sched.waiters(operation)?.first()?;
        self.machine.waiter(first).map(|waiter| (first, waiter))
    }

    /// Sends `worker` the message `build` makes from `operation`'s first waiter.
    ///
    /// Both are always there, so nothing here branches on them: the scheduler places
    /// work only on a worker that registered (and a worker's link outlives its stream),
    /// and only operations a waiter submitted (waiters leave only once it is finished).
    /// A stream that has just ended drops the message; its lease is reconciled later.
    fn send_for(
        &self,
        worker: &WorkerId,
        operation: OperationId,
        build: impl FnOnce(&Waiter) -> server_message::Message,
    ) {
        let message = self
            .first_waiter(operation)
            .map(|(_, waiter)| build(waiter));
        // Whether it was sent: a stream that has just ended drops it.
        let _sent = self.links.get(worker).zip(message).map(|(link, message)| {
            link.outbound
                .send(Ok(ServerMessage {
                    message: Some(message),
                }))
                .is_ok()
        });
    }

    /// Tells the worker a lease is placed on it, before the grant commits.
    fn offer(&self, grant: &LeaseGrant) {
        let (operation, lease, node) = (
            self.operation_name(grant.operation),
            grant.lease,
            &grant.worker,
        );
        // The key joins the daemon's `lease phases:` line (`lease=`) to the action, also
        // for a lease whose result is never taken (cancelled).
        let key = self
            .first_waiter(grant.operation)
            .map(|(_, w)| tracing::field::display(w.usage.key));
        tracing::info!(%operation, %lease, %node, key, "lease granted");
        self.record_granted(grant);
        self.send_for(&grant.worker, grant.operation, |w| {
            server_message::Message::LeaseOffer(LeaseOffer {
                lease_id: Some(wire_lease(grant.lease)),
                kind: w.kind.name().to_owned(),
                action_digest: Some(kbf_front::digest_to_proto(&w.key.action)),
            })
        });
    }

    /// Sends the `Start` of a committed lease and marks its callers executing. The
    /// `Start` names the newest heartbeat taken on the worker's stream and the window
    /// after it in which the daemon may still act on the `Start` (issue #23).
    fn start(&mut self, start: StartLease) {
        let heartbeat_seq = self.links.get(&start.worker).map_or(0, |l| l.newest_beat);
        self.send_for(&start.worker, start.operation, |_| {
            server_message::Message::Start(Start {
                lease_id: Some(wire_lease(start.lease)),
                kind: start.kind.name().to_owned(),
                action_digest: Some(kbf_front::digest_to_proto(&start.key.action)),
                millicpus: start.resources.cpu_millis,
                memory_bytes: start.resources.memory_bytes,
                heartbeat_seq,
                valid_for_ms: START_VALIDITY_MS,
                device_id: start.device.clone().unwrap_or_default(),
            })
        });
        let waiters = self.sched.waiters(start.operation).unwrap_or_default();
        for stage in waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Executing);
        }
        let now = SystemTime::now();
        // An operation with a `Start` has its first waiter (see `send_for`).
        let queued = self
            .first_waiter(start.operation)
            .map_or(now, |(_, w)| w.queued);
        let sent = Sent {
            operation: start.operation,
            action: start.key.action,
            stamp: Stamp {
                queued,
                started: now,
            },
        };
        self.machine.start(start.lease, sent);
        self.record_started(&start);
    }

    /// Tells an operation's callers why it waits, or that it no longer waits for a
    /// worker that can run it. Logged, so an operator sees it too.
    fn waiting(&self, waiting: &Waiting) {
        let operation = waiting.operation;
        let stage = match &waiting.reason {
            Some(why) => {
                tracing::warn!(%operation, reason = %why, "no live worker can run the operation");
                Stage::Waiting(why.clone())
            }
            None => {
                tracing::info!(%operation, "a live worker can run the operation again");
                Stage::Queued
            }
        };
        // A caller that joins a waiting operation is told again; the others, who
        // already know, see no repeated update.
        let waiters = self.sched.waiters(operation).unwrap_or_default();
        for channel in waiters.iter().filter_map(|id| self.stages.get(id)) {
            channel.send_if_modified(|now| {
                let changed = *now != stage;
                if changed {
                    now.clone_from(&stage);
                }
                changed
            });
        }
    }

    /// Answers the callers of an operation the scheduler refused FAILED_PRECONDITION
    /// with the reason, and keeps them for the finished retention. Nothing is cached,
    /// so nothing is awaited.
    fn refuse(&mut self, refusal: &Refusal) {
        tracing::warn!(operation = %refusal.operation, reason = %refusal.reason, "operation refused");
        self.record_refused(refusal);
        let finished = failed(Code::FailedPrecondition, &refusal.reason);
        // A lease given up before the operation was refused may still be listed.
        self.machine.forget_leases(refusal.operation);
        for stage in refusal.waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Done(finished.clone()));
        }
        self.keep_finished(refusal.operation, refusal.waiters.clone(), &finished);
    }

    /// Works out what the callers of a finished operation get, and keeps them for the
    /// finished retention. `detail` is what the accepted report carried beyond its
    /// outcome, if anything.
    fn settle(&mut self, answer: &Answer, detail: Option<Detail>) -> Settled {
        // The report's detail, kept while the store held its answer back.
        let detail = detail.or_else(|| self.parked.remove(&answer.lease));
        self.machine.forget_leases(answer.operation);
        let waiters: Vec<&Waiter> = answer
            .waiters
            .iter()
            .filter_map(|id| self.machine.waiter(*id))
            .collect();
        let stages = answer
            .waiters
            .iter()
            .filter_map(|id| self.stages.get(id).cloned())
            .collect();
        let reason = detail.as_ref().and_then(Detail::reason);
        let mut stored = None;
        let (finished, write) = match (detail, answer.outcome) {
            (Some(Detail::Ran(pending)), _) => {
                let Pending { result, record } = *pending;
                stored = Some(record.result);
                let cacheable =
                    result.exit_code == 0 && waiters.first().is_some_and(|w| !w.do_not_cache);
                let write = waiters
                    .first()
                    .filter(|_| cacheable)
                    .map(|w| (w.key.action, record));
                (Finished::Ran(Box::new(result)), write)
            }
            (Some(Detail::TimedOut(partial)), Outcome::Failed(Failure::Timeout)) => {
                let status = status(Code::DeadlineExceeded, TIMED_OUT);
                (Finished::TimedOut { status, partial }, None)
            }
            (_, Outcome::Failed(Failure::Timeout)) => {
                (failed(Code::DeadlineExceeded, TIMED_OUT), None)
            }
            (Some(Detail::Invalid(why)), _) => (failed(Code::InvalidArgument, &why), None),
            (Some(Detail::Missing(blobs)), _) => {
                (Finished::Failed(kbf_front::missing_status(&blobs)), None)
            }
            (_, Outcome::Failed(Failure::OutOfMemory)) => {
                (memory::finished(&answer.memory_runs), None)
            }
            (_, outcome) => (unrun(outcome, &answer.farm_runs), None),
        };
        self.record_finished(answer, &finished, stored, reason);
        self.keep_finished(answer.operation, answer.waiters.clone(), &finished);
        Settled {
            finished,
            write,
            stages,
        }
    }
}

/// A worker's report as [`State::take_report`] feeds it: the lease, the scheduler's
/// outcome, and what the report measured for the `action usage:` line and for the
/// invocation record (`None` for an operation no recorded call waits on).
struct Report {
    lease: LeaseId,
    outcome: Outcome,
    measured: Option<Measured>,
    run: Option<kbf_invocation::Run>,
}

/// Now on the wall clock, in milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// What the callers of a timed-out action are told.
const TIMED_OUT: &str = "the action timed out";

fn failed(code: Code, message: &str) -> Finished {
    Finished::Failed(status(code, message))
}

fn status(code: Code, message: &str) -> rpc::Status {
    rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    }
}

/// [`START_VALIDITY`] in milliseconds, as `Start.valid_for_ms` carries it.
const START_VALIDITY_MS: u64 = START_VALIDITY.as_millis() as u64;

fn wire_lease(lease: LeaseId) -> worker::LeaseId {
    worker::LeaseId {
        term: lease.term,
        seq: lease.seq,
    }
}

/// How a result that carries no detail and is no timeout, invalid input, missing blob or
/// memory kill is answered: a farm fault names its runs; anything else (a committed
/// `Completed` whose worker report this process never saw) is the fixed INTERNAL.
fn unrun(outcome: Outcome, farm_runs: &[FarmRun]) -> Finished {
    match outcome {
        Outcome::Failed(Failure::NodeMemoryPressure | Failure::Lost(_) | Failure::Infra) => {
            fault::finished(farm_runs)
        }
        _ => failed(Code::Internal, "the farm could not run the action"),
    }
}

#[cfg(test)]
mod tests;
