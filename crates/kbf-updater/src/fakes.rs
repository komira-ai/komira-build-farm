//! One scripted fake per port, for this crate's tests and for the adapters' tests to come.
//!
//! Every fake is a cheap handle (clones share state) and records what it was asked in one
//! shared [`Log`], so a test can say not only that host B was installed but that it was
//! installed after host A was healthy, and that host C was never asked about at all. Unscripted
//! answers are the happy path: safe to stop, install succeeds, healthy at once.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::inventory::{Host, Role};
use crate::ports::{
    Apply, Clock, Drain, Health, HealthCheck, InstallAnswer, Installed, Installer, PortError,
    Report, SafeToStop, Stop, Verdict,
};
use crate::version::Version;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Something a port was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Check(String),
    Install(String, String),
    Query(String),
    Health(String),
    /// A node drain was asked for, with the deadline in seconds.
    Drain(String, u64),
    /// The node's leases were read.
    Leases(String),
    Undrain(String),
}

impl Event {
    /// The call as a short word: its kind, then the detail a test tells calls apart by (the
    /// version of an install and the deadline of a drain).
    pub fn word(&self) -> String {
        match self {
            Event::Check(_) => "check".to_owned(),
            Event::Install(_, version) => format!("install {version}"),
            Event::Query(_) => "query".to_owned(),
            Event::Health(_) => "health".to_owned(),
            Event::Drain(_, secs) => format!("drain {secs}"),
            Event::Leases(_) => "leases".to_owned(),
            Event::Undrain(_) => "undrain".to_owned(),
        }
    }
}

/// The calls made to the fakes, in order.
#[derive(Clone, Default)]
pub struct Log(Arc<Mutex<Vec<Event>>>);

impl Log {
    fn push(&self, e: Event) {
        lock(&self.0).push(e);
    }

    pub fn events(&self) -> Vec<Event> {
        lock(&self.0).clone()
    }

    /// The aliases installed, in order, one entry per install call.
    pub fn installs(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Install(alias, _) => Some(alias),
                _ => None,
            })
            .collect()
    }

    /// Every call that names `alias`.
    pub fn touching(&self, alias: &str) -> Vec<Event> {
        self.events()
            .into_iter()
            .filter(|e| match e {
                Event::Check(a)
                | Event::Install(a, _)
                | Event::Query(a)
                | Event::Health(a)
                | Event::Drain(a, _)
                | Event::Leases(a)
                | Event::Undrain(a) => a == alias,
            })
            .collect()
    }

    pub fn clear(&self) {
        lock(&self.0).clear();
    }
}

/// Per alias: answers to give first, in order, then one answer for ever after.
struct Script<T: Clone> {
    map: Mutex<BTreeMap<String, (VecDeque<T>, T)>>,
    default: T,
}

impl<T: Clone> Script<T> {
    fn new(default: T) -> Script<T> {
        Script {
            map: Mutex::new(BTreeMap::new()),
            default,
        }
    }

    fn set(&self, alias: &str, first: Vec<T>, then: T) {
        lock(&self.map).insert(alias.to_owned(), (first.into(), then));
    }

    fn has(&self, alias: &str) -> bool {
        lock(&self.map).contains_key(alias)
    }

    fn next(&self, alias: &str) -> T {
        match lock(&self.map).get_mut(alias) {
            Some((queue, then)) => queue.pop_front().unwrap_or_else(|| then.clone()),
            None => self.default.clone(),
        }
    }
}

/// A clock that moves only when something sleeps on it.
#[derive(Clone, Default)]
pub struct FakeClock(Arc<Mutex<Duration>>);

impl FakeClock {
    pub fn new() -> FakeClock {
        FakeClock::default()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        *lock(&self.0)
    }

    fn sleep(&self, d: Duration) {
        *lock(&self.0) += d;
    }
}

/// The verdict "safe, nothing to report".
pub fn safe() -> Stop {
    Stop::Answer(Verdict {
        safe: true,
        reasons: Vec::new(),
        planned_down: false,
    })
}

/// A verdict "not safe", with one reason.
pub fn unsafe_because(reason: &str) -> Stop {
    Stop::Answer(Verdict {
        safe: false,
        reasons: vec![reason.to_owned()],
        planned_down: false,
    })
}

#[derive(Clone)]
pub struct FakeSafeToStop {
    log: Log,
    script: Arc<Script<Result<Stop, PortError>>>,
}

impl FakeSafeToStop {
    pub fn new(log: &Log) -> FakeSafeToStop {
        FakeSafeToStop {
            log: log.clone(),
            script: Arc::new(Script::new(Ok(safe()))),
        }
    }

