//! A simulated cell for the scheduler: a leader running [`Scheduler`] as `kbf-server`
//! does, a control log, and workers that follow the daemon's rules, on the `kbf-sim`
//! kernel. The failure family (`sim_f2_failures.rs`) drives it; it is shared so later
//! families can build on the same nodes.
//!
//! What each node models, and where it differs from the code it stands for:
//!
//! - **Leader** ([`leader`]): the farm core of `crates/kbf-server/src/farm.rs`. Only the
//!   first `Hello` of a stream registers; a `Hello` resent on it becomes
//!   `Event::Capacity`; a replaced stream's heartbeats are dropped unacknowledged; each
//!   `Start` names the newest heartbeat taken on the stream; a `Result` is fed only from
//!   the node holding the operation's current lease (`State::holder`), else refused; the
//!   leases a heartbeat lists and the scheduler no longer holds are cancelled
//!   (`not_held`). Unlike the single-node server, records go through the log node and
//!   come back later, in log order; a placement round runs every second (the server
//!   also runs one after every input); and a `ResultAck` says `accepted` once the result
//!   is proposed, not answered. A `Result` that names another action than its lease's
//!   is refused. It can restart (a fresh scheduler of a new term, named as the lease
//!   epoch in `Welcome`, as `kbf-server` picks one per process) and pause (its clock
//!   stops with it). For F2.4 it can also take a worker's newest heartbeat again at
//!   `START_GRACE - 1 ms`, `START_GRACE` and `START_GRACE + 1 ms` after a held-back
//!   `Start` (a duplicate delivered late), so reconciliation meets that boundary
//!   exactly.
//! - **Log** ([`log`]): commits each record after a random delay, so the log order is
//!   not always the order records were proposed in; each leader incarnation has its own
//!   log, as the single-node server's in-process log dies with it. The leader's
//!   `Append`s go over the same faulty bus as every other message, so a duplicated
//!   `Append` (3% in [`calm_network`], 20% in F2.10) commits its record twice, at two
//!   indexes, and the scheduler is fed that committed record twice. The real log
//!   appends each proposal once; this is a fault the scheduler tolerates, not one the
//!   server produces.
//! - **Worker** ([`worker`]): the daemon of `crates/kbf-daemon`. It fences every lease
//!   T after the send of its newest acknowledged heartbeat (today the daemon self-fences
//!   every lease, whatever the `Start` says), acts on a `Start` only within W of sending
//!   the heartbeat it names, ignores a `Start` for a lease whose result is unacknowledged,
//!   reports a fenced or cancelled run `ABORTED`, keeps every result until its
//!   `ResultAck`, lists it in every heartbeat, and resends it with every heartbeat and on
//!   every new stream. (On the wire it is resent only on a new stream; the sim's links
//!   lose messages a TCP stream would not, so it resends more often.) Every result
//!   names the action of its lease's `Start`. On a `Welcome` that names another lease
//!   epoch it kills the runs and forgets the results of the earlier one, unsent. It can
//!   die, freeze (suspend: on resume it fences first), reconnect, and resend its
//!   `Hello`.
//!
//! Every input the leader feeds the scheduler is checked at once by [`check::Check`];
//! what needs every node (two runs of one operation at once, a `Start` before its
//! commit) is checked at the end of the run by the family file.

pub mod check;
pub mod leader;
pub mod log;
pub mod worker;

use std::time::Duration;

use kbf_sched::Request;
use kbf_sim::{Chance, Faults, Node, NodeId, NodeInput, Output, Partition, Sim};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseId, Outcome, Qos,
    Resources, StartLease, StateMachine, WorkerId,
};

pub use leader::{Leader, LeaderPlan};
pub use log::{Log, LogPlan};
pub use worker::{Worker, WorkerPlan};

/// One gibibyte.
pub const GIB: u64 = 1 << 30;
/// How often the leader runs a placement round.
pub const TICK: Duration = Duration::from_secs(1);
/// How often a worker heartbeats (the server's interval is at most 7 s).
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// What nodes send each other. `stream` stands for the worker's gRPC stream: a daemon
/// numbers its streams, and a new one replaces the old.
#[derive(Clone, Debug)]
pub enum Msg {
    /// Leader to log: commit `record` in the log of leader incarnation `incarnation`.
    Append {
        incarnation: u64,
        record: ControlRecord,
    },
    /// Log to leader: `record` is committed at `index`.
    Committed {
        incarnation: u64,
        index: u64,
        record: ControlRecord,
    },
    /// The first message of a stream, and resent on it when the node report changes.
    Hello {
        node: WorkerId,
        stream: u64,
        capacity: Resources,
    },
    /// The server's answer to the first `Hello` of a stream, naming its lease epoch.
    Welcome {
        stream: u64,
        epoch: u64,
    },
    Heartbeat {
        stream: u64,
        seq: u64,
        running: Vec<LeaseId>,
    },
    HeartbeatAck {
        stream: u64,
        seq: u64,
    },
    /// `heartbeat_seq` names the newest heartbeat the server had taken on the stream.
    /// `incarnation` is for the checks only: the daemon never reads it.
    Start {
        stream: u64,
        start: StartLease,
        heartbeat_seq: u64,
        incarnation: u64,
    },
    /// A `Result`: like the wire's, it names the lease and the action its `Start` named.
    Report {
        stream: u64,
        lease: LeaseId,
        outcome: Outcome,
        action: Option<Digest>,
    },
    ResultAck {
        lease: LeaseId,
        accepted: bool,
    },
    Cancel {
        stream: u64,
        lease: LeaseId,
    },
    /// The stream ended (the server process went away, or the stream is unknown to it).
    Goodbye {
        stream: u64,
    },
}

