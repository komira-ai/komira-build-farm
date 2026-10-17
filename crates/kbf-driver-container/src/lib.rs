//! The container driver for `kbf-daemon`: runs each `action` lease in a fresh rootless
//! container, behind the daemon's `Runtime` trait
//! (`docs/design/daemon.md#the-container-driver`), through `crun` run directly from a
//! bundle the driver writes (the `oci` module documents it). `crun` is the only
//! container runtime kbf has: a launch takes milliseconds.
//!
//! What one lease gets:
//! - the image named by digest in the action's `container-image` platform property
//!   (`docker://<repo>@sha256:<d>`); a tag, or an index digest, is `INVALID_ARGUMENT`.
//!   The node fetches it from its registry, digest-verified end to end (`kbf-image`), and
//!   unpacks it once;
//! - the input root as a read-only overlay lower layer at `/kbf/root`, every write in
//!   a per-lease upper directory that the outputs are read from. The upper directory
//!   holds only what the action created or changed, so an output path that is already
//!   in the input root is refused as `INVALID_ARGUMENT` before anything runs;
//! - no network (a network namespace of its own: loopback only);
//! - the `Command`'s environment variables, none of the image's or the node's (plus
//!   `HOSTNAME` and `HOME` when the `Command` sets neither), run as the container's
//!   root, with the pids, `/dev/shm` and `nofile` and `nproc` ulimits of
//!   [`ContainerLimits`], whatever the image says;
//! - no id of the daemon's user (the container's ids are the user's subordinate ids,
//!   which [`check_daemon_user`] requires at startup);
//! - nothing of the node's cold-store key: `crun` starts without the node's `AWS_*`
//!   variables (`credential`), so no runtime feature that copies the host's
//!   environment into a container has the key to copy; and a key file of mode `0600`
//!   is not for the action's ids to read, the subordinate ids above;
//! - a lease cgroup under the daemon's `actions/` cgroup, which the daemon makes in
//!   its delegated cgroup at start ([`delegate`]; [`adopt`] checks one given instead)
//!   and whose limits ([`capacity`]) bound what the node reports, with
//!   a hard cap `memory.max` = booking x 1.5 + 512 MiB, `memory.oom.group=1` on the
//!   lease cgroup and on the container, swap allowed (no `memory.swap.max`), a
//!   watch that kills a lease which keeps pushing into swap at its own cap, past a
//!   threshold of swap it pushed there itself (not swap the host moved; [`SwapKill`]), `cpu.weight` from the booked CPU, and `--oom-score-adj=0` on the
//!   container (the daemon's leaf gets `memory.min`);
//! - a wall-clock timeout, which still stores what the action wrote to stdout and
//!   stderr ([`kbf_daemon::RuntimeError::TimedOut`]), stdout and stderr captured into
//!   the CAS (each within `--output-max-stdio-bytes`), the exit code from `crun run`'s status, and a kernel OOM
//!   kill told apart by the lease cgroup's `memory.events`: a kill at the lease's own
//!   cap is [`kbf_daemon::RuntimeError::OutOfMemory`], one without it (the `actions/`
//!   backstop or the host ran short) is [`kbf_daemon::RuntimeError::BusyNode`];
//! - outputs, stdout and stderr stored one [`CHUNK`] at a time ([`FileBlob`]), never
//!   read into memory whole;
//! - cleanup on every path: the container, the lease cgroup and the scratch directory
//!   are removed after success, failure, timeout, kill, and when the daemon drops the
//!   run. The scratch directory is removed by a walk without recursion, so no depth of
//!   tree the action leaves can overflow the clean step's stack;
//! - leases named `kbf-lease-<term>-<seq>`, by which the next daemon on the node finds
//!   what this one leaves if it is killed: at start, [`ContainerRuntime::new`] removes
//!   every lease scratch directory, each with its container and lease cgroup, before the
//!   daemon says `Hello`;
//! - a keeper per lease (`keeper`), which kills the lease cgroup (`cgroup.kill`) when
//!   the daemon dies, so a killed daemon's container stops at once, restarted or not;
//! - with the node's input cache ([`CacheUse`]), input files that are hard links to
//!   read-only cache entries the container cannot write (`docs/design/worker-cache.md`
//!   section 5.1): the input root's directories alone are given to the container's root
//!   ([`chown_dirs`]), and an input an output names is a private copy.
//!
//! Modules:
//! - [`image`]: the `container-image` property;
//! - [`cas`]: [`FileBlob`] and [`MemoryCas`]; blobs are read and written through
//!   `kbf_daemon`'s `Cas` trait, the one the daemon's CAS client implements;
//! - [`tree`]: writing an input root (with `kbf_daemon::tree`) and reading outputs
//!   back;
//! - [`unpack_layers`]: the `__unpack-layers` helper mode, which applies an image's layers
//!   in a helper container;
//! - `container`: [`ContainerLimits`], the spec of one container and how it ended;
//! - `running`: the `crun run` of a lease, on a thread of its own;
//! - [`subids`]: the daemon user's subordinate id ranges, checked at startup;
//! - [`delegate`](mod@delegate): the daemon's delegated cgroup subtree, set up at
//!   startup, and what leases may use of the node;
//! - `oci`: `crun` itself: the bundle, helper containers, the image store;
//! - [`ContainerRuntime`]: the six driver steps.
//!
//! Not yet: re-adopting leases after a daemon restart (one systemd unit per lease) and
//! networked actions.

pub mod cas;
mod cgroup;
mod cgroup_usage;
pub mod chown_dirs;
mod container;
mod credential;
pub mod delegate;
mod hashing;
pub mod image;
mod input_phases;
#[cfg(test)]
mod intake_tests;
mod keeper;
pub mod numa;
mod oci;
mod outputs;
mod program;
mod remove;
mod running;
mod runtime;
pub mod subids;
pub mod tree;
pub mod unpack_layers;

// The keepers this crate's unit tests start come from the test binary.
#[cfg(test)]
kbf_daemon::keeper_test_hook!();

pub use cas::{CHUNK, FileBlob, MemoryCas};
pub use cgroup::{SwapKill, cpu_weight, memory_max};
pub use container::{ContainerLimits, EXEC_ROOT};
pub use delegate::{
    DelegateError, Delegation, adopt, capacity, delegate, enable_cpuset, limit_cpus,
};
pub use image::{ImageCheckError, ImageError, ImageRef};
pub use input_phases::Spot;
pub use numa::{CpuSet, LeasePlacement, NodeMemory, Pin, Placement, Placer};
pub use oci::{OciConfig, OciError};
pub use outputs::OutputLimits;
pub use runtime::{
    CacheUse, ConfigError, ContainerConfig, ContainerRuntime, DRIVER, KIND, StartError,
};
pub use subids::{
    DaemonIds, IdFiles, SubidError, check_daemon_user, check_subordinate_ids, daemon_ids,
};
