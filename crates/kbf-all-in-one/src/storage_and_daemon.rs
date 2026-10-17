//! `kbf storage-and-daemon` (`docs/design/architecture-v2.md`, sections 1 and 10): the
//! storage node and a worker daemon in one process, the daemon reading and writing the
//! node's storage through calls, with no socket between.
//!
//! - **One command line** ([`command`]): every flag of `kbf storage` (its store, write,
//!   log and listener flags; the other flags of `kbf-server` it refuses by name, as
//!   `kbf storage` does) and every flag of `kbf-daemon`. The three thread overrides both
//!   have size the daemon half, as in `kbf all-in-one`; the storage half sizes itself as
//!   `kbf storage` does with none of them given. Refused: `--cas` and any `--driver`
//!   but `container`, as in `kbf all-in-one`.
//! - **The storage half** starts as `kbf storage` does
//!   ([`kbf_server::storage_half::start`]), on an async runtime of its own, and prints
//!   `kbf storage`'s start line on stdout. Its listener serves the frontend and other
//!   nodes as any storage node's does.
//! - **The daemon half** starts as `kbf-daemon` does, on an async runtime of its own. Its
//!   control plane is a remote daemon's: `--server` names the frontend's (today the
//!   server's) worker listener, over mutual TLS with the node's certificate. Its data
//!   plane depends on whether the node registers with a frontend (`--frontend`):
//!   - **Without one** the node is the whole farm, a map of itself:
//!     [`crate::LocalDataPlane`] over the storage half's [`kbf_storage::Storage`]
//!     (`kbf_node::start_with`): no gRPC client, no TLS and no encoding for a blob.
//!   - **With one** the farm has other nodes, and a blob belongs to the member the
//!     frontend's map names, not to this node (`kbf_node::start_member`). The daemon
//!     routes every blob by the map its frontend sends it (`Welcome.map`), as a
//!     standalone daemon does: a blob owned by this node's member (the one registered
//!     under `--advertise-addr`, or the bound address) goes in-process to the storage
//!     half, with no socket; any other goes over mutual TLS to its owner. **No map, no
//!     write**: until the daemon holds a map every call fails UNAVAILABLE (there is no
//!     `--cas` leg to fall back on), and until the storage half holds one its service refuses
//!     every call, its own member's included (UNAVAILABLE, as it refuses any caller's). The
//!     in-process plane of the one-box mode moves output files without spooling them
//!     (`Cas::intake`); the routed plane does not yet.
//! - **One cache**: when the hot store is a directory on `--data-dir`'s filesystem, as
//!   in `kbf all-in-one` (`crate::one_cache`).
//! - **Stopping**: as `kbf all-in-one`, the storage half in the server half's place.
//!   SIGTERM or SIGINT stops the storage half; the daemon half stops after it and the
//!   process exits 0. A start the storage half refuses exits 2; one the daemon half
//!   refuses exits as `kbf-daemon` would; a storage half that stops serving exits 1.
//!
//! Linux only, the container driver only, as `kbf all-in-one`.

use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::sync::Arc;

use clap::{ArgMatches, Command, CommandFactory as _, FromArgMatches as _};
#[cfg(target_os = "linux")]
use kbf_node::LocalMember;
#[cfg(target_os = "linux")]
use kbf_server::storage_half::StorageHalf;
use kbf_server::storage_node::{self, StorageFlags};

use crate::cli::refuse_remote_plane;
#[cfg(target_os = "linux")]
use crate::run::{Starter, in_process};

/// What the mode is, for `--help`.
const ABOUT: &str = "The storage node and a worker daemon in one process, the daemon \
                     reading and writing the node's storage in-process (Linux, the \
                     container driver only).";

/// The mode's name, as `kbf` spells it.
const NAME: &str = "storage-and-daemon";

/// The help position of the storage node's first flag: after every flag of the daemon's.
const STORAGE_FLAGS_FROM: usize = 1000;

/// The one command: the daemon's flags, then the storage node's that do not share a
/// name with one of them.
#[must_use]
pub fn command() -> Command {
    let daemon = kbf_node::Cli::command();
    let taken: BTreeSet<clap::Id> = daemon.get_arguments().map(|a| a.get_id().clone()).collect();
    // Listed after the daemon's, in their own order: both commands number their flags from
    // 0, and flags that tie would interleave in `--help`.
    let storage: Vec<clap::Arg> = storage_node::command()
        .get_arguments()
        .filter(|a| !taken.contains(a.get_id()))
        .enumerate()
        .map(|(i, a)| a.clone().display_order(STORAGE_FLAGS_FROM + i))
        .collect();
    daemon.name(NAME).bin_name(NAME).about(ABOUT).args(storage)
}

