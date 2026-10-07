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

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use kbf_front::{Cache, Dispatch, Finished, MetaLog, Stage, Submission, Ticket};
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::ObjectStore;
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{self, LeaseOffer, ResultAck, ServerMessage, Start, server_message};
use kbf_sched::{Event, Input, OpState, Scheduler};
use kbf_types::{
    Answer, ControlRecord, Digest, Effect, Failure, FarmTime, LeaseGrant, LeaseId, OperationId,
    Outcome, Resources, StartLease, StateMachine, WaiterId, WorkerId,
};
use tokio::sync::{mpsc, watch};
use tonic::{Code, Status};

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
    state: Mutex<State>,
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
}

/// An OK result and its action-cache record, from the report to the answer.
#[derive(Debug)]
struct Pending {
    result: ActionResult,
    record: ActionRecord,
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
}

/// What a finished operation tells its callers, and the action-cache entry to write
/// first, if any.
struct Settled {
    finished: Finished,
    write: Option<(Digest, ActionRecord)>,
    stages: Vec<watch::Sender<Stage>>,
}

impl<M: MetaLog, O: ObjectStore> Farm<M, O> {
    /// A farm over `cache`, with an empty scheduler.
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>) -> Self {
        Self {
            cache,
            epoch: Instant::now(),
            state: Mutex::new(State {
                sched: Scheduler::new(SINGLE_NODE_TERM),
                next_waiter: 0,
                next_stream: 0,
                waiters: BTreeMap::new(),
                names: BTreeMap::new(),
                links: BTreeMap::new(),
                started: BTreeMap::new(),
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
    /// session). Messages still arriving on its earlier stream are ignored from now on.
    pub fn register(
        &self,
        worker: &WorkerId,
        capacity: Resources,
        outbound: Outbound,
        welcome: ServerMessage,
    ) -> StreamId {
        let now = self.now();
        let mut state = self.lock();
        let stream = StreamId(state.next_stream);
        state.next_stream += 1;
        // The receiver is the stream's own response, alive until the stream ends.
        let _ = outbound.send(Ok(welcome));
        state
            .links
            .insert(worker.clone(), Link { stream, outbound });
        let event = Event::WorkerUp {
            worker: worker.clone(),
            capacity,
        };
        state.feed_quiet(now, event);
        stream
    }

    /// A `Hello` resent on `stream`: the node report changed. Changes the worker's
    /// capacity and nothing else; ignored if `stream` was replaced.
    pub fn resize(&self, worker: &WorkerId, stream: StreamId, capacity: Resources) {
        let now = self.now();
        let mut state = self.lock();
        if state.is_current(worker, stream) {
            let event = Event::Capacity {
                worker: worker.clone(),
                capacity,
            };
            state.feed_quiet(now, event);
        }
    }

    /// A heartbeat on `stream`. Returns whether it was taken (and is to be
    /// acknowledged): a heartbeat from a replaced stream is dropped.
    pub fn heartbeat(&self, worker: &WorkerId, stream: StreamId, running: Vec<LeaseId>) -> bool {
        let now = self.now();
        let mut state = self.lock();
        if !state.is_current(worker, stream) {
            return false;
        }
        let event = Event::Heartbeat {
            worker: worker.clone(),
            running,
        };
        state.feed_quiet(now, event);
        true
    }

    /// Expires leases of silent workers and places queued work.
    pub fn tick(&self) {
        let now = self.now();
        self.lock().feed_quiet(now, Event::Tick);
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
        let (outcome, mut pending) = self.outcome(lease, result).await;

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
                .map(|a| state.settle(a, pending.take()))
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

    /// The scheduler's outcome for a `Result`, and the cache record of an OK one. An OK
    /// result whose outputs are not all stored is an infrastructure failure: accepting
    /// it would answer callers with files nobody can fetch.
    async fn outcome(&self, lease: LeaseId, result: worker::Result) -> (Outcome, Option<Pending>) {
        let code = result.status.as_ref().map_or(Code::Ok as i32, |s| s.code);
        match (Code::from_i32(code), result.action_result) {
            (Code::Ok, Some(result)) => match self.cache.prepare_action_result(&result).await {
                Ok(record) => {
                    let outcome = Outcome::Completed {
                        action_result: record.result,
                    };
                    (outcome, Some(Pending { result, record }))
                }
                Err(e) => {
                    tracing::warn!(%lease, error = %e, "an OK result whose outputs are not stored");
                    (Outcome::Failed(Failure::Infra), None)
                }
            },
            (Code::DeadlineExceeded, _) => (Outcome::Failed(Failure::Timeout), None),
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

    /// Sends the `Start` of a committed lease and marks its callers executing.
    fn start(&mut self, start: StartLease) {
        self.send_for(&start.worker, start.operation, |w| {
            server_message::Message::Start(Start {
                lease_id: Some(wire_lease(start.lease)),
                kind: w.kind.clone(),
                action_digest: Some(kbf_front::digest_to_proto(&start.key.action)),
                millicpus: start.resources.cpu_millis,
                memory_bytes: start.resources.memory_bytes,
            })
        });
        let waiters = self.sched.waiters(start.operation).unwrap_or_default();
        for w in waiters.iter().filter_map(|id| self.waiters.get(id)) {
            w.stage.send_replace(Stage::Executing);
        }
        self.started.insert(start.lease, start.operation);
    }

    /// Forgets a finished operation and works out what its callers get. `pending` is
    /// the OK result the answer accepted, if it accepted one.
    fn settle(&mut self, answer: &Answer, pending: Option<Pending>) -> Settled {
        self.started.retain(|_, op| *op != answer.operation);
        let waiters: Vec<Waiter> = answer
            .waiters
            .iter()
            .filter_map(|id| self.waiters.remove(id))
            .collect();
        for w in &waiters {
            self.names.remove(&w.name);
        }
        let (finished, write) = match (pending, answer.outcome) {
            (Some(Pending { result, record }), _) => {
                let cacheable =
                    result.exit_code == 0 && waiters.first().is_some_and(|w| !w.do_not_cache);
                let write = waiters
                    .first()
                    .filter(|_| cacheable)
                    .map(|w| (w.key.action, record));
                (Finished::Ran(Box::new(result)), write)
            }
            (None, Outcome::Failed(Failure::Timeout)) => {
                (failed(Code::DeadlineExceeded, "the action timed out"), None)
            }
            (None, _) => (
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

fn failed(code: Code, message: &str) -> Finished {
    Finished::Failed(rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

fn wire_lease(lease: LeaseId) -> worker::LeaseId {
    worker::LeaseId {
        term: lease.term,
        seq: lease.seq,
    }
}
