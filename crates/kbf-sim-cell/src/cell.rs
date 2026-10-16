//! A scenario, the cell it builds, and the run loop that checks after every step.

use std::collections::BTreeSet;
use std::time::Duration;

use kbf_meta::ObjectId;

use kbf_sim::{Chance, Faults, Node, NodeId, NodeInput, Output, Partition, Sim, SimRng, TraceHash};
use kbf_store::Fault;
use kbf_types::{Digest, Effect, FarmTime, Resources, StateMachine};

use crate::check::Checker;
use crate::client::RETRY;
use crate::server::Rollback;
use crate::{
    Cut, Down, FakeClient, FakeDaemon, Met, Msg, ObjectStore, Server, ServerFault, ServerPlan,
    StoreFault, StorePlan, StorePoint, StoreStats, TierPlan, TierPoint, TierSeen, action,
};

/// What the server meets in a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Nothing: only the network's faults. Every property is enforced.
    Calm,
    /// One or two process kills.
    Restart,
    /// A power cut, now or part-way through a later commit.
    Crash,
    /// An I/O error that stops the log, then a restart.
    IoError,
    /// One daemon cut off from the cell for longer than the scheduler's grace G.
    Partition,
    /// The object store down for minutes: hanging (PUTs held) on even seeds, refusing
    /// every PUT on odd ones.
    StoreOutage,
    /// As [`Kind::StoreOutage`], for one hour to one and a half.
    StoreOutageHours,
    /// The store answering every PUT seconds late for a while, past the client's retry.
    StoreSlow,
    /// The store failing PUTs here and there, some after keeping the object.
    StoreErrors,
    /// The store keeping objects but losing their acknowledgement.
    StoreLostAck,
    /// The server going down (killed, or a power cut) at a point of the write path,
    /// while the store is slow, so PUTs are in flight.
    StoreCrash,
    /// The local write tier, with only the network's faults.
    TierCalm,
    /// The server going down at a point of the tier's write or flush path
    /// ([`TierPoint`]), while the store is slow, so flushes are in flight.
    TierCrash,
    /// As [`Kind::StoreOutage`], with the tier: it takes writes until its budget, then
    /// the direct path refuses them.
    TierStoreOutage,
    /// As [`Kind::StoreOutageHours`], with the tier.
    TierStoreOutageHours,
    /// The tier's disk full (`ENOSPC`, even seeds) or failing (an I/O error, odd seeds)
    /// at one to three operations.
    TierDiskFull,
    /// A bad sector in a staged block before its flush.
    TierBadBlock,
    /// A tier whose budget holds little beyond its staged blocks, so flushed copies
    /// are evicted while reads of them are in flight.
    TierEviction,
    /// All-in-one: daemon 0 is the daemon half of the server's process, and one or two
    /// kills of that process take both halves down mid-build; both start again.
    WholeProcess,
}

impl Kind {
    /// Every kind, in the order the sweeps run them.
    pub const ALL: [Self; 19] = [
        Self::Calm,
        Self::Restart,
        Self::Crash,
        Self::IoError,
        Self::Partition,
        Self::StoreOutage,
        Self::StoreOutageHours,
        Self::StoreSlow,
        Self::StoreErrors,
        Self::StoreLostAck,
        Self::StoreCrash,
        Self::TierCalm,
        Self::TierCrash,
        Self::TierStoreOutage,
        Self::TierStoreOutageHours,
        Self::TierDiskFull,
        Self::TierBadBlock,
        Self::TierEviction,
        Self::WholeProcess,
    ];

    /// Whether the server restarts, or its log stops, in a run of this kind.
    #[must_use]
    pub fn restarts(self) -> bool {
        matches!(
            self,
            Self::Restart
                | Self::Crash
                | Self::IoError
                | Self::StoreCrash
                | Self::TierCrash
                | Self::WholeProcess
        )
    }

    /// The local write tier a run of this kind gives the server, if any.
    #[must_use]
    pub fn tier(self) -> Option<TierPlan> {
        let plan = |budget, seal_after| TierPlan {
            budget,
            seal_after: Duration::from_secs(seal_after),
            segment_limit: TIER_SEGMENT,
        };
        match self {
            Self::TierCalm | Self::TierCrash | Self::TierDiskFull => Some(plan(160, 5)),
            Self::TierStoreOutage | Self::TierStoreOutageHours => Some(plan(96, 5)),
            // Blocks stay staged long enough for the bad sector to land in one.
            Self::TierBadBlock => Some(plan(160, 20)),
            Self::TierEviction => Some(plan(64, 5)),
            _ => None,
        }
    }

