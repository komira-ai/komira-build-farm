//! The daemon model: streams, heartbeats, the self-fence T, the `Start` window W,
//! results kept until acknowledged, `Cancel`, and the faults a machine has (death,
//! suspend, reconnects, a changed node report).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_sched::fence::{SELF_FENCE, START_VALIDITY};
use kbf_sim::{Chance, Event, NodeInput, Output, SimRng};
use kbf_types::{
    Digest, Effect, Failure, FarmTime, LeaseId, OperationId, Outcome, Resources, StartLease,
    WorkerId,
};

use super::{GIB, HEARTBEAT, Msg, leader_id, result_digest};

/// How long a daemon waits for `Welcome` before it opens another stream.
const WELCOME_WAIT: Duration = Duration::from_secs(10);
/// How long after a stream ends the daemon opens the next.
const RECONNECT_AFTER: Duration = Duration::from_secs(1);
/// How long a run takes, in seconds (`lo..=hi`), drawn per run.
const RUN_SECS: (u64, u64) = (10, 90);

const T_HEARTBEAT: u64 = 0;
const T_BOOT: u64 = 1;
const T_DIE: u64 = 2;
const T_FENCE: u64 = 3;
const T_RECONNECT: u64 = 4;
const T_FREEZE: u64 = 10_000;
const T_RESUME: u64 = 20_000;
const T_SCRIPTED_RECONNECT: u64 = 30_000;
const T_REPORT_CHANGE: u64 = 40_000;
const T_WELCOME: u64 = 1 << 20;
const T_RUN: u64 = 1 << 40;

/// What one worker does, besides following the daemon's rules.
#[derive(Clone, Debug)]
pub struct WorkerPlan {
    /// The simulated node.
    pub name: &'static str,
    /// The node id its `Hello` claims (its own name, unless two daemons share one).
    pub node: &'static str,
    pub cores: u64,
    /// When the daemon starts (ms).
    pub boot_at: u64,
    /// When the machine dies for good (ms after boot).
    pub die_at: Option<u64>,
    /// The machine dies for good right after it sends the result of this many runs.
    pub die_after_results: Option<u64>,
    /// Suspends: (when, for how long), in ms after boot.
    pub freezes: Vec<(u64, u64)>,
    /// When the daemon drops its stream and opens a new one (ms after boot).
    pub reconnects: Vec<u64>,
    /// When the node report changes and `Hello` is resent on the stream (ms after boot).
    pub report_changes: Vec<u64>,
    /// Leases listed in every heartbeat that this daemon does not hold.
    pub phantoms: Vec<LeaseId>,
    /// The chance a `ResultAck` is lost on its way in.
    pub lose_acks: Chance,
    /// The chance a `Result` is sent twice.
    pub repeat_reports: Chance,
    /// Whether the `Start` window W is enforced. Off only to show the sim catches its
    /// absence (a mutant of the model itself).
    pub start_window: bool,
}

impl WorkerPlan {
    /// A worker with `cores` cores and nothing unusual.
    #[must_use]
    pub fn plain(name: &'static str, cores: u64) -> Self {
        Self {
            name,
            node: name,
            cores,
            boot_at: 0,
            die_at: None,
            die_after_results: None,
            freezes: Vec::new(),
            reconnects: Vec::new(),
            report_changes: Vec::new(),
            phantoms: Vec::new(),
            lose_acks: Chance::never(),
            repeat_reports: Chance::never(),
            start_window: true,
        }
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Finished,
    Fenced,
    Cancelled,
    Died,
}

/// One run of a lease on this worker.
#[derive(Clone, Debug)]
pub struct Run {
    pub lease: LeaseId,
    /// The leader incarnation and operation the `Start` came from (for the checks).
    pub incarnation: u64,
    pub operation: OperationId,
    pub action: Digest,
    /// When it executed: closed spans; a frozen machine executes nothing.
    pub spans: Vec<(FarmTime, FarmTime)>,
    open: Option<FarmTime>,
    left: Duration,
    generation: u64,
    pub end: Option<(FarmTime, End)>,
}

impl Run {
    fn close(&mut self, now: FarmTime) {
        if let Some(from) = self.open.take() {
            self.left = self
                .left
                .saturating_sub(now.saturating_duration_since(from));
            self.spans.push((from, now));
        }
    }
}

/// What the daemon did with a `Start` it received on its current stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Took {
    Ran,
    /// A resend for a lease already running.
    Resend,
    /// Its lease's result is unacknowledged.
    Unacked,
    /// Arrived W or more after the heartbeat it names was sent, or names one forgotten.
    Late,
    /// Contact was already lost: answered UNAVAILABLE.
    Unavailable,
}

