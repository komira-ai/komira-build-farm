//! The native driver for `kbf-daemon`: runs each `action` lease as plain processes on
//! the node, for Macs (where there are no containers), behind the daemon's `Runtime`
//! trait.
//!
//! What one lease gets:
//! - a fresh lease directory under the configured scratch root, the input root written
//!   into it, the working directory and each output's parent made (REAPI);
//! - its `arguments` run with the Command's environment and nothing else, stdin from
//!   `/dev/null`, stdout and stderr captured to files, as the leader of a new process
//!   group;
//! - no network unless the action's `network` platform property allows it, where the
//!   node can enforce that ([`network`]: `sandbox-exec` on macOS; not enforced
//!   elsewhere, and reported as the `network_isolation` capability);
//! - a memory watch: every poll, the physical footprint of all the action's processes
//!   together ([`procs`]); past the lease's limit, the whole tree is killed and the
//!   lease ends RESOURCE_EXHAUSTED ([`kbf_daemon::RuntimeError::OutOfMemory`]);
//! - a wall-clock timeout, and the daemon's kill, each ending the whole process tree;
//! - on every path, also a normal exit, every process of the action ended before the
//!   outputs are read: SIGKILL to the group and to each process the tracker knows,
//!   repeated until a snapshot shows none alive; any that survive fail the lease;
//! - outputs, stdout and stderr read with `kbf-outputs`: no symlink followed at any
//!   level; depth, entry, byte and stdio limits; files of more than a chunk streamed
//!   to the CAS ([`kbf_daemon::Cas::put_chunks`]); names that are not UTF-8 refused;
//! - the lease directory removed afterwards (`kbf_outputs::remove_tree`: iterative,
//!   by descriptor, giving back permissions the action took away and, on macOS,
//!   clearing the immutable and append-only flags it set); a lease directory
//!   that stays fails the lease, so a dirty node is loud. A run the daemon drops is
//!   cleaned by `Drop`.
//!
//! The container driver's own output walk is public too, but does not build on macOS;
//! moving it onto `kbf-outputs` is a follow-up.
//!
//! At start the runtime removes every lease directory a previous daemon left; one that
//! cannot be removed is moved aside into `quarantine/` under the scratch root and
//! logged, and the daemon starts anyway (an action decides what its directory holds,
//! so refusing to start would let one build step take the node out of the farm).
//!
//! Known gaps, all of them closed only by running each lease as its own user:
//! - **Processes that leave the tree.** A process that calls `setsid` and is orphaned
//!   between two polls is not seen ([`procs`]), so it outlives the lease. Work handed
//!   to launchd runs outside the tree, the sandbox and its network policy: the
//!   no-network profile denies `launchctl submit`/`bootstrap`/`load` and `open`
//!   ([`network::NO_NETWORK_PROFILE`]), but an action that asks for the network runs
//!   unsandboxed and can still do both, and any action can still write a
//!   `LaunchAgents` plist (run at the daemon user's next login) or use `at`, `cron`
//!   or a loopback service.
//! - **The daemon's own files.** Actions run as the daemon's user, so they can read
//!   what it can, the node's TLS private key (`--key`) included, and change what it
//!   owns: other leases' directories, the scratch root, the daemon's configuration.
//! - **The signal race.** [`procs::kill_all`] signals pids from a snapshot; one
//!   recycled in between is a process of the daemon's user killed by mistake.
//! - **Network "off" is not airtight**: Unix sockets stay open, the system resolver's
//!   among them, so DNS lookups still leave the node and can carry data ([`network`]).
//!
//! Not yet: a per-lease user (which would make "every process of the action" exact,
//! keep actions out of each other's files and the daemon's key, and close the gaps
//! above), CPU limits, resource usage in the result, and network isolation on Linux.

mod cas;
mod config;
pub mod network;
pub mod procs;
mod runtime;
mod sweep;

pub use config::{MemoryPolicy, NativeConfig};
pub use runtime::{DRIVER, KIND, NativeRuntime};