    /// The kind its `Debug` name names, for a replay line.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| format!("{k:?}") == name)
    }
}

/// The whole input of a run, drawn from its kind and seed.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub kind: Kind,
    pub seed: u64,
    pub faults: Faults,
    pub server: ServerPlan,
    pub store: StorePlan,
    pub daemons: usize,
    /// The daemon that is the daemon half of the server's process
    /// ([`Kind::WholeProcess`]): it dies and starts again with the server.
    pub colocated: Option<usize>,
    /// A daemon cut off from every other node from a time, for [`CUT_FOR`].
    pub partition: Option<(usize, FarmTime)>,
    /// Each client's actions, and when it starts on each.
    pub clients: Vec<Vec<(u64, FarmTime)>>,
    /// The run stops here. Every fault has ended long enough before it for every
    /// action to be answered.
    pub end: FarmTime,
    /// The one crash [`Scenario::crash_at`] put in place of the seed's: the replay line
    /// names it, so a failure replays the crash that failed.
    pub pinned: Option<(StorePoint, Down)>,
    /// The same for [`Scenario::tier_crash_at`].
    pub pinned_tier: Option<(TierPoint, Down)>,
}

/// How many distinct actions the clients draw theirs from, so some are shared.
const POOL: u64 = 10;
const ACTIONS_PER_CLIENT: usize = 6;
/// With the tier: more blobs (1 to 30 bytes each), so blocks fill and seal by size,
/// and budgets of a few blocks are met.
const TIER_POOL: u64 = 30;
const TIER_ACTIONS_PER_CLIENT: usize = 8;
/// The size at which the tier's open block seals in the cell.
const TIER_SEGMENT: u64 = 48;
const LAST_FAULT_MS: u64 = 60_000;
const DAEMONS: usize = 3;
/// How long a [`Kind::Partition`] daemon is cut off: past the grace G (60 s).
pub const CUT_FOR: Duration = Duration::from_secs(70);

