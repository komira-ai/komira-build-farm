//! Properties 1 to 7 of docs/design/ha.md section 15, for one server, checked after
//! every step (7 at the end of the run).
//!
//! 1. **Replica equality.** A replica that replays the server's durable log through
//!    the server's applied index has the server's applied state (and its `Bootstrap`
//!    names the server's log): every entry the server has applied, and so answered
//!    from, a restart or a follower would apply alike.
//! 2. **One run.** No operation (a term and an operation id) has two unfenced runs at
//!    once.
//! 3. **Leases kept.** No lease is given up while its daemon, heard within T, listed
//!    it.
//! 4. **Exactly one result.** No lease's result is accepted twice, and every result a
//!    daemon reports is accepted once by the end; an action has an action-cache entry
//!    if and only if a result of it was accepted.
//! 5. **No false present.** Every blob the server answers present is in the object
//!    store then. (No loss fault and no collection yet, so its later clauses are
//!    vacuous.)
//! 6. **Monotone time.** Farm time never goes back from one entry of the durable log to
//!    the next, nor across a restart.
//! 7. **Liveness.** Every client's every action is answered by the end of the run.
//!
//! Two more, of the write path (docs/design/write-tier-scenarios.md), checked against
//! where bytes are durable ([`BlobHomes`]: the object store today, a staged block too
//! once the local write tier lands):
//!
//! 8. **No false ack.** Every upload the server acknowledges is held by the object its
//!    PUT wrote, and present in the server's applied index at every step the server is
//!    up from then on, across restarts (no collection runs in the cell): an ack is
//!    never sent for a commit that a crash can still lose.
//! 9. **Nothing half-indexed.** Every blob the applied index answers present is held
//!    by the object its location names: no fault (a failed, held or unanswered PUT, a
//!    crash at any point of the write path) leaves an index entry without its bytes.
//!
//! Four more, of the local write tier (`check/tier.rs`), in every run with one, besides
//! the tier halves of 8 and 9 (a staged object's block is a home too):
//!
//! 10. **Ids never move** (invariant C). A blob's location is fixed by its first
//!     commit unless its object is marked unreachable, and the store never holds two
//!     contents under one id (a reflush writes the same bytes). Checked in every run.
//! 11. **Within budget** (invariant D). The tier's counted and flushed bytes never pass
//!     the hot store's budget.
//! 12. **No leak** (invariant E). At each start, the tier directory holds only `TIER`,
//!     staged blocks at their committed length and flushed copies, and the budget
//!     counts exactly the staged bytes.
//! 13. **Drained** (invariant F). At the end no object is left staged.
//!
//! One more, of all-in-one (its design, komira-build-farm#657, sections 7.4 and 8),
//! in [`Kind::WholeProcess`] runs, where the daemon half stores each output before it
//! reports and dies with the server:
//!
//! 14. **Answered with outputs.** Every action a client has been answered for has an
//!     action-cache entry the server serves (a hit, so its result blob is indexed), at
//!     every step the server is up from then on, across kills of the whole process:
//!     no action is answered as succeeded with its outputs missing, and the index and
//!     the action cache survive the kill. An action whose run the kill cut is answered
//!     only by a later run, which the client's retry starts.
//!
//! Properties 3 and 4 do not hold yet once the server restarts or its log stops,
//! because the scheduler's state is not in the log: property 3 is enforced in
//! runs without a restart only ([`Kind::restarts`]), until control inputs go through
//! the log (ha.md section 16, PR 11), and property 4 in runs without a restart only,
//! until a result and its action-cache entry commit together (PR 12): in
//! [`Kind::Calm`] and the store kinds but [`Kind::StoreCrash`]. A daemon cut off past
//! T may have a result refused, so property 4's "accepted once" does not apply to
//! [`Kind::Partition`] runs, nor its "entry for each result" to [`Kind::TierBadBlock`]
//! runs (the entry's result blob may be in the damaged block).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use kbf_meta::{ActionAnswer, BlobAnswer, MetaState, ObjectId};
use kbf_sim::{NodeId, Sim};
use kbf_types::{Digest, FarmTime, LeaseId, OperationId};

