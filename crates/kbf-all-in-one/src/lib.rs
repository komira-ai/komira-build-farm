//! `kbf-all-in-one`: the farm server and a worker daemon in one process, for a one-node
//! Linux farm (the README says what it does today and what it does not do yet; the
//! design is komira-build-farm#657). The daemon's blobs move through calls into
//! the server's cache ([`LocalDataPlane`]), and its sessions (`Hello`, leases,
//! heartbeats, results) through calls into the server's worker service
//! ([`LocalControlPlane`]), not over the network; the server half's worker listener
//! still serves remote daemons.
//!
//! [`main`] is the binary: the daemon's helper re-execs first, before any flag is read
//! (`kbf_node::dispatch_helpers`: a keeper or `__chown-dirs` run of this binary never
//! opens the server's directories or starts a runtime); then one command line holding
//! both halves' flags ([`cli`]); then the server half, started as `kbf-server` starts
//! (`kbf_server::start`, on an async runtime of its own, sized as `kbf-server` sizes
//! one), its start line printed to stdout; then the daemon half, as `kbf-daemon` starts
//! (`kbf_node::start_with`, on its own runtime), with the in-process planes in place of
//! the transfer engine and of the dial to `--server`. SIGTERM or SIGINT stops the server
//! half as it stops `kbf-server`; the daemon half stops once the server half has,
//! abandoning the leases still running, and the process exits 0. A server half that
//! fails while serving stops the daemon too, and the process exits 1. A start the server
//! half refuses exits 2; one the daemon half refuses exits as `kbf-daemon` would.
//!
//! Linux only, the container driver only: elsewhere [`main`] exits 2 saying so.
//!
//! Not done yet (the README lists it): a durable scheduler state, without which a restart
//! loses every lease; the in-process planes' admission; the `--reserve-cpus` split
//! between the halves; the whole-process
//! crash simulation, the driver-panic test and the rest of the helper-dispatch tests;
//! and the deploy shape, the ownership step and the unit.

pub mod cli;
mod control;
mod intake;
mod plane;
#[cfg(target_os = "linux")]
mod run;

pub use control::LocalControlPlane;
pub use plane::LocalDataPlane;

use std::process::ExitCode;

/// The `kbf-all-in-one` binary, as the module docs say.
#[cfg(target_os = "linux")]
#[must_use]
pub fn main() -> ExitCode {
    if let Some(code) = kbf_node::dispatch_helpers() {
        return code;
    }
    run::main()
}

/// The `kbf-all-in-one` binary off Linux: it refuses to run.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn main() -> ExitCode {
    eprintln!(
        "kbf-all-in-one runs on Linux only, with the container driver; elsewhere run \
         kbf-server and kbf-daemon"
    );
    ExitCode::from(2)
}