/// A `Start` the daemon received on its current stream.
#[derive(Clone, Debug)]
pub struct StartSeen {
    pub at: FarmTime,
    pub lease: LeaseId,
    pub incarnation: u64,
    pub took: Took,
}

/// Counts of what happened, for the scenarios' reach checks.
#[derive(Clone, Debug, Default)]
pub struct WorkerStats {
    pub fenced: u64,
    pub cancelled: u64,
    pub goodbyes: u64,
    pub streams: u64,
    pub lost_acks: u64,
    /// Results the server refused (not the holder of the current lease).
    pub refused: u64,
    pub resent_hellos: u64,
    /// Fences done at a resume, before anything else.
    pub fenced_on_resume: u64,
}

pub struct Worker {
    pub plan: WorkerPlan,
    node: WorkerId,
    capacity: Resources,
    stream: u64,
    welcomed: bool,
    hello_sent: FarmTime,
    next_beat: u64,
    /// Send times a `Start` on this stream may name (0 is the `Hello`).
    window: BTreeMap<u64, FarmTime>,
    /// Send times of heartbeats not yet acknowledged.
    unacked_beats: BTreeMap<u64, FarmTime>,
    /// The send time of the newest acknowledged message.
    confirmed: Option<FarmTime>,
    /// Every run, in start order.
    pub runs: Vec<Run>,
    /// Runs executing (or frozen) now, by lease.
    live: BTreeMap<LeaseId, usize>,
    /// Results kept until acknowledged.
    pub unacked: BTreeMap<LeaseId, Outcome>,
    pub starts: Vec<StartSeen>,
    timers: BTreeMap<u64, (usize, u64)>,
    next_timer: u64,
    pub dead: bool,
    frozen: bool,
    beat_due: bool,
    buffered: Vec<(Event<Msg>, u64)>,
    pub stats: WorkerStats,
    out: Vec<Output<Msg>>,
}

impl Worker {
    #[must_use]
    pub fn new(plan: WorkerPlan) -> Self {
        Self {
            node: WorkerId::new(plan.node),
            capacity: Resources::new(plan.cores * 1_000, plan.cores * 4 * GIB),
            plan,
            stream: 0,
            welcomed: false,
            hello_sent: FarmTime::default(),
            next_beat: 0,
            window: BTreeMap::new(),
            unacked_beats: BTreeMap::new(),
            confirmed: None,
            runs: Vec::new(),
            live: BTreeMap::new(),
            unacked: BTreeMap::new(),
            starts: Vec::new(),
            timers: BTreeMap::new(),
            next_timer: T_RUN,
            dead: false,
            frozen: false,
            beat_due: false,
            buffered: Vec::new(),
            stats: WorkerStats::default(),
            out: Vec::new(),
        }
    }

