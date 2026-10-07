//! Platform-aware placement: an action goes only to a worker whose node report
//! satisfies its platform; work no live worker can run waits with a reason and is
//! refused after the unservable wait, the refusal committed before it is answered.

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{Event, Input, OpState, PLACEMENT_ROUND, Request, Scheduler, UNSERVABLE_WAIT};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, OperationId,
    Qos, Refusal, RefusalRecord, Resources, StateMachine, WaiterId, Waiting, WorkerId,
};

const GIB: u64 = 1 << 30;
/// The unservable wait these tests run with.
const WAIT: Duration = Duration::from_secs(30);

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

/// Action `n`, one core and 1 GiB, needing what the REAPI `platform` asks for.
fn request(n: u64, platform: &[(&str, &str)]) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform(platform.iter().copied()).unwrap(),
    }
}

fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).unwrap()
}

fn mac() -> NodeCaps {
    NodeCaps::from_report([("arch", "arm64"), ("os", "macos")]).unwrap()
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn waiting(op: u64, reason: Option<&str>) -> Effect {
    Effect::Waiting(Waiting {
        operation: OperationId(op),
        reason: reason.map(str::to_owned),
    })
}

/// A scheduler for term 1 with a short unservable wait and a clock the test moves.
struct Harness {
    s: Scheduler,
    now: FarmTime,
}

impl Harness {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1).with_unservable_wait(WAIT),
            now: FarmTime::default(),
        }
    }

    fn at_secs(&mut self, secs: u64) -> &mut Self {
        self.now = FarmTime::from_millis(secs * 1_000);
        self
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s.apply(Input::new(self.now, event))
    }

    /// Registers `name` with 4 cores, 8 GiB and `caps`.
    fn worker(&mut self, name: &str, caps: NodeCaps) {
        let event = Event::WorkerUp {
            worker: w(name),
            capacity: Resources::new(4_000, 8 * GIB),
            caps,
        };
        assert!(self.feed(event).is_empty());
    }

    fn submit(&mut self, waiter: u64, request: Request) -> Vec<Effect> {
        self.feed(Event::Submit {
            waiter: WaiterId(waiter),
            request,
        })
    }

    fn tick(&mut self) -> Vec<Effect> {
        self.feed(Event::Tick)
    }

    /// Ticks, and splits what it proposed into grants and everything else.
    fn grants(&mut self) -> (Vec<LeaseGrant>, Vec<Effect>) {
        let mut grants = Vec::new();
        let mut rest = Vec::new();
        for effect in self.tick() {
            match effect {
                Effect::Commit(ControlRecord::Lease(g)) => grants.push(g),
                other => rest.push(other),
            }
        }
        (grants, rest)
    }
}

/// Catches: placement that ignores the platform, so a Linux action lands on a Mac
/// (the first worker by name, with room) and a Mac action on a Linux machine.
#[test]
fn an_action_goes_only_to_a_worker_that_satisfies_its_platform() {
    let mut h = Harness::new();
    h.worker("a-mac", mac());
    h.worker("b-linux", linux());
    h.submit(1, request(1, &[("OSFamily", "linux")]));
    h.submit(2, request(2, &[("OSFamily", "Darwin")]));
    h.submit(3, request(3, &[("ISA", "x86-64")]));
    h.submit(4, request(4, &[]));
    let (grants, rest) = h.grants();
    assert!(rest.is_empty(), "{rest:?}");
    let placed: Vec<(u64, &str)> = grants
        .iter()
        .map(|g| (g.operation.0, g.worker.as_str()))
        .collect();
    assert_eq!(
        placed,
        [(0, "b-linux"), (1, "a-mac"), (2, "b-linux"), (3, "a-mac")]
    );
}