use crate::cell::{CellNode, Kind, Scenario};
use crate::client::{FakeClient, Job};
use crate::replica::{self, Broken};
use crate::server::Seen;
use crate::store::BlobHomes;

pub(crate) mod tier;

/// A property of ha.md section 15.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    ReplicaEquality,
    OneRun,
    LeasesKept,
    ExactlyOneResult,
    NoFalsePresent,
    MonotoneTime,
    Liveness,
    NoFalseAck,
    NothingHalfIndexed,
    IdsNeverMove,
    WithinBudget,
    NoLeak,
    Drained,
    AnsweredWithOutputs,
}

impl fmt::Display for Property {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::ReplicaEquality => "1 (replica equality)",
            Self::OneRun => "2 (one run)",
            Self::LeasesKept => "3 (leases kept)",
            Self::ExactlyOneResult => "4 (exactly one result)",
            Self::NoFalsePresent => "5 (no false present)",
            Self::MonotoneTime => "6 (monotone time)",
            Self::Liveness => "7 (liveness)",
            Self::NoFalseAck => "8 (no false ack)",
            Self::NothingHalfIndexed => "9 (nothing half-indexed)",
            Self::IdsNeverMove => "10 (ids never move)",
            Self::WithinBudget => "11 (within budget)",
            Self::NoLeak => "12 (no leak)",
            Self::Drained => "13 (drained)",
            Self::AnsweredWithOutputs => "14 (answered with outputs)",
        };
        f.write_str(name)
    }
}

/// `Ok`, or `property` broken as `detail` says.
fn hold_if(detail: Option<String>, property: Property) -> Result<(), Broken> {
    detail.map_or(Ok(()), |d| Err((property, d)))
}

/// Property 2 over every unfenced run: (lease, operation, node).
fn one_run<'a>(
    runs: impl Iterator<Item = (LeaseId, OperationId, &'a NodeId)>,
) -> Result<(), Broken> {
    let mut by_operation: BTreeMap<(u64, u64), Vec<&NodeId>> = BTreeMap::new();
    for (lease, operation, node) in runs {
        by_operation
            .entry((lease.term, operation.0))
            .or_default()
            .push(node);
    }
    let twice = by_operation.iter().find(|(_, on)| on.len() > 1);
    let detail = twice.map(|(op, on)| format!("operation {op:?} runs unfenced on {on:?}"));
    hold_if(detail, Property::OneRun)
}

/// Property 5 over the blobs answered present since the last check.
fn no_false_present(
    present: &[(FarmTime, Digest)],
    holds: impl Fn(&Digest) -> bool,
) -> Result<(), Broken> {
    let lacking = present.iter().find(|(_, d)| !holds(d));
    let detail = lacking.map(|(at, d)| format!("{d:?} answered present at {} ms", at.as_millis()));
    hold_if(detail, Property::NoFalsePresent)
}

/// Property 8 over every upload acknowledged so far: each is held by the object its
/// PUT wrote, and `indexed` (the applied index, when the server is up).
fn no_false_ack(
    acked: &[(FarmTime, Digest, ObjectId)],
    homes: &dyn BlobHomes,
    indexed: &dyn Fn(&Digest) -> bool,
) -> Result<(), Broken> {
    let detail = acked.iter().find_map(|(at, d, object)| {
        let ms = at.as_millis();
        if !homes.holds(*object, d) {
            Some(format!("{d:?} acknowledged at {ms} ms, {object} lacks it"))
        } else if !indexed(d) {
            Some(format!("{d:?} acknowledged at {ms} ms, not indexed"))
        } else {
            None
        }
    });
    hold_if(detail, Property::NoFalseAck)
}

/// Property 9 over `uploaded`, each with its answer in the applied index.
fn nothing_half_indexed(
    answers: &[(&Digest, BlobAnswer)],
    homes: &dyn BlobHomes,
) -> Result<(), Broken> {
    let detail = answers.iter().find_map(|(d, answer)| match answer {
        BlobAnswer::Present { location: at, .. } if !homes.holds(at.object, d) => {
            Some(format!("{d:?} indexed in {}, which lacks it", at.object))
        }
        _ => None,
    });
    hold_if(detail, Property::NothingHalfIndexed)
}

