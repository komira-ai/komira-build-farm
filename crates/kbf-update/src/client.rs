//! The update client: a [`Handle`] the daemon's session feeds with the frontend's offers
//! and orders, and a [`Worker`] that fetches in the background and watches the helper.
//!
//! What the client decides, and how:
//!
//! - **Offers** are level-triggered. One is acted on only if its generation is above every
//!   generation seen, so the frontend may send the same offer after every Welcome. An
//!   offer that fails [`Offer::check`] is refused with the reason and nothing is fetched.
//!   A newer generation abandons a fetch in flight. One fetch runs at a time.
//! - **Orders** are accepted only from the epoch of the current session, above every
//!   order id accepted before, for a commit the helper reports as held, and an activation
//!   only with no lease live. An accepted activation closes the update gate.
//! - **Status** is derived from what the client and the helper's status file say, and
//!   published on a watch channel after every change.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_threads::disk::{FreeSpace, Statvfs};
use tokio::sync::{Notify, watch};

use crate::floors::{Floors, FloorsError, FloorsFile};
use crate::helper::{self, Helper, HelperState, Report};
use crate::inbox::Inbox;
use crate::source::Source;
use crate::stage::{Backoff, Job, Stop, stage};
use crate::types::{Offer, Order, State, Status, Verb};

/// How a client is set up.
#[derive(Clone)]
pub struct Config {
    /// The update directory (`--update-dir`): the inbox is under it, and the helper's
    /// status file in it.
    pub dir: PathBuf,
    /// The platform of this node, as offers name it.
    pub platform: String,
    /// The commit this process was built from.
    pub running_commit: String,
    /// How often the helper's status file is read.
    pub poll_every: Duration,
    /// How long a failed fetch waits before it tries again.
    pub backoff: Backoff,
    /// Where free disk space is read from.
    pub space: Arc<dyn FreeSpace + Send + Sync>,
}

impl Config {
    /// The configuration of the node `platform` running `running_commit`, with the
    /// update directory `dir`.
    #[must_use]
    pub fn new(dir: PathBuf, platform: String, running_commit: String) -> Self {
        Self {
            dir,
            platform,
            running_commit,
            poll_every: Duration::from_secs(2),
            backoff: Backoff::default(),
            space: Arc::new(Statvfs),
        }
    }
}

/// Why a client cannot start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("update directory {path}: {source}")]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    State(#[from] FloorsError),
}

/// What the client did with an order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Written to the inbox; the status acknowledges it.
    Accepted,
    /// The same order again: nothing written, the status acknowledges it again.
    Replayed,
    /// Not written, and why.
    Refused(String),
}

impl Decision {
    /// Why the order was refused; `None` when it was not.
    #[must_use]
    pub fn refusal(&self) -> Option<&str> {
        match self {
            Self::Refused(why) => Some(why),
            Self::Accepted | Self::Replayed => None,
        }
    }
}

/// The update gate: closed from an accepted activation until the helper settles it.
#[derive(Debug)]
struct Gate {
    order_id: u64,
    commit: String,
}

/// What the client knows, under one lock.
struct Core {
    /// The newest offer generation acted on, refused ones included.
    seen: u64,
    floors: Floors,
    /// The commit of the newest accepted offer.
    offered_commit: Option<String>,
    /// An accepted offer the worker has not taken yet.
    pending: Option<Offer>,
    fetching: bool,
    refusal: Option<String>,
    acked_order: u64,
    helper: Helper,
    gate: Option<Gate>,
}

impl Core {
    /// Takes `offer` if it is newer than any seen; whether the worker has work now.
    fn offer(&mut self, offer: Offer, platform: &str) -> bool {
        if offer.generation <= self.seen {
            return false;
        }
        self.seen = offer.generation;
        // A newer offer, refused or not, replaces one the worker has not started.
        self.pending = None;
        if let Err(why) = offer.check(platform) {
            tracing::warn!(generation = offer.generation, %why, "offer refused");
            self.refusal = Some(why.to_string());
            return false;
        }
        self.refusal = None;
        self.offered_commit = Some(offer.commit.clone());
        self.pending = Some(offer);
        true
    }