/// Catches: an action no live worker satisfies run elsewhere, left queued forever
/// without a word, refused before the wait is up, or answered before its refusal is
/// committed. Also: a refused key left in the in-flight table, which would join the
/// next caller to a dead operation.
#[test]
fn work_no_worker_satisfies_waits_with_a_reason_then_is_refused() {
    let mut h = Harness::new();
    h.worker("linux", linux());
    assert!(h.submit(1, request(1, &[("OSFamily", "macos")])).is_empty());
    let (grants, rest) = h.grants();
    assert!(grants.is_empty());
    let reason = "none of the 1 live worker(s) satisfies the action's platform; the \
                  closest, linux, lacks os=macos";
    assert_eq!(rest, [waiting(0, Some(reason))]);
    assert_eq!(h.s.waiting(OperationId(0)), Some(reason));

    // Unchanged, it says nothing more until the wait is up.
    assert!(h.at_secs(WAIT.as_secs() - 1).tick().is_empty());
    let proposed = h.at_secs(WAIT.as_secs()).tick();
    let [Effect::Commit(ControlRecord::Refusal(record))] = proposed.as_slice() else {
        panic!("{proposed:?}");
    };
    assert_eq!(record.operation, OperationId(0));
    assert!(record.reason.starts_with(reason), "{}", record.reason);
    assert!(record.reason.contains("waited 30 s"), "{}", record.reason);
    assert_eq!(h.s.state(OperationId(0)), Some(&OpState::Queued));
    assert_eq!(
        h.s.queued().count(),
        0,
        "a proposed refusal leaves the queue"
    );

    // A worker that could run it now arrives before the refusal commits: too late.
    h.worker("mac", mac());
    assert!(h.tick().is_empty());
    let answered = h.feed(Event::Committed(ControlRecord::Refusal(record.clone())));
    assert_eq!(
        answered,
        [Effect::Refuse(Refusal {
            operation: OperationId(0),
            waiters: vec![WaiterId(1)],
            reason: record.reason.clone(),
        })]
    );
    assert_eq!(
        h.s.state(OperationId(0)),
        Some(&OpState::Refused {
            reason: record.reason.clone()
        })
    );
    assert!(h.s.state(OperationId(0)).unwrap().is_done());
    assert_eq!(h.s.waiting(OperationId(0)), None);
    // Committed again (a duplicate in the log), it is stale.
    assert!(
        h.feed(Event::Committed(ControlRecord::Refusal(record.clone())))
            .is_empty()
    );

    // The same action again is a new operation, and runs on the Mac.
    assert!(h.submit(2, request(1, &[("OSFamily", "macos")])).is_empty());
    let (grants, _) = h.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(
        (grants[0].operation, grants[0].worker.as_str()),
        (OperationId(1), "mac")
    );
}

/// Catches: a wait that never ends once a worker that can run it arrives (no grant,
/// or callers still told it cannot run), and a wait that does not restart from the
/// moment it begins again, so work is refused on time spent running.
#[test]
fn a_worker_that_can_run_it_ends_the_wait_and_the_wait_restarts() {
    let mut h = Harness::new();
    h.submit(1, request(1, &[("OSFamily", "macos")]));
    assert_eq!(h.tick(), [waiting(0, Some("no worker is connected"))]);

    // The reason changes as workers arrive, and the wait still counts from the start.
    h.at_secs(10).worker("linux", linux());
    let (_, rest) = h.grants();
    let [
        Effect::Waiting(Waiting {
            reason: Some(reason),
            ..
        }),
    ] = rest.as_slice()
    else {
        panic!("{rest:?}");
    };
    assert!(reason.contains("lacks os=macos"), "{reason}");

    h.at_secs(20).worker("mac", mac());
    let (grants, rest) = h.grants();
    assert_eq!(rest, [waiting(0, None)]);
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].worker, w("mac"));
    assert_eq!(h.s.waiting(OperationId(0)), None);

    // The Mac goes silent; after G its lease is given up and the operation queued
    // again, with only Linux live. It waits the whole bound again from there.
    h.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));
    let heard_linux = |h: &mut Harness| {
        h.feed(Event::Heartbeat {
            worker: w("linux"),
            running: Vec::new(),
        })
    };
    h.at_secs(79);
    heard_linux(&mut h);
    let gone = h.at_secs(80).tick();
    assert!(
        matches!(
            gone.as_slice(),
            [Effect::Waiting(Waiting {
                reason: Some(_),
                ..
            })]
        ),
        "{gone:?}"
    );
    h.at_secs(80 + WAIT.as_secs() - 1);
    heard_linux(&mut h);
    assert!(h.tick().is_empty());
    let refused = h.at_secs(80 + WAIT.as_secs()).tick();
    assert!(
        matches!(
            refused.as_slice(),
            [Effect::Commit(ControlRecord::Refusal(_))]
        ),
        "{refused:?}"
    );
}