/// Where `answer` says the blob is, if it says present.
fn located(answer: BlobAnswer) -> Option<kbf_meta::Location> {
    match answer {
        BlobAnswer::Present { location, .. } => Some(location),
        BlobAnswer::Unavailable | BlobAnswer::Absent => None,
    }
}

/// How many of the store's `objects` no index answer names: objects a PUT left that
/// nothing points at, for the planned orphan sweep.
pub(crate) fn orphans(
    objects: impl Iterator<Item = ObjectId>,
    answers: impl Iterator<Item = BlobAnswer>,
) -> BTreeSet<ObjectId> {
    let named: BTreeSet<ObjectId> = answers.filter_map(located).map(|at| at.object).collect();
    objects.filter(|o| !named.contains(o)).collect()
}

/// Property 14 over the actions answered so far (when, and which): each is a hit.
fn answered_with_outputs(
    answered: impl IntoIterator<Item = (FarmTime, Digest)>,
    meta: &MetaState,
) -> Result<(), Broken> {
    let detail = answered
        .into_iter()
        .find_map(|(at, action)| match meta.action(&action) {
            ActionAnswer::Hit { .. } => None,
            ActionAnswer::Miss(miss) => Some(format!(
                "{action:?} answered at {} ms, not served now: {miss:?}",
                at.as_millis()
            )),
        });
    hold_if(detail, Property::AnsweredWithOutputs)
}

/// Property 3.
fn leases_kept(seen: &Seen) -> Result<(), Broken> {
    let listed = seen.requeues.iter().find(|r| r.3);
    let detail = listed.map(|(at, lease, worker, _)| {
        format!(
            "lease {lease} of {worker} given up at {} ms while listed",
            at.as_millis()
        )
    });
    hold_if(detail, Property::LeasesKept)
}

/// Property 4, after a step: no result accepted twice, and an action-cache entry
/// exactly for each action with an accepted result.
fn one_result(seen: &Seen, meta: &MetaState) -> Result<(), Broken> {
    let twice = seen.accepted.iter().find(|(_, n)| **n > 1);
    let uncached = seen
        .won
        .iter()
        .find(|a| !matches!(meta.action(a), ActionAnswer::Hit { .. }));
    let entries = meta.action_count();
    let detail = match (twice, uncached) {
        (Some((lease, n)), _) => Some(format!("lease {lease}'s result accepted {n} times")),
        (None, Some(a)) => Some(format!("{a:?} won a result but has no action-cache entry")),
        (None, None) => (entries != seen.won.len()).then(|| {
            format!(
                "{entries} action-cache entries for {} actions with a result",
                seen.won.len()
            )
        }),
    };
    hold_if(detail, Property::ExactlyOneResult)
}

/// Property 4, at the end: every lease a daemon reported was accepted.
fn all_accepted(
    reported: &BTreeSet<LeaseId>,
    accepted: &BTreeMap<LeaseId, u32>,
) -> Result<(), Broken> {
    let lost = reported
        .iter()
        .find(|l| accepted.get(l).is_none_or(|n| *n == 0));
    let detail = lost.map(|lease| format!("lease {lease} reported, never accepted"));
    hold_if(detail, Property::ExactlyOneResult)
}

/// Property 7.
fn liveness(
    node: &NodeId,
    jobs: &[Job],
    refused: Option<&(FarmTime, String)>,
) -> Result<(), Broken> {
    let open: Vec<&Job> = jobs.iter().filter(|j| j.answered.is_none()).collect();
    let detail = open.first().map(|first| {
        format!(
            "{node} has {} actions unanswered, first {first:?}; first refused open: {refused:?}",
            open.len()
        )
    });
    hold_if(detail, Property::Liveness)
}

/// The checks' memory across steps.
pub(crate) struct Checker<'a> {
    scenario: &'a Scenario,
    newest_time: FarmTime,
    present_checked: usize,
    /// Each blob's location as first answered (property 10).
    locations: BTreeMap<Digest, kbf_meta::Location>,
}

