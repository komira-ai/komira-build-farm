//! Cordon and drain over a seed sweep: three workers that always heartbeat, actions
//! submitted at random, each running for a random time, and an operator who cordons,
//! drains (with a random deadline) and uncordons workers at random. The control log is
//! in process: a `Commit` is fed straight back.
//!
//! The checks, after every input of every seed:
//! - no lease is ever granted to a worker while it is cordoned;
//! - a drain never kills: every operation is granted at most once (no lease is given
//!   up and granted again) and answered exactly once, by its result;
//! - a cordon never refuses work: workers are always up and large enough, so work
//!   that only cordoned workers could run waits, however long, and is never refused
//!   (the sweep shows some waited longer than the unservable wait);
//! - a drain's state is true: `Drained` only while the worker holds no lease,
//!   `Draining` only before its deadline and while it holds one, `Paused` only at or
//!   after its deadline;
//! - once every worker is uncordoned at the end, all the work runs;
//! - a seed replays to the same trace.

use std::collections::BTreeMap;

use kbf_caps::NodeCaps;
use kbf_sched::{Cordon, DaemonInstance, Event, Input, Request, Scheduler, UNSERVABLE_WAIT};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, OperationId,
    Outcome, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const SEEDS: u64 = 64;
const GIB: u64 = 1 << 30;
const WORKERS: [&str; 3] = ["w0", "w1", "w2"];
/// Submissions and operator actions happen before this second.
const ACTIVE_UNTIL: u64 = 900;
const END: u64 = 1_500;

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

struct World {
    sched: Scheduler,
    rng: SimRng,
    now: u64,
    next: u64,
    /// Running leases: when each reports.
    running: BTreeMap<u64, Vec<LeaseGrant>>,
    grants: BTreeMap<OperationId, u32>,
    answers: BTreeMap<OperationId, u32>,
    submitted: u64,
    trace: Vec<String>,
    /// How often each drain state was seen, to show the sweep reaches them.
    seen: BTreeMap<&'static str, u64>,
    /// Operations told they wait for a cordon, and since when (seconds).
    cordon_wait: BTreeMap<OperationId, u64>,
    /// The longest any operation waited for a cordon before it was granted (seconds).
    longest_cordon_wait: u64,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut w = Self {
            sched: Scheduler::new(1),
            rng: SimRng::from_seed(seed),
            now: 0,
            next: 0,
            running: BTreeMap::new(),
            grants: BTreeMap::new(),
            answers: BTreeMap::new(),
            submitted: 0,
            trace: Vec::new(),
            seen: BTreeMap::new(),
            cordon_wait: BTreeMap::new(),
            longest_cordon_wait: 0,
        };
        for name in WORKERS {
            let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).unwrap();
            w.feed(Event::WorkerUp {
                worker: WorkerId::new(name),
                instance: DaemonInstance::new(name),
                capacity: Resources::new(4_000, 8 * GIB),
                caps,
            });
        }
        w
    }

    fn feed(&mut self, event: Event) {
        self.trace.push(format!("{} {event:?}", self.now));
        let now = FarmTime::from_millis(self.now * 1_000);
        let mut effects: Vec<Effect> = self.sched.apply(Input::new(now, event));
        while !effects.is_empty() {
            let mut more = Vec::new();
            for effect in effects {
                self.trace.push(format!("  {effect:?}"));
                more.extend(self.carry_out(now, effect));
            }
            effects = more;
            self.check_drains(now);
        }
        self.check_drains(now);
    }

    fn carry_out(&mut self, now: FarmTime, effect: Effect) -> Vec<Effect> {
        match effect {
            Effect::Commit(record) => {
                if let ControlRecord::Lease(grant) = &record {
                    let cordon = self.sched.cordon(&grant.worker);
                    assert_eq!(cordon, None, "a grant to cordoned {}", grant.worker);
                    *self.grants.entry(grant.operation).or_default() += 1;
                    if let Some(since) = self.cordon_wait.remove(&grant.operation) {
                        let waited = self.now - since;
                        self.longest_cordon_wait = self.longest_cordon_wait.max(waited);
                    }
                }
                self.sched.apply(Input::new(now, Event::Committed(record)))
            }
            Effect::Start(start) => {
                let grant = LeaseGrant {
                    lease: start.lease,
                    operation: start.operation,
                    worker: start.worker,
                };
                let ends = self.now + self.rng.between(5, 60);
                self.running.entry(ends).or_default().push(grant);
                Vec::new()
            }
            Effect::Answer(answer) => {
                *self.answers.entry(answer.operation).or_default() += 1;
                Vec::new()
            }
            Effect::Waiting(waiting) => {
                let reason = waiting.reason.as_deref().unwrap_or_default();
                assert!(
                    waiting.reason.is_none()
                        || reason.starts_with("every live worker that can run it is cordoned"),
                    "{waiting:?}"
                );
                if waiting.reason.is_some() {
                    self.cordon_wait
                        .entry(waiting.operation)
                        .or_insert(self.now);
                }
                Vec::new()
            }
            Effect::Refuse(refusal) => {
                panic!("workers are always up; a cordon refused {refusal:?}")
            }
        }
    }

    /// The drain states the scheduler reports are true.
    fn check_drains(&mut self, now: FarmTime) {
        for name in WORKERS {
            let worker = WorkerId::new(name);
            let leases = self.sched.leases_on(&worker);
            let state = match self.sched.cordon(&worker) {
                Some(Cordon::Drained) => {
                    assert!(leases.is_empty(), "{name} drained with {leases:?}");
                    "drained"
                }
                Some(Cordon::Draining { deadline }) => {
                    assert!(now < *deadline, "{name} draining past its deadline");
                    assert!(!leases.is_empty(), "{name} draining with no lease");
                    "draining"
                }
                Some(Cordon::Paused { deadline }) => {
                    assert!(now >= *deadline, "{name} paused before its deadline");
                    "paused"
                }
                Some(Cordon::Cordoned) => "cordoned",
                None => "serving",
            };
            *self.seen.entry(state).or_default() += 1;
        }
    }

    fn second(&mut self, active: bool) {
        for name in WORKERS {
            let worker = WorkerId::new(name);
            let running = self.sched.leases_on(&worker);
            self.feed(Event::Heartbeat { worker, running });
        }
        if active && self.rng.chance(Chance::percent(20)) {
            let n = self.next;
            self.next += 1;
            self.submitted += 1;
            let request = Request {
                key: ActionKey {
                    instance: "main".to_owned(),
                    action: digest(n),
                },
                qos: Qos::Ci,
                resources: Resources::new(1_000, GIB),
                hermetic: true,
                do_not_cache: false,
                needs: kbf_caps::Request::default(),
            };
            self.feed(Event::Submit {
                waiter: WaiterId(n),
                request,
            });
        }
        if active && self.rng.chance(Chance::percent(4)) {
            let pick = usize::try_from(self.rng.below(3)).expect("small");
            let worker = WorkerId::new(WORKERS[pick]);
            let event = match self.rng.below(3) {
                0 => Event::Cordon { worker },
                1 => {
                    let deadline = self.now + self.rng.between(1, 90);
                    Event::Drain {
                        worker,
                        deadline: FarmTime::from_millis(deadline * 1_000),
                    }
                }
                _ => Event::Uncordon { worker },
            };
            self.feed(event);
        }
        if let Some(done) = self.running.remove(&self.now) {
            for grant in done {
                self.feed(Event::Report {
                    operation: grant.operation,
                    lease: grant.lease,
                    outcome: Outcome::Completed {
                        action_result: digest(1_000_000 + grant.operation.0),
                    },
                });
            }
        }
        self.feed(Event::Tick);
    }
}