/// Catches: work larger than every worker that satisfies its platform (two GPUs, the
/// nodes have one) left queued forever, as if it only waited for room.
#[test]
fn work_larger_than_every_matching_worker_is_unservable() {
    let mut h = Harness::new();
    let event = Event::WorkerUp {
        worker: w("gpu"),
        capacity: Resources::new(4_000, 8 * GIB).with_gpus(1),
        caps: linux(),
    };
    h.feed(event);
    let big = Request {
        resources: Resources::new(1_000, GIB).with_gpus(2),
        ..request(1, &[("OSFamily", "linux")])
    };
    h.submit(1, big);
    let reason = "the 1 live worker(s) that satisfy the action's platform are all smaller \
                  than its request (1000 millicores, 1073741824 bytes of memory, 2 GPU(s))";
    assert_eq!(h.tick(), [waiting(0, Some(reason))]);

    // Work that only waits for room says nothing.
    let fill = Request {
        resources: Resources::new(4_000, GIB),
        ..request(2, &[])
    };
    h.submit(2, fill);
    h.submit(3, request(3, &[]));
    let (grants, rest) = h.grants();
    assert_eq!((grants.len(), rest.as_slice()), (1, &[][..]));
    assert_eq!(h.s.waiting(OperationId(2)), None);
}

/// Catches: a caller that joins a waiting twin and is never told why it waits.
#[test]
fn a_caller_joining_a_waiting_twin_is_told_why() {
    let mut h = Harness::new();
    h.submit(1, request(1, &[("OSFamily", "macos")]));
    h.tick();
    assert_eq!(
        h.submit(2, request(1, &[("OSFamily", "macos")])),
        [waiting(0, Some("no worker is connected"))]
    );
    assert_eq!(
        h.s.waiters(OperationId(0)),
        Some(&[WaiterId(1), WaiterId(2)][..])
    );
    // A joiner of a twin that can run says nothing.
    h.worker("mac", mac());
    h.tick();
    assert!(h.submit(3, request(1, &[("OSFamily", "macos")])).is_empty());
}

/// Catches: a refusal applied to an operation that is not queued (granted since, or
/// unknown), which would answer callers while a lease runs it.
#[test]
fn a_stale_refusal_is_dropped() {
    let mut h = Harness::new();
    h.worker("linux", linux());
    h.submit(1, request(1, &[]));
    let (grants, _) = h.grants();
    let refusal = |op| {
        Event::Committed(ControlRecord::Refusal(RefusalRecord {
            operation: OperationId(op),
            reason: "late".to_owned(),
        }))
    };
    assert!(h.feed(refusal(0)).is_empty());
    assert!(matches!(
        h.s.state(OperationId(0)),
        Some(OpState::Leased { .. })
    ));
    assert!(h.feed(refusal(9)).is_empty());
    assert_eq!(grants.len(), 1);

    // A networked operation is not in the in-flight table; refusing it is clean too.
    let networked = Request {
        hermetic: false,
        ..request(2, &[("OSFamily", "macos")])
    };
    h.submit(2, networked);
    h.tick();
    assert_eq!(h.at_secs(WAIT.as_secs()).tick().len(), 1);
    let answered = h.feed(refusal(1));
    assert!(
        matches!(answered.as_slice(), [Effect::Refuse(_)]),
        "{answered:?}"
    );
}