impl<'a> Checker<'a> {
    pub(crate) fn new(scenario: &'a Scenario) -> Self {
        Self {
            scenario,
            newest_time: FarmTime::default(),
            present_checked: 0,
            locations: BTreeMap::new(),
        }
    }

    /// Panics, naming the run and the replay command, if `result` is a broken property.
    #[track_caller]
    fn hold(&self, at: FarmTime, result: Result<(), Broken>) {
        if let Err((property, detail)) = result {
            let s = self.scenario;
            panic!(
                "{:?} seed {} at {} ms: property {property} violated: {detail}\nreplay: {}",
                s.kind,
                s.seed,
                at.as_millis(),
                s.replay_line()
            );
        }
    }

    pub(crate) fn after_step(&mut self, sim: &Sim<CellNode>) {
        let now = sim.now();
        let server = server(sim);
        self.hold(now, server.check_durable());
        let meta = server.meta();
        if let Some(meta) = &meta {
            self.hold(now, replica::monotone(self.newest_time, meta.now()));
            self.newest_time = meta.now();
        }
        let runs = sim
            .nodes()
            .filter_map(|(id, n)| n.daemon().map(|d| (id, d)))
            .flat_map(|(id, d)| d.unfenced_runs(now).map(move |(l, o)| (l, o, id)));
        self.hold(now, one_run(runs));
        let seen = &server.seen;
        let store = sim
            .nodes()
            .find_map(|(_, n)| n.store())
            .expect("the cell has an object store");
        let present = &seen.present[self.present_checked..];
        self.present_checked = seen.present.len();
        let indexed = |d: &Digest| {
            meta.as_ref()
                .is_none_or(|m| matches!(m.blob(d), BlobAnswer::Present { .. }))
        };
        if self.scenario.server.tier.is_none() {
            self.hold(now, no_false_present(present, |d| store.holds_digest(d)));
            self.hold(now, no_false_ack(&seen.acked, store, &indexed));
            if let Some(meta) = &meta {
                let answers: Vec<_> = seen.uploaded.iter().map(|d| (d, meta.blob(d))).collect();
                self.hold(now, nothing_half_indexed(&answers, store));
            }
        } else if let Some(meta) = &meta {
            self.tier_halves(now, server, store, meta, present);
        }
        self.hold(now, tier::tier_seen(&seen.tier));
        if let Some(usage) = server.tier_usage() {
            self.hold(now, tier::within_budget(usage));
        }
        if let Some(meta) = &meta {
            let answers: Vec<_> = seen.uploaded.iter().map(|d| (d, meta.blob(d))).collect();
            let unreachable = |o| meta.unreachable(o).is_some();
            let moved = tier::ids_never_move(
                &mut self.locations,
                &answers,
                &unreachable,
                store.rewrites(),
            );
            self.hold(now, moved);
        }
        let kind = self.scenario.kind;
        if !kind.restarts() {
            self.hold(now, leases_kept(seen));
        }
        if let (true, Some(meta)) = (one_result_holds(kind), &meta) {
            self.hold(now, one_result(seen, meta));
        }
        if let (Kind::WholeProcess, Some(meta)) = (kind, &meta) {
            let answered = sim
                .nodes()
                .filter_map(|(_, n)| n.client())
                .flat_map(FakeClient::jobs)
                .filter_map(|j| Some((j.answered?, j.action)));
            self.hold(now, answered_with_outputs(answered, meta));
        }
    }