    pub fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    pub fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        if self.dead {
            return Vec::new();
        }
        if self.frozen {
            match input.event {
                Event::Timer { tag } if (T_RESUME..T_SCRIPTED_RECONNECT).contains(&tag) => {
                    self.resume(now);
                }
                // The heartbeat loop is due again once the machine runs.
                Event::Timer { tag: T_HEARTBEAT } => self.beat_due = true,
                // Runs do not advance while frozen: their timers are re-armed on resume.
                Event::Timer { tag } if tag >= T_RUN => {}
                event => self.buffered.push((event, input.entropy)),
            }
            return Vec::new();
        }
        self.handle(now, input.event, input.entropy);
        Vec::new()
    }

    fn handle(&mut self, now: FarmTime, event: Event<Msg>, entropy: u64) {
        // Whatever wakes the daemon, the fence is checked first.
        self.fence(now);
        let mut rng = SimRng::from_seed(entropy);
        match event {
            Event::Start => {
                self.timer(Duration::from_millis(self.plan.boot_at), T_BOOT);
            }
            Event::Timer { tag: T_BOOT } => self.boot(now),
            Event::Timer { tag: T_HEARTBEAT } => {
                self.beat(now);
                self.timer(HEARTBEAT, T_HEARTBEAT);
            }
            Event::Timer { tag: T_DIE } => self.die(now),
            Event::Timer { tag: T_FENCE } => {}
            Event::Timer { tag: T_RECONNECT } => self.connect(now),
            Event::Timer { tag } if (T_FREEZE..T_RESUME).contains(&tag) => {
                let i = usize::try_from(tag - T_FREEZE).expect("small");
                self.freeze(now, i);
            }
            Event::Timer { tag } if (T_SCRIPTED_RECONNECT..T_REPORT_CHANGE).contains(&tag) => {
                self.connect(now);
            }
            Event::Timer { tag } if (T_REPORT_CHANGE..T_WELCOME).contains(&tag) => {
                if self.stream > 0 {
                    self.stats.resent_hellos += 1;
                    self.hello();
                }
            }
            Event::Timer { tag } if (T_WELCOME..T_RUN).contains(&tag) => {
                if tag - T_WELCOME == self.stream && !self.welcomed {
                    self.connect(now);
                }
            }
            Event::Timer { tag } => {
                let (i, generation) = self.timers.remove(&tag).expect("run timers name runs");
                if self.runs[i].generation == generation && self.runs[i].open.is_some() {
                    self.finish(now, i, &mut rng);
                }
            }
            Event::Message { msg, .. } => self.receive(now, msg, &mut rng),
        }
    }

    fn receive(&mut self, now: FarmTime, msg: Msg, rng: &mut SimRng) {
        match msg {
            Msg::Welcome { stream } => {
                if stream == self.stream && !self.welcomed {
                    self.welcomed = true;
                    self.confirm(now, self.hello_sent);
                    let kept: Vec<(LeaseId, Outcome)> =
                        self.unacked.iter().map(|(l, o)| (*l, *o)).collect();
                    for (lease, outcome) in kept {
                        self.report(lease, outcome, rng);
                    }
                    self.beat(now);
                }
            }
            Msg::HeartbeatAck { stream, seq } => {
                if stream == self.stream
                    && let Some(&sent) = self.unacked_beats.get(&seq)
                {
                    self.unacked_beats.retain(|&s, _| s > seq);
                    self.window.retain(|&s, _| s >= seq);
                    self.confirm(now, sent);
                }
            }
            Msg::Start {
                stream,
                start,
                heartbeat_seq,
                incarnation,
            } => {
                // A `Start` on a stream that is gone went down with it.
                if stream == self.stream && self.welcomed {
                    self.start(now, start, heartbeat_seq, incarnation, rng);
                }
            }
            Msg::Cancel { stream, lease } => {
                if stream == self.stream
                    && let Some(i) = self.live.remove(&lease)
                {
                    self.stats.cancelled += 1;
                    self.stop(now, i, End::Cancelled, rng);
                }
            }
            Msg::ResultAck { lease, accepted } => {
                if rng.chance(self.plan.lose_acks) {
                    self.stats.lost_acks += 1;
                } else if self.unacked.remove(&lease).is_some() && !accepted {
                    self.stats.refused += 1;
                }
            }
            Msg::Goodbye { stream } => {
                if stream == self.stream && self.stream > 0 {
                    self.stats.goodbyes += 1;
                    self.welcomed = false;
                    self.timer(RECONNECT_AFTER, T_RECONNECT);
                }
            }
            other => panic!("worker {} got {other:?}", self.plan.name),
        }
    }

    fn boot(&mut self, now: FarmTime) {
        let ms = Duration::from_millis;
        if let Some(at) = self.plan.die_at {
            self.timer(ms(at), T_DIE);
        }
        let freezes: Vec<u64> = self.plan.freezes.iter().map(|f| f.0).collect();
        for (i, at) in freezes.into_iter().enumerate() {
            self.timer(ms(at), T_FREEZE + i as u64);
        }
        for (i, at) in self.plan.reconnects.clone().into_iter().enumerate() {
            self.timer(ms(at), T_SCRIPTED_RECONNECT + i as u64);
        }
        for (i, at) in self.plan.report_changes.clone().into_iter().enumerate() {
            self.timer(ms(at), T_REPORT_CHANGE + i as u64);
        }
        self.connect(now);
        self.timer(HEARTBEAT, T_HEARTBEAT);
    }

    /// Opens a new stream: its `Hello` goes out, and nothing named on the old one counts.
    fn connect(&mut self, now: FarmTime) {
        self.stream += 1;
        self.stats.streams += 1;
        self.welcomed = false;
        self.next_beat = 0;
        self.hello_sent = now;
        self.window.clear();
        self.window.insert(0, now);
        self.unacked_beats.clear();
        self.hello();
        self.timer(WELCOME_WAIT, T_WELCOME + self.stream);
    }

    fn hello(&mut self) {
        let msg = Msg::Hello {
            node: self.node.clone(),
            // One daemon process per simulated node: its own name is its instance id.
            instance: self.plan.name,
            stream: self.stream,
            capacity: self.capacity,
        };
        self.send(msg);
    }

    /// One heartbeat, listing every lease held (running, or with a result not yet
    /// acknowledged), and every kept result resent.
    fn beat(&mut self, now: FarmTime) {
        if !self.welcomed {
            return;
        }
        self.next_beat += 1;
        let seq = self.next_beat;
        self.window.insert(seq, now);
        self.unacked_beats.insert(seq, now);
        let mut running: BTreeSet<LeaseId> = self.live.keys().copied().collect();
        running.extend(self.unacked.keys().copied());
        running.extend(self.plan.phantoms.iter().copied());
        self.send(Msg::Heartbeat {
            stream: self.stream,
            seq,
            running: running.into_iter().collect(),
        });
        let kept: Vec<(LeaseId, Outcome)> = self.unacked.iter().map(|(l, o)| (*l, *o)).collect();
        for (lease, outcome) in kept {
            self.send(Msg::Report {
                stream: self.stream,
                lease,
                outcome,
            });
        }
    }

    fn start(
        &mut self,
        now: FarmTime,
        start: StartLease,
        heartbeat_seq: u64,
        incarnation: u64,
        rng: &mut SimRng,
    ) {
        let lease = start.lease;
        let in_window = self
            .window
            .get(&heartbeat_seq)
            .is_some_and(|&sent| now.saturating_duration_since(sent) < START_VALIDITY);
        let took = if self.unacked.contains_key(&lease) {
            Took::Unacked
        } else if self.plan.start_window && !in_window {
            Took::Late
        } else if self.lost(now) {
            Took::Unavailable
        } else if self.live.contains_key(&lease) {
            Took::Resend
        } else {
            Took::Ran
        };
        self.starts.push(StartSeen {
            at: now,
            lease,
            incarnation,
            took,
        });
        match took {
            Took::Unavailable => {
                let outcome = Outcome::Failed(Failure::Infra);
                self.unacked.insert(lease, outcome);
                self.report(lease, outcome, rng);
            }
            Took::Ran => {
                let secs = rng.between(RUN_SECS.0, RUN_SECS.1);
                let i = self.runs.len();
                self.runs.push(Run {
                    lease,
                    incarnation,
                    operation: start.operation,
                    action: start.key.action,
                    spans: Vec::new(),
                    open: Some(now),
                    left: Duration::from_secs(secs),
                    generation: 0,
                    end: None,
                });
                self.live.insert(lease, i);
                self.arm(i);
                if let Some(d) = self.deadline() {
                    self.timer(d.saturating_duration_since(now), T_FENCE);
                }
            }
            Took::Resend | Took::Unacked | Took::Late => {}
        }
    }

    fn arm(&mut self, i: usize) {
        let tag = self.next_timer;
        self.next_timer += 1;
        self.timers.insert(tag, (i, self.runs[i].generation));
        let left = self.runs[i].left;
        self.timer(left, tag);
    }

    fn finish(&mut self, now: FarmTime, i: usize, rng: &mut SimRng) {
        let lease = self.runs[i].lease;
        self.live.remove(&lease);
        self.stop(now, i, End::Finished, rng);
        let finished = self
            .runs
            .iter()
            .filter(|r| r.end.is_some_and(|e| e.1 == End::Finished));
        if self.plan.die_after_results == Some(finished.count() as u64) {
            self.die(now);
        }
    }

    /// Ends run `i` (already out of `live`) and reports it: its result if it finished,
    /// else `ABORTED`.
    fn stop(&mut self, now: FarmTime, i: usize, end: End, rng: &mut SimRng) {
        let run = &mut self.runs[i];
        run.close(now);
        run.end = Some((now, end));
        let lease = run.lease;
        let outcome = match end {
            End::Finished => Outcome::Completed {
                action_result: result_digest(&run.action, lease),
            },
            End::Fenced | End::Cancelled | End::Died => Outcome::Failed(Failure::Infra),
        };
        if end != End::Died {
            self.unacked.insert(lease, outcome);
            self.report(lease, outcome, rng);
        }
    }

    fn report(&mut self, lease: LeaseId, outcome: Outcome, rng: &mut SimRng) {
        // A Result goes out on a stream; without one, after the next Welcome.
        if !self.welcomed {
            return;
        }
        let msg = Msg::Report {
            stream: self.stream,
            lease,
            outcome,
        };
        if rng.chance(self.plan.repeat_reports) {
            self.send(msg.clone());
        }
        self.send(msg);
    }

    fn confirm(&mut self, now: FarmTime, sent: FarmTime) {
        if self.confirmed.is_none_or(|c| sent > c) {
            self.confirmed = Some(sent);
        }
        if let Some(d) = self.deadline()
            && !self.live.is_empty()
        {
            self.timer(d.saturating_duration_since(now), T_FENCE);
        }
    }

    fn deadline(&self) -> Option<FarmTime> {
        self.confirmed.map(|c| c.saturating_add(SELF_FENCE))
    }

    fn lost(&self, now: FarmTime) -> bool {
        self.deadline().is_some_and(|d| now >= d)
    }

    /// Kills every live run once contact is lost: T after the newest acknowledged send.
    fn fence(&mut self, now: FarmTime) {
        if !self.lost(now) || self.live.is_empty() {
            return;
        }
        let live = std::mem::take(&mut self.live);
        let mut rng = SimRng::from_seed(now.as_millis());
        for i in live.into_values() {
            self.stats.fenced += 1;
            self.stop(now, i, End::Fenced, &mut rng);
        }
    }

    fn die(&mut self, now: FarmTime) {
        self.dead = true;
        let live = std::mem::take(&mut self.live);
        let mut rng = SimRng::from_seed(0);
        for i in live.into_values() {
            self.stop(now, i, End::Died, &mut rng);
        }
    }

    fn freeze(&mut self, now: FarmTime, i: usize) {
        self.frozen = true;
        for &r in self.live.values() {
            self.runs[r].close(now);
            self.runs[r].generation += 1;
        }
        let dur = Duration::from_millis(self.plan.freezes[i].1);
        self.timer(dur, T_RESUME + i as u64);
    }

    /// The machine runs again: the daemon fences first (its clock counts the suspend),
    /// then the runs still allowed go on, then what arrived meanwhile is handled.
    fn resume(&mut self, now: FarmTime) {
        self.frozen = false;
        let before = self.stats.fenced;
        self.fence(now);
        self.stats.fenced_on_resume += self.stats.fenced - before;
        let live: Vec<usize> = self.live.values().copied().collect();
        for i in live {
            self.runs[i].open = Some(now);
            self.arm(i);
        }
        if std::mem::take(&mut self.beat_due) {
            self.beat(now);
            self.timer(HEARTBEAT, T_HEARTBEAT);
        }
        for (event, entropy) in std::mem::take(&mut self.buffered) {
            self.handle(now, event, entropy);
        }
    }

    fn timer(&mut self, after: Duration, tag: u64) {
        self.out.push(Output::Timer { after, tag });
    }

    fn send(&mut self, msg: Msg) {
        self.out.push(Output::Send {
            to: leader_id(),
            msg,
        });
    }
}
