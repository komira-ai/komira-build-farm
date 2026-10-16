//! The simulated server: `kbf-server`'s Raft metadata log on a fault-injecting disk.
//!
//! - **Metadata** is a [`RaftMetaLog`] over the server's [`FaultFs`], opened as the
//!   server opens it (a new log's id drawn from the seed). Its futures never wait: each
//!   is polled once, inside a Tokio runtime context for the tick task the open spawns,
//!   which never runs. Farm time is the simulation's clock: the server commits
//!   `Tick(now)` every [`TICK`] instead.
//! - **The farm core** is a [`Scheduler`] of the log's term and a [`FarmMachine`],
//!   fed as `crates/kbf-server/src/farm.rs` feeds them: each input and then a tick, a
//!   committed record fed straight back, a `Start` recorded in the machine, a result
//!   taken only from the holder of the operation's current lease and written to the
//!   action cache before its callers are answered (and answered even if that write
//!   fails), `Welcome` naming the log id as the lease epoch. Both live in memory, so a
//!   restart loses them, as the server does today.
//! - **Writes** go to the object store first; the blob is committed to the log once
//!   the store has it, and only then is the client told. A PUT the store answers as
//!   failed, or not answered within [`PUT_TIMEOUT`], is answered `UNAVAILABLE`
//!   (`UploadFailed`) and indexes nothing, even if the store answers it later; the
//!   client's retry is a new PUT under a new object id.
//! - **Faults** ([`ServerFault`]): a process kill, a power cut (now, or part-way
//!   through a later filesystem operation), a crash at a named point of the write path
//!   ([`StorePoint`], armed in a [`CrashPlan`]), and an I/O error, after which the log
//!   refuses every commit while the server goes on answering reads from its applied
//!   state. A server that has stopped or died starts again after
//!   [`ServerPlan::down_for`] over what the disk kept; a server whose log stopped
//!   starts again after its machine reboots.
//! - **The local write tier** ([`TierPlan`], `server/tier.rs`), when the plan has one:
//!   `kbf-tier`'s [`kbf_tier::Tier`] on a second fault-injecting disk, attached at each
//!   start as `kbf-front`'s `Cache::attach_tier` attaches it (the log raised to
//!   machine version 2 first). Uploads are staged and acknowledged once synced and
//!   indexed; a flusher seals, PUTs each sealed block whole and commits
//!   `ObjectFlushed` every [`TICK`]; reads of present blobs are served from the tier or
//!   the store. A crash or power cut takes both disks down together.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kbf_daemon::FENCE_AFTER;
use kbf_front::{Described, MetaLog};
use kbf_log::FarmLogId;
use kbf_meta::{
    ActionAnswer, ActionRecord, Applied, Closure, Command, Epoch, MetaState, ObjectId, Retention,
    Role,
};
use kbf_sched::{DaemonInstance, Event as SchedEvent, Input, OpState, Request, Scheduler};
use kbf_server::RaftMetaLog;
use kbf_server::action_usage::Subject;
use kbf_server::machine::{FarmMachine, Sent, Waiter};
use kbf_server::stamp::Stamp;
use kbf_sim::{Event, NodeId, NodeInput, Output};
use kbf_store::{Fault, FaultFs};
use kbf_types::{
    ActionKey, Answer, Digest, Effect, FarmTime, LeaseId, LeaseKind, Outcome, Qos, Resources,
    StartLease, StateMachine, WaiterId, WorkerId,
};

mod tier;
mod write;

pub use self::tier::{Rollback, TierCut, TierPlan, TierSeen};
use crate::Msg;
use crate::hook::{CrashPlan, Down, StorePoint, TierPoint};
use crate::replica::{self, Broken, Replica};

/// How often the server commits farm time and runs a placement round.
pub const TICK: Duration = Duration::from_secs(1);
/// The heartbeat interval `Welcome` names.
pub const HEARTBEAT: Duration = Duration::from_secs(5);
const T_TICK: u64 = 0;
const T_START: u64 = 1;
const T_FAULT: u64 = 1 << 16;
/// A PUT's deadline: this plus the server's count of PUTs sent.
const T_PUT: u64 = 1 << 32;
/// A read of a present blob: this plus the server's count of reads.
const T_READ: u64 = 1 << 48;
/// How long the server waits for the store to answer a PUT (the object store client's
/// request timeout) before it answers the upload `UNAVAILABLE`.
pub const PUT_TIMEOUT: Duration = Duration::from_secs(30);