impl Scenario {
    /// The scenario `kind` draws from `seed`.
    #[must_use]
    pub fn new(kind: Kind, seed: u64) -> Self {
        let mut rng = SimRng::from_seed(seed ^ 0x5ce1_1ce1);
        let at = |rng: &mut SimRng| FarmTime::from_millis(rng.between(5_000, LAST_FAULT_MS));
        // A write-path fault early in the run, so the uploads of later jobs meet it.
        let early = |rng: &mut SimRng| FarmTime::from_millis(rng.between(1_000, 20_000));
        let mut faults = Vec::new();
        let mut store = Vec::new();
        match kind {
            Kind::Calm | Kind::TierCalm | Kind::TierEviction => {}
            Kind::Restart | Kind::WholeProcess => {
                for _ in 0..rng.between(1, 2) {
                    faults.push((at(&mut rng), ServerFault::Kill));
                }
            }
            Kind::Crash => {
                let keep = usize::try_from(rng.between(0, 64)).expect("small");
                let names = rng.next_u64();
                let fault = if rng.chance(Chance::percent(50)) {
                    ServerFault::PowerCut { keep, names }
                } else {
                    let after = rng.between(0, 30);
                    ServerFault::PowerCutAt { after, keep, names }
                };
                faults.push((at(&mut rng), fault));
            }
            Kind::IoError => {
                let after = rng.between(0, 30);
                faults.push((at(&mut rng), ServerFault::IoErrorAt { after }));
            }
            Kind::Partition => {}
            Kind::StoreOutage
            | Kind::StoreOutageHours
            | Kind::TierStoreOutage
            | Kind::TierStoreOutageHours => {
                let from = early(&mut rng);
                let minutes = if matches!(kind, Kind::StoreOutage | Kind::TierStoreOutage) {
                    rng.between(2, 10)
                } else {
                    rng.between(60, 90)
                };
                let until = from.saturating_add(Duration::from_secs(minutes * 60));
                // Even seeds hang, odd seeds refuse: a sweep of two meets both.
                let fault = if seed.is_multiple_of(2) {
                    StoreFault::Pause { until }
                } else {
                    StoreFault::FailUntil { until }
                };
                store.push((from, fault));
            }
            Kind::StoreSlow => {
                let from = early(&mut rng);
                let until = from.saturating_add(Duration::from_secs(rng.between(10, 60)));
                let by = Duration::from_millis(rng.between(500, 20_000));
                store.push((from, StoreFault::Slow { until, by }));
            }
            Kind::StoreErrors => {
                for _ in 0..rng.between(1, 3) {
                    let n = u32::try_from(rng.between(1, 10)).expect("small");
                    store.push((at(&mut rng), StoreFault::FailNext { n }));
                    let n = u32::try_from(rng.between(1, 5)).expect("small");
                    store.push((at(&mut rng), StoreFault::KeepButFail { n }));
                }
            }
            Kind::StoreLostAck => {
                for _ in 0..rng.between(1, 3) {
                    let n = u32::try_from(rng.between(1, 5)).expect("small");
                    store.push((early(&mut rng), StoreFault::LoseAck { n }));
                }
            }
            Kind::StoreCrash => {
                let point = match rng.between(0, 3) {
                    0 => StorePoint::PutSent,
                    1 => StorePoint::BeforeCommit,
                    // A `PutBlob` commit is two operations: the append and the sync.
                    2 => StorePoint::InCommit {
                        after: rng.between(0, 1),
                    },
                    _ => StorePoint::BeforeReply,
                };
                let down = if rng.chance(Chance::percent(50)) {
                    Down::Kill
                } else {
                    let keep = usize::try_from(rng.between(0, 64)).expect("small");
                    Down::PowerCut {
                        keep,
                        names: rng.next_u64(),
                    }
                };
                let nth = u32::try_from(rng.between(0, 1)).expect("small");
                faults.push((early(&mut rng), ServerFault::CrashAt { point, nth, down }));
                let until = FarmTime::from_millis(LAST_FAULT_MS);
                let by = Duration::from_millis(rng.between(100, 2_000));
                store.push((FarmTime::default(), StoreFault::Slow { until, by }));
            }
            Kind::TierCrash => {
                let n = rng.between(0, TierPoint::ALL.len() as u64 - 1);
                let point = match TierPoint::ALL[usize::try_from(n).expect("small")] {
                    // A commit is two operations of the raft disk: the append and the sync.
                    TierPoint::InPutStaged { .. } => TierPoint::InPutStaged {
                        after: rng.between(0, 1),
                    },
                    TierPoint::InFlushCommit { .. } => TierPoint::InFlushCommit {
                        after: rng.between(0, 1),
                    },
                    other => other,
                };
                let down = if rng.chance(Chance::percent(50)) {
                    Down::Kill
                } else {
                    let keep = usize::try_from(rng.between(0, 64)).expect("small");
                    Down::PowerCut {
                        keep,
                        names: rng.next_u64(),
                    }
                };
                let nth = u32::try_from(rng.between(0, 1)).expect("small");
                let crash = ServerFault::TierCrashAt { point, nth, down };
                faults.push((early(&mut rng), crash));
                let until = FarmTime::from_millis(LAST_FAULT_MS);
                let by = Duration::from_millis(rng.between(100, 2_000));
                store.push((FarmTime::default(), StoreFault::Slow { until, by }));
            }
            Kind::TierDiskFull => {
                let fault = if seed.is_multiple_of(2) {
                    Fault::NoSpace
                } else {
                    Fault::Error
                };
                for _ in 0..rng.between(1, 3) {
                    let after = rng.between(0, 20);
                    faults.push((at(&mut rng), ServerFault::TierDiskAt { after, fault }));
                }
            }
            Kind::TierBadBlock => {
                let when = FarmTime::from_millis(rng.between(10_000, LAST_FAULT_MS));
                faults.push((when, ServerFault::TierFlip));
            }
        }
        let partition = (kind == Kind::Partition).then(|| {
            let daemon = usize::try_from(rng.between(0, DAEMONS as u64 - 1)).expect("small");
            (daemon, at(&mut rng))
        });
        faults.sort_by_key(|(t, _)| *t);
        store.sort_by_key(|(t, _)| *t);
        // The store is quiet once its last window has ended and its last slowed answer
        // is in. Each counted failure or lost ack costs one client one more retry.
        let quiet = store
            .iter()
            .filter_map(|(_, f)| Some(f.until()?.saturating_add(f.delay())))
            .fold(FarmTime::from_millis(LAST_FAULT_MS), FarmTime::max);
        let retries: u32 = store.iter().map(|(_, f)| f.count()).sum();
        let quiet = quiet.saturating_add(RETRY * retries);
        let (pool, per_client) = if kind.tier().is_some() {
            (TIER_POOL, TIER_ACTIONS_PER_CLIENT)
        } else {
            (POOL, ACTIONS_PER_CLIENT)
        };
        let clients = (0..3)
            .map(|_| {
                (0..per_client)
                    .map(|_| {
                        let n = rng.between(0, pool - 1);
                        (n, FarmTime::from_millis(rng.between(0, LAST_FAULT_MS)))
                    })
                    .collect()
            })
            .collect();
        Self {
            kind,
            seed,
            faults: Faults {
                min_delay: Duration::from_millis(1),
                max_delay: Duration::from_millis(50),
                drop: Chance::percent(1),
                duplicate: Chance::percent(3),
                reorder: Chance::percent(3),
            },
            server: ServerPlan {
                faults,
                down_for: Duration::from_secs(3),
                tier: kind.tier(),
            },
            store: StorePlan { faults: store },
            daemons: DAEMONS,
            colocated: (kind == Kind::WholeProcess).then_some(0),
            partition,
            clients,
            end: quiet.saturating_add(Duration::from_secs(150)),
            pinned: None,
            pinned_tier: None,
        }
    }

