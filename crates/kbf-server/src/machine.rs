//! The farm machine: the farm core's state beside the scheduler that every server of a
//! replicated farm will hold alike. It holds the record of each caller waiting on an
//! operation, the finished operations whose callers are still kept, and the leases
//! whose `Start` was sent.
//!
//! It is pure: its methods change it only from their arguments. Nothing here reads a
//! clock (a wall-clock time comes in as a value), a random source or a hashed map's
//! iteration order; this module denies clippy's `disallowed_methods` and
//! `disallowed_types` (`clippy.toml`) to keep it so. Two machines given the same calls
//! in the same order are equal, and iterate their maps in the same order.
//!
//! What one server alone holds stays in `crate::farm`: each caller's `watch` channel,
//! the worker streams and the heartbeat each `Start` names. The scheduler is not part
//! of the machine yet; the methods that depend on it take what they need from it as
//! arguments.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::{BTreeMap, VecDeque};
use std::time::SystemTime;

use kbf_types::{ActionKey, Digest, LeaseId, LeaseKind, OperationId, WaiterId};

use crate::stamp::Stamp;

/// The REAPI name of the operation `waiter` waits on, in the process of `term`:
/// `operations/{term}-{waiter}`.
///
/// Waiter ids count from 0 in every process, so the term is what keeps a name from
/// naming two operations: without it, a client's `WaitExecution` with a name from the
/// process before a restart attaches to whichever operation of the new process got
/// the same number, and hands it that action's result (issue #154).
fn operation_name(term: u64, waiter: WaiterId) -> String {
    format!("operations/{term}-{}", waiter.0)
}

/// The waiter an operation name of the process of `term` names: the inverse of
/// [`operation_name`]. `None` for a name of another term, and for any spelling
/// [`operation_name`] does not write (signs, leading zeros, other prefixes).
fn parse_operation_name(name: &str, term: u64) -> Option<WaiterId> {
    let (named_term, waiter) = name.strip_prefix("operations/")?.split_once('-')?;
    if canonical_u64(named_term)? != term {
        return None;
    }
    canonical_u64(waiter).map(WaiterId)
}

/// `text` as a `u64`, if it is that number's decimal spelling exactly (`u64`'s
/// `FromStr` also takes a `+` sign and leading zeros).
fn canonical_u64(text: &str) -> Option<u64> {
    let n: u64 = text.parse().ok()?;
    (n.to_string() == text).then_some(n)
}

/// A caller waiting on an operation. Its name is not kept: it is
/// [`FarmMachine::name`] of its id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Waiter {
    /// The instance name and action it submitted.
    pub(crate) key: ActionKey,
    pub(crate) kind: LeaseKind,
    pub(crate) do_not_cache: bool,
    /// When it was submitted, on the wall clock.
    pub(crate) queued: SystemTime,
}

/// A lease whose `Start` was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sent {
    pub(crate) operation: OperationId,
    /// The action the `Start` named.
    pub(crate) action: Digest,
    /// When the operation was queued and the `Start` sent.
    pub(crate) stamp: Stamp,
}

/// The waiter records and the started table of one farm (see the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FarmMachine {
    /// The scheduler term every operation name carries.
    term: u64,
    /// The id the next caller gets.
    next_waiter: u64,
    /// The callers of operations the scheduler holds: unfinished ones, and finished
    /// ones for the scheduler's finished retention, so that a WaitExecution on one
    /// gets its result (issue #165).
    waiters: BTreeMap<WaiterId, Waiter>,
    /// Finished operations whose callers are still kept, in the order they finished,
    /// with those callers. Each leaves once the scheduler has dropped its operation.
    finished: VecDeque<(OperationId, Vec<WaiterId>)>,
    /// Leases whose `Start` was sent.
    started: BTreeMap<LeaseId, Sent>,
}

impl FarmMachine {
    /// An empty machine whose operation names carry `term`.
    pub(crate) const fn new(term: u64) -> Self {
        Self {
            term,
            next_waiter: 0,
            waiters: BTreeMap::new(),
            finished: VecDeque::new(),
            started: BTreeMap::new(),
        }
    }

    /// Records a new caller and returns its id, the next in order.
    pub(crate) fn submit(&mut self, waiter: Waiter) -> WaiterId {
        let id = WaiterId(self.next_waiter);
        self.next_waiter += 1;
        self.waiters.insert(id, waiter);
        id
    }