/// A fault the server meets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerFault {
    /// The process is killed: everything it wrote stays readable.
    Kill,
    /// The machine loses power: only what was synced survives, with up to `keep`
    /// bytes of each file's unsynced tail and the name changes `names` selects
    /// ([`FaultFs::crash_reordered`]).
    PowerCut { keep: usize, names: u64 },
    /// As [`ServerFault::PowerCut`], at the `after`-th filesystem operation from now:
    /// part-way through a commit.
    PowerCutAt { after: u64, keep: usize, names: u64 },
    /// The `after`-th filesystem operation from now fails: the log stops.
    IoErrorAt { after: u64 },
    /// From now, the `nth` time the write path reaches `point`, the server goes `down`.
    CrashAt {
        point: StorePoint,
        nth: u32,
        down: Down,
    },
    /// From now, the `nth` time the write tier reaches `point`, the server goes `down`.
    TierCrashAt {
        point: TierPoint,
        nth: u32,
        down: Down,
    },
    /// The `after`-th operation on the tier's disk from now meets `fault`: a full disk
    /// ([`Fault::NoSpace`], below the tier's budget: other tenants filled it) or an I/O
    /// error ([`Fault::Error`]).
    TierDiskAt { after: u64, fault: Fault },
    /// A bad sector: one byte of the oldest staged block, in a committed record, is
    /// flipped on the disk.
    TierFlip,
}

/// When the server meets its faults, and how long it stays down after each.
#[derive(Clone, Debug, Default)]
pub struct ServerPlan {
    pub faults: Vec<(FarmTime, ServerFault)>,
    pub down_for: Duration,
    /// The local write tier, if the server runs one.
    pub tier: Option<TierPlan>,
}

/// What the server did that the checks read.
#[derive(Debug, Default)]
pub struct Seen {
    /// Each blob answered present, and when.
    pub present: Vec<(FarmTime, Digest)>,
    /// Each lease given up, and whether its daemon was heard within T listing it.
    pub requeues: Vec<(FarmTime, LeaseId, WorkerId, bool)>,
    /// How many times each lease's result was accepted, for each lease a holder
    /// reported (0: refused).
    pub accepted: BTreeMap<LeaseId, u32>,
    /// The actions with an accepted completed result.
    pub won: BTreeSet<Digest>,
    /// How many times the server opened its log, and each refusal.
    pub starts: u64,
    pub refused: Vec<(FarmTime, String)>,
    /// Every digest a client asked to store.
    pub uploaded: BTreeSet<Digest>,
    /// Each upload acknowledged: when, the blob, and the object its PUT wrote.
    pub acked: Vec<(FarmTime, Digest, ObjectId)>,
    /// Each crash point that took the server down, and when: a point is recorded
    /// where it fires (an in-commit crash once its disk fault has failed the `PutBlob`
    /// commit, not when it is planted).
    pub crashed_at: Vec<(FarmTime, StorePoint)>,
    /// Each PUT whose deadline answered its upload `UNAVAILABLE`.
    pub timed_out: BTreeSet<ObjectId>,
    /// Each PUT the store answered failed while its upload waited.
    pub put_failed: BTreeSet<ObjectId>,
    /// Each `PutBlob` commit a crash cut part-way through, and what the next start
    /// found of it.
    pub cuts: Vec<Cut>,
    /// What the local write tier did.
    pub tier: TierSeen,
    /// Each action started, and the runs of the server's process ([`Seen::starts`])
    /// that started it.
    pub started: BTreeMap<Digest, BTreeSet<u64>>,
}

/// A `PutBlob` commit a crash cut part-way through ([`StorePoint::InCommit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cut {
    pub object: ObjectId,
    pub digest: Digest,
    /// How the server went down.
    pub down: Down,
    /// The most unsynced bytes any file held when it did ([`FaultFs::unsynced`]): a
    /// power cut keeping fewer, and more than none, tears the commit's record.
    pub unsynced: usize,
    /// Whether the next start's index has the blob in `object`: `None` until it
    /// starts.
    pub survived: Option<bool>,
}