    /// Properties 5, 8 and 9 with the tier: against the store and the staged blocks
    /// as a power cut now would leave them; acknowledgements and entries in an object
    /// a bad sector damaged are held to the disk-loss clause instead.
    fn tier_halves(
        &self,
        now: FarmTime,
        server: &crate::Server,
        store: &crate::ObjectStore,
        meta: &MetaState,
        present: &[(FarmTime, Digest)],
    ) {
        let seen = &server.seen;
        let disk = server.tier_durable();
        let homes = tier::TierHomes {
            store,
            disk: &disk,
            meta,
        };
        let damaged = &seen.tier.damaged;
        // Answered present in this step, so present in the index now.
        let held = |d: &Digest| {
            located(meta.blob(d))
                .is_some_and(|at| damaged.contains(&at.object) | homes.holds(at.object, d))
        };
        self.hold(now, no_false_present(present, held));
        // An ack is held to the disk-loss clause if its object was damaged, or the
        // object the index keeps the blob in was (it was acknowledged as a duplicate).
        let hit_by = |d: &Digest, o: &ObjectId| {
            damaged.contains(o)
                || self
                    .locations
                    .get(d)
                    .is_some_and(|l| damaged.contains(&l.object))
        };
        let (hit, whole): (Vec<_>, Vec<_>) = seen
            .acked
            .iter()
            .copied()
            .partition(|(_, d, o)| hit_by(d, o));
        let indexed = |d: &Digest| matches!(meta.blob(d), BlobAnswer::Present { .. });
        self.hold(now, no_false_ack(&whole, &homes, &indexed));
        self.hold(now, tier::damaged_acks(&hit, damaged, &|d| meta.blob(d)));
        let answers: Vec<_> = seen
            .uploaded
            .iter()
            .map(|d| (d, meta.blob(d)))
            .filter(|(_, a)| {
                !matches!(a, BlobAnswer::Present { location, .. } if damaged.contains(&location.object))
            })
            .collect();
        self.hold(now, nothing_half_indexed(&answers, &homes));
    }

    pub(crate) fn at_end(&self, sim: &Sim<CellNode>) {
        let now = sim.now();
        let seen = &server(sim).seen;
        if let (Some(_), Some(meta)) = (self.scenario.server.tier, server(sim).meta()) {
            self.hold(now, tier::drained(&meta));
        }
        for (id, node) in sim.nodes() {
            if let Some(client) = node.client() {
                self.hold(now, liveness(id, client.jobs(), seen.refused.first()));
            }
            if let (true, Some(daemon)) = (one_result_holds(self.scenario.kind), node.daemon()) {
                self.hold(now, all_accepted(daemon.reported(), &seen.accepted));
            }
        }
    }
}

/// Whether property 4 is enforced in runs of `kind`: no restart, no daemon cut off,
/// and no bad sector (an action-cache entry whose result blob is in the damaged block
/// is not servable until the blob is uploaded again).
fn one_result_holds(kind: Kind) -> bool {
    !kind.restarts() && !matches!(kind, Kind::Partition | Kind::TierBadBlock)
}

/// The cell's server.
pub(crate) fn server(sim: &Sim<CellNode>) -> &crate::Server {
    sim.nodes()
        .find_map(|(_, n)| n.server())
        .expect("the cell has a server")
}

#[cfg(test)]
mod tests {
    use kbf_meta::{
        ActionRecord, Applied, Closure, Command, Location, ObjectId, Retention, Role, StoreId,
    };
    use kbf_types::{DigestFunction, WorkerId};

    use super::*;
    use crate::{Down, StorePoint, action, input_of};

    fn lease(seq: u64) -> LeaseId {
        LeaseId::new(1, seq)
    }

    fn property(r: Result<(), Broken>) -> Property {
        r.expect_err("broken").0
    }

    /// Catches a one-run check that misses two runs of one operation (on one node or
    /// two), or that takes runs of two terms' operations of one id for one operation.
    #[test]
    fn two_runs_of_one_operation_break_one_run() {
        let (a, b) = (NodeId::from("daemon-0"), NodeId::from("daemon-1"));
        let apart = [
            (lease(1), OperationId(0), &a),
            (lease(2), OperationId(1), &b),
        ];
        assert!(one_run(apart.into_iter()).is_ok());
        let terms = [
            (lease(1), OperationId(0), &a),
            (LeaseId::new(2, 1), OperationId(0), &b),
        ];
        assert!(one_run(terms.into_iter()).is_ok());
        let twice = [
            (lease(1), OperationId(0), &a),
            (lease(2), OperationId(0), &a),
        ];
        assert_eq!(property(one_run(twice.into_iter())), Property::OneRun);
    }

    /// Catches a no-false-present check that never asks the store.
    #[test]
    fn a_present_blob_the_store_lacks_breaks_no_false_present() {
        let d = Digest::new(DigestFunction::Sha256, [3; 32], 3);
        let present = [(FarmTime::from_millis(9), d)];
        assert!(no_false_present(&present, |_| true).is_ok());
        assert_eq!(
            property(no_false_present(&present, |_| false)),
            Property::NoFalsePresent
        );
    }

