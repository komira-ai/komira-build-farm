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
//! [`State::feed`]). The replicated log replaces exactly that step.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
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
use kbf_sched::{Cordon, Event, Input, OpState, Scheduler};
use kbf_types::{
    Answer, ControlRecord, Digest, Effect, Failure, FarmTime, LeaseGrant, LeaseId, OperationId,
    Outcome, Refusal, Resources, StartLease, StateMachine, WaiterId, Waiting, WorkerId,
};
use tokio::sync::{mpsc, watch};
use tonic::{Code, Status};

use crate::fleet::{NodeView, NodesView, PlacementView, SoftwareView};

/// The scheduler term of a single node. Raft supplies terms once it is wired.
pub const SINGLE_NODE_TERM: u64 = 1;

/// Where the server sends a worker's messages: the outbound half of its stream.
pub type Outbound = mpsc::UnboundedSender<Result<ServerMessage, Status>>;

/// Names one worker stream, so messages from a replaced stream can be told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StreamId(u64);

/// The farm core. Cheap to share: every method takes `&self`.
#[derive(Debug)]
pub struct Farm<M, O> {
    cache: Arc<Cache<M, O>>,
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

/// A caller waiting on an operation.
#[derive(Debug)]
struct Waiter {
    name: String,
    key: kbf_types::ActionKey,
    kind: String,
    do_not_cache: bool,
    stage: watch::Sender<Stage>,
}

/// A worker's newest stream.
#[derive(Debug)]
struct Link {
    stream: StreamId,
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
    next_waiter: u64,
    next_stream: u64,
    waiters: BTreeMap<WaiterId, Waiter>,
    names: BTreeMap<String, WaiterId>,
    links: BTreeMap<WorkerId, Link>,
    /// Leases whose `Start` was sent, and their operations.
    started: BTreeMap<LeaseId, OperationId>,
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
    /// A farm over `cache`, with an empty scheduler that refuses queued work once no
    /// live worker has been able to run it for `unservable_wait`.
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>, unservable_wait: Duration) -> Self {
        Self {
            cache,
            epoch: Instant::now(),
            epoch_unix_ms: unix_ms(),
            state: Mutex::new(State {
                sched: Scheduler::new(SINGLE_NODE_TERM).with_unservable_wait(unservable_wait),
                next_waiter: 0,
                next_stream: 0,
                waiters: BTreeMap::new(),
                names: BTreeMap::new(),
                links: BTreeMap::new(),
                started: BTreeMap::new(),
                software: BTreeMap::new(),
                paused: BTreeSet::new(),
            }),
        }
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

    /// The first `Hello` of a stream: queues `welcome` on `outbound`, makes this the
    /// worker's stream for every `Start` from now on, and registers the worker (a new
    /// session) with its `capacity` and `caps`. Messages still arriving on its earlier
    /// stream are ignored from now on.
    pub fn register(
        &self,
        worker: &WorkerId,
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
            outbound,
            newest_beat: 0,
        };
        state.links.insert(worker.clone(), link);
        let event = Event::WorkerUp {
            worker: worker.clone(),
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
    /// replaced (a newer stream sends its own after its `Welcome`).
    pub fn node_status(&self, worker: &WorkerId, stream: StreamId, status: NodeStatus) {
        let received = unix_ms();
        let mut state = self.lock();
        if state.is_current(worker, stream) {
            let view = SoftwareView::new(status, received);
            state.software.insert(worker.clone(), view);
        }
    }

    /// Every node registered since this farm started, in node-id order.
    pub fn nodes(&self) -> NodesView {
        let state = self.lock();
        let nodes = state
            .links
            .keys()
            .map(|worker| self.node(&state, worker))
            .collect();
        NodesView { nodes }
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
    /// current lease; an accepted OK result is written to the action cache (unless the
    /// action is `do_not_cache` or exited non-zero) before its callers are answered.
    /// Returns the acknowledgement, or `None` for a `Result` without a lease id.
    pub async fn report(&self, worker: &WorkerId, result: worker::Result) -> Option<ResultAck> {
        let wire_lease = result.lease_id?;
        let lease = LeaseId::new(wire_lease.term, wire_lease.seq);
        let refused = ResultAck {
            lease_id: Some(wire_lease),
            accepted: false,
        };
        let Some(operation) = self.lock().holder(lease, worker) else {
            tracing::warn!(%worker, %lease, "result refused: not the holder of the current lease");
            return Some(refused);
        };
        let (outcome, mut detail) = self.outcome(lease, result).await;

        let now = self.now();
        let (answers, accepted) = {
            let mut state = self.lock();
            let answers = state.feed(
                now,
                Event::Report {
                    operation,
                    lease,
                    outcome,
                },
            );
            // Only a report leads to an answer, and the in-process log commits its record
            // at once: an accepted result is answered here, and a refused one (its lease
            // given up while its outputs were checked) never is. The replicated log will
            // answer once the record commits, carrying the result with it.
            let accepted = answers.iter().any(|a| a.lease == lease);
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
    /// nobody can fetch.
    async fn outcome(&self, lease: LeaseId, result: worker::Result) -> (Outcome, Option<Detail>) {
        let code = result.status.as_ref().map_or(Code::Ok as i32, |s| s.code);
        match (Code::from_i32(code), result.action_result) {
            (Code::Ok, Some(result)) => match self.cache.prepare_action_result(&result).await {
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
            },
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
        let waiter = WaiterId(state.next_waiter);
        state.next_waiter += 1;
        let name = format!("operations/{}", waiter.0);
        let (stage, receiver) = watch::channel(Stage::Queued);
        let action = submission.request.key.action;
        state.names.insert(name.clone(), waiter);
        state.waiters.insert(
            waiter,
            Waiter {
                name: name.clone(),
                key: submission.request.key.clone(),
                kind: submission.kind,
                do_not_cache: submission.request.do_not_cache,
                stage,
            },
        );
        state.feed_quiet(
            now,
            Event::Submit {
                waiter,
                request: submission.request,
            },
        );
        // A caller that joined a twin already started sees it executing.
        let joined_started = state.started.values().any(|op| {
            state
                .sched
                .waiters(*op)
                .is_some_and(|w| w.contains(&waiter))
        });
        if let Some(w) = state.waiters.get(&waiter).filter(|_| joined_started) {
            w.stage.send_replace(Stage::Executing);
        }
        Ok(Ticket {
            name,
            action,
            stage: receiver,
        })
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        let state = self.lock();
        let waiter = state.waiters.get(state.names.get(name)?)?;
        Some(Ticket {
            name: waiter.name.clone(),
            action: waiter.key.action,
            stage: waiter.stage.subscribe(),
        })
    }
}

impl State {
    fn is_current(&self, worker: &WorkerId, stream: StreamId) -> bool {
        self.links.get(worker).is_some_and(|l| l.stream == stream)
    }

    /// The operation whose current lease is `lease`, if `worker` holds it.
    fn holder(&self, lease: LeaseId, worker: &WorkerId) -> Option<OperationId> {
        let operation = *self.started.get(&lease)?;
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
        current.then_some(operation)
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
        answers
    }

    /// The first waiter of `operation`: its key and lease kind are the operation's.
    fn first_waiter(&self, operation: OperationId) -> Option<&Waiter> {
        let first = self.sched.waiters(operation)?.first()?;
        self.waiters.get(first)
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
        let message = self.first_waiter(operation).map(build);
        let _ = self.links.get(worker).zip(message).map(|(link, message)| {
            link.outbound.send(Ok(ServerMessage {
                message: Some(message),
            }))
        });
    }

    /// Tells the worker a lease is placed on it, before the grant commits.
    fn offer(&self, grant: &LeaseGrant) {
        self.send_for(&grant.worker, grant.operation, |w| {
            server_message::Message::LeaseOffer(LeaseOffer {
                lease_id: Some(wire_lease(grant.lease)),
                kind: w.kind.clone(),
                action_digest: Some(kbf_front::digest_to_proto(&w.key.action)),
            })
        });
    }

    /// Sends the `Start` of a committed lease and marks its callers executing. The
    /// `Start` names the newest heartbeat taken on the worker's stream and the window
    /// after it in which the daemon may still act on the `Start` (issue #23).
    fn start(&mut self, start: StartLease) {
        let heartbeat_seq = self.links.get(&start.worker).map_or(0, |l| l.newest_beat);
        self.send_for(&start.worker, start.operation, |w| {
            server_message::Message::Start(Start {
                lease_id: Some(wire_lease(start.lease)),
                kind: w.kind.clone(),
                action_digest: Some(kbf_front::digest_to_proto(&start.key.action)),
                millicpus: start.resources.cpu_millis,
                memory_bytes: start.resources.memory_bytes,
                heartbeat_seq,
                valid_for_ms: START_VALIDITY_MS,
            })
        });
        let waiters = self.sched.waiters(start.operation).unwrap_or_default();
        for w in waiters.iter().filter_map(|id| self.waiters.get(id)) {
            w.stage.send_replace(Stage::Executing);
        }
        self.started.insert(start.lease, start.operation);
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
        for w in waiters.iter().filter_map(|id| self.waiters.get(id)) {
            w.stage.send_if_modified(|now| {
                let changed = *now != stage;
                if changed {
                    now.clone_from(&stage);
                }
                changed
            });
        }
    }

    /// Forgets every lease of a finished `operation` whose `Start` was sent.
    fn forget_leases(&mut self, operation: OperationId) {
        self.started.retain(|_, op| *op != operation);
    }

    /// Forgets an operation the scheduler refused and answers its callers
    /// FAILED_PRECONDITION with the reason. Nothing is cached, so nothing is awaited.
    fn refuse(&mut self, refusal: &Refusal) {
        tracing::warn!(operation = %refusal.operation, reason = %refusal.reason, "operation refused");
        let finished = failed(Code::FailedPrecondition, &refusal.reason);
        // A lease given up before the operation was refused may still be listed.
        self.forget_leases(refusal.operation);
        for w in refusal
            .waiters
            .iter()
            .filter_map(|id| self.waiters.remove(id))
        {
            self.names.remove(&w.name);
            w.stage.send_replace(Stage::Done(finished.clone()));
        }
    }

    /// Forgets a finished operation and works out what its callers get. `detail` is
    /// what the accepted report carried beyond its outcome, if anything.
    fn settle(&mut self, answer: &Answer, detail: Option<Detail>) -> Settled {
        self.forget_leases(answer.operation);
        let waiters: Vec<Waiter> = answer
            .waiters
            .iter()
            .filter_map(|id| self.waiters.remove(id))
            .collect();
        for w in &waiters {
            self.names.remove(&w.name);
        }
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
            _ => (
                failed(Code::Internal, "the farm could not run the action"),
                None,
            ),
        };
        Settled {
            finished,
            write,
            stages: waiters.into_iter().map(|w| w.stage).collect(),
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