/// The server node.
pub struct Server {
    plan: ServerPlan,
    disk: FaultFs,
    /// The local write tier's directory, on a disk of its own.
    tier_disk: FaultFs,
    /// The instant farm time 0 stands for: the tier's clock is farm time.
    base: Instant,
    runtime: tokio::runtime::Runtime,
    up: Option<Up>,
    /// A planned crash, armed in the disk: how the server goes down there.
    armed: Option<Down>,
    /// The write path's crash points.
    hooks: CrashPlan<StorePoint>,
    /// The write tier's crash points.
    tier_hooks: CrashPlan<TierPoint>,
    /// A crash point reached in this input: the out queue's length then, and how the
    /// server goes down.
    dying: Option<(usize, Down)>,
    /// PUTs sent, over every start: each PUT's deadline timer is tagged with it.
    puts_sent: u64,
    /// Reads started, over every start: each read's timer is tagged with it.
    reads_sent: u64,
    restarting: bool,
    store: NodeId,
    /// What the last property-1 check saw: (incarnation, applied index, disk ops).
    checked: Cell<Option<(u64, u64, u64)>>,
    /// The last property-1 check's replica, and the incarnation it was read in.
    replica: Cell<Option<(u64, Replica)>>,
    pub seen: Seen,
    out: Vec<Output<Msg>>,
}

struct Up {
    log: RaftMetaLog<FaultFs>,
    stopped: bool,
    /// The out queue's length when a commit failed, if one has.
    failed_at: Option<usize>,
    writer: Epoch,
    /// The next object's sequence number in `writer`, shared with the tier's blocks.
    next_object: Arc<AtomicU64>,
    lease_epoch: u64,
    sched: Scheduler,
    machine: FarmMachine,
    streams: BTreeMap<WorkerId, u64>,
    heard: BTreeMap<WorkerId, (FarmTime, Vec<LeaseId>)>,
    callers: BTreeMap<WaiterId, NodeId>,
    uploads: BTreeMap<ObjectId, (NodeId, Digest)>,
    /// Each PUT waiting for the store, by its deadline timer's tag.
    deadlines: BTreeMap<u64, ObjectId>,
    /// The local write tier, once attached.
    tier: Option<tier::CellTier>,
}

/// Polls `future` once: the metadata log answers without waiting.
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(v) => v,
        Poll::Pending => unreachable!("the metadata log answers without waiting"),
    }
}

/// A new log's id draw that draws `id`.
fn drawn(id: FarmLogId) -> impl FnOnce() -> std::io::Result<FarmLogId> {
    move || Ok(id)
}

fn wall(t: FarmTime) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(t.as_millis())
}

/// The request every action makes: one core and 1 GiB, hermetic.
fn request(action: Digest) -> Request {
    Request {
        key: key(action),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: kbf_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

fn key(action: Digest) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action,
    }
}