    /// A store holding exactly `objects`.
    struct Holds(Vec<(ObjectId, Digest)>);

    impl BlobHomes for Holds {
        fn holds(&self, object: ObjectId, digest: &Digest) -> bool {
            self.0.contains(&(object, *digest))
        }
    }

    fn object(seq: u64) -> ObjectId {
        ObjectId::new(kbf_meta::Epoch::new(1), seq)
    }

    /// Catches a no-false-ack check that never asks the store, asks it about another
    /// object than the PUT's, or never asks the index.
    #[test]
    fn an_ack_the_store_or_index_lacks_breaks_no_false_ack() {
        let d = Digest::new(DigestFunction::Sha256, [4; 32], 4);
        let acked = [(FarmTime::from_millis(3), d, object(1))];
        let homes = Holds(vec![(object(1), d)]);
        assert!(no_false_ack(&acked, &homes, &|_| true).is_ok());
        let elsewhere = Holds(vec![(object(2), d)]);
        let broken = no_false_ack(&acked, &elsewhere, &|_| true);
        assert_eq!(property(broken), Property::NoFalseAck);
        let unindexed = no_false_ack(&acked, &homes, &|_| false);
        assert_eq!(property(unindexed), Property::NoFalseAck);
    }

    /// Catches a half-indexed check that never asks the store, asks it about another
    /// object than the location's, or fails a blob the index does not answer present.
    #[test]
    fn an_entry_whose_object_lacks_it_breaks_nothing_half_indexed() {
        let d = Digest::new(DigestFunction::Sha256, [5; 32], 5);
        let at = |seq| Location {
            store: StoreId::new(0),
            object: object(seq),
            offset: 0,
        };
        let homes = Holds(vec![(object(1), d)]);
        let good = [(
            &d,
            BlobAnswer::Present {
                location: at(1),
                staged_on: None,
            },
        )];
        assert!(nothing_half_indexed(&good, &homes).is_ok());
        let gone = [(&d, BlobAnswer::Absent), (&d, BlobAnswer::Unavailable)];
        assert!(nothing_half_indexed(&gone, &Holds(Vec::new())).is_ok());
        let half = [(
            &d,
            BlobAnswer::Present {
                location: at(2),
                staged_on: None,
            },
        )];
        let broken = nothing_half_indexed(&half, &homes);
        assert_eq!(property(broken), Property::NothingHalfIndexed);
    }

    /// Catches an orphan count that counts an object the index names, or misses one
    /// it does not (an absent or unavailable answer names no object).
    #[test]
    fn objects_no_entry_names_are_orphans() {
        let at = |seq| BlobAnswer::Present {
            location: Location {
                store: StoreId::new(0),
                object: object(seq),
                offset: 0,
            },
            staged_on: None,
        };
        let objects = [object(1), object(2), object(3)];
        let answers = [at(1), BlobAnswer::Absent, BlobAnswer::Unavailable, at(3)];
        let left = orphans(objects.into_iter(), answers.into_iter());
        assert_eq!(left, BTreeSet::from([object(2)]));
        assert_eq!(orphans(objects.into_iter(), [].into_iter()).len(), 3);
    }

    /// Catches a leases-kept check that fails an unlisted requeue (a lost `Start` is
    /// rightly given up), or passes a listed one.
    #[test]
    fn a_listed_lease_given_up_breaks_leases_kept() {
        let mut seen = Seen::default();
        let worker = WorkerId::new("daemon-0");
        seen.requeues
            .push((FarmTime::default(), lease(1), worker.clone(), false));
        assert!(leases_kept(&seen).is_ok());
        seen.requeues
            .push((FarmTime::default(), lease(2), worker, true));
        assert_eq!(property(leases_kept(&seen)), Property::LeasesKept);
    }