    /// The caller `id`, while it is kept.
    pub(crate) fn waiter(&self, id: WaiterId) -> Option<&Waiter> {
        self.waiters.get(&id)
    }

    /// How many callers are kept.
    pub(crate) fn waiters_kept(&self) -> usize {
        self.waiters.len()
    }

    /// The REAPI name of the operation caller `id` waits on.
    pub(crate) fn name(&self, id: WaiterId) -> String {
        operation_name(self.term, id)
    }

    /// The kept caller `name` names, if this machine wrote that name.
    pub(crate) fn named(&self, name: &str) -> Option<(WaiterId, &Waiter)> {
        let id = parse_operation_name(name, self.term)?;
        self.waiter(id).map(|waiter| (id, waiter))
    }

    /// Records that the `Start` of `lease` was sent.
    pub(crate) fn start(&mut self, lease: LeaseId, sent: Sent) {
        self.started.insert(lease, sent);
    }

    /// What was sent for `lease`, if its `Start` was sent and it is not forgotten.
    pub(crate) fn sent(&self, lease: LeaseId) -> Option<Sent> {
        self.started.get(&lease).copied()
    }

    /// The operation of each lease whose `Start` was sent, in lease order.
    pub(crate) fn started_operations(&self) -> impl Iterator<Item = OperationId> + '_ {
        self.started.values().map(|sent| sent.operation)
    }

    /// Forgets every lease of a finished `operation` whose `Start` was sent.
    pub(crate) fn forget_leases(&mut self, operation: OperationId) {
        self.started.retain(|_, sent| sent.operation != operation);
    }

    /// Keeps `waiters`, the callers of `operation`, which has just finished, until
    /// [`Self::forget_dropped`] finds the scheduler has dropped it.
    pub(crate) fn keep_finished(&mut self, operation: OperationId, waiters: Vec<WaiterId>) {
        self.finished.push_back((operation, waiters));
    }

    /// Forgets the callers of each finished operation the scheduler has dropped, from
    /// the oldest up to the first that `held` says it still holds, and returns them.
    ///
    /// The scheduler drops them in the order they finished, and only those that
    /// finished at one farm time can be kept here in another order, so stopping at the
    /// first it still holds leaves none behind for longer than the input that drops it.
    pub(crate) fn forget_dropped(&mut self, held: impl Fn(OperationId) -> bool) -> Vec<WaiterId> {
        let dropped = self
            .finished
            .iter()
            .position(|(operation, _)| held(*operation))
            .unwrap_or(self.finished.len());
        let forgotten: Vec<WaiterId> = self
            .finished
            .drain(..dropped)
            .flat_map(|(_, waiters)| waiters)
            .collect();
        for id in &forgotten {
            self.waiters.remove(id);
        }
        forgotten
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use kbf_sim::SimRng;
    use kbf_types::DigestFunction;

    use super::*;

    const TERM: u64 = 0x0199_8a6b_2c3d_4e5f;

    fn at(millis: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(millis)
    }

    fn digest(n: u8) -> Digest {
        Digest {
            function: DigestFunction::Sha256,
            hash: [n; 32],
            size_bytes: u64::from(n),
        }
    }

    fn waiter(n: u8) -> Waiter {
        Waiter {
            key: ActionKey {
                instance: format!("instance-{}", n % 2),
                action: digest(n),
            },
            kind: if n.is_multiple_of(3) {
                LeaseKind::WholeMachine
            } else {
                LeaseKind::Action
            },
            do_not_cache: n.is_multiple_of(5),
            queued: at(u64::from(n)),
        }
    }

    fn sent(operation: u64, n: u8) -> Sent {
        Sent {
            operation: OperationId(operation),
            action: digest(n),
            stamp: Stamp {
                queued: at(u64::from(n)),
                started: at(u64::from(n) + 1),
            },
        }
    }

    /// Applies one call, drawn from `rng`, to `machine`. Every value the call takes
    /// is drawn from `rng` too, so a seed names exactly one call sequence.
    fn step(machine: &mut FarmMachine, rng: &mut SimRng) {
        let small = |rng: &mut SimRng| u8::try_from(rng.below(32)).unwrap_or_default();
        match rng.below(5) {
            0 | 1 => {
                machine.submit(waiter(small(rng)));
            }
            2 => {
                let lease = LeaseId::new(rng.between(1, 3), rng.below(64));
                machine.start(lease, sent(rng.below(8), small(rng)));
            }
            3 => {
                let operation = OperationId(rng.below(8));
                machine.forget_leases(operation);
                let waiters = (0..rng.below(4)).map(|_| WaiterId(rng.below(64)));
                machine.keep_finished(operation, waiters.collect());
            }
            _ => {
                let still_held = rng.below(8);
                machine.forget_dropped(|operation| operation.0 >= still_held);
            }
        }
    }

    /// Catches a machine that is not a function of its calls: a hashed map in it (its
    /// iteration order is drawn per map, so two equal machines list their callers or
    /// leases in different orders, and a snapshot or digest of them differs), and a
    /// call that reads a clock or a random source instead of its arguments. Over a
    /// sweep of seeds, two machines given the same generated calls are equal after
    /// every call, and list their callers and leases in the same, ascending order.
    #[test]
    fn machines_given_the_same_calls_are_equal_and_list_in_key_order() {
        let mut busiest = 0;
        for seed in 0..64 {
            let mut calls = SimRng::from_seed(seed);
            let mut replay = SimRng::from_seed(seed);
            let (mut a, mut b) = (FarmMachine::new(TERM), FarmMachine::new(TERM));
            for _ in 0..200 {
                step(&mut a, &mut calls);
                step(&mut b, &mut replay);
                assert_eq!(a, b, "seed {seed}");
            }
            let order = |m: &FarmMachine| {
                let waiters: Vec<WaiterId> = m.waiters.keys().copied().collect();
                let leases: Vec<LeaseId> = m.started.keys().copied().collect();
                (waiters, leases)
            };
            let (waiters, leases) = order(&a);
            assert_eq!((waiters.clone(), leases.clone()), order(&b), "seed {seed}");
            assert!(waiters.is_sorted(), "seed {seed}: callers {waiters:?}");
            assert!(leases.is_sorted(), "seed {seed}: leases {leases:?}");
            busiest = busiest.max(waiters.len().min(leases.len()));
        }
        // The sweep is only a check if the maps it orders hold several entries.
        assert!(busiest >= 8, "no seed filled both maps: {busiest}");
    }

    /// Catches an equality that leaves a field out (a hand-written `PartialEq` that
    /// skips one, or a field kept outside the struct): two replicas that differ only
    /// there would compare equal, so a divergence there would never be seen. Each
    /// field is changed in turn, and the machine must then differ from the original.
    #[test]
    fn equality_covers_every_field() {
        let mut base = FarmMachine::new(TERM);
        let first = base.submit(waiter(1));
        base.submit(waiter(2));
        base.keep_finished(OperationId(4), vec![first]);
        base.start(LeaseId::new(1, 7), sent(4, 9));
        let lease = LeaseId::new(1, 7);
        type Change = fn(&mut FarmMachine);
        let changes: [(&str, Change); 15] = [
            ("term", |m| m.term += 1),
            ("next_waiter", |m| m.next_waiter += 1),
            ("waiters: one more", |m| {
                m.waiters.insert(WaiterId(9), waiter(1));
            }),
            ("waiter.key.instance", |m| {
                m.waiters
                    .values_mut()
                    .for_each(|w| w.key.instance.push('x'));
            }),
            ("waiter.key.action", |m| {
                m.waiters
                    .values_mut()
                    .for_each(|w| w.key.action = digest(3));
            }),
            ("waiter.kind", |m| {
                m.waiters
                    .values_mut()
                    .for_each(|w| w.kind = LeaseKind::WholeMachine);
            }),
            ("waiter.do_not_cache", |m| {
                m.waiters.values_mut().for_each(|w| w.do_not_cache = true);
            }),
            ("waiter.queued", |m| {
                m.waiters.values_mut().for_each(|w| w.queued = at(99));
            }),
            ("finished: operation", |m| m.finished[0].0 = OperationId(5)),
            ("finished: callers", |m| m.finished[0].1.push(WaiterId(1))),
            ("started: lease", |m| {
                let sent = m.started.remove(&LeaseId::new(1, 7));
                m.started.extend(sent.map(|s| (LeaseId::new(1, 8), s)));
            }),
            ("sent.operation", |m| {
                m.started
                    .values_mut()
                    .for_each(|s| s.operation = OperationId(5));
            }),
            ("sent.action", |m| {
                m.started.values_mut().for_each(|s| s.action = digest(3));
            }),
            ("sent.stamp.queued", |m| {
                m.started.values_mut().for_each(|s| s.stamp.queued = at(99));
            }),
            ("sent.stamp.started", |m| {
                m.started
                    .values_mut()
                    .for_each(|s| s.stamp.started = at(99));
            }),
        ];
        for (field, change) in changes {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(changed, base, "a change to {field} is not seen");
        }
        assert_eq!(base.sent(lease), Some(sent(4, 9)));
    }

    /// Catches `forget_dropped` forgetting past the first operation the scheduler still
    /// holds (a caller of a kept operation would get NOT_FOUND), or keeping one it has
    /// dropped; and the callers it forgets not being returned, so the farm would keep
    /// their channels.
    #[test]
    fn forget_dropped_stops_at_the_first_held_operation() {
        let mut m = FarmMachine::new(TERM);
        let ids: Vec<WaiterId> = (0..4).map(|n| m.submit(waiter(n))).collect();
        m.keep_finished(OperationId(1), vec![ids[0], ids[1]]);
        m.keep_finished(OperationId(2), vec![ids[2]]);
        m.keep_finished(OperationId(3), vec![ids[3]]);
        // Operation 1 is dropped, 2 is held, 3 is dropped but finished after 2.
        let forgotten = m.forget_dropped(|operation| operation == OperationId(2));
        assert_eq!(forgotten, vec![ids[0], ids[1]]);
        assert_eq!(m.waiter(ids[0]), None);
        assert_eq!(m.waiters_kept(), 2);
        assert_eq!(m.waiter(ids[3]), Some(&waiter(3)));
        assert_eq!(m.forget_dropped(|_| false), vec![ids[2], ids[3]]);
        assert!(m.waiters.is_empty() && m.finished.is_empty());
    }

    /// Catches a lookup by name that finds a caller under a name the machine never
    /// wrote, or misses one it did: `named` is the inverse of `name` for kept callers.
    #[test]
    fn a_kept_caller_is_found_by_its_name_only() {
        let mut m = FarmMachine::new(TERM);
        let id = m.submit(waiter(1));
        let name = m.name(id);
        assert_eq!(name, format!("operations/{TERM}-0"));
        assert_eq!(m.named(&name), Some((id, &waiter(1))));
        assert_eq!(m.named(&format!("operations/{TERM}-1")), None);
        assert_eq!(FarmMachine::new(TERM + 1).named(&name), None);
    }

    /// Catches a name parser that is not the exact inverse of [`operation_name`]: one
    /// that ignores the term or accepts another (issue #154), and one that accepts a
    /// spelling the server never writes, which would give one operation several names.
    #[test]
    fn operation_names_parse_only_as_this_term_writes_them() {
        let term = TERM;
        for n in [0, 1, 42, u64::MAX] {
            let name = operation_name(term, WaiterId(n));
            assert_eq!(
                parse_operation_name(&name, term),
                Some(WaiterId(n)),
                "{name}"
            );
            for other in [0, term - 1, term + 1, u64::MAX] {
                assert_eq!(
                    parse_operation_name(&name, other),
                    None,
                    "{name} as {other}"
                );
            }
        }
        assert_eq!(operation_name(7, WaiterId(3)), "operations/7-3");
        for refused in [
            "",
            "operations/",
            "operations/7",
            "operations/7-",
            "operations/-3",
            "operations/7--3",
            "operations/7-3-1",
            "operations/07-3",
            "operations/7-03",
            "operations/+7-3",
            "operations/7-+3",
            "operations/7-3 ",
            " operations/7-3",
            "operations/7-18446744073709551616",
            "Operations/7-3",
            "operation/7-3",
            "operations/7_3",
            "7-3",
            "operations/3",
            "operations/cached/7-3",
        ] {
            assert_eq!(parse_operation_name(refused, 7), None, "{refused:?}");
        }
        assert_eq!(parse_operation_name("operations/7-0", 7), Some(WaiterId(0)));
    }
}