impl Server {
    /// A server with an empty disk that meets `plan`'s faults and writes its objects
    /// to `store`.
    ///
    /// # Panics
    ///
    /// If a Tokio runtime cannot be built.
    #[must_use]
    pub fn new(plan: ServerPlan, store: NodeId) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        Self {
            plan,
            disk: FaultFs::new(),
            tier_disk: FaultFs::new(),
            base: Instant::now(),
            runtime,
            up: None,
            armed: None,
            hooks: CrashPlan::default(),
            tier_hooks: CrashPlan::default(),
            dying: None,
            puts_sent: 0,
            reads_sent: 0,
            restarting: false,
            store,
            checked: Cell::new(None),
            replica: Cell::new(None),
            seen: Seen::default(),
            out: Vec::new(),
        }
    }

    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    /// Whether the server's process is running (its log open, or stopped and
    /// answering reads).
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.up.is_some()
    }

    /// The applied metadata state, while the server is up.
    #[must_use]
    pub fn meta(&self) -> Option<MetaState> {
        let up = self.up.as_ref()?;
        ready(up.log.query(MetaState::clone)).ok()
    }

    /// Properties 1 and 6 against the disk: a replica that replays the durable log
    /// (what a power cut now would leave) through the server's applied index has the
    /// server's applied state and `Bootstrap`, and its farm time never goes back from
    /// one entry to the next. Skipped while the server is down, and when neither its
    /// applied index nor its disk has changed since the last check.
    ///
    /// # Errors
    ///
    /// The property that does not hold, and how.
    pub fn check_durable(&self) -> Result<(), Broken> {
        let Some(up) = &self.up else { return Ok(()) };
        let applied = up.log.applied_index();
        let key = (self.seen.starts, applied, self.disk.ops());
        if self.checked.replace(Some(key)) == Some(key) {
            return Ok(());
        }
        let entries = replica::durable_entries(self.disk.crash(0))?;
        let starts = self.seen.starts;
        let mut replica = self
            .replica
            .take()
            .filter(|(s, r)| *s == starts && r.extends(&entries))
            .map(|(_, r)| r)
            .unwrap_or_default();
        replica.advance(&entries, applied)?;
        let live = ready(up.log.query(MetaState::clone)).expect("a query is never refused");
        let held = replica::same_as_live(&replica.state, &live, up.log.log_id());
        self.replica.replace(Some((starts, replica)));
        held
    }

    fn timer(&mut self, after: Duration, tag: u64) {
        self.out.push(Output::Timer { after, tag });
    }

    /// Opens the log over what the disk holds and starts an empty farm core of its
    /// term. A refused open is recorded and tried again after `down_for`.
    fn start(&mut self, now: FarmTime, entropy: u64) {
        let log_id = FarmLogId::new(NonZeroU64::new(entropy).unwrap_or(NonZeroU64::MIN));
        let opened = {
            let _context = self.runtime.enter();
            ready(RaftMetaLog::open(
                self.disk.clone(),
                Retention::default(),
                "cell/",
                drawn(log_id),
            ))
        };
        self.seen.starts += 1;
        let log = match opened {
            Ok(log) => log,
            Err(e) => {
                self.seen.refused.push((now, e.to_string()));
                self.restart_later();
                return;
            }
        };
        let leadership = log.leadership().expect("a raft log names its term");
        let Ok(Applied::Epoch(writer)) = ready(log.commit(Command::AllocEpoch)) else {
            self.down(Down::Kill);
            return;
        };
        let up = Up {
            lease_epoch: leadership.log_id.get(),
            sched: Scheduler::new(leadership.term).recording_requeues(),
            machine: FarmMachine::new(leadership.term),
            log,
            stopped: false,
            failed_at: None,
            writer,
            next_object: Arc::new(AtomicU64::new(0)),
            streams: BTreeMap::new(),
            heard: BTreeMap::new(),
            callers: BTreeMap::new(),
            uploads: BTreeMap::new(),
            deadlines: BTreeMap::new(),
            tier: None,
        };
        write::read_back_cuts(&up, &mut self.seen.cuts);
        self.up = Some(up);
        match self.attach_tier(now) {
            Ok(()) => {}
            Err(None) => self.down(Down::Kill),
            Err(Some(refused)) => {
                self.up = None;
                self.seen.refused.push((now, refused));
                self.restart_later();
            }
        }
    }

    fn restart_later(&mut self) {
        if !std::mem::replace(&mut self.restarting, true) {
            self.timer(self.plan.down_for, T_START);
        }
    }

    /// The process dies: killed, or in a power cut.
    fn down(&mut self, how: Down) {
        self.up = None;
        self.armed = None;
        (self.disk, self.tier_disk) = match how {
            Down::PowerCut { keep, names } => (
                self.disk.crash_reordered(keep, names),
                self.tier_disk.crash_reordered(keep, names),
            ),
            Down::Kill => (self.disk.restart(), self.tier_disk.restart()),
        };
        self.restart_later();
    }

    fn fault(&mut self, fault: ServerFault) {
        match fault {
            ServerFault::Kill if self.up.is_some() => self.down(Down::Kill),
            ServerFault::PowerCut { keep, names } if self.up.is_some() => {
                self.down(Down::PowerCut { keep, names });
            }
            ServerFault::PowerCutAt { after, keep, names } if self.up.is_some() => {
                self.disk.fail_at(self.disk.ops() + after, Fault::Crash);
                self.armed = Some(Down::PowerCut { keep, names });
            }
            ServerFault::CrashAt { point, nth, down } => self.hooks.arm(point, nth, down),
            ServerFault::IoErrorAt { after } if self.up.is_some() => {
                self.disk.fail_at(self.disk.ops() + after, Fault::Error);
            }
            ServerFault::TierCrashAt { point, nth, down } => {
                self.tier_hooks.arm(point, nth, down);
            }
            // Both act on what is there: a full disk counts operations from now, up or
            // down; a bad sector needs a staged block.
            ServerFault::TierDiskAt { after, fault } => {
                let at = self.tier_disk.ops() + after;
                self.tier_disk.fail_at(at, fault);
            }
            ServerFault::TierFlip => self.flip_a_staged_record(),
            _ => {}
        }
    }

    /// After an input: a crash point reached kills the process there, and a commit
    /// that failed at a planned crash kills it at the failure (nothing it queued after
    /// either leaves it); a commit that failed otherwise has stopped the log, and the
    /// server is restarted after `down_for`.
    fn settle_failure(&mut self) {
        if let Some((at, how)) = self.dying.take() {
            self.out.truncate(at);
            self.down(how);
            return;
        }
        let Some(at) = self.up.as_mut().and_then(|up| up.failed_at.take()) else {
            return;
        };
        if let Some(how) = self.armed.take() {
            self.out.truncate(at);
            self.down(how);
        } else {
            self.restart_later();
        }
    }
}