/// Catches: capabilities read only at registration, so a node whose report changes
/// (a label added) keeps being matched on its old report.
#[test]
fn a_resent_report_changes_what_the_worker_matches() {
    let mut h = Harness::new();
    h.worker("mac", mac());
    h.submit(1, request(1, &[("label.pool", "darwin")]));
    assert_eq!(h.tick().len(), 1, "waits with a reason");
    let labelled =
        NodeCaps::from_report([("arch", "arm64"), ("os", "macos"), ("label.pool", "darwin")])
            .unwrap();
    let resend = Event::Capacity {
        worker: w("mac"),
        capacity: Resources::new(4_000, 8 * GIB),
        caps: labelled,
    };
    assert!(h.feed(resend).is_empty());
    let (grants, rest) = h.grants();
    assert_eq!((grants.len(), rest), (1, vec![waiting(0, None)]));
}

/// Catches: the check for unservable work stopping with the grant limit, so an
/// action behind a full round never starts its wait and is never refused.
#[test]
fn work_behind_a_full_round_still_starts_its_wait() {
    let mut h = Harness::new();
    let event = Event::WorkerUp {
        worker: w("linux"),
        capacity: Resources::new(1_000_000, 1_000 * GIB),
        caps: linux(),
    };
    h.feed(event);
    let n = PLACEMENT_ROUND as u64 + 2;
    for i in 0..n {
        h.submit(i, request(i, &[("OSFamily", "linux")]));
    }
    h.submit(n, request(n, &[("OSFamily", "macos")]));
    let (grants, rest) = h.grants();
    assert_eq!(grants.len(), PLACEMENT_ROUND);
    assert!(
        matches!(rest.as_slice(), [Effect::Waiting(Waiting { operation, reason: Some(_) })] if operation.0 == n),
        "{rest:?}"
    );
}

/// Catches: a default wait so short that work submitted while daemons reconnect after
/// a server restart is refused, or one so long a build hangs for an hour.
#[test]
fn the_default_wait_is_minutes() {
    assert_eq!(UNSERVABLE_WAIT, Duration::from_secs(300));
    let mut s = Scheduler::new(1);
    let at = |secs: u64| FarmTime::from_millis(secs * 1_000);
    let submit = Event::Submit {
        waiter: WaiterId(1),
        request: request(1, &[]),
    };
    s.apply(Input::new(at(0), submit));
    s.apply(Input::new(at(0), Event::Tick));
    assert!(s.apply(Input::new(at(299), Event::Tick)).is_empty());
    assert_eq!(s.apply(Input::new(at(300), Event::Tick)).len(), 1);
}

/// The scheduler's lease grace G: a worker not heard from for this long is not live.
const GRACE: u64 = 60;

impl Harness {
    /// Hears from `worker` at `secs`, then ticks.
    fn tick_hearing(&mut self, secs: u64, worker: &str) -> Vec<Effect> {
        self.at_secs(secs);
        self.feed(Event::Heartbeat {
            worker: w(worker),
            running: Vec::new(),
        });
        self.tick()
    }

    /// Ticks every second of `secs`, hearing from `worker` each time, and checks that
    /// nothing is refused: each operation of `ops` is still queued after every tick.
    fn wait_through(&mut self, secs: std::ops::Range<u64>, worker: &str, ops: &[u64]) {
        for t in secs {
            let effects = self.tick_hearing(t, worker);
            assert!(effects.is_empty(), "t={t}: {effects:?}");
            for &op in ops {
                assert_eq!(
                    self.s.state(OperationId(op)),
                    Some(&OpState::Queued),
                    "t={t}"
                );
                assert!(self.s.queued().any(|q| q == OperationId(op)), "t={t}");
            }
        }
    }
}

