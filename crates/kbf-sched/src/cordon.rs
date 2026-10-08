//! Cordon and drain: an operator takes a worker out of placement without stopping its
//! work (`docs/design/fleet-updates.md` sections 3.3 and 4.2, steps 3 and 4).
//!
//! A **cordoned** worker is offered no new lease; the leases it holds run on. A
//! **drain** cordons the worker and waits, until a deadline, for those leases to end:
//! it is **drained** once the worker holds none, and **paused** if the deadline passes
//! first. A drain never kills or gives up a lease. A paused drain stays paused even
//! when the leases end later: nothing proceeds by itself (section 4.3); the operator
//! drains again with a new deadline, or uncordons. An uncordon ends either.
//!
//! A cordon names a worker, not a session: it holds across the worker's streams, so
//! a node that reboots during its update comes back cordoned. It is scheduler state
//! fed as an input, not a control-log record yet: like the rest of the scheduler's
//! state it is gone after a server restart.

use std::collections::BTreeMap;

use kbf_types::{FarmTime, WorkerId};

/// Where a cordoned worker is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cordon {
    /// Placement skips it. Nothing is waited for.
    Cordoned,
    /// Placement skips it, and its leases are waited for until `deadline`.
    Draining {
        /// When the drain pauses if leases still run.
        deadline: FarmTime,
    },
    /// It holds no lease: it may be taken out of service.
    Drained,
    /// `deadline` passed while it still held leases. They run on; the drain waits for
    /// an operator.
    Paused {
        /// The deadline that passed.
        deadline: FarmTime,
    },
}

/// Every cordoned worker, by name.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cordons(BTreeMap<WorkerId, Cordon>);

impl Cordons {
    /// Whether placement must skip `worker`.
    pub(crate) fn skips(&self, worker: &WorkerId) -> bool {
        self.0.contains_key(worker)
    }

    pub(crate) fn get(&self, worker: &WorkerId) -> Option<&Cordon> {
        self.0.get(worker)
    }

    /// Cordons `worker`. A drain already under way is left as it is.
    pub(crate) fn cordon(&mut self, worker: WorkerId) {
        self.0.entry(worker).or_insert(Cordon::Cordoned);
    }

    /// Cordons `worker` and (re)starts its drain with `deadline`.
    pub(crate) fn drain(&mut self, worker: WorkerId, deadline: FarmTime) {
        self.0.insert(worker, Cordon::Draining { deadline });
    }

    /// Ends `worker`'s cordon and any drain.
    pub(crate) fn uncordon(&mut self, worker: &WorkerId) {
        self.0.remove(worker);
    }

    /// Moves every drain on at `now`: drained once `holds` says the worker holds no
    /// lease, else paused once the deadline has passed.
    pub(crate) fn progress(&mut self, now: FarmTime, holds: impl Fn(&WorkerId) -> bool) {
        for (worker, cordon) in &mut self.0 {
            if let Cordon::Draining { deadline } = *cordon {
                if !holds(worker) {
                    *cordon = Cordon::Drained;
                } else if now >= deadline {
                    *cordon = Cordon::Paused { deadline };
                }
            }
        }
    }
}