impl Up {
    fn commit(&mut self, out: &[Output<Msg>], command: Command) -> Option<Applied> {
        if self.stopped {
            return None;
        }
        let applied = ready(self.log.commit(command)).ok();
        if applied.is_none() {
            self.stopped = true;
            self.failed_at = Some(out.len());
        }
        applied
    }

    fn query<R: Send>(&self, f: impl FnOnce(&MetaState) -> R + Send) -> R {
        ready(self.log.query(f)).expect("a query is never refused")
    }

    /// Feeds `event` and then a tick, carrying out the effects in order, as the farm
    /// core does; returns the answers.
    fn feed(&mut self, now: FarmTime, event: SchedEvent, cx: &mut Cx<'_>) -> Vec<Answer> {
        let mut effects: VecDeque<Effect> = self.sched.apply(Input::new(now, event)).into();
        effects.extend(self.sched.apply(Input::new(now, SchedEvent::Tick)));
        let mut answers = Vec::new();
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(record) => {
                    let committed = SchedEvent::Committed(record);
                    effects.extend(self.sched.apply(Input::new(now, committed)));
                }
                Effect::Start(start) => self.start_lease(now, &start, cx),
                Effect::Answer(answer) => answers.push(answer),
                // Every daemon serves every action, so nothing waits for a worker or is
                // refused; a refusal would leave its callers unanswered (property 7).
                Effect::Waiting(_) | Effect::Refuse(_) => {}
            }
        }
        for requeue in self.sched.take_requeues() {
            let listed = self
                .heard
                .get(&requeue.worker)
                .is_some_and(|(at, running)| {
                    now.saturating_duration_since(*at) < FENCE_AFTER
                        && running.contains(&requeue.lease)
                });
            let seen = (now, requeue.lease, requeue.worker, listed);
            cx.seen.requeues.push(seen);
        }
        self.forget_dropped();
        answers
    }

    /// Forgets the callers of the finished operations the scheduler has dropped.
    fn forget_dropped(&mut self) {
        let sched = &self.sched;
        self.machine.forget_dropped(|op| sched.state(op).is_some());
        let machine = &self.machine;
        self.callers.retain(|id, _| machine.waiter(*id).is_some());
    }

    fn start_lease(&mut self, now: FarmTime, start: &StartLease, cx: &mut Cx<'_>) {
        let worker = &start.worker;
        let process = cx.seen.starts;
        let runs = cx.seen.started.entry(start.key.action).or_default();
        runs.insert(process);
        let stream = self.streams.get(worker).copied().unwrap_or_default();
        cx.send(
            NodeId::from(worker.as_str()),
            Msg::Start {
                stream,
                lease: start.lease,
                operation: start.operation,
                action: start.key.action,
            },
        );
        let queued = self
            .sched
            .waiters(start.operation)
            .and_then(|w| w.first())
            .and_then(|id| self.machine.waiter(*id))
            .map_or(wall(now), |w| w.queued);
        let sent = Sent {
            operation: start.operation,
            action: start.key.action,
            stamp: Stamp {
                queued,
                started: wall(now),
            },
        };
        self.machine.start(start.lease, sent);
    }

    fn answer_callers(&self, waiters: &[WaiterId], cached: bool, cx: &mut Cx<'_>) {
        let callers = waiters
            .iter()
            .filter_map(|id| Some((self.callers.get(id)?, self.machine.waiter(*id)?)));
        for (caller, waiter) in callers {
            let action = waiter.key.action;
            cx.send(caller.clone(), Msg::Answered { action, cached });
        }
    }

    fn hello(
        &mut self,
        now: FarmTime,
        from: &NodeId,
        stream: u64,
        instance: String,
        capacity: Resources,
        cx: &mut Cx<'_>,
    ) {
        let worker = WorkerId::new(from.as_str());
        if self.streams.get(&worker).is_some_and(|s| *s >= stream) {
            return;
        }
        self.streams.insert(worker.clone(), stream);
        self.machine.register(&worker);
        let welcome = Msg::Welcome {
            stream,
            epoch: self.lease_epoch,
            interval_ms: u64::try_from(HEARTBEAT.as_millis()).expect("small"),
        };
        cx.send(from.clone(), welcome);
        let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64")])
            .expect("a valid report")
            .with_drivers(["container"]);
        let event = SchedEvent::WorkerUp {
            worker,
            instance: DaemonInstance::new(instance),
            predecessor_stopped: false,
            capacity,
            caps,
        };
        self.feed(now, event, cx);
    }

    fn heartbeat(
        &mut self,
        now: FarmTime,
        from: &NodeId,
        stream: u64,
        seq: u64,
        running: Vec<LeaseId>,
        cx: &mut Cx<'_>,
    ) {
        let worker = WorkerId::new(from.as_str());
        if self.streams.get(&worker) != Some(&stream) {
            cx.send(from.clone(), Msg::Reset { stream });
            return;
        }
        cx.send(from.clone(), Msg::HeartbeatAck { stream, seq });
        self.heard.insert(worker.clone(), (now, running.clone()));
        let event = SchedEvent::Heartbeat {
            worker: worker.clone(),
            running: running.clone(),
        };
        self.feed(now, event, cx);
        let cancels: Vec<LeaseId> = self.sched.not_held(&worker, &running).collect();
        for lease in cancels {
            cx.send(from.clone(), Msg::Cancel { stream, lease });
        }
    }

    /// The operation whose current lease is `lease`, if `worker` holds it and this
    /// process sent its `Start`.
    fn holder(&self, lease: LeaseId, worker: &WorkerId) -> Option<Sent> {
        let sent = self.machine.sent(lease)?;
        let current = matches!(
            self.sched.state(sent.operation)?,
            OpState::Leased { lease: held, worker: holder, committed: true }
                | OpState::Running { lease: held, worker: holder }
                if *held == lease && holder == worker
        );
        current.then_some(sent)
    }

    fn report(
        &mut self,
        now: FarmTime,
        from: &NodeId,
        lease: LeaseId,
        action: Digest,
        outcome: Outcome,
        cx: &mut Cx<'_>,
    ) {
        let worker = WorkerId::new(from.as_str());
        let holder = self
            .holder(lease, &worker)
            .filter(|sent| sent.action == action);
        let Some(sent) = holder else {
            cx.send(
                from.clone(),
                Msg::ResultAck {
                    lease,
                    accepted: false,
                },
            );
            return;
        };
        let event = SchedEvent::Report {
            operation: sent.operation,
            lease,
            outcome,
        };
        let answers = self.feed(now, event, cx);
        let accepted = answers.iter().any(|a| a.lease == lease);
        for answer in answers {
            self.settle(&answer, cx);
        }
        *cx.seen.accepted.entry(lease).or_default() += u32::from(accepted);
        cx.send(from.clone(), Msg::ResultAck { lease, accepted });
    }

    /// Writes a completed answer's action-cache entry, then answers its callers.
    fn settle(&mut self, answer: &Answer, cx: &mut Cx<'_>) {
        self.machine.forget_leases(answer.operation);
        let action = answer
            .waiters
            .first()
            .and_then(|id| self.machine.waiter(*id))
            .map(|w| w.key.action);
        if let (Outcome::Completed { action_result }, Some(action)) = (answer.outcome, action) {
            cx.seen.won.insert(action);
            let record = ActionRecord {
                result: action_result,
                closure: Closure::new(),
            };
            let put = Command::PutAction {
                role: Role::Daemon,
                action,
                record,
            };
            // A failed write is not retried; its callers are answered all the same.
            self.commit(cx.out, put);
        }
        self.answer_callers(&answer.waiters, false, cx);
        self.machine
            .keep_finished(answer.operation, answer.waiters.clone());
        self.forget_dropped();
    }

    fn execute(&mut self, now: FarmTime, from: &NodeId, action: Digest, cx: &mut Cx<'_>) {
        if let ActionAnswer::Hit { .. } = self.query(|m| m.action(&action)) {
            cx.send(
                from.clone(),
                Msg::Answered {
                    action,
                    cached: true,
                },
            );
            return;
        }
        let waiter = self.machine.submit(Waiter {
            key: key(action),
            kind: LeaseKind::Action,
            do_not_cache: false,
            queued: wall(now),
            // The simulation's actions have no `Command`: the `action usage:` line is not
            // what it checks.
            usage: Subject::of(Described::default()).0,
        });
        self.callers.insert(waiter, from.clone());
        let event = SchedEvent::Submit {
            waiter,
            request: request(action),
        };
        self.feed(now, event, cx);
    }

    fn receive(&mut self, now: FarmTime, from: &NodeId, msg: Msg, cx: &mut Cx<'_>) {
        match msg {
            Msg::FindMissing { digests } => {
                let missing = self.query(|m| m.find_missing(&digests)).missing;
                for digest in digests.iter().filter(|d| !missing.contains(d)) {
                    cx.seen.present.push((now, *digest));
                }
                let reply = Msg::Missing {
                    asked: digests,
                    missing,
                };
                cx.send(from.clone(), reply);
            }
            Msg::Execute { action } => self.execute(now, from, action, cx),
            Msg::Hello {
                stream,
                instance,
                capacity,
            } => self.hello(now, from, stream, instance, capacity, cx),
            Msg::Heartbeat {
                stream,
                seq,
                running,
            } => self.heartbeat(now, from, stream, seq, running, cx),
            Msg::Report {
                lease,
                action,
                outcome,
                ..
            } => self.report(now, from, lease, action, outcome, cx),
            _ => {}
        }
    }
}