    /// Opens the gate again when the helper has settled the order that closed it
    /// without restarting this process.
    fn settle_gate(&mut self) {
        let Some(gate) = &self.gate else { return };
        if let Helper::Reported(report) = &self.helper
            && report.acked_order_id >= gate.order_id
            && matches!(report.state, HelperState::Reverted | HelperState::Refused)
        {
            tracing::info!(order = gate.order_id, "the gate opens");
            self.gate = None;
        }
    }

    fn report(&self) -> Option<&Report> {
        match &self.helper {
            Helper::Reported(report) => Some(report),
            Helper::Missing | Helper::Unreadable(_) => None,
        }
    }

    /// The state and the reason for it.
    fn state(&self) -> (State, String) {
        if let Helper::Unreadable(why) = &self.helper {
            return (State::Refused, why.clone());
        }
        if let Some(why) = &self.refusal {
            return (State::Refused, why.clone());
        }
        if self.fetching || self.pending.is_some() {
            return (State::Fetching, String::new());
        }
        let Some(report) = self.report() else {
            return (State::Manual, String::new());
        };
        let state = match report.state {
            HelperState::Idle => State::Idle,
            HelperState::Staged if report.staged == self.offered_commit => State::Staged,
            HelperState::Staged => State::Idle,
            HelperState::ActivatedUnconfirmed => State::ActivatedUnconfirmed,
            HelperState::Confirmed => State::Confirmed,
            HelperState::Reverted => State::Reverted,
            HelperState::Refused => return (State::Refused, report.refusal.clone()),
        };
        (state, String::new())
    }

    fn status(&self, running_commit: &str) -> Status {
        let (state, refusal) = self.state();
        let report = self.report();
        Status {
            running_commit: running_commit.to_owned(),
            installed_commit: report.and_then(|r| r.installed.clone()),
            staged_commit: report.and_then(|r| r.staged.clone()),
            state,
            refusal,
            acked_order_id: self.acked_order,
            offered_generation: self.seen,
            helper_present: self.helper != Helper::Missing,
            staged: report.and_then(|r| r.staged_facts),
            gated: self.gate.is_some(),
        }
    }
}

struct Shared {
    config: Config,
    source: Arc<dyn Source>,
    inbox: Inbox,
    floors: FloorsFile,
    core: Mutex<Core>,
    status: watch::Sender<Status>,
    /// The newest generation seen; a fetch of an older one ends.
    newest: watch::Sender<u64>,
    /// Rung when the worker has an offer to take.
    wake: Notify,
}

