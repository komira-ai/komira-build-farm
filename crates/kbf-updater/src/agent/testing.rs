//! Fakes of the agent's ports, for its tests: one shared log of what happened in what order,
//! a helper that behaves like the real one (a release on trial, confirmed or reverted), a
//! drain that remembers, a health check that is told what to say, and a clock a test moves.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::coordinator::{Coordinator, CoordinatorError};
use super::local::{Ended, HostState, Local, LocalError, Staged, Trial};
use crate::inventory::{Host, Role};
use crate::ports::{Clock, Drain, Health, HealthCheck, PortError};
use crate::version::Version;
use crate::wire::{self, Heartbeat, Reply};

pub const COMMIT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const COMMIT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
pub const VA: &str = "0.1.5+gaaaaaaaaaaaa-2";
pub const VB: &str = "0.1.7+gbbbbbbbbbbbb-3";

pub fn version(text: &str) -> Version {
    Version::parse(text).expect("a version")
}

pub fn host(role: Role) -> Host {
    Host {
        alias: "n1".to_owned(),
        address: "localhost".to_owned(),
        role,
        host_key: String::new(),
        node_id: Some("n1".to_owned()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What happened, in order, across all the fakes.
#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<Vec<String>>>);

impl Events {
    pub fn push(&self, event: impl Into<String>) {
        lock(&self.0).push(event.into());
    }

    pub fn all(&self) -> Vec<String> {
        lock(&self.0).clone()
    }

    /// The events that start with one of `prefixes`.
    pub fn of(&self, prefixes: &[&str]) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|e| prefixes.iter().any(|p| e.starts_with(p)))
            .collect()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.of(&[prefix]).len()
    }
}

/// A clock a test moves: `sleep` moves it.
#[derive(Default)]
pub struct ManualClock(Mutex<Duration>);

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        *lock(&self.0) += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *lock(&self.0)
    }

    fn sleep(&self, d: Duration) {
        self.advance(d);
    }
}

/// The helper's world: what runs, what is on trial, how the next calls go.
pub struct LocalWorld {
    pub state: HostState,
    pub install: Result<Staged, LocalError>,
    pub switch: Result<u64, LocalError>,
    pub confirm: Result<(), LocalError>,
    pub revert: Result<(), LocalError>,
    pub query: Result<(), LocalError>,
    /// The helper stops answering `query` once it has been asked to go back.
    pub mute_after_revert: bool,
    /// The version of the release that ran before the one on trial.
    pub before: Option<Version>,
    /// Called at the start of an install, without the lock held.
    pub on_install: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Called at the start of a confirm, without the lock held.
    pub on_confirm: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// A helper that behaves like the real one.
#[derive(Clone)]
pub struct FakeLocal {
    pub events: Events,
    pub world: Arc<Mutex<LocalWorld>>,
}

impl FakeLocal {
    /// A host that runs `VA`, with `VB` on offer and a switch whose deadline is `deadline`.
    pub fn new(events: &Events) -> Self {
        let world = LocalWorld {
            state: HostState {
                commit: Some(COMMIT_A.to_owned()),
                version: Some(version(VA)),
                node_id: Some("n1".to_owned()),
                pending: None,
                last: None,
                busy: false,
            },
            install: Ok(Staged {
                commit: COMMIT_B.to_owned(),
                current: false,
            }),
            switch: Ok(10_600),
            confirm: Ok(()),
            revert: Ok(()),
            query: Ok(()),
            mute_after_revert: false,
            before: None,
            on_install: None,
            on_confirm: None,
        };
        Self {
            events: events.clone(),
            world: Arc::new(Mutex::new(world)),
        }
    }

    pub fn world(&self) -> MutexGuard<'_, LocalWorld> {
        lock(&self.world)
    }
}

impl Local for FakeLocal {
    fn query(&self) -> Result<HostState, LocalError> {
        self.events.push("local:query");
        let world = self.world();
        world.query.clone()?;
        Ok(world.state.clone())
    }

    fn install(&self, version: &Version, channel: &str) -> Result<Staged, LocalError> {
        self.events
            .push(format!("local:install {version} {channel}"));
        let hook = self.world().on_install.clone();
        if let Some(hook) = hook {
            hook();
        }
        self.world().install.clone()
    }

    fn switch(&self, version: &Version) -> Result<u64, LocalError> {
        self.events.push(format!("local:switch {version}"));
        let mut world = self.world();
        let deadline = world.switch.clone()?;
        world.before = world.state.version.clone();
        world.state.pending = Some(Trial {
            version: Some(version.clone()),
            commit: COMMIT_B.to_owned(),
            deadline_unix: Some(deadline),
        });
        world.state.commit = Some(COMMIT_B.to_owned());
        world.state.version = Some(version.clone());
        Ok(deadline)
    }

    fn confirm(&self, version: &Version) -> Result<(), LocalError> {
        self.events.push(format!("local:confirm {version}"));
        let hook = self.world().on_confirm.clone();
        if let Some(hook) = hook {
            hook();
        }
        let mut world = self.world();
        world.confirm.clone()?;
        world.state.pending = None;
        world.state.last = Some(Ended {
            target: version.to_string(),
            outcome: wire::Outcome::Succeeded,
            reason: None,
        });
        Ok(())
    }