    pub fn script(
        &self,
        alias: &str,
        first: Vec<Result<Stop, PortError>>,
        then: Result<Stop, PortError>,
    ) {
        self.script.set(alias, first, then);
    }
}

impl SafeToStop for FakeSafeToStop {
    fn check(&self, host: &Host) -> Result<Stop, PortError> {
        self.log.push(Event::Check(host.alias.clone()));
        self.script.next(&host.alias)
    }
}

/// The node id and the running commit a host reports.
type Identity = (Option<String>, Option<String>);

type Hook = Arc<dyn Fn(&Host) + Send + Sync>;

#[derive(Clone)]
pub struct FakeInstaller {
    log: Log,
    installed: Arc<Mutex<BTreeMap<String, Installed>>>,
    /// `None` answers as the host's role does: `Installed` for a Linux host, `ApplyStarted`
    /// for a Mac.
    installs: Arc<Script<Result<Option<InstallAnswer>, PortError>>>,
    /// Per alias: the apply state a Mac reports on each query once an apply is going.
    applies: Arc<Script<Option<Apply>>>,
    /// The Macs whose apply has started: an install was answered, or a test said so.
    applying: Arc<Mutex<BTreeSet<String>>>,
    queries: Arc<Script<Option<Result<Installed, PortError>>>>,
    /// Per alias: the node id and the running commit the host reports.
    identity: Arc<Mutex<BTreeMap<String, Identity>>>,
    hook: Arc<Mutex<Option<Hook>>>,
}

impl FakeInstaller {
    pub fn new(log: &Log) -> FakeInstaller {
        FakeInstaller {
            log: log.clone(),
            installed: Arc::default(),
            installs: Arc::new(Script::new(Ok(None))),
            applies: Arc::new(Script::new(None)),
            applying: Arc::default(),
            queries: Arc::new(Script::new(None)),
            identity: Arc::default(),
            hook: Arc::default(),
        }
    }

    /// What the host has installed before any install (unscripted hosts have `0.0.0`).
    pub fn set_installed(&self, alias: &str, what: Installed) {
        lock(&self.installed).insert(alias.to_owned(), what);
    }

    pub fn installed(&self, alias: &str) -> Installed {
        lock(&self.installed)
            .get(alias)
            .cloned()
            .unwrap_or_else(|| Installed::Version("0.0.0".to_owned()))
    }

    /// Answers of `install` for a host: `first` in order, then `then`. A successful install
    /// makes the host report the version.
    pub fn script_install(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        let answer = |r: Result<(), PortError>| r.map(|()| None);
        self.installs
            .set(alias, first.into_iter().map(answer).collect(), answer(then));
    }

    /// As [`FakeInstaller::script_install`], with the answer each install gives.
    pub fn script_install_answers(
        &self,
        alias: &str,
        first: Vec<Result<InstallAnswer, PortError>>,
        then: Result<InstallAnswer, PortError>,
    ) {
        let answer = |r: Result<InstallAnswer, PortError>| r.map(Some);
        self.installs
            .set(alias, first.into_iter().map(answer).collect(), answer(then));
    }

    /// The apply state a Mac host reports on each query once its apply has started: `first`
    /// in order, then `then`. A Mac reports none before that (an install answered, or
    /// [`FakeInstaller::begin_apply`]) and, unscripted, `done` after; a Linux host always
    /// reports none.
    pub fn script_apply(&self, alias: &str, first: Vec<Option<Apply>>, then: Option<Apply>) {
        self.applies.set(alias, first, then);
    }

    /// The Mac's apply is going now, as if an install had started it before the test began.
    pub fn begin_apply(&self, alias: &str) {
        lock(&self.applying).insert(alias.to_owned());
        if !self.applies.has(alias) {
            self.applies.set(alias, Vec::new(), Some(Apply::Done));
        }
    }

    /// Answers of `query` for a host: `first` in order, then what the host has installed.
    pub fn script_query(&self, alias: &str, first: Vec<Result<Installed, PortError>>) {
        self.queries
            .set(alias, first.into_iter().map(Some).collect(), None);
    }

    /// What the host reports as its node id and running commit. Unscripted hosts report the
    /// node id `<alias>-node` and no commit.
    pub fn set_identity(&self, alias: &str, node_id: Option<&str>, running_commit: Option<&str>) {
        lock(&self.identity).insert(
            alias.to_owned(),
            (
                node_id.map(str::to_owned),
                running_commit.map(str::to_owned),
            ),
        );
    }

    /// Call `f` at the start of every install (a test reads the record from it).
    pub fn on_install(&self, f: impl Fn(&Host) + Send + Sync + 'static) {
        *lock(&self.hook) = Some(Arc::new(f));
    }
}