    /// Catches each half of property 4 going unchecked: a result accepted twice, a
    /// won result without its cache entry, a reported lease refused or never seen, a
    /// cache entry without a won result.
    #[test]
    fn each_break_of_one_result_is_reported() {
        let meta = MetaState::new(Retention::default());
        let mut seen = Seen::default();
        assert!(one_result(&seen, &meta).is_ok());
        seen.accepted.insert(lease(1), 2);
        assert_eq!(
            property(one_result(&seen, &meta)),
            Property::ExactlyOneResult
        );
        seen.accepted.insert(lease(1), 1);
        seen.won.insert(action(0));
        assert_eq!(
            property(one_result(&seen, &meta)),
            Property::ExactlyOneResult
        );
        let reported = BTreeSet::from([lease(1), lease(2)]);
        let lost = all_accepted(&reported, &seen.accepted);
        assert_eq!(property(lost), Property::ExactlyOneResult);
        seen.accepted.insert(lease(2), 0);
        let refused = all_accepted(&reported, &seen.accepted);
        assert_eq!(property(refused), Property::ExactlyOneResult);
        seen.accepted.insert(lease(2), 1);
        assert!(all_accepted(&reported, &seen.accepted).is_ok());
        // An action-cache entry for an action with no accepted result.
        let mut cached = MetaState::new(Retention::default());
        let Applied::Epoch(epoch) = cached.execute(1, Command::AllocEpoch) else {
            panic!("an epoch")
        };
        let result = input_of(&action(0));
        let location = Location {
            store: StoreId::new(0),
            object: ObjectId::new(epoch, 0),
            offset: 0,
        };
        cached.execute(
            2,
            Command::PutBlob {
                digest: result,
                location,
            },
        );
        let record = ActionRecord {
            result,
            closure: Closure::new(),
        };
        cached.execute(
            3,
            Command::PutAction {
                role: Role::Daemon,
                action: action(0),
                record,
            },
        );
        assert_eq!(
            property(one_result(&Seen::default(), &cached)),
            Property::ExactlyOneResult
        );
    }

    /// Catches an answered-with-outputs check that passes an action with no entry
    /// (the log refuses an entry whose result blob it lacks, as the write of a result
    /// reported before its output was stored is refused), or fails a servable one.
    #[test]
    fn an_answer_the_server_would_not_serve_breaks_answered_with_outputs() {
        let mut meta = MetaState::new(Retention::default());
        let epoch = kbf_meta::Epoch::new(1);
        assert_eq!(meta.execute(1, Command::AllocEpoch), Applied::Epoch(epoch));
        let answered = [(FarmTime::from_millis(4), action(0))];
        let none = answered_with_outputs(answered, &meta);
        assert_eq!(property(none), Property::AnsweredWithOutputs);
        let result = crate::output_of(&action(0));
        let put = || Command::PutAction {
            role: Role::Daemon,
            action: action(0),
            record: ActionRecord {
                result,
                closure: Closure::new(),
            },
        };
        assert!(matches!(meta.execute(2, put()), Applied::Action(Err(_))));
        let early = answered_with_outputs(answered, &meta);
        assert_eq!(property(early), Property::AnsweredWithOutputs);
        let location = Location {
            store: StoreId::new(0),
            object: ObjectId::new(epoch, 0),
            offset: 0,
        };
        meta.execute(
            3,
            Command::PutBlob {
                digest: result,
                location,
            },
        );
        assert!(matches!(meta.execute(4, put()), Applied::Action(Ok(()))));
        assert!(answered_with_outputs(answered, &meta).is_ok());
        assert!(answered_with_outputs([], &MetaState::new(Retention::default())).is_ok());
    }

    /// Catches a liveness check that passes an unanswered action.
    #[test]
    fn an_unanswered_action_breaks_liveness() {
        let client = FakeClient::new(NodeId::from("server"), [(action(0), FarmTime::default())]);
        let node = NodeId::from("client-0");
        assert_eq!(
            property(liveness(&node, client.jobs(), None)),
            Property::Liveness
        );
    }