/// Both halves' flags, from one command line.
#[derive(Clone, Debug)]
pub struct Flags {
    /// The storage half's: `kbf storage`'s.
    pub storage: StorageFlags,
    /// The daemon half's.
    pub daemon: kbf_node::Cli,
}

/// Both halves' flags from what [`command`] matched.
///
/// # Errors
/// A `--driver` other than `container`, `--cas`, and what `kbf storage` refuses (a flag
/// of `kbf-server` a storage node does not take, named).
pub fn parse(matches: &ArgMatches) -> Result<Flags, clap::Error> {
    refuse_remote_plane(matches, "kbf storage-and-daemon", "storage", command)?;
    let daemon = kbf_node::Cli::from_arg_matches(matches)?;
    let mut storage = storage_node::parse(matches)?;
    // The thread flags matched are the daemon's.
    storage.server.threads = kbf_threads::PoolArgs::default();
    Ok(Flags { storage, daemon })
}

/// Starts both halves and serves until the storage half stops (the module docs).
#[cfg(target_os = "linux")]
#[must_use]
pub fn run(flags: &Flags) -> std::process::ExitCode {
    kbf_server::log_file::init(&flags.storage.server);
    crate::run::exit("kbf storage-and-daemon", "storage", serve(prepared(flags)))
}

/// `kbf storage-and-daemon` off Linux: it refuses to run.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn run(_flags: &Flags) -> std::process::ExitCode {
    eprintln!(
        "kbf storage-and-daemon runs on Linux only, with the container driver; elsewhere \
         run kbf storage and kbf daemon"
    );
    std::process::ExitCode::from(2)
}

/// Both halves' flags as the mode runs them: given one cache when they share a disk
/// (`crate::one_cache`).
#[cfg(target_os = "linux")]
fn prepared(flags: &Flags) -> Flags {
    let mut flags = flags.clone();
    crate::one_cache::share(&mut flags.storage.server, &mut flags.daemon);
    flags
}

/// The storage half on a runtime of its own, then the daemon half over it in-process.
/// Both halves start from `flags`, as [`prepared`] made them: no half sees another copy.
#[cfg(target_os = "linux")]
fn serve(flags: Flags) -> Result<(), crate::run::Failed> {
    use crate::run::{Failed, with_daemon};

    let sizes = kbf_server::sizes_on(&flags.storage.server, kbf_threads::Topology::detect());
    sizes.log();
    let tokio = sizes.runtime().map_err(Failed::server)?;
    let start = kbf_server::storage_half::start(&flags.storage, kbf_server::interrupted);
    let half = tokio.block_on(start);
    let half = half.map_err(Failed::server)?;
    println!("{}", half.start_line());
    let start = starter(&flags, &half, tokio.handle());
    let serving = Box::pin(half.serve());
    with_daemon(&tokio, serving, start, &flags.daemon)
}

/// Whether the daemon half routes its blobs by the storage map: when the node registers
/// with a frontend, which holds the farm's map and the other members. Without one the
/// node is the whole farm, a map of itself.
#[cfg(target_os = "linux")]
fn routes_by_map(flags: &Flags) -> bool {
    flags.storage.frontend.is_some()
}

/// This node's own member for the daemon half's plane: registered under the address the
/// node registers with the frontend (`--advertise-addr`, or the bound address), so the
/// plane finds itself in the map by the address the map carries.
#[cfg(target_os = "linux")]
fn local_member(flags: &Flags, half: &StorageHalf) -> LocalMember {
    let addr = flags.storage.registers_as(half.addr);
    LocalMember::new(addr, half.in_process())
}

/// The daemon half's start over `half` (the module docs, **The daemon half**).
#[cfg(target_os = "linux")]
fn starter(flags: &Flags, half: &StorageHalf, tokio: &tokio::runtime::Handle) -> Starter {
    if routes_by_map(flags) {
        let local = local_member(flags, half);
        return Box::new(move |cli| kbf_node::start_member(cli, local));
    }
    let store = kbf_server::LocalStore::over(half.storage());
    in_process(Arc::new(crate::LocalDataPlane::new(store, tokio.clone())))
}

#[cfg(test)]
#[path = "storage_and_daemon_tests.rs"]
mod tests;
