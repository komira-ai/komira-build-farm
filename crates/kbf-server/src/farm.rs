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
//! die with the process, each process grants them under its own term
//! ([`process_term`]), which it also names as its lease epoch in `Welcome` (issue
//! #137). Operations die with it too, so the term is part of every operation name
//! (issue #154).
//!
//! **Placement in the log** (issue #166): each grant at debug level, and each lease
//! the scheduler gives up at info, with the operation, the lease, the node and why.
//! An accepted result carries the node that ran it and when it was queued
//! ([`crate::stamp`]).

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Dispatch, Finished, MetaLog, Stage, Submission, Ticket};
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::ObjectStore;
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    self, Cancel, LeaseOffer, NodeStatus, ResultAck, ServerMessage, Start, server_message,
};
use kbf_sched::fence::START_VALIDITY;
use kbf_sched::{Cordon, DaemonInstance, Event, Input, OpState, Requeue, Scheduler};
use kbf_types::{
    Answer, ControlRecord, Digest, Effect, Failure, FarmTime, LeaseGrant, LeaseId, OperationId,
    Outcome, Refusal, Resources, StartLease, StateMachine, WaiterId, Waiting, WorkerId,
};
use tokio::sync::{mpsc, watch};
use tonic::{Code, Status};

use crate::fleet::{NodeView, NodesView, PlacementView, SoftwareView, attention_changes};
use crate::machine::{FarmMachine, Sent, Waiter};
use crate::memory;
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
/// (one chance in 65 536). The replicated log's term replaces this once it is wired.
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
pub struct Farm<M, O> {
    cache: Arc<Cache<M, O>>,
    /// This process's scheduler term ([`process_term`]): every lease it grants carries
    /// it, and `Welcome` names it as the lease epoch.
    term: u64,
    epoch: Instant,
    /// The wall-clock time of `epoch`, in milliseconds since the Unix epoch: how farm
    /// times are shown to operators.
    epoch_unix_ms: u64,
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
}

#[derive(Debug)]
struct State {
    sched: Scheduler,
    /// The callers' records, the finished operations whose callers are kept, and the
    /// leases whose `Start` was sent.
    machine: FarmMachine,
    /// The channel of each caller the machine keeps, forgotten with its record.
    stages: BTreeMap<WaiterId, watch::Sender<Stage>>,
    next_stream: u64,
    links: BTreeMap<WorkerId, Link>,
    /// Each node's newest `NodeStatus`, kept across its streams.
    software: BTreeMap<WorkerId, SoftwareView>,
    /// Nodes whose drain has paused, once logged.
    paused: BTreeSet<WorkerId>,
}

/// What a finished operation tells its callers, and the action-cache entry to write
/// first, if any.
struct Settled {
    finished: Finished,
    write: Option<(Digest, ActionRecord)>,
    stages: Vec<watch::Sender<Stage>>,
}

impl<M: MetaLog, O: ObjectStore> Farm<M, O> {
    /// A farm over `cache`, with an empty scheduler of a new term ([`process_term`])
    /// that refuses queued work once no live worker has been able to run it for
    /// `unservable_wait`, and keeps a finished operation, which WaitExecution still
    /// answers, for `finished_retention`.
    #[must_use]
    pub fn new(
        cache: Arc<Cache<M, O>>,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        let term = process_term();
        let sched = Scheduler::new(term)
            .with_unservable_wait(unservable_wait)
            .with_finished_retention(finished_retention)
            .recording_requeues();
        Self {
            cache,
            term,
            epoch: Instant::now(),
            epoch_unix_ms: unix_ms(),
            state: Mutex::new(State {
                sched,
                machine: FarmMachine::new(term),
                stages: BTreeMap::new(),
                next_stream: 0,
                links: BTreeMap::new(),
                software: BTreeMap::new(),
                paused: BTreeSet::new(),
            }),
        }
    }

    /// This process's scheduler term, which `Welcome` names as the lease epoch.
    #[must_use]
    pub const fn term(&self) -> u64 {
        self.term
    }

