//! Which live workers can run a request, for one placement round.
//!
//! A worker can run a request if its capabilities satisfy the request's platform
//! (`kbf_caps::Request::matches`) and its whole capacity holds the request vector; it
//! can run it now if its free room does. Matches are memoised per distinct platform
//! request within the round, so a queue of many actions with the same platform matches
//! each live worker once.

use std::collections::BTreeMap;

use kbf_types::{FarmTime, WorkerId};

use crate::input::Request;
use crate::scheduler::Worker;

/// Whether some live worker could run a request, ignoring what is booked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// A live worker satisfies the platform and is large enough.
    Servable,
    /// None is; why, for the request's callers.
    Unservable(String),
}

/// The live workers of one round, and the platform matches made so far.
#[derive(Debug)]
pub(crate) struct Servable {
    /// Live workers, in name order.
    live: Vec<WorkerId>,
    /// For each platform request seen this round, the indices into `live` of the
    /// workers that satisfy it.
    memo: Vec<(kbf_caps::Request, Vec<usize>)>,
}

impl Servable {
    /// The round's view of `workers` at `now`.
    pub(crate) fn new(workers: &BTreeMap<WorkerId, Worker>, now: FarmTime) -> Self {
        let live = workers
            .iter()
            .filter(|(_, w)| w.alive(now))
            .map(|(name, _)| name.clone())
            .collect();
        Self {
            live,
            memo: Vec::new(),
        }
    }

    /// Where in `memo` the live workers that satisfy `needs` are listed.
    fn matching(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        needs: &kbf_caps::Request,
    ) -> usize {
        if let Some(at) = self.memo.iter().position(|(seen, _)| seen == needs) {
            return at;
        }
        let matching = self
            .live
            .iter()
            .enumerate()
            .filter(|(_, name)| needs.matches(&workers[*name].caps))
            .map(|(i, _)| i)
            .collect();
        self.memo.push((needs.clone(), matching));
        self.memo.len() - 1
    }

    /// The first live worker, in name order, that satisfies `request`'s platform and
    /// has room for it now. Books the request on it.
    pub(crate) fn fit(
        &mut self,
        workers: &mut BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> Option<WorkerId> {
        let at = self.matching(workers, &request.needs);
        let name = self.memo[at]
            .1
            .iter()
            .map(|&i| &self.live[i])
            .find(|name| workers[*name].free().fits(&request.resources))?
            .clone();
        let worker = workers.get_mut(&name).expect("live workers are registered");
        worker.booked = worker.booked.saturating_add(request.resources);
        Some(name)
    }

    /// Whether any live worker could run `request` once its bookings end, and if none
    /// could, why.
    pub(crate) fn verdict(
        &mut self,
        workers: &BTreeMap<WorkerId, Worker>,
        request: &Request,
    ) -> Verdict {
        if self.live.is_empty() {
            return Verdict::Unservable("no worker is connected".to_owned());
        }
        let at = self.matching(workers, &request.needs);
        let matching = &self.memo[at].1;
        if matching.is_empty() {
            let (closest, unmet) = self
                .live
                .iter()
                .map(|name| (name, request.needs.unmet(&workers[name].caps)))
                .min_by_key(|(_, unmet)| unmet.len())
                .expect("there is a live worker");
            let unmet: Vec<String> = unmet.iter().map(ToString::to_string).collect();
            return Verdict::Unservable(format!(
                "none of the {} live worker(s) satisfies the action's platform; the \
                 closest, {closest}, lacks {}",
                self.live.len(),
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
