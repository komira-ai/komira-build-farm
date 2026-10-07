//! `kbf-daemon`: the worker daemon.
//!
//! It holds one outbound mutual-TLS `kbf.worker.v1` stream to a server front, reports
//! its node (detected through `kbf-caps`), sends heartbeats, and runs a lease manager:
//! work starts only on `Start`, each lease reports one `Result`, kept until the server
//! acknowledges it, and every running lease is killed and reported when no heartbeat
//! has been acknowledged for the fence time T. Execution goes through the [`Runtime`]
//! trait. A runtime reads an action and its inputs from the front's CAS and writes the
//! outputs back through a [`Cas`]; [`CasClient`] is the one that talks to a front.
//!
//! - [`config`]: command-line flags and TLS files.
//! - [`report`]: the node report and its hash.
//! - [`runtime`]: the runtime trait and [`FakeRuntime`], which runs nothing.
//! - [`cas`]: the [`Cas`] trait and the front's client.
//! - [`tree`]: writing an input root from the CAS and reading outputs back.
//! - [`usage`]: what an action used, measured by the kernel, and where it is carried.
//! - [`LocalRuntime`]: **tests only**, runs actions as plain child processes.
//! - [`Daemon`]: the session loop, with the contact clock and lease manager inside.
//!
//! The `kbf-daemon` binary still offers only [`FakeRuntime`]: the container driver
//! (`kbf-driver-container`) is the runtime for farm nodes, and [`LocalRuntime`]
//! isolates nothing.

pub mod cas;
pub mod config;
mod contact;
mod daemon;
mod lease;
#[cfg(target_os = "linux")]
mod local;
pub mod report;
pub mod runtime;
pub mod tree;
#[cfg(target_os = "linux")]
pub mod usage;

pub use cas::{Cas, CasClient, CasError};
pub use config::{Args, DaemonConfig, FENCE_AFTER, RuntimeKind, TlsFiles};
pub use daemon::{Daemon, Event, PROTOCOL_VERSION, SessionError};
#[cfg(target_os = "linux")]
pub use local::{LOCAL_DRIVER, LocalRuntime};
pub use report::NodeReport;
pub use runtime::{FakeRuntime, Runtime, RuntimeError, Work};