    /// Farm time: milliseconds since this farm was built.
    fn now(&self) -> FarmTime {
        let millis = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        FarmTime::from_millis(millis)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing under the lock panics midway through a change.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The first `Hello` of a stream, from the daemon process `instance`: queues
    /// `welcome` on `outbound`, makes this the worker's stream for every `Start` from now
    /// on, and registers the worker (a new session) with its `capacity` and `caps`.
    /// Messages still arriving on its earlier stream are ignored from now on. A stream
    /// of another process than the earlier stream's is logged: leases that process ran
    /// are kept until it has fenced (issue #140).
    pub fn register(
        &self,
        worker: &WorkerId,
        instance: DaemonInstance,
        capacity: Resources,
        caps: NodeCaps,
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
        if replaced.is_some_and(|old| !old.instance.same_as(&instance)) {
            // The earlier process's leases are kept until it has fenced.
            let cause = "the daemon restarted, or two daemons hold this node's certificate";
            tracing::warn!(%worker, cause, "another daemon process registered as this node");
        }
        let event = Event::WorkerUp {
            worker: worker.clone(),
            instance,
            capacity,
            caps,
        };
        state.feed_quiet(now, event);
        stream
    }

    /// A `Hello` resent on `stream`: the node report changed. Changes the worker's
    /// capacity and capabilities and nothing else; ignored if `stream` was replaced.
    pub fn resize(&self, worker: &WorkerId, stream: StreamId, capacity: Resources, caps: NodeCaps) {
        let now = self.now();
        let mut state = self.lock();
        if state.is_current(worker, stream) {
            let event = Event::Capacity {
                worker: worker.clone(),
                capacity,
                caps,
            };
            state.feed_quiet(now, event);
        }
    }

    /// Heartbeat `seq` on `stream`. Returns whether it was taken (and is to be
    /// acknowledged): a heartbeat from a replaced stream is dropped. Each lease it
    /// lists that the scheduler no longer holds on `worker` is sent a `Cancel`.
    pub fn heartbeat(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        seq: u64,
        running: Vec<LeaseId>,
    ) -> bool {
        let now = self.now();
        let mut state = self.lock();
        let Some(link) = state.links.get_mut(worker).filter(|l| l.stream == stream) else {
            return false;
        };
        link.newest_beat = link.newest_beat.max(seq);
        let outbound = link.outbound.clone();
        let event = Event::Heartbeat {
            worker: worker.clone(),
            running: running.clone(),
        };
        state.feed_quiet(now, event);
        for lease in state.sched.not_held(worker, &running) {
            tracing::info!(%worker, %lease, "a lease not held here is listed: cancelled");
            let cancel = server_message::Message::Cancel(Cancel {
                lease_id: Some(wire_lease(lease)),
            });
            // A stream that has just ended drops it; the next heartbeat listing the
            // lease, on the next stream, sends it again.
            let _ = outbound.send(Ok(ServerMessage {
                message: Some(cancel),
            }));
        }
        true
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
        let before = state.software.get(worker).map_or(&[][..], |s| &s.xcodes);
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
        state.software.insert(worker.clone(), view);
        changes
    }

    /// This build, and every node registered since this farm started, in node-id order.
    pub fn nodes(&self) -> NodesView {
        let state = self.lock();
        let nodes = state
            .links
            .keys()
            .map(|worker| self.node(&state, worker))
            .collect();
        NodesView::of_this_build(nodes)
    }

    /// `worker` as `GET /v1/nodes` lists it, if it has registered.
    pub fn node_view(&self, worker: &WorkerId) -> Option<NodeView> {
        let state = self.lock();
        state
            .links
            .contains_key(worker)
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
        if !state.links.contains_key(worker) {
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
        Ok(self.node(&state, worker))
    }

    /// One node's view; `worker` is registered.
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
        NodeView {
            node_id: worker.as_str().to_owned(),
            connected: !state.links[worker].outbound.is_closed(),
            software: state.software.get(worker).cloned(),
            needs_attention: state
                .software
                .get(worker)
                .map(SoftwareView::needs_attention)
                .unwrap_or_default(),
            placement,
        }
    }

    /// Expires leases of silent workers and places queued work. Logs each drain that
    /// has paused at its deadline, once.
    pub fn tick(&self) {
        let now = self.now();
        let mut state = self.lock();
        state.feed_quiet(now, Event::Tick);
        let paused: BTreeSet<WorkerId> = state
            .links
            .keys()
            .filter(|w| matches!(state.sched.cordon(w), Some(Cordon::Paused { .. })))
            .cloned()
            .collect();
        for worker in paused.difference(&state.paused) {
            let leases = state.sched.leases_on(worker).len();
            tracing::warn!(%worker, leases, "drain paused at its deadline; leases run on");
        }
        state.paused = paused;
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
        let lease = LeaseId::new(wire_lease.term, wire_lease.seq);
        let refused = ResultAck {
            lease_id: Some(wire_lease),
            accepted: false,
        };
        let holding = self.lock().holder(lease, worker);
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
        let (outcome, mut detail) = self.outcome(lease, ran_on, result).await;

        let now = self.now();
        let (answers, accepted) = {
            let mut state = self.lock();
            let (answers, accepted) = state.take_report(now, worker, operation, lease, outcome);
            let settled: Vec<Settled> = answers
                .iter()
                .map(|a| state.settle(a, detail.take()))
                .collect();
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

    /// The scheduler's outcome for a `Result`, with the cache record of an OK one or
    /// the reason for an INVALID_ARGUMENT one. An OK result whose outputs are not all
    /// stored is an infrastructure failure: accepting it would answer callers with files
    /// nobody can fetch. An OK result gets the node it `ran_on` and the server's times
    /// ([`Stamp::apply`]) before it is checked and its cache record made.
    async fn outcome(
        &self,
        lease: LeaseId,
        ran_on: (&WorkerId, Stamp),
        result: worker::Result,
    ) -> (Outcome, Option<Detail>) {
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
                        (Outcome::Failed(Failure::Infra), None)
                    }
                }
            }
            (Code::DeadlineExceeded, _) => (Outcome::Failed(Failure::Timeout), None),
            (Code::InvalidArgument, _) => {
                let why = result.status.map(|s| s.message).unwrap_or_default();
                (
                    Outcome::Failed(Failure::Invalid),
                    Some(Detail::Invalid(why)),
                )
            }
            (code, _) => {
                tracing::info!(%lease, ?code, "lease failed");
                (Outcome::Failed(Failure::Infra), None)
            }
        }
    }

    /// Writes the action-cache entry, if any, then answers the callers.
    async fn deliver(&self, settled: Settled) {
        if let Some((action, record)) = settled.write
            && let Err(e) = self
                .cache
                .commit_action_record(Role::Daemon, action, record)
                .await
        {
            tracing::error!(%action, error = %e, "action-cache write of an accepted result failed");
        }
        for stage in settled.stages {
            stage.send_replace(Stage::Done(settled.finished.clone()));
        }
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> Dispatch for Farm<M, O> {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let now = self.now();
        let mut state = self.lock();
        let action = submission.request.key.action;
        let instance = submission.request.key.instance.clone();
        let waiter = state.machine.submit(Waiter {
            key: submission.request.key.clone(),
            kind: submission.request.kind,
            do_not_cache: submission.request.do_not_cache,
            queued: SystemTime::now(),
        });
        let name = state.machine.name(waiter);
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
    /// How many runs of `operation` the scheduler has recorded as killed for memory.
    fn memory_runs(&self, operation: OperationId) -> usize {
        self.sched.memory_runs(operation).map_or(0, <[_]>::len)
    }

    /// Feeds `worker`'s report that `lease` of `operation` ended with `outcome`, and
    /// returns the answers it led to and whether the scheduler took it. A busy node's
    /// memory kill the scheduler took is logged for operators (`crate::memory`).
    ///
    /// Only a report leads to an answer, and the in-process log commits its record at
    /// once: an accepted result is answered here, and a refused one (its lease given up
    /// while its outputs were checked) never is. A memory kill the scheduler takes is
    /// answered, or records a run and runs the operation again. The replicated log will
    /// answer once the record commits, carrying the result with it.
    ///
    /// Not generic, unlike `Farm::report`, so every test binary's runs count toward the
    /// same function's coverage.
    fn take_report(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        operation: OperationId,
        lease: LeaseId,
        outcome: Outcome,
    ) -> (Vec<Answer>, bool) {
        let killed_before = self.memory_runs(operation);
        let event = Event::Report {
            operation,
            lease,
            outcome,
        };
        let answers = self.feed(now, event);
        let rerun = self.memory_runs(operation) > killed_before;
        let accepted = rerun || answers.iter().any(|a| a.lease == lease);
        if accepted && outcome == Outcome::Failed(Failure::NodeMemoryPressure) {
            memory::pressure(worker, self.sched.memory_pressure(worker));
        }
        (answers, accepted)
    }

    fn is_current(&self, worker: &WorkerId, stream: StreamId) -> bool {
        self.links.get(worker).is_some_and(|l| l.stream == stream)
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

    /// Feeds an input that cannot lead to an answer: only a `Report` proposes a result,
    /// and only a committed result answers.
    fn feed_quiet(&mut self, now: FarmTime, event: Event) {
        let answers = self.feed(now, event);
        debug_assert!(
            answers.is_empty(),
            "an answer without a report: {answers:?}"
        );
    }

    /// Feeds `event`, then a tick (so placement follows at once), and carries out the
    /// effects in order. Returns the answers, for the caller to deliver once the lock
    /// is released.
    fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Answer> {
        let mut effects: VecDeque<Effect> = self.sched.apply(Input::new(now, event)).into();
        effects.extend(self.sched.apply(Input::new(now, Event::Tick)));
        self.log_requeues();
        let mut answers = Vec::new();
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(record) => {
                    if let ControlRecord::Lease(grant) = &record {
                        self.offer(grant);
                    }
                    // Single node: appended is committed. The replicated log goes here.
                    let committed = Event::Committed(record);
                    effects.extend(self.sched.apply(Input::new(now, committed)));
                }
                Effect::Start(start) => self.start(start),
                Effect::Answer(answer) => answers.push(answer),
                Effect::Waiting(waiting) => self.waiting(&waiting),
                Effect::Refuse(refusal) => self.refuse(&refusal),
            }
        }
        self.log_requeues();
        self.forget_dropped();
        answers
    }

    /// Logs the leases the scheduler has given up since the last call, one INFO line
    /// each, naming the operation by its first caller's name. Called before any
    /// effect is carried out (an effect can finish an operation, and with a zero
    /// finished retention drop it and forget its callers) and again at the end of a
    /// feed. A requeue only names an operation the scheduler held when it was made;
    /// one dropped since is logged with an empty name, never looked up by index.
    fn log_requeues(&mut self) {
        for Requeue {
            operation,
            lease,
            worker: node,
            reason,
        } in self.sched.take_requeues()
        {
            let operation = self.operation_name(operation);
            // One line: the coverage of a logged field counts only where it is logged.
            tracing::info!(%operation, %lease, %node, %reason, "lease given up; requeued");
        }
    }

    /// Keeps the callers of `operation`, which has just finished, until the scheduler
    /// drops it.
    fn keep_finished(&mut self, operation: OperationId, waiters: Vec<WaiterId>) {
        self.machine.keep_finished(operation, waiters);
        self.forget_dropped();
    }

    /// Forgets the callers, and their channels, of every finished operation the
    /// scheduler has dropped ([`FarmMachine::forget_dropped`]): a WaitExecution on one
    /// is NOT_FOUND from now on.
    fn forget_dropped(&mut self) {
        let sched = &self.sched;
        let forgotten = self
            .machine
            .forget_dropped(|operation| sched.state(operation).is_some());
        for id in forgotten {
            self.stages.remove(&id);
        }
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
        let _ = self.links.get(worker).zip(message).map(|(link, message)| {
            link.outbound.send(Ok(ServerMessage {
                message: Some(message),
            }))
        });
    }

    /// Tells the worker a lease is placed on it, before the grant commits.
    fn offer(&self, grant: &LeaseGrant) {
        let (operation, lease, node) = (
            self.operation_name(grant.operation),
            grant.lease,
            &grant.worker,
        );
        tracing::debug!(%operation, %lease, %node, "lease granted");
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
        let finished = failed(Code::FailedPrecondition, &refusal.reason);
        // A lease given up before the operation was refused may still be listed.
        self.machine.forget_leases(refusal.operation);
        for stage in refusal.waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Done(finished.clone()));
        }
        self.keep_finished(refusal.operation, refusal.waiters.clone());
    }

    /// Works out what the callers of a finished operation get, and keeps them for the
    /// finished retention. `detail` is what the accepted report carried beyond its
    /// outcome, if anything.
    fn settle(&mut self, answer: &Answer, detail: Option<Detail>) -> Settled {
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
        let (finished, write) = match (detail, answer.outcome) {
            (Some(Detail::Ran(pending)), _) => {
                let Pending { result, record } = *pending;
                let cacheable =
                    result.exit_code == 0 && waiters.first().is_some_and(|w| !w.do_not_cache);
                let write = waiters
                    .first()
                    .filter(|_| cacheable)
                    .map(|w| (w.key.action, record));
                (Finished::Ran(Box::new(result)), write)
            }
            (_, Outcome::Failed(Failure::Timeout)) => {
                (failed(Code::DeadlineExceeded, "the action timed out"), None)
            }
            (Some(Detail::Invalid(why)), _) => (failed(Code::InvalidArgument, &why), None),
            (_, Outcome::Failed(kill @ (Failure::OutOfMemory | Failure::NodeMemoryPressure))) => {
                (memory::finished(kill, &answer.memory_runs), None)
            }
            _ => (
                failed(Code::Internal, "the farm could not run the action"),
                None,
            ),
        };
        self.keep_finished(answer.operation, answer.waiters.clone());
        Settled {
            finished,
            write,
            stages,
        }
    }
}

/// Now on the wall clock, in milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

fn failed(code: Code, message: &str) -> Finished {
    Finished::Failed(rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

/// [`START_VALIDITY`] in milliseconds, as `Start.valid_for_ms` carries it.
const START_VALIDITY_MS: u64 = START_VALIDITY.as_millis() as u64;

fn wire_lease(lease: LeaseId) -> worker::LeaseId {
    worker::LeaseId {
        term: lease.term,
        seq: lease.seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a server process that reuses an earlier process's term (issue #137:
    /// every process granted leases from `(1, 0)`), and one whose term does not order
    /// after that of a process started a few milliseconds before. A term is also never
    /// 1, the term every server before the fix used.
    #[test]
    fn each_process_term_is_new_and_later() {
        let first = process_term();
        std::thread::sleep(Duration::from_millis(3));
        let second = process_term();
        assert!(second > first, "{second} does not order after {first}");
        assert!(first >> 16 > 0, "{first} could be a pre-fix term");
    }
}
