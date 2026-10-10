//! Which live workers can run a request, for one placement round.
//!
//! A cordoned worker is not one of them: placement skips it, and work only it could
//! run waits with that reason for as long as the cordon lasts; it is never refused for
//! it (a cordon is temporary by intent). A worker can run a request if it reports a
//! driver that serves the request's lease kind ([`LeaseKind::drivers`]), its
//! capabilities satisfy the request's platform (`kbf_caps::Request::matches`) and its
//! whole capacity holds the request vector. It can run it now if, for an `action`, its
//! free room holds the vector and no whole-machine lease holds it; for a
//! `whole_machine` lease, if it holds no lease at all. Matches are memoised per
//! distinct platform request and kind within the round, so a queue of many actions
//! with the same platform matches each live worker once.

use std::collections::{BTreeMap, BTreeSet};

use kbf_caps::NodeCaps;
use kbf_types::{FarmTime, LeaseKind, Resources, WorkerId};

use crate::cordon::Cordons;
use crate::input::Request;
use crate::scheduler::Worker;

/// Whether some live worker could run a request, ignoring what is booked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// A live worker satisfies the platform and is large enough.
    Servable,
    /// None is; why, for the request's callers.
    Unservable(String),
    /// None that placement may use is, but a cordoned one is; why, naming them. The
    /// work waits: a cordon is temporary by intent, so it is not refused for it.
    Cordoned(String),
}

/// The live workers of one round, and the platform matches made so far.
#[derive(Debug)]
pub(crate) struct Servable {
    /// Live workers placement may use, in name order.
    live: Vec<WorkerId>,
    /// Live workers placement skips because they are cordoned, in name order.
    cordoned: Vec<WorkerId>,
    /// For each platform request and lease kind seen this round, the indices into
    /// `live` of the workers that serve the kind and satisfy the request.
    memo: Vec<((kbf_caps::Request, LeaseKind), Vec<usize>)>,
}

/// Whether a node that offers `caps` runs a driver serving `kind`.
pub(crate) fn serves(caps: &NodeCaps, kind: LeaseKind) -> bool {
    caps.drivers.iter().any(|driver| kind.served_by(driver))
}

impl Servable {
    /// The round's view of `workers` at `now`, without the `cordons`.
    pub(crate) fn new(
        workers: &BTreeMap<WorkerId, Worker>,
        cordons: &Cordons,
        now: FarmTime,
    ) -> Self {
        let (cordoned, live) = workers
            .iter()
            .filter(|(_, w)| w.alive(now))
            .map(|(name, _)| name.clone())
            .partition(|name| cordons.skips(name));
        Self {
            live,
            cordoned,
            memo: Vec::new(),
        }
    }

    /// Where in `memo` the live workers that serve `request`'s kind and satisfy its
    /// platform are listed.
    fn matching(&mut self, workers: &BTreeMap<WorkerId, Worker>, request: &Request) -> usize {
        let key = (request.needs.clone(), request.kind);
        if let Some(at) = self.memo.iter().position(|(seen, _)| *seen == key) {
            return at;
        }
        let matching = self
            .live
            .iter()
            .enumerate()
            .filter(|(_, name)| {
                let caps = &workers[*name].caps;
                serves(caps, request.kind) && request.needs.matches(caps)
            })
            .map(|(i, _)| i)
            .collect();
        self.memo.push((key, matching));
        self.memo.len() - 1
    }

    /// The first live worker, in name order and not in `reserved`, that serves
    /// `request`'s kind, satisfies its platform and has room for it now. Books it there
    /// and returns the worker with what was booked: the request vector for an action,
    /// the worker's whole capacity for a whole-machine lease.
    pub(crate) fn fit(
        &mut self,
        workers: &mut BTreeMap<WorkerId, Worker>,
        request: &Request,
        reserved: &BTreeSet<WorkerId>,
    ) -> Option<(WorkerId, Resources)> {
        let at = self.matching(workers, request);
        let name = self.memo[at]
            .1
            .iter()
            .map(|&i| &self.live[i])
            .find(|name| !reserved.contains(*name) && workers[*name].has_room(request))?
            .clone();
        let worker = workers.get_mut(&name).expect("live workers are registered");
        let booked = match request.kind {
            LeaseKind::WholeMachine => {
                worker.whole = true;
                worker.capacity
            }
            LeaseKind::Action | LeaseKind::Vm => request.resources,
        };
        worker.booked = worker.booked.saturating_add(booked);
        worker.leases += 1;
        Some((name, booked))
    }

