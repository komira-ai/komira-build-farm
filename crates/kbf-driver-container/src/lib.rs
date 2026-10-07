//! The container driver for `kbf-daemon`: runs each `action` lease in a fresh rootless
//! Podman container, behind the daemon's `Runtime` trait (RFC section 10).
//!
//! What one lease gets:
//! - the image named by digest in the action's `container-image` platform property
//!   (`docker://<repo>@sha256:<d>`); a tag, or an index digest, is `INVALID_ARGUMENT`;
//! - the input root as a read-only overlay lower layer at `/kbf/root`, every write in
//!   a per-lease upper directory that the outputs are read from. The upper directory
//!   holds only what the action created or changed, so an output path that is already
//!   in the input root is refused as `INVALID_ARGUMENT` before anything runs;
//! - no network (`--network=none`: loopback only);
//! - a lease cgroup under the daemon's delegated `actions/` cgroup, with
//!   `memory.high` = reservation x 1.5 + 512 MiB, swap allowed, no per-lease hard cap,
//!   `cpu.weight` from the booked CPU, and `memory.oom.group=1` on the container;
//! - a wall-clock timeout, stdout and stderr captured into the CAS, the exit code from
//!   Podman's record, and a kernel OOM kill (exit 137 plus `oom_kill` in the lease
//!   cgroup's `memory.events`) reported as an infrastructure failure;
//! - cleanup on every path: the container, the lease cgroup and the scratch directory
//!   are removed after success, failure, timeout, kill, and when the daemon drops the
//!   run.
//!
//! Modules:
//! - [`image`]: the `container-image` property;
//! - [`cas`]: the [`Cas`] trait the driver reads and writes blobs through, and
//!   [`MemoryCas`];
//! - [`tree`]: writing an input root and reading outputs back;
//! - [`PodmanRuntime`]: the six driver steps.
//!
//! Not yet: re-adopting leases after a daemon restart (one systemd unit per lease),
//! raising `memory.high` while the node has room, image import into the node's store,
//! networked actions, and the private Podman socket for service containers.

pub mod cas;
mod cgroup;
pub mod image;
mod outputs;
mod podman;
mod runtime;
pub mod tree;

pub use cas::{Cas, CasError, MemoryCas};
pub use cgroup::{cpu_weight, memory_high};
pub use image::{ImageError, ImageRef};
pub use podman::EXEC_ROOT;
pub use runtime::{ConfigError, DRIVER, KIND, PodmanConfig, PodmanRuntime};