    /// When [`Scenario::crash_at`] crashes the server.
    pub const PINNED_AT: FarmTime = FarmTime::from_millis(10_000);

    /// The [`Kind::StoreCrash`] scenario `seed` draws, with its crash replaced by one at
    /// `point` (its first reach from [`Scenario::PINNED_AT`]), going `down`.
    #[must_use]
    pub fn crash_at(seed: u64, point: StorePoint, down: Down) -> Self {
        let mut s = Self::new(Kind::StoreCrash, seed);
        let crash = ServerFault::CrashAt {
            point,
            nth: 0,
            down,
        };
        s.server.faults = vec![(Self::PINNED_AT, crash)];
        s.pinned = Some((point, down));
        s
    }

    /// The [`Kind::TierCrash`] scenario `seed` draws, with its crash replaced by one at
    /// tier `point` (its first reach from [`Scenario::PINNED_AT`]), going `down`.
    #[must_use]
    pub fn tier_crash_at(seed: u64, point: TierPoint, down: Down) -> Self {
        let mut s = Self::new(Kind::TierCrash, seed);
        let crash = ServerFault::TierCrashAt {
            point,
            nth: 0,
            down,
        };
        s.server.faults = vec![(Self::PINNED_AT, crash)];
        s.pinned_tier = Some((point, down));
        s
    }

    /// The command that reruns this scenario: its kind, its seed and, if pinned, its
    /// crash (`<point>/<down>`).
    #[must_use]
    pub fn replay_line(&self) -> String {
        let store = self.pinned.map(|(point, down)| format!("{point}/{down}"));
        let tier = self
            .pinned_tier
            .map(|(point, down)| format!("{point}/{down}"));
        let crash = store
            .or(tier)
            .map(|c| format!(" KBF_SIM_CRASH={c}"))
            .unwrap_or_default();
        format!(
            "KBF_SIM_KIND={:?} KBF_SIM_SEED={}{crash} cargo test -p kbf-sim-cell --test cell \
             -- --ignored replay",
            self.kind, self.seed
        )
    }