/// What a handler writes to besides the farm core.
struct Cx<'a> {
    out: &'a mut Vec<Output<Msg>>,
    seen: &'a mut Seen,
}

impl Cx<'_> {
    fn send(&mut self, to: NodeId, msg: Msg) {
        self.out.push(Output::Send { to, msg });
    }
}

impl StateMachine for Server {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        match input.event {
            Event::Start => {
                self.timer(TICK, T_TICK);
                for (i, (at, _)) in self.plan.faults.iter().enumerate() {
                    let after = at.saturating_duration_since(now);
                    self.out.push(Output::Timer {
                        after,
                        tag: T_FAULT + i as u64,
                    });
                }
                self.start(now, input.entropy);
            }
            Event::Timer { tag: T_TICK } => {
                self.timer(TICK, T_TICK);
                if let Some(up) = self.up.as_mut() {
                    let mut cx = Cx {
                        out: &mut self.out,
                        seen: &mut self.seen,
                    };
                    up.commit(cx.out, Command::Tick(now));
                    up.feed(now, SchedEvent::Tick, &mut cx);
                }
                self.flush_round(now);
            }
            Event::Timer { tag: T_START } => {
                self.restarting = false;
                if self.up.take().is_some() {
                    // A stopped log: the machine reboots, and only what was synced is
                    // read back. A new process over the same page cache could read
                    // back a write whose sync failed; `--runtime-dir` is planned to
                    // refuse that (docs/deploy/storage-node.md).
                    self.disk = self.disk.crash(0);
                    self.tier_disk = self.tier_disk.crash(0);
                }
                self.start(now, input.entropy);
            }
            Event::Timer { tag } if tag >= T_READ => self.read_due(now, tag),
            Event::Timer { tag } if tag >= T_PUT => self.put_deadline(now, tag),
            Event::Timer { tag } => {
                let index = usize::try_from(tag - T_FAULT).expect("a fault index");
                let fault = self.plan.faults[index].1;
                self.fault(fault);
            }
            Event::Message { from, msg } => {
                // The write path and the tier, then reads and the farm core.
                let asked = match &msg {
                    Msg::FindMissing { digests } => digests.clone(),
                    _ => Vec::new(),
                };
                let rest = self
                    .write_path(now, &from, msg)
                    .and_then(|msg| self.tier_path(now, msg));
                if let (Some(msg), Some(up)) = (rest, self.up.as_mut()) {
                    let mut cx = Cx {
                        out: &mut self.out,
                        seen: &mut self.seen,
                    };
                    up.receive(now, &from, msg, &mut cx);
                }
                self.start_reads(&asked);
            }
        }
        self.settle_failure();
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use kbf_server::raft_log::RECORD;