impl Shared {
    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, core: &Core) {
        let now = core.status(&self.config.running_commit);
        self.status.send_if_modified(|status| {
            let changed = *status != now;
            if changed {
                *status = now;
            }
            changed
        });
    }

    /// Reads the helper's status file again.
    fn refresh(&self) {
        let mut core = self.core();
        core.helper = helper::read(&self.config.dir.join("status.json"));
        core.settle_gate();
        self.publish(&core);
    }

    fn offer(&self, offer: Offer) {
        let mut core = self.core();
        let work = core.offer(offer, &self.config.platform);
        self.newest.send_replace(core.seen);
        self.publish(&core);
        if work {
            self.wake.notify_one();
        }
    }

    /// Takes the offer the worker is to fetch.
    fn begin_fetch(&self) -> Option<Offer> {
        let mut core = self.core();
        let offer = core.pending.take()?;
        core.fetching = true;
        self.publish(&core);
        Some(offer)
    }

    /// Fetches `offer` into the inbox.
    async fn fetch(&self, offer: &Offer) -> Result<(), Stop> {
        let job = Job {
            source: &*self.source,
            inbox: &self.inbox,
            space: &*self.config.space,
            dir: &self.config.dir,
            backoff: self.config.backoff,
        };
        let mut newer = self.newest.subscribe();
        let generation = offer.generation;
        let abandoned = Box::pin(async move {
            // The sender lives as long as `self`: the wait cannot fail.
            let _ = newer.wait_for(|seen| *seen > generation).await;
        });
        stage(&job, offer, abandoned).await
    }

    fn end_fetch(&self, offer: &Offer, outcome: Result<(), Stop>) {
        let mut core = self.core();
        core.fetching = false;
        match outcome {
            Ok(()) => {
                tracing::info!(generation = offer.generation, commit = %offer.commit, "offer staged in the inbox");
                core.floors.generation = core.floors.generation.max(offer.generation);
                if let Err(e) = self.floors.save(&core.floors) {
                    // The directory is in the inbox, which a restart finds.
                    tracing::warn!(error = %e, "the newest generation could not be recorded");
                }
            }
            Err(Stop::Refused(why)) if offer.generation == core.seen => {
                tracing::warn!(generation = offer.generation, %why, "offer could not be fetched");
                core.refusal = Some(why);
            }
            Err(Stop::Refused(_) | Stop::Abandoned) => {}
        }
        self.publish(&core);
    }

    fn order(&self, order: &Order, epoch: u64, live: &dyn Fn() -> usize) -> Decision {
        let mut core = self.core();
        core.helper = helper::read(&self.config.dir.join("status.json"));
        let decision = self.decide(&mut core, order, epoch, live);
        match &decision {
            Decision::Refused(why) => {
                tracing::warn!(order = order.order_id, %why, "order refused");
                core.refusal = Some(why.clone());
            }
            Decision::Accepted | Decision::Replayed => core.refusal = None,
        }
        self.publish(&core);
        decision
    }

    fn decide(
        &self,
        core: &mut Core,
        order: &Order,
        epoch: u64,
        live: &dyn Fn() -> usize,
    ) -> Decision {
        let id = order.order_id;
        if order.epoch != epoch {
            return Decision::Refused(format!(
                "order {id} is of epoch {}; this session's is {epoch}",
                order.epoch
            ));
        }
        // The same order again is answered, not written twice. (If its file is missing,
        // the first try died between recording the id and writing it: write it now.)
        if id > 0
            && id == core.floors.order_id
            && epoch == core.floors.order_epoch
            && self.inbox.has_order(id)
        {
            core.acked_order = id;
            return Decision::Replayed;
        }
        // Epoch first, then id: a restored store raises the epoch and counts ids again, so
        // a lower id of a newer epoch is newer, and any id of an older epoch is not.
        if epoch < core.floors.order_epoch {
            return Decision::Refused(format!(
                "order {id} is of epoch {epoch}, older than the newest accepted, {}",
                core.floors.order_epoch
            ));
        }
        if id == 0 || (epoch == core.floors.order_epoch && id < core.floors.order_id) {
            return Decision::Refused(format!(
                "order id {id} is not above the newest accepted, {}",
                core.floors.order_id
            ));
        }
        let Some(verb) = order.verb else {
            return Decision::Refused(format!("order {id} names no verb"));
        };
        let Some(report) = core.report() else {
            return Decision::Refused(
                "no update helper reports on this host, so it cannot take an order".to_owned(),
            );
        };
        if !report.holds(&order.commit) {
            return Decision::Refused(format!(
                "commit {} is neither staged nor installed on this host",
                order.commit
            ));
        }
        let activate = verb == Verb::Activate;
        let leases = if activate { live() } else { 0 };
        if leases > 0 {
            return Decision::Refused(format!("{leases} leases are live; not activating"));
        }
        let floors = Floors {
            order_id: id,
            order_epoch: epoch,
            ..core.floors
        };
        if let Err(e) = self.floors.save(&floors) {
            return Decision::Refused(format!("cannot record order {id}: {e}"));
        }
        core.floors = floors;
        // Immediately before the order is written: the first look may be a while ago.
        let leases = if activate { live() } else { 0 };
        if leases > 0 {
            return Decision::Refused(format!("{leases} leases are live; not activating"));
        }
        if let Err(e) = self.inbox.write_order(order, verb.name()) {
            return Decision::Refused(format!("cannot write order {id}: {e}"));
        }
        core.acked_order = id;
        match verb {
            Verb::Activate => {
                core.gate = Some(Gate {
                    order_id: id,
                    commit: order.commit.clone(),
                });
            }
            Verb::Revert => core.gate = None,
            Verb::Confirm => {}
        }
        Decision::Accepted
    }
}