fn run(seed: u64) -> World {
    let mut w = World::new(seed);
    while w.now < END {
        if w.now == ACTIVE_UNTIL {
            for name in WORKERS {
                w.feed(Event::Uncordon {
                    worker: WorkerId::new(name),
                });
            }
        }
        w.second(w.now < ACTIVE_UNTIL);
        w.now += 1;
    }
    w
}

/// Catches: cordon ignored in placement, a drain that gives up or requeues a lease
/// (a second grant of one operation), a drain state that lies about the leases or the
/// deadline, an uncordon that leaves work stuck, and work refused because only
/// cordoned workers could run it.
#[test]
fn cordon_and_drain_hold_over_seeds() {
    let mut seen: BTreeMap<&str, u64> = BTreeMap::new();
    let mut longest_cordon_wait = 0;
    for seed in 0..SEEDS {
        let w = run(seed);
        longest_cordon_wait = longest_cordon_wait.max(w.longest_cordon_wait);
        for (state, n) in &w.seen {
            *seen.entry(state).or_default() += n;
        }
        assert_eq!(
            w.answers.len() as u64,
            w.submitted,
            "seed {seed}: all answered"
        );
        assert!(
            w.answers.values().all(|&n| n == 1),
            "seed {seed}: answered once"
        );
        assert!(
            w.grants.values().all(|&n| n == 1),
            "seed {seed}: granted once"
        );
    }
    // The sweep reaches every state, so each check above was exercised.
    for state in ["serving", "cordoned", "draining", "drained", "paused"] {
        assert!(
            seen.get(state).is_some_and(|&n| n > 0),
            "{state} never seen: {seen:?}"
        );
    }
    // Some work waited for a cordon past the unservable wait, and still ran.
    assert!(
        longest_cordon_wait > UNSERVABLE_WAIT.as_secs(),
        "longest cordon wait {longest_cordon_wait} s"
    );
}

/// Catches: placement or drain progress that depends on anything but the seed.
#[test]
fn a_seed_replays() {
    assert_eq!(run(7).trace, run(7).trace);
}