    use super::*;

    fn input(at: u64, event: Event<Msg>) -> NodeInput<Msg> {
        NodeInput {
            now: FarmTime::from_millis(at),
            entropy: 9,
            event,
        }
    }

    fn server(faults: Vec<(FarmTime, ServerFault)>) -> Server {
        let plan = ServerPlan {
            faults,
            down_for: Duration::from_secs(3),
            tier: None,
        };
        Server::new(plan, NodeId::from("store"))
    }

    fn restarts(s: &mut Server) -> usize {
        let out = s.take_outputs();
        out.iter()
            .filter(|o| matches!(o, Output::Timer { tag: T_START, .. }))
            .count()
    }

    /// Catches a refused open that is dropped silently (no record for the liveness
    /// line, no retry) or that leaves a server up.
    #[test]
    fn a_refused_open_is_recorded_and_retried() {
        let mut s = server(Vec::new());
        s.disk.put(RECORD, b"not a record");
        s.apply(input(0, Event::Start));
        assert!(s.up.is_none());
        assert_eq!(s.seen.refused.len(), 1, "{:?}", s.seen.refused);
        assert_eq!(restarts(&mut s), 1);
        s.apply(input(3_000, Event::Timer { tag: T_START }));
        assert_eq!(s.seen.refused.len(), 2);
        assert_eq!(restarts(&mut s), 1);
    }

