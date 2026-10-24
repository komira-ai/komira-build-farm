//! The ports of the roll: everything the state machine asks of the outside world. The real
//! adapters are [`crate::safe_http`] (safe-to-stop), [`crate::ssh_installer`] (install),
//! [`crate::node_drain`] (drain) and [`crate::node_health`] (health). [`crate::fakes`] has one scripted fake per port.
//!
//! The traits are synchronous: a roll is one host at a time and waits on each answer, so
//! there is nothing to overlap, and a caller runs [`crate::Updater::drive`] on its own thread.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::inventory::Host;
use crate::version::Version;

/// Why a port call failed, in the one distinction the roll acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortErrorKind {
    /// The host or service could not be reached or did not answer (a refused connection, a
    /// timeout, a 5xx): no answer, as opposed to an answer that says no.
    Unreachable,
    /// Anything else: the far side answered and refused, or failed.
    Other,
}

/// A port could not be asked or did not answer: a refused connection, a timeout, a 5xx. One
/// line, never raw ssh or apt output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortError {
    kind: PortErrorKind,
    text: String,
}

impl PortError {
    /// No answer: the host could not be reached.
    pub fn unreachable(text: impl Into<String>) -> PortError {
        PortError {
            kind: PortErrorKind::Unreachable,
            text: text.into(),
        }
    }

    /// An answer that is a refusal or a failure.
    pub fn other(text: impl Into<String>) -> PortError {
        PortError {
            kind: PortErrorKind::Other,
            text: text.into(),
        }
    }

    pub fn kind(&self) -> PortErrorKind {
        self.kind
    }

    pub fn is_unreachable(&self) -> bool {
        self.kind == PortErrorKind::Unreachable
    }
}

/// `PortError("..".into())` builds an [`PortErrorKind::Other`] error, as the tuple struct this
/// type was did.
#[allow(non_snake_case)]
pub fn PortError(text: String) -> PortError {
    PortError::other(text)
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            PortErrorKind::Unreachable => write!(f, "unreachable: {}", self.text),
            PortErrorKind::Other => f.write_str(&self.text),
        }
    }
}

impl std::error::Error for PortError {}

/// The answer of `GET /v1/members/{id}/safe-to-stop`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub safe: bool,
    pub reasons: Vec<String>,
    pub planned_down: bool,
}

/// What safe-to-stop said about a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The cluster does not know the host (404).
    Unknown,
    Answer(Verdict),
}

/// Whether a host may be stopped now.
pub trait SafeToStop: Send + Sync {
    fn check(&self, host: &Host) -> Result<Stop, PortError>;
}

/// What the host reports as installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The package version installed (any text the package manager reports).
    Version(String),
    /// An install is running now (the package lock is held).
    InProgress,
    /// The package is not installed: the query's `installed_version` is `null`. A fresh host.
    /// There is nothing to be older than, so the roll installs on it like on any other that
    /// is not at the target.
    NotInstalled,
}

/// What the host's `query` reported: the installed package, and who the host says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub installed: Installed,
    /// The commit of the kbf build on the host (`running_commit` of the query), if it says.
    pub running_commit: Option<String>,
    /// The node id from the host's kbf configuration (`node_id` of the query), if it has one.
    pub node_id: Option<String>,
    /// How the host's last apply stands (`apply` of a Mac's query); none on a Linux host and
    /// on a Mac that has had no apply.
    pub apply: Option<Apply>,
}

/// How a Mac's last apply stands: the `apply` of its `query`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Apply {
    Running,
    Done,
    /// `failed:<reason>`: the reason as the host spells it (lower case letters, `_` and `:`;
    /// the tail says what became of the old release).
    Failed(String),
}

/// What the host said to an install that it accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallAnswer {
    /// The package manager finished: the target is installed (a Linux host).
    Installed,
    /// The target was already installed and running; nothing ran.
    AlreadyInstalled,
    /// The release is checked and its apply is launched, and goes on by itself (a Mac): the
    /// host's `query` says how it ends.
    ApplyStarted,
}

/// The host-side install (the forced-command wrapper over ssh, later).
pub trait Installer: Send + Sync {
    /// Install `version`. A Linux host answers when the package manager is done; a Mac
    /// answers [`InstallAnswer::ApplyStarted`] and finishes on its own. `Err` carries one line
    /// for a person: the wrapper's refusal or the failed install. An older version than the
    /// installed one is installed too: a rollback is an install.
    fn install(&self, host: &Host, version: &Version) -> Result<InstallAnswer, PortError>;
    /// What the host reports, without changing anything.
    fn report(&self, host: &Host) -> Result<Report, PortError>;
    /// What is installed, without changing anything.
    fn query(&self, host: &Host) -> Result<Installed, PortError> {
        self.report(host).map(|r| r.installed)
    }
}

/// A shared installer is an installer: the health check asks the host what it runs through
/// the same one the roll installs with.
impl<T: Installer + ?Sized> Installer for std::sync::Arc<T> {
    fn install(&self, host: &Host, version: &Version) -> Result<InstallAnswer, PortError> {
        (**self).install(host, version)
    }

    fn report(&self, host: &Host) -> Result<Report, PortError> {
        (**self).report(host)
    }
}

/// Taking a daemon host's node out of placement at the frontend, and giving it back.
pub trait Drain: Send + Sync {
    /// Cordon the node of `host` and ask the frontend to wait for its leases for `deadline`.
    fn drain(&self, host: &Host, deadline: Duration) -> Result<(), PortError>;
    /// How many leases the node of `host` holds now.
    fn leases(&self, host: &Host) -> Result<usize, PortError>;
    /// Return the node of `host` to placement. This is unconditional: a node an operator
    /// cordoned before the roll is uncordoned after it, so cordon it again if it must stay out.
    /// A storage host is never drained, even one that also runs a daemon, and needs no node id.
    fn undrain(&self, host: &Host) -> Result<(), PortError>;
}