    /// The scenario a replay line's `KBF_SIM_KIND`, `KBF_SIM_SEED` and `KBF_SIM_CRASH`
    /// name; `None` if one does not parse, or a crash is pinned to a kind other than
    /// [`Kind::StoreCrash`] (a write-path point) or [`Kind::TierCrash`] (a tier point).
    #[must_use]
    pub fn from_replay(kind: &str, seed: &str, crash: Option<&str>) -> Option<Self> {
        let kind = Kind::parse(kind)?;
        let seed = seed.parse().ok()?;
        let Some(crash) = crash else {
            return Some(Self::new(kind, seed));
        };
        let (point, down) = crash.split_once('/')?;
        let down = Down::parse(down)?;
        match kind {
            Kind::StoreCrash => Some(Self::crash_at(seed, StorePoint::parse(point)?, down)),
            Kind::TierCrash => Some(Self::tier_crash_at(seed, TierPoint::parse(point)?, down)),
            _ => None,
        }
    }
}

/// The node kinds of the cell.
pub enum CellNode {
    Server(Box<Server>),
    Daemon(Box<FakeDaemon>),
    Client(Box<FakeClient>),
    Store(Box<ObjectStore>),
}

impl CellNode {
    /// The node as a server, if it is one.
    #[must_use]
    pub fn server(&self) -> Option<&Server> {
        if let Self::Server(s) = self {
            Some(s)
        } else {
            None
        }
    }

    /// The node as a daemon, if it is one.
    #[must_use]
    pub fn daemon(&self) -> Option<&FakeDaemon> {
        if let Self::Daemon(d) = self {
            Some(d)
        } else {
            None
        }
    }

    /// The node as a client, if it is one.
    #[must_use]
    pub fn client(&self) -> Option<&FakeClient> {
        if let Self::Client(c) = self {
            Some(c)
        } else {
            None
        }
    }

    /// The node as the object store, if it is it.
    #[must_use]
    pub fn store(&self) -> Option<&ObjectStore> {
        if let Self::Store(s) = self {
            Some(s)
        } else {
            None
        }
    }
}

impl StateMachine for CellNode {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        match self {
            Self::Server(s) => s.apply(input),
            Self::Daemon(d) => d.apply(input),
            Self::Client(c) => c.apply(input),
            Self::Store(s) => s.apply(input),
        }
    }
}

impl Node for CellNode {
    type Msg = Msg;

    fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        match self {
            Self::Server(s) => s.take_outputs(),
            Self::Daemon(d) => d.take_outputs(),
            Self::Client(c) => c.take_outputs(),
            Self::Store(s) => s.take_outputs(),
        }
    }
}

pub(crate) fn server_id() -> NodeId {
    NodeId::from("server")
}

pub(crate) fn store_id() -> NodeId {
    NodeId::from("store")
}

/// What a run did, for the tests to read.
#[derive(Clone, Debug)]
pub struct RunReport {
    pub trace: TraceHash,
    /// How many times the server opened its log.
    pub starts: u64,
    /// How many results were accepted.
    pub accepted: usize,
    /// How many blobs were answered present.
    pub present: usize,
    /// Each upload acknowledged: when, the blob, and the object its PUT wrote.
    pub acked: Vec<(FarmTime, Digest, ObjectId)>,
    /// How many crash points took the server down.
    pub crashes: usize,
    /// What the object store did.
    pub store: StoreStats,
    /// Each PUT a store fault met, and how ([`ObjectStore::met`]).
    pub met: Vec<(ObjectId, Digest, Met)>,
    /// The objects PUT again while the store held them ([`ObjectStore::repeated`]).
    pub repeated: BTreeSet<ObjectId>,
    /// The PUTs whose deadline answered their upload `UNAVAILABLE`.
    pub timed_out: BTreeSet<ObjectId>,
    /// The PUTs the store answered failed while their upload waited.
    pub put_failed: BTreeSet<ObjectId>,
    /// Each `PutBlob` commit a crash cut, and what the next start found of it.
    pub cuts: Vec<Cut>,
    /// Objects in the store that no index entry names at the end (left for the planned
    /// orphan sweep); `None` if the server is down at the end.
    pub orphans: Option<BTreeSet<ObjectId>>,
    /// What the local write tier did.
    pub tier: TierSeen,
    /// Each flushed segment a store fault met, and how.
    pub flush_met: Vec<(ObjectId, Met)>,
    /// A binary that runs only machine version 1 opening the durable log at the end
    /// (scenario 30); `None` without a tier, or with the server down.
    pub rollback: Option<Rollback>,
    /// The runs, staged outputs and unacknowledged results the daemon half lost with
    /// its process ([`Kind::WholeProcess`]; 0 otherwise).
    pub lost: usize,
    /// How many actions were started by more than one run of the server's process.
    pub rerun: usize,
}