    /// Catches a server that comes up although its first commit (the writer epoch)
    /// failed.
    #[test]
    fn a_failed_first_commit_takes_the_server_down() {
        // How many filesystem operations an open of a new log takes.
        let probe = server(Vec::new());
        let opened = {
            let _context = probe.runtime.enter();
            ready(RaftMetaLog::open(
                probe.disk.clone(),
                Retention::default(),
                "cell/",
                || Ok(FarmLogId::new(NonZeroU64::MIN)),
            ))
        };
        drop(opened.expect("a new log opens"));
        let mut s = server(Vec::new());
        s.disk.fail_at(probe.disk.ops(), Fault::Error);
        s.apply(input(0, Event::Start));
        assert!(s.up.is_none());
        assert!(s.seen.refused.is_empty());
        assert_eq!(restarts(&mut s), 1);
    }

    /// Catches a second restart planned while one is pending: a stopped log's server
    /// killed before its restart would start twice; and a fault that acts on a server
    /// already down.
    #[test]
    fn a_kill_after_the_log_stopped_restarts_once() {
        let at = FarmTime::from_millis(1_500);
        let faults = vec![
            (
                FarmTime::from_millis(1),
                ServerFault::IoErrorAt { after: 0 },
            ),
            (at, ServerFault::Kill),
            (at, ServerFault::PowerCut { keep: 0, names: 0 }),
            (
                at,
                ServerFault::PowerCutAt {
                    after: 0,
                    keep: 0,
                    names: 0,
                },
            ),
            (at, ServerFault::IoErrorAt { after: 0 }),
        ];
        let mut s = server(faults);
        s.apply(input(0, Event::Start));
        s.take_outputs();
        s.apply(input(1, Event::Timer { tag: T_FAULT }));
        s.apply(input(1_000, Event::Timer { tag: T_TICK }));
        assert!(s.up.as_ref().is_some_and(|up| up.stopped));
        assert_eq!(restarts(&mut s), 1);
        s.apply(input(1_500, Event::Timer { tag: T_FAULT + 1 }));
        assert!(s.up.is_none());
        assert_eq!(restarts(&mut s), 0);
        // Faults that meet a server already down do nothing.
        for tag in T_FAULT + 2..T_FAULT + 5 {
            s.apply(input(1_600, Event::Timer { tag }));
        }
        assert!(s.up.is_none() && s.armed.is_none());
        assert_eq!(restarts(&mut s), 0);
    }
}