    /// The worker to hold for a whole-machine `request` that fits nowhere now: `kept`
    /// if it still could run it, else the one that could which holds the fewest leases,
    /// then has the least booked, then comes first by name. Never one in `reserved`.
    pub(crate) fn reserve(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
        reserved: &BTreeSet<WorkerId>,
        kept: Option<&WorkerId>,
    ) -> Option<WorkerId> {
        let at = self.matching(workers, request);
        let candidates: Vec<&WorkerId> = self.memo[at]
            .1
            .iter()
            .map(|&i| &self.live[i])
            .filter(|name| {
                !reserved.contains(*name) && workers[*name].capacity.fits(&request.resources)
            })
            .collect();
        if let Some(kept) = kept.filter(|kept| candidates.contains(kept)) {
            return Some(kept.clone());
        }
        candidates
            .into_iter()
            .min_by_key(|name| {
                let w = &workers[*name];
                (w.leases, w.booked)
            })
            .cloned()
    }

    /// Whether any live worker could run `request` once its bookings end, and if none
    /// could, why. When only cordoned workers could, that is the verdict; the cordoned
    /// workers are looked at only then.
    pub(crate) fn verdict(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> Verdict {
        let verdict = self.uncordoned_verdict(workers, request);
        if verdict == Verdict::Servable {
            return verdict;
        }
        let could: Vec<&str> = self
            .cordoned
            .iter()
            .filter(|name| {
                let w = &workers[*name];
                serves(&w.caps, request.kind)
                    && request.needs.matches(&w.caps)
                    && w.capacity.fits(&request.resources)
            })
            .map(WorkerId::as_str)
            .collect();
        if could.is_empty() {
            return verdict;
        }
        Verdict::Cordoned(format!(
            "every live worker that can run it is cordoned: {}",
            could.join(", ")
        ))
    }

    /// [`Self::verdict`] over the workers placement may use.
    fn uncordoned_verdict(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> Verdict {
        if self.live.is_empty() {
            let why = if self.cordoned.is_empty() {
                "no worker is connected"
            } else {
                "every connected worker is cordoned"
            };
            return Verdict::Unservable(why.to_owned());
        }
        let kind = request.kind;
        let serving = self
            .live
            .iter()
            .filter(|name| serves(&workers[*name].caps, kind))
            .count();
        if serving == 0 {
            return Verdict::Unservable(format!(
                "none of the {} live worker(s) serves lease kind {kind}: that needs a \
                 driver among {}",
                self.live.len(),
                kind.drivers().join(", ")
            ));
        }
        let at = self.matching(workers, request);
        let matching = &self.memo[at].1;
        if matching.is_empty() {
            let workers_named = if serving == self.live.len() {
                format!("{serving} live worker(s)")
            } else {
                format!("{serving} live worker(s) serving lease kind {kind}")
            };
            let (closest, unmet) = self
                .live
                .iter()
                .filter(|name| serves(&workers[*name].caps, kind))
                .map(|name| (name, request.needs.unmet(&workers[name].caps)))
                .min_by_key(|(_, unmet)| unmet.len())
                .expect("there is a live worker");
            let unmet: Vec<String> = unmet.iter().map(ToString::to_string).collect();
            return Verdict::Unservable(format!(
                "none of the {workers_named} satisfies the action's platform; the closest, \
                 {closest}, lacks {}",
                unmet.join(", ")
            ));
        }
        let large_enough = matching
            .iter()
            .any(|&i| workers[&self.live[i]].capacity.fits(&request.resources));
        if large_enough {
            return Verdict::Servable;
        }
        let r = request.resources;
        Verdict::Unservable(format!(
            "the {} live worker(s) that satisfy the action's platform are all smaller than \
             its request ({} millicores, {} bytes of memory, {} GPU(s))",
            matching.len(),
            r.cpu_millis,
            r.memory_bytes,
            r.gpus
        ))
    }
}
