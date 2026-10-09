//! `kbf-daemon`: the worker daemon.
//!
//! It holds one outbound mutual-TLS `kbf.worker.v1` stream to a server front, reports
//! its node (detected through `kbf-caps`), sends heartbeats, and runs a lease manager:
//! work starts only on `Start`, and only while the `Start` is inside the window it
//! names (issue #23); each lease reports one `Result`, kept until the server
//! acknowledges it; a lease the server cancels is killed; and every running lease is
//! killed and reported when no heartbeat has been acknowledged for the fence time T,
//! measured on a clock that counts the time the machine was suspended.
//! Execution goes through the [`Runtime`] trait. A runtime reads an action and its inputs from the front's CAS and writes the
//! outputs back through a [`Cas`]; [`CasClient`] is the one that talks to a front.
//!
//! - [`config`]: command-line flags and TLS files.
//! - [`report`]: the node report and its hash.
//! - [`status`]: the node's software status (OS, kernel, daemon, Xcodes).
//! - [`runtime`]: the runtime trait and [`FakeRuntime`], which runs nothing.
//! - [`cas`]: the [`Cas`] trait and the front's client.
//! - [`clock`]: the suspend-counting [`Clock`] the fence reads (issue #78).
//! - [`tree`]: writing an input root from the CAS and reading outputs back.
//! - [`usage`]: what an action used, measured by the kernel, and where it is carried.
//! - [`LocalRuntime`]: **tests only**, runs actions as plain child processes.
//! - [`Daemon`]: the session loop, with the contact clock and lease manager inside.
//!
//! The `kbf-daemon` binary lives in the `kbf-node` crate: it picks a driver
//! (`--driver fake|container|native`), and the drivers depend on this crate, so a
//! binary here naming them would be a cycle Cargo refuses. It never offers
//! [`LocalRuntime`], which isolates nothing.

pub mod cas;
pub mod clock;
pub mod config;
mod contact;
mod daemon;
mod lease;
#[cfg(target_os = "linux")]
mod local;
pub mod report;
pub mod runtime;
pub mod status;
pub mod tree;
#[cfg(target_os = "linux")]
pub mod usage;
mod window;

/// The daemon's version as it reports it (`daemon_version` in `Hello` and
/// `NodeStatus`, which `GET /v1/nodes` shows): the package version, `+`, and the
/// commit it was built from (12 hex digits), or `unknown` when the build had no git
/// checkout (issue #170). `build.rs` embeds the commit.
pub const DAEMON_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT"));

pub use cas::{Cas, CasClient, CasError};
pub use clock::{Clock, Moment, SystemClock};
pub use config::{Args, DaemonConfig, FENCE_AFTER, RECHECK_EVERY, TlsFiles};
pub use daemon::{Daemon, Event, PROTOCOL_VERSION, SessionError};
#[cfg(target_os = "linux")]
pub use local::{LOCAL_DRIVER, LocalRuntime};
pub use report::NodeReport;
pub use runtime::{FakeRuntime, Runtime, RuntimeError, Work};

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::DAEMON_VERSION;

    /// Catches (issue #170): a version that names the package version alone, so two
    /// builds of it read the same in `/v1/nodes`; and a commit that is not this
    /// checkout's (the build script fell back to `unknown`, or embedded another).
    #[test]
    fn the_version_names_the_commit_built() {
        let (package, commit) = DAEMON_VERSION.split_once('+').expect("a + in the version");
        assert_eq!(package, env!("CARGO_PKG_VERSION"));
        let head = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .expect("git runs");
        assert!(head.status.success(), "the tests run in a git checkout");
        let head = String::from_utf8(head.stdout).expect("UTF-8");
        assert_eq!(commit, head.trim());
        assert_eq!(commit.len(), 12, "{commit}");
        assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()), "{commit}");
    }
}