/// Whether a host runs the target yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// The frontend's active child runs the target and the swap is complete; or the node is
    /// Active and its `running_commit` is the target's.
    Healthy,
    /// Not yet; the text says what is missing.
    Waiting(String),
    /// The host will not get there: an abandoned swap. Fails the roll without waiting out the
    /// bound.
    Abandoned(String),
}

/// The health of a host, by its role.
pub trait HealthCheck: Send + Sync {
    /// Why `host` must not be installed on at all, if the adapter could not judge it after:
    /// asked before the installer is called for the host, and a reason fails the roll there.
    fn refusal(&self, host: &Host) -> Option<String>;

    fn check(&self, host: &Host, target: &Version) -> Result<Health, PortError>;

    /// [`HealthCheck::check`] for a host whose node the roll holds drained: the node is
    /// not asked to be serving, which it cannot be until the roll gives it back.
    fn check_held(&self, host: &Host, target: &Version) -> Result<Health, PortError>;
}

/// Time. A roll waits only through this port, so a test runs hours of timeouts in no time.
pub trait Clock: Send + Sync {
    /// A monotonic reading; only differences mean anything.
    fn now(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// The real clock: monotonic time since it was made, and a thread sleep.
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> SystemClock {
        SystemClock {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> SystemClock {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a system clock that does not move or does not wait: the roll's bounds would
    /// never expire, or would expire at once.
    #[test]
    fn the_system_clock_moves_and_sleeps() {
        let clock = SystemClock::default();
        let before = clock.now();
        clock.sleep(Duration::from_millis(5));
        assert!(clock.now() - before >= Duration::from_millis(5));
    }

    /// Catches a verdict that does not read the #1489 body, which is what the safe-to-stop
    /// adapter will deserialize.
    #[test]
    fn a_verdict_reads_the_safe_to_stop_body() {
        let v: Verdict = serde_json::from_str(
            "{\"safe\":false,\"reasons\":[\"m2 is behind\"],\"planned_down\":true}",
        )
        .expect("body");
        assert_eq!(
            v,
            Verdict {
                safe: false,
                reasons: vec!["m2 is behind".into()],
                planned_down: true
            }
        );
        assert_eq!(PortError("down".into()).to_string(), "down");
    }

    /// Catches a shared installer that does not reach the one it holds: install and report
    /// through an `Arc` are the inner installer's, and its default `query` is the report's
    /// installed part.
    #[test]
    fn a_shared_installer_is_the_installer_it_holds() {
        let log = crate::fakes::Log::default();
        let inner = crate::fakes::FakeInstaller::new(&log);
        let shared: std::sync::Arc<dyn Installer> = std::sync::Arc::new(inner.clone());
        let host = Host {
            alias: "a".to_owned(),
            address: "h.example".to_owned(),
            role: crate::inventory::Role::Daemon,
            host_key: "ssh-ed25519 AAAA".to_owned(),
            node_id: None,
        };
        let v = Version::parse("2.0").expect("v");
        shared.install(&host, &v).expect("install");
        assert_eq!(inner.installed("a"), Installed::Version("2.0".into()));
        let report = shared.report(&host).expect("report");
        assert_eq!(report.installed, Installed::Version("2.0".into()));
        assert_eq!(report.node_id.as_deref(), Some("a-node"));
        assert_eq!(shared.query(&host), Ok(Installed::Version("2.0".into())));
    }

    /// An installer that does nothing, and keeps the default of the restart.
    struct Plain;

    impl Installer for Plain {
        fn install(&self, _host: &Host, _version: &Version) -> Result<InstallAnswer, PortError> {
            Err(PortError::other("install"))
        }

        fn report(&self, _host: &Host) -> Result<Report, PortError> {
            Err(PortError::other("report"))
        }
    }

    /// Asserts that a shared installer passes every call on to the one it holds.
    #[test]
    fn a_shared_installer_passes_the_calls_on() {
        let shared: std::sync::Arc<dyn Installer> = std::sync::Arc::new(Plain);
        let host = Host {
            alias: "a".to_owned(),
            address: "h.example".to_owned(),
            role: crate::inventory::Role::Daemon,
            host_key: "ssh-ed25519 AAAA".to_owned(),
            node_id: None,
        };
        let v = Version::parse("2.0").expect("v");
        fn refused<T>(what: &str) -> Result<T, PortError> {
            Err(PortError::other(what))
        }
        assert_eq!(shared.install(&host, &v), refused("install"));
        assert_eq!(shared.report(&host), refused("report"));
        assert_eq!(shared.query(&host), refused("report"));
    }

    /// Catches a kind that is lost or shown wrongly: the tuple-style call is `Other`, the
    /// constructors set their kind, and only `Unreachable` is announced in the text.
    #[test]
    fn a_port_error_has_a_kind() {
        let other = PortError("refused".into());
        assert_eq!(other.kind(), PortErrorKind::Other);
        assert!(!other.is_unreachable());
        assert_eq!(other, PortError::other("refused"));
        let gone = PortError::unreachable("ssh: timed out");
        assert_eq!(gone.kind(), PortErrorKind::Unreachable);
        assert!(gone.is_unreachable());
        assert_eq!(gone.to_string(), "unreachable: ssh: timed out");
        assert_ne!(gone, PortError::other("ssh: timed out"));
    }
}