/// Runs `scenario` to its end, checking every property after every step.
///
/// # Panics
///
/// When a property does not hold: the message names the scenario, the seed, the
/// property and how it broke.
#[must_use]
pub fn run(scenario: &Scenario) -> RunReport {
    let mut sim = Sim::new(scenario.seed, scenario.faults.clone());
    let store = ObjectStore::new(scenario.store.clone());
    sim.add_node(store_id(), CellNode::Store(Box::new(store)));
    let server = Server::new(scenario.server.clone(), store_id());
    sim.add_node(server_id(), CellNode::Server(Box::new(server)));
    for n in 0..scenario.daemons {
        let capacity = Resources::new(2_000, 4 << 30);
        let daemon = if scenario.colocated == Some(n) {
            FakeDaemon::in_process(server_id(), &format!("daemon-{n}"), capacity)
        } else {
            FakeDaemon::new(server_id(), format!("daemon-{n}-1"), capacity)
        };
        sim.add_node(
            NodeId::new(format!("daemon-{n}")),
            CellNode::Daemon(Box::new(daemon)),
        );
    }
    for (n, jobs) in scenario.clients.iter().enumerate() {
        let jobs = jobs.iter().map(|(a, at)| (action(*a), *at));
        let client = FakeClient::new(server_id(), jobs);
        sim.add_node(
            NodeId::new(format!("client-{n}")),
            CellNode::Client(Box::new(client)),
        );
    }
    if let Some((n, at)) = scenario.partition {
        let cut = vec![NodeId::new(format!("daemon-{n}"))];
        sim.partition_at(at, Partition::new([cut]));
        sim.partition_at(at.saturating_add(CUT_FOR), Partition::none());
    }
    let half = scenario
        .colocated
        .map(|n| NodeId::new(format!("daemon-{n}")));
    let mut was_up = false;
    let mut checker = Checker::new(scenario);
    while sim.now() <= scenario.end {
        // The server's tick timer keeps the queue from ever running dry.
        let _ = sim.step();
        was_up = share_fate(&mut sim, was_up);
        checker.after_step(&sim);
    }
    checker.at_end(&sim);
    let server = crate::check::server(&sim);
    let seen = &server.seen;
    let store = sim
        .nodes()
        .find_map(|(_, n)| n.store())
        .expect("the cell has an object store");
    let orphans = server.meta().map(|meta| {
        let answers = seen.uploaded.iter().map(|d| meta.blob(d));
        crate::check::orphans(store.objects().map(|(o, _)| o), answers)
    });
    let rollback = scenario.server.tier.and_then(|_| server.rollback());
    let lost = half
        .and_then(|h| {
            sim.node(&h)
                .and_then(CellNode::daemon)
                .map(FakeDaemon::lost)
        })
        .unwrap_or(0);
    RunReport {
        trace: sim.trace_hash(),
        starts: seen.starts,
        accepted: seen.accepted.values().filter(|n| **n > 0).count(),
        present: seen.present.len(),
        acked: seen.acked.clone(),
        crashes: seen.crashed_at.len(),
        store: store.stats(),
        met: store.met().to_vec(),
        repeated: store.repeated().clone(),
        timed_out: seen.timed_out.clone(),
        put_failed: seen.put_failed.clone(),
        cuts: seen.cuts.clone(),
        orphans,
        tier: seen.tier.clone(),
        flush_met: store.flush_met().to_vec(),
        rollback,
        lost,
        rerun: seen.started.values().filter(|runs| runs.len() > 1).count(),
    }
}

/// The daemon half of the server's process goes down when the server's process does,
/// in the same step, and starts again when it starts; returns whether the server is up
/// now. `was_up` is what the last call returned.
fn share_fate(sim: &mut Sim<CellNode>, was_up: bool) -> bool {
    let up = crate::check::server(sim).is_up();
    if up != was_up {
        for (_, node) in sim.nodes_mut() {
            if let CellNode::Daemon(daemon) = node {
                daemon.server_process(up);
            }
        }
    }
    up
}