impl Installer for FakeInstaller {
    fn install(&self, host: &Host, version: &Version) -> Result<InstallAnswer, PortError> {
        self.log
            .push(Event::Install(host.alias.clone(), version.to_string()));
        // Cloned out, so a hook that re-enters the installer does not deadlock on the slot.
        let hook = lock(&self.hook).clone();
        if let Some(hook) = hook {
            hook(host);
        }
        let answer = self.installs.next(&host.alias)?.unwrap_or(match host.role {
            Role::MacDaemon => InstallAnswer::ApplyStarted,
            _ => InstallAnswer::Installed,
        });
        self.set_installed(&host.alias, Installed::Version(version.to_string()));
        self.begin_apply(&host.alias);
        Ok(answer)
    }

    fn report(&self, host: &Host) -> Result<Report, PortError> {
        self.log.push(Event::Query(host.alias.clone()));
        let installed = self
            .queries
            .next(&host.alias)
            .unwrap_or_else(|| Ok(self.installed(&host.alias)))?;
        let (node_id, running_commit) = lock(&self.identity)
            .get(&host.alias)
            .cloned()
            .unwrap_or_else(|| (Some(format!("{}-node", host.alias)), None));
        let apply = match host.role {
            Role::MacDaemon if lock(&self.applying).contains(&host.alias) => {
                self.applies.next(&host.alias)
            }
            _ => None,
        };
        Ok(Report {
            installed,
            running_commit,
            node_id,
            apply,
        })
    }
}

/// A scripted drain of nodes: every call is in the [`Log`] by the host's alias.
#[derive(Clone)]
pub struct FakeDrain {
    log: Log,
    drains: Arc<Script<Result<(), PortError>>>,
    leases: Arc<Script<Result<usize, PortError>>>,
    undrains: Arc<Script<Result<(), PortError>>>,
}

impl FakeDrain {
    pub fn new(log: &Log) -> FakeDrain {
        FakeDrain {
            log: log.clone(),
            drains: Arc::new(Script::new(Ok(()))),
            leases: Arc::new(Script::new(Ok(0))),
            undrains: Arc::new(Script::new(Ok(()))),
        }
    }

    pub fn script_drain(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        self.drains.set(alias, first, then);
    }

    /// The lease counts a node reports, in order, then `then` for ever after.
    pub fn script_leases(
        &self,
        alias: &str,
        first: Vec<Result<usize, PortError>>,
        then: Result<usize, PortError>,
    ) {
        self.leases.set(alias, first, then);
    }

    pub fn script_undrain(
        &self,
        alias: &str,
        first: Vec<Result<(), PortError>>,
        then: Result<(), PortError>,
    ) {
        self.undrains.set(alias, first, then);
    }
}

impl Drain for FakeDrain {
    fn drain(&self, host: &Host, deadline: Duration) -> Result<(), PortError> {
        self.log
            .push(Event::Drain(host.alias.clone(), deadline.as_secs()));
        self.drains.next(&host.alias)
    }

    fn leases(&self, host: &Host) -> Result<usize, PortError> {
        self.log.push(Event::Leases(host.alias.clone()));
        self.leases.next(&host.alias)
    }

    fn undrain(&self, host: &Host) -> Result<(), PortError> {
        self.log.push(Event::Undrain(host.alias.clone()));
        self.undrains.next(&host.alias)
    }
}

#[derive(Clone)]
pub struct FakeHealth {
    log: Log,
    script: Arc<Script<Result<Health, PortError>>>,
    /// Answers for a host the roll holds drained; unscripted hosts answer as `script` does.
    held: Arc<Script<Option<Result<Health, PortError>>>>,
}

impl FakeHealth {
    pub fn new(log: &Log) -> FakeHealth {
        FakeHealth {
            log: log.clone(),
            script: Arc::new(Script::new(Ok(Health::Healthy))),
            held: Arc::new(Script::new(None)),
        }
    }

    pub fn script(
        &self,
        alias: &str,
        first: Vec<Result<Health, PortError>>,
        then: Result<Health, PortError>,
    ) {
        self.script.set(alias, first, then);
    }

    /// The answers of `check_held` for a host: `first` in order, then `then`.
    pub fn script_held(
        &self,
        alias: &str,
        first: Vec<Result<Health, PortError>>,
        then: Result<Health, PortError>,
    ) {
        self.held
            .set(alias, first.into_iter().map(Some).collect(), Some(then));
    }
}

impl HealthCheck for FakeHealth {
    fn refusal(&self, _host: &Host) -> Option<String> {
        None
    }

    fn check(&self, host: &Host, _target: &Version) -> Result<Health, PortError> {
        self.log.push(Event::Health(host.alias.clone()));
        self.script.next(&host.alias)
    }