/// The part of the client the daemon's session talks to. Cheap to clone.
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}

impl Handle {
    /// Takes an offer the frontend sent. Returns at once; the worker fetches.
    pub fn offer(&self, offer: Offer) {
        self.shared.offer(offer);
    }

    /// Decides an order the frontend sent. `epoch` is the lease epoch of the current
    /// session's Welcome (0 when it named none); `live` counts the leases that are live
    /// now, and is asked again immediately before an activation is written.
    #[must_use]
    pub fn order(&self, order: &Order, epoch: u64, live: &dyn Fn() -> usize) -> Decision {
        self.shared.order(order, epoch, live)
    }

    /// The status, now and after each change.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<Status> {
        self.shared.status.subscribe()
    }

    /// Reads the helper's status file now, rather than at the worker's next look.
    pub fn refresh(&self) {
        self.shared.refresh();
    }

    /// While the update gate is closed, the reason a lease cannot start.
    #[must_use]
    pub fn gate(&self) -> Option<String> {
        let core = self.shared.core();
        let gate = core.gate.as_ref()?;
        Some(format!("update to {} ordered", &gate.commit[..12]))
    }
}

/// The background half: fetches offers one at a time and re-reads the helper's status.
pub struct Worker {
    shared: Arc<Shared>,
}

impl Worker {
    /// Runs until it is dropped.
    pub async fn run(self) {
        let never = tokio::select! {
            never = self.poll() => never,
            never = self.fetch() => never,
        };
        match never {}
    }

    async fn poll(&self) -> Infallible {
        let mut tick = tokio::time::interval(self.shared.config.poll_every);
        loop {
            tick.tick().await;
            self.shared.refresh();
        }
    }

    async fn fetch(&self) -> Infallible {
        loop {
            self.shared.wake.notified().await;
            while let Some(offer) = self.shared.begin_fetch() {
                let outcome = self.shared.fetch(&offer).await;
                self.shared.end_fetch(&offer, outcome);
            }
        }
    }
}

/// Starts a client.
///
/// # Errors
/// The update directory or the client's state file cannot be used. The client does not
/// start on a state it cannot read.
pub fn start(config: Config, source: Arc<dyn Source>) -> Result<(Handle, Worker), StartError> {
    let io = |source| StartError::Directory {
        path: config.dir.clone(),
        source,
    };
    let inbox = Inbox::open(&config.dir).map_err(io)?;
    let (floors_file, floors) = FloorsFile::open(&config.dir)?;
    let offered_commit = inbox.offers().map_err(io)?.pop().map(|(_, commit)| commit);
    let core = Core {
        seen: floors.generation,
        floors,
        offered_commit,
        pending: None,
        fetching: false,
        refusal: None,
        // An order recorded and written before a restart is still acknowledged.
        acked_order: if inbox.has_order(floors.order_id) {
            floors.order_id
        } else {
            0
        },
        helper: helper::read(&config.dir.join("status.json")),
        gate: None,
    };
    let (status, _) = watch::channel(core.status(&config.running_commit));
    let shared = Arc::new(Shared {
        newest: watch::channel(core.seen).0,
        config,
        source,
        inbox,
        floors: floors_file,
        core: Mutex::new(core),
        status,
        wake: Notify::new(),
    });
    Ok((
        Handle {
            shared: Arc::clone(&shared),
        },
        Worker { shared },
    ))
}

#[cfg(test)]
mod harness;
#[cfg(test)]
mod order_tests;
#[cfg(test)]
mod tests;
