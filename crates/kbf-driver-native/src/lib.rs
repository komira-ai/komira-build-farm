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
//! - outputs read with `kbf-outputs` (no symlink followed at any level; depth, entry
//!   and byte limits; large files streamed to the CAS; UTF-8 names);
//! - the lease directory removed afterwards (`kbf_outputs::remove_tree`: iterative, by
//!   descriptor), and its absence checked; a lease directory that stays fails the
//!   lease, so a dirty node is loud. A run the daemon drops is cleaned by `Drop`.
//!
//! Not yet: a per-lease user (which would make "every process of the action" exact
//! and keep actions out of each other's files), CPU limits, resource usage in the
//! result, and network isolation on Linux.

mod config;
pub mod network;
pub mod procs;
mod runtime;

pub use config::{MemoryPolicy, NativeConfig};
pub use runtime::{DRIVER, KIND, NativeRuntime};
