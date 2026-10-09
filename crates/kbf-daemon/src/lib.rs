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

pub use cas::{Cas, CasClient, CasError};
pub use clock::{Clock, Moment, SystemClock};
pub use config::{Args, DaemonConfig, FENCE_AFTER, RECHECK_EVERY, TlsFiles};
pub use daemon::{Daemon, Event, PROTOCOL_VERSION, SessionError};
#[cfg(target_os = "linux")]
pub use local::{LOCAL_DRIVER, LocalRuntime};
pub use report::NodeReport;
pub use runtime::{FakeRuntime, Runtime, RuntimeError, Work};
pub use status::DriverReport;