    /// Catches a checker that does not stop the run, or whose message lacks the seed,
    /// the property or the replay command.
    #[test]
    #[should_panic(
        expected = "Calm seed 5 at 7 ms: property 2 (one run) violated: x\nreplay: KBF_SIM_KIND=Calm KBF_SIM_SEED=5"
    )]
    fn a_broken_property_panics_with_its_replay_line() {
        let scenario = Scenario::new(Kind::Calm, 5);
        let checker = Checker::new(&scenario);
        checker.hold(
            FarmTime::from_millis(7),
            Err((Property::OneRun, "x".to_owned())),
        );
    }

    /// Catches a pinned crash's failure whose replay line leaves the crash out: the
    /// replay would run the seed's own crash, not the one that failed.
    #[test]
    #[should_panic(expected = "replay: KBF_SIM_KIND=StoreCrash KBF_SIM_SEED=5 \
                                KBF_SIM_CRASH=in-commit-1/cut-3-0 cargo test")]
    fn a_pinned_crash_panics_with_its_crash_in_the_replay_line() {
        let cut = Down::PowerCut { keep: 3, names: 0 };
        let scenario = Scenario::crash_at(5, StorePoint::InCommit { after: 1 }, cut);
        let checker = Checker::new(&scenario);
        checker.hold(
            FarmTime::from_millis(7),
            Err((Property::OneRun, "x".to_owned())),
        );
    }

    /// Catches a replay line that does not read back to its scenario (another crash,
    /// seed or kind), and a malformed one read as some scenario.
    #[test]
    fn a_replay_line_reads_back_to_its_scenario() {
        let var = |line: &str, name: &str| -> Option<String> {
            line.split_whitespace()
                .find_map(|w| w.strip_prefix(&format!("{name}=")).map(str::to_owned))
        };
        let cut = Down::PowerCut { keep: 9, names: 5 };
        let pinned = Scenario::crash_at(11, StorePoint::BeforeReply, cut);
        let tier = Scenario::tier_crash_at(3, crate::TierPoint::InFlushCommit { after: 1 }, cut);
        for s in [pinned, tier, Scenario::new(Kind::StoreSlow, 4)] {
            let line = s.replay_line();
            let kind = var(&line, "KBF_SIM_KIND").expect("a kind");
            let seed = var(&line, "KBF_SIM_SEED").expect("a seed");
            let crash = var(&line, "KBF_SIM_CRASH");
            let back = Scenario::from_replay(&kind, &seed, crash.as_deref()).expect("parses");
            assert_eq!(format!("{back:?}"), format!("{s:?}"), "{line}");
        }
        for (kind, seed, crash) in [
            ("Stormy", "1", None),
            ("Calm", "x", None),
            ("Calm", "1", Some("put-sent/kill")),
            ("StoreCrash", "1", Some("put-sent")),
            ("StoreCrash", "1", Some("nowhere/kill")),
            ("StoreCrash", "1", Some("put-sent/off")),
            ("TierCrash", "1", Some("put-sent/kill")),
            ("StoreCrash", "1", Some("tier-acked/kill")),
        ] {
            let read = Scenario::from_replay(kind, seed, crash);
            assert!(read.is_none(), "{kind} {seed} {crash:?}");
        }
    }

    /// Catches a renumbered property: failure lines and the docs name them so.
    #[test]
    fn each_property_prints_its_number() {
        let all = [
            Property::ReplicaEquality,
            Property::OneRun,
            Property::LeasesKept,
            Property::ExactlyOneResult,
            Property::NoFalsePresent,
            Property::MonotoneTime,
            Property::Liveness,
            Property::NoFalseAck,
            Property::NothingHalfIndexed,
            Property::IdsNeverMove,
            Property::WithinBudget,
            Property::NoLeak,
            Property::Drained,
            Property::AnsweredWithOutputs,
        ];
        let names: Vec<String> = all.iter().map(ToString::to_string).collect();
        let want = [
            "1 (replica equality)",
            "2 (one run)",
            "3 (leases kept)",
            "4 (exactly one result)",
            "5 (no false present)",
            "6 (monotone time)",
            "7 (liveness)",
            "8 (no false ack)",
            "9 (nothing half-indexed)",
            "10 (ids never move)",
            "11 (within budget)",
            "12 (no leak)",
            "13 (drained)",
            "14 (answered with outputs)",
        ];
        assert_eq!(names, want);
    }
}