/// Who runs the checks, for the replay line a failure prints.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    /// The scenario's name, as the replay test takes it in `KBF_SIM_SCENARIO`.
    pub scenario: &'static str,
    pub seed: u64,
}

impl Ctx {
    /// The command that replays this run.
    #[must_use]
    pub fn replay(&self) -> String {
        format!(
            "KBF_SIM_SEED={} KBF_SIM_SCENARIO={} cargo test -p kbf-sched --test sim_f2_failures -- --ignored --exact replay",
            self.seed, self.scenario
        )
    }

    /// Panics with the seed, the invariant, what broke it and the replay command.
    #[track_caller]
    pub fn fail(&self, at: FarmTime, invariant: &str, detail: &str) -> ! {
        panic!(
            "{} seed {} at {} ms: {invariant} violated: {detail}\nreplay: {}",
            self.scenario,
            self.seed,
            at.as_millis(),
            self.replay()
        )
    }
}

/// The whole input of one run, drawn from the seed by the family file.
#[derive(Clone, Debug)]
pub struct Plan {
    pub ctx: Ctx,
    pub faults: Faults,
    /// Partitions in force from `.0` to `.1` (ms), isolating the named nodes from the
    /// rest. They must not overlap.
    pub partitions: Vec<(u64, u64, Vec<&'static str>)>,
    pub leader: LeaderPlan,
    pub log: LogPlan,
    pub workers: Vec<WorkerPlan>,
    /// How many callers submit, and by when (ms) the last has.
    pub callers: u64,
    pub arrive_by: u64,
    /// The run stops here (ms). Every fault has ended long enough before it for every
    /// caller to be answered.
    pub end: u64,
}

/// The node kinds of the cell.
pub enum Cell {
    Leader(Box<Leader>),
    Log(Log),
    Worker(Box<Worker>),
}

impl StateMachine for Cell {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        match self {
            Cell::Leader(l) => l.apply(input),
            Cell::Log(g) => g.apply(input),
            Cell::Worker(w) => w.apply(input),
        }
    }
}

impl Node for Cell {
    type Msg = Msg;

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        match self {
            Cell::Leader(l) => l.take_outputs(),
            Cell::Log(g) => g.take_outputs(),
            Cell::Worker(w) => w.take_outputs(),
        }
    }
}

#[must_use]
pub fn leader_id() -> NodeId {
    NodeId::from("leader")
}

#[must_use]
pub fn log_id() -> NodeId {
    NodeId::from("log")
}

/// The action digest of caller `n`'s request.
#[must_use]
pub fn action(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// The `ActionResult` digest a run of `action` under `lease` produces: one value per
/// (action, lease), so an answer shows which run it came from.
#[must_use]
pub fn result_digest(action: &Digest, lease: LeaseId) -> Digest {
    let mut hash = action.hash;
    hash[8] = 0xAA;
    hash[16..24].copy_from_slice(&lease.term.to_be_bytes());
    hash[24..32].copy_from_slice(&lease.seq.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, action.size_bytes)
}

/// Caller `n`'s request: one core and 1 GiB at QoS `ci`, any platform. Every third is
/// networked; the daemon self-fences all of them today, so that changes only dedup.
#[must_use]
pub fn request(n: u64) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: action(n),
        },
        qos: Qos::Ci,
        resources: Resources::new(1_000, GIB),
        needs: kbf_caps::Request::default(),
        hermetic: !n.is_multiple_of(3),
        do_not_cache: false,
    }
}

/// A run of `plan`, to `plan.end`.
pub struct World {
    pub plan: Plan,
    pub sim: Sim<Cell>,
}

impl World {
    #[must_use]
    pub fn run(plan: Plan) -> Self {
        let mut sim = Sim::new(plan.ctx.seed, plan.faults.clone());
        sim.add_node(leader_id(), Cell::Leader(Box::new(Leader::new(&plan))));
        sim.add_node(log_id(), Cell::Log(Log::new(plan.log)));
        for w in &plan.workers {
            sim.add_node(w.name, Cell::Worker(Box::new(Worker::new(w.clone()))));
        }
        for (from, to, isolated) in &plan.partitions {
            let group: Vec<NodeId> = isolated.iter().map(|n| NodeId::from(*n)).collect();
            sim.partition_at(FarmTime::from_millis(*from), Partition::new([group]));
            sim.partition_at(FarmTime::from_millis(*to), Partition::none());
        }
        sim.run_until(FarmTime::from_millis(plan.end));
        Self { plan, sim }
    }

    #[must_use]
    pub fn leader(&self) -> &Leader {
        match self.sim.node(&leader_id()) {
            Some(Cell::Leader(l)) => l,
            _ => unreachable!("the leader is a leader"),
        }
    }

    #[must_use]
    pub fn log(&self) -> &Log {
        match self.sim.node(&log_id()) {
            Some(Cell::Log(g)) => g,
            _ => unreachable!("the log is a log"),
        }
    }

    pub fn workers(&self) -> impl Iterator<Item = (&NodeId, &Worker)> {
        self.sim.nodes().filter_map(|(id, n)| match n {
            Cell::Worker(w) => Some((id, &**w)),
            _ => None,
        })
    }

    #[must_use]
    pub fn worker(&self, name: &str) -> &Worker {
        match self.sim.node(&NodeId::from(name)) {
            Some(Cell::Worker(w)) => w,
            _ => unreachable!("{name} is a worker"),
        }
    }

    /// The end of the run.
    #[must_use]
    pub fn end(&self) -> FarmTime {
        FarmTime::from_millis(self.plan.end)
    }
}

/// The default network: short delays, some duplicates and reordering, no loss.
#[must_use]
pub fn calm_network() -> Faults {
    Faults {
        min_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(50),
        duplicate: Chance::percent(3),
        reorder: Chance::percent(3),
        ..Faults::default()
    }
}
