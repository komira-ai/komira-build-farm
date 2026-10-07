//! GPU placement: a GPU request goes only to a node with a free GPU, and a GPU is held
//! by one lease at a time, from its grant until the lease ends.

use kbf_sched::{Event, Input, OpState, Request, Scheduler};
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, OperationId,
    Outcome, Qos, Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// Action `n`, one core and 1 GiB, and `gpus` GPUs.
fn request(n: u8, gpus: u64) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: digest(n),
        },
        qos: Qos::Ci,
        resources: Resources::new(1_000, GIB).with_gpus(gpus),
        hermetic: true,
        do_not_cache: false,
    }
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A scheduler at a fixed farm time: nothing here waits on a grace period.
struct Harness(Scheduler);

impl Harness {
    fn new() -> Self {
        Self(Scheduler::new(1))
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.0
            .apply(Input::new(FarmTime::from_millis(1_000), event))
    }

    /// Registers `name` (again) with 8 cores, 16 GiB and `gpus` GPUs.
    fn worker(&mut self, name: &str, gpus: u64) {
        let capacity = Resources::new(8_000, 16 * GIB).with_gpus(gpus);
        assert!(
            self.feed(Event::WorkerUp {
                worker: w(name),
                capacity,
            })
            .is_empty()
        );
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// Ticks and returns the grants proposed.
    fn tick(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .map(|e| match e {
                Effect::Commit(ControlRecord::Lease(g)) => g,
                other => panic!("a tick proposed {other:?}"),
            })
            .collect()
    }

    /// Commits `grant` and reports it started.
    fn start(&mut self, grant: &LeaseGrant) {
        let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        assert!(
            matches!(effects.as_slice(), [Effect::Start(_)]),
            "{effects:?}"
        );
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
    }

    /// Reports `grant` completed and commits the result.
    fn finish(&mut self, grant: &LeaseGrant) {
        let outcome = Outcome::Completed {
            action_result: digest(200),
        };
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome,
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("{proposed:?}");
        };
        let answered = self.feed(Event::Committed(record.clone()));
        assert!(
            matches!(answered.as_slice(), [Effect::Answer(_)]),
            "{answered:?}"
        );
    }

    fn gpus_booked(&self, name: &str) -> u64 {
        self.0.booked(&w(name)).expect("registered").gpus
    }
}

/// Catches: a GPU booked as if it were shared, so two GPU leases run on a one-GPU
/// node at once; a GPU not given back when its lease ends, so the next GPU action
/// queues forever; and CPU-only work held back by a booked GPU.
#[test]
fn a_one_gpu_node_holds_one_gpu_lease_at_a_time() {
    let mut h = Harness::new();
    h.worker("g", 1);
    h.submit(1, request(1, 1));
    h.submit(2, request(2, 1));
    let [first] = h.tick().try_into().unwrap();
    assert_eq!(first.worker, w("g"));
    assert_eq!(h.gpus_booked("g"), 1);
    assert!(h.tick().is_empty(), "a second GPU lease on a one-GPU node");

    // The node has room for CPU-only work beside the GPU lease.
    h.submit(3, request(3, 0));
    let [plain] = h.tick().try_into().unwrap();
    assert_eq!(plain.worker, w("g"));
    assert_eq!(h.gpus_booked("g"), 1);

    h.start(&first);
    h.finish(&first);
    assert_eq!(h.gpus_booked("g"), 0, "the GPU outlived its lease");
    let [second] = h.tick().try_into().unwrap();
    assert_eq!(second.worker, w("g"));
    assert_eq!(
        h.0.state(second.operation),
        Some(&OpState::Leased {
            lease: second.lease,
            worker: w("g"),
            committed: false,
        })
    );
}

/// Catches: a GPU request placed on a node without a GPU (first fit would pick `a`, the
/// first in name order), and one placed on a node with fewer GPUs than it asks for.
#[test]
fn a_gpu_request_waits_for_a_node_with_a_gpu() {
    let mut h = Harness::new();
    h.worker("a", 0);
    h.worker("b", 1);
    h.submit(1, request(1, 2));
    h.submit(2, request(2, 1));
    let [one] = h.tick().try_into().unwrap();
    assert_eq!((one.operation, one.worker), (OperationId(1), w("b")));
    assert_eq!(
        h.0.queued().collect::<Vec<_>>(),
        [OperationId(0)],
        "the two-GPU request was placed"
    );
    assert_eq!(h.gpus_booked("a"), 0);

    h.worker("c", 2);
    let [two] = h.tick().try_into().unwrap();
    assert_eq!((two.operation, two.worker), (OperationId(0), w("c")));
    assert_eq!(h.gpus_booked("c"), 2);
}

/// Catches: a GPU not given back when its lease is given up (here, a worker that
/// registers again and lists nothing running), so the requeued action can never be
/// placed again on the node's only GPU.
#[test]
fn a_given_up_lease_frees_its_gpu() {
    let mut h = Harness::new();
    h.worker("g", 1);
    h.submit(1, request(1, 1));
    let [grant] = h.tick().try_into().unwrap();
    h.start(&grant);

    h.worker("g", 1);
    assert!(
        h.feed(Event::Heartbeat {
            worker: w("g"),
            running: Vec::new(),
        })
        .is_empty()
    );
    assert_eq!(h.0.state(grant.operation), Some(&OpState::Queued));
    assert_eq!(h.gpus_booked("g"), 0);
    let [again] = h.tick().try_into().unwrap();
    assert_eq!((again.operation, again.worker), (grant.operation, w("g")));
    assert_ne!(again.lease, grant.lease);
}