    fn revert(&self) -> Result<(), LocalError> {
        self.events.push("local:revert");
        let mut world = self.world();
        world.revert.clone()?;
        let target = world.state.pending.take().and_then(|t| t.version);
        world.state.version = world.before.clone();
        world.state.commit = Some(COMMIT_A.to_owned());
        if world.mute_after_revert {
            world.query = Err(LocalError::Unreachable("gone".to_owned()));
        }
        world.state.last = target.map(|t| Ended {
            target: t.to_string(),
            outcome: wire::Outcome::Failed,
            reason: Some("reverted".to_owned()),
        });
        Ok(())
    }
}

/// A drain that remembers whether the node is out.
#[derive(Clone)]
pub struct FakeDrain {
    pub events: Events,
    pub drained: Arc<Mutex<bool>>,
    /// The leases the node holds on each look; the last repeats.
    pub leases: Arc<Mutex<VecDeque<usize>>>,
    pub drain_error: Arc<Mutex<Option<PortError>>>,
    pub leases_error: Arc<Mutex<Option<PortError>>>,
    pub undrain_error: Arc<Mutex<Option<PortError>>>,
}

impl FakeDrain {
    pub fn new(events: &Events) -> Self {
        Self {
            events: events.clone(),
            drained: Arc::default(),
            leases: Arc::new(Mutex::new(VecDeque::from([0]))),
            drain_error: Arc::default(),
            leases_error: Arc::default(),
            undrain_error: Arc::default(),
        }
    }

    pub fn is_drained(&self) -> bool {
        *lock(&self.drained)
    }
}

impl Drain for FakeDrain {
    fn drain(&self, host: &Host, deadline: Duration) -> Result<(), PortError> {
        self.events
            .push(format!("drain {} {}", host.alias, deadline.as_secs()));
        if let Some(e) = lock(&self.drain_error).clone() {
            return Err(e);
        }
        *lock(&self.drained) = true;
        Ok(())
    }

    fn leases(&self, _host: &Host) -> Result<usize, PortError> {
        self.events.push("leases");
        if let Some(e) = lock(&self.leases_error).clone() {
            return Err(e);
        }
        let mut leases = lock(&self.leases);
        Ok(if leases.len() > 1 {
            leases.pop_front().unwrap_or(0)
        } else {
            leases.front().copied().unwrap_or(0)
        })
    }

    fn undrain(&self, _host: &Host) -> Result<(), PortError> {
        self.events.push("undrain");
        if let Some(e) = lock(&self.undrain_error).clone() {
            return Err(e);
        }
        *lock(&self.drained) = false;
        Ok(())
    }
}

/// The answers a health check gives for a version, in turn.
pub type Answers = VecDeque<Result<Health, PortError>>;

/// A health check told what each version looks like.
#[derive(Clone)]
pub struct FakeHealth {
    pub events: Events,
    /// What `check_held` and `check` say of a version, until it is told otherwise. Each
    /// version has a queue; the last answer repeats.
    pub says: Arc<Mutex<Vec<(String, Answers)>>>,
    pub refuses: Arc<Mutex<Option<String>>>,
}

impl FakeHealth {
    pub fn new(events: &Events) -> Self {
        Self {
            events: events.clone(),
            says: Arc::default(),
            refuses: Arc::default(),
        }
    }

    /// `version` answers `answers` in turn, and the last for ever.
    pub fn say(&self, version: &str, answers: Vec<Result<Health, PortError>>) {
        let mut says = lock(&self.says);
        says.retain(|(v, _)| v != version);
        says.push((version.to_owned(), answers.into()));
    }

    fn answer(&self, kind: &str, version: &Version) -> Result<Health, PortError> {
        self.events.push(format!("health:{kind} {version}"));
        let mut says = lock(&self.says);
        let Some((_, queue)) = says.iter_mut().find(|(v, _)| v == version.as_str()) else {
            return Ok(Health::Waiting("nothing said".to_owned()));
        };
        if queue.len() > 1 {
            queue.pop_front().unwrap_or(Ok(Health::Healthy))
        } else {
            queue.front().cloned().unwrap_or(Ok(Health::Healthy))
        }
    }
}

impl HealthCheck for FakeHealth {
    fn refusal(&self, _host: &Host) -> Option<String> {
        lock(&self.refuses).clone()
    }

    fn check(&self, _host: &Host, target: &Version) -> Result<Health, PortError> {
        self.answer("check", target)
    }

    fn check_held(&self, _host: &Host, target: &Version) -> Result<Health, PortError> {
        self.answer("held", target)
    }
}

/// A coordinator that records the heartbeats it is sent and gives the same reply to each.
#[derive(Clone, Default)]
pub struct FakeCoordinator {
    pub beats: Arc<Mutex<Vec<Heartbeat>>>,
    /// The reply to every heartbeat; none: nothing.
    pub answer: Arc<Mutex<Option<Result<Reply, CoordinatorError>>>>,
}

impl FakeCoordinator {
    pub fn beats(&self) -> Vec<Heartbeat> {
        lock(&self.beats).clone()
    }

    pub fn reply(&self, reply: Result<Reply, CoordinatorError>) {
        *lock(&self.answer) = Some(reply);
    }
}

impl Coordinator for FakeCoordinator {
    fn heartbeat(&self, beat: &Heartbeat) -> Result<Reply, CoordinatorError> {
        lock(&self.beats).push(beat.clone());
        lock(&self.answer).clone().unwrap_or(Ok(Reply::Nothing))
    }
}