/// Every effect is a proposed refusal; the refused operations, in order.
fn refusals(effects: &[Effect]) -> Vec<u64> {
    effects
        .iter()
        .map(|e| match e {
            Effect::Commit(ControlRecord::Refusal(r)) => r.operation.0,
            other => panic!("not a refusal: {other:?}"),
        })
        .collect()
}

/// Catches: a refusal late or early against the deadline, in particular a wait that
/// restarts whenever its reason changes (workers that cannot run it come and go), so
/// work churned by an unrelated worker is refused late or never. The wait counts from
/// the first tick at which no live worker could run it, through the reason's change:
/// still queued at every tick up to W-1 s after it, refused at the tick at W.
#[test]
fn the_refusal_comes_at_the_deadline_through_reason_changes() {
    let mut h = Harness::new();
    h.submit(1, request(1, &[("OSFamily", "macos")]));
    assert_eq!(h.tick(), [waiting(0, Some("no worker is connected"))]);

    // A worker that cannot run it arrives: the reason changes, the deadline does not.
    h.at_secs(10).worker("linux", linux());
    let (grants, rest) = h.grants();
    assert!(grants.is_empty());
    assert!(
        matches!(rest.as_slice(), [Effect::Waiting(Waiting { reason: Some(r), .. })] if r.contains("lacks os=macos")),
        "{rest:?}"
    );

    h.wait_through(11..WAIT.as_secs(), "linux", &[0]);
    let refused = h.tick_hearing(WAIT.as_secs(), "linux");
    assert_eq!(refusals(&refused), [0], "refused at W after the wait began");
    assert_eq!(h.s.queued().count(), 0);
}

/// Catches: a wait that does not restart when a worker that could run the work (but
/// is busy) arrives and then leaves, so the work is refused at once on time it spent
/// servable; and one refused late after it restarts. Op 1 waits from t=0, is servable
/// once the Mac arrives at t=10 (op 0 fills the Mac), and waits again from the tick
/// at which the Mac stops being live; it is refused exactly W after that, not before.
#[test]
fn the_wait_restarts_when_a_worker_that_could_run_it_leaves() {
    let mut h = Harness::new();
    h.worker("linux", linux());
    let mut whole_mac = request(1, &[("OSFamily", "macos")]);
    whole_mac.resources = Resources::new(4_000, 8 * GIB);
    h.submit(1, whole_mac);
    h.submit(2, request(2, &[("OSFamily", "macos")]));
    let (grants, rest) = h.grants();
    assert!(grants.is_empty());
    assert_eq!(rest.len(), 2, "{rest:?}");

    // The Mac arrives: op 0 fills it, and op 1 waits only for room, not for a worker.
    h.at_secs(10).worker("mac", mac());
    let (grants, rest) = h.grants();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].operation, OperationId(0));
    assert_eq!(rest, [waiting(0, None), waiting(1, None)]);
    assert_eq!(h.s.waiting(OperationId(1)), None);
    h.feed(Event::Committed(ControlRecord::Lease(grants[0].clone())));

    // The Mac is never heard from again; it is live until 10 + G.
    let gone = 10 + GRACE;
    h.wait_through(11..gone, "linux", &[1]);
    let effects = h.tick_hearing(gone, "linux");
    assert!(
        effects.iter().all(|e| matches!(
            e,
            Effect::Waiting(Waiting {
                reason: Some(_),
                ..
            })
        )),
        "{effects:?}"
    );
    assert_eq!(effects.len(), 2, "both wait again: {effects:?}");

    h.wait_through(gone + 1..gone + WAIT.as_secs(), "linux", &[0, 1]);
    let refused = h.tick_hearing(gone + WAIT.as_secs(), "linux");
    assert_eq!(
        refusals(&refused),
        [0, 1],
        "refused at W after the Mac left"
    );
}
