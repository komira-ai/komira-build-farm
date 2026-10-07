//! `kbf-daemon`: the worker daemon, v0.
//!
//! It holds one outbound mutual-TLS `kbf.worker.v1` stream to a server front, reports
//! its node (detected through `kbf-caps`), sends heartbeats, and runs a lease manager:
//! work starts only on `Start`, each lease reports one `Result`, and every running
//! lease is killed and reported when no heartbeat has been acknowledged for the fence
//! time T. Execution goes through the [`Runtime`] trait. The container driver
//! (`kbf-driver-container`) implements it; the `kbf-daemon` binary still offers only
//! [`FakeRuntime`], because the driver depends on this crate and Cargo refuses the
//! cycle a binary here naming the driver would make.
//!
//! - [`config`]: command-line flags and TLS files.
//! - [`report`]: the node report and its hash.
//! - [`runtime`]: the runtime trait and the fake.
//! - [`Daemon`]: the session loop, with the contact clock and lease manager inside.

pub mod config;
mod contact;
mod daemon;
mod lease;
pub mod report;
pub mod runtime;

pub use config::{Args, DaemonConfig, FENCE_AFTER, RuntimeKind, TlsFiles};
pub use daemon::{Daemon, Event, PROTOCOL_VERSION, SessionError};
pub use report::NodeReport;
pub use runtime::{FakeRuntime, Runtime, RuntimeError, Work};