    fn check_held(&self, host: &Host, _target: &Version) -> Result<Health, PortError> {
        self.log.push(Event::Health(host.alias.clone()));
        self.held
            .next(&host.alias)
            .unwrap_or_else(|| self.script.next(&host.alias))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Role;

    fn host(alias: &str) -> Host {
        Host {
            alias: alias.to_owned(),
            address: "h.example".to_owned(),
            role: Role::Storage,
            host_key: "ssh-ed25519 AAAA".to_owned(),
            node_id: None,
        }
    }

    /// Catches fakes that do not behave as their scripts say, which would make every roll test
    /// below prove nothing: answers come in order and then repeat the last, unscripted hosts
    /// get the happy path, a successful install shows in the next query, a failed one does
    /// not, a scripted query overrides the installed state once, and the log keeps order.
    #[test]
    fn the_fakes_follow_their_scripts_and_log_in_order() {
        let log = Log::default();
        let (a, b) = (host("a"), host("b"));
        let v = Version::parse("2.0").expect("v");

        let stop = FakeSafeToStop::new(&log);
        stop.script(
            "a",
            vec![Ok(unsafe_because("m2 behind"))],
            Err(PortError("down".into())),
        );
        assert_eq!(stop.check(&a), Ok(unsafe_because("m2 behind")));
        assert_eq!(stop.check(&a), Err(PortError("down".into())));
        assert_eq!(stop.check(&a), Err(PortError("down".into())));
        assert_eq!(stop.check(&b), Ok(safe()));

        let inst = FakeInstaller::new(&log);
        assert_eq!(inst.query(&a), Ok(Installed::Version("0.0.0".into())));
        inst.script_install("b", vec![Err(PortError("lock".into()))], Ok(()));
        assert_eq!(inst.install(&b, &v), Err(PortError("lock".into())));
        assert_eq!(inst.installed("b"), Installed::Version("0.0.0".into()));
        assert_eq!(inst.install(&b, &v), Ok(InstallAnswer::Installed));
        assert_eq!(inst.query(&b), Ok(Installed::Version("2.0".into())));
        inst.script_query("b", vec![Ok(Installed::InProgress)]);
        assert_eq!(inst.query(&b), Ok(Installed::InProgress));
        assert_eq!(inst.query(&b), Ok(Installed::Version("2.0".into())));
        inst.set_installed("a", Installed::InProgress);
        assert_eq!(inst.installed("a"), Installed::InProgress);

        let health = FakeHealth::new(&log);
        health.script(
            "a",
            vec![Ok(Health::Waiting("swap".into()))],
            Ok(Health::Healthy),
        );
        assert_eq!(health.check(&a, &v), Ok(Health::Waiting("swap".into())));
        assert_eq!(health.check(&a, &v), Ok(Health::Healthy));
        assert_eq!(health.check(&b, &v), Ok(Health::Healthy));

        assert_eq!(log.installs(), ["b", "b"]);
        assert_eq!(log.touching("b").len(), 7);
        assert_eq!(log.events()[0], Event::Check("a".into()));
        log.clear();
        assert!(log.events().is_empty());
    }

    /// Catches a hook that is not called before the install result (the point where a test
    /// looks at the record on disk).
    #[test]
    fn the_install_hook_runs_before_the_install_is_recorded_as_done() {
        let log = Log::default();
        let inst = FakeInstaller::new(&log);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let probe = inst.clone();
        inst.on_install(move |h| {
            lock(&sink).push(probe.installed(&h.alias));
        });
        let v = Version::parse("2.0").expect("v");
        inst.install(&host("a"), &v).expect("ok");
        assert_eq!(*lock(&seen), [Installed::Version("0.0.0".into())]);
    }

    /// Catches a clock that does not advance on sleep: every timeout test would spin.
    #[test]
    fn the_fake_clock_moves_by_sleeping() {
        let clock = FakeClock::new();
        clock.sleep(Duration::from_secs(5));
        clock.sleep(Duration::from_secs(5));
        assert_eq!(clock.now(), Duration::from_secs(10));
    }

    /// Asserts that every kind of call has its own word, with the detail the tests tell calls
    /// apart by: the version of an install and the deadline of a drain.
    #[test]
    fn every_call_has_its_own_word() {
        let a = || "a".to_owned();
        let words: Vec<String> = [
            Event::Check(a()),
            Event::Install(a(), "2.0".into()),
            Event::Query(a()),
            Event::Health(a()),
            Event::Drain(a(), 600),
            Event::Leases(a()),
            Event::Undrain(a()),
        ]
        .iter()
        .map(Event::word)
        .collect();
        assert_eq!(
            words,
            [
                "check",
                "install 2.0",
                "query",
                "health",
                "drain 600",
                "leases",
                "undrain",
            ]
        );
    }
}
