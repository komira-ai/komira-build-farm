//! What the driver tells the runtime about one action's container: [`ContainerSpec`],
//! the [`ContainerLimits`] it gets, and how it [`Ended`]. The OCI backend (`oci`) turns
//! the spec into a bundle for `crun`.

use std::path::{Path, PathBuf};

use kbf_daemon::trees::SharedMount;

/// Where the input root appears inside the container.
pub const EXEC_ROOT: &str = "/kbf/root";

/// How every lease's container, lease cgroup and scratch directory are named:
/// `kbf-lease-<term>-<seq>`.
pub(crate) const LEASE_PREFIX: &str = "kbf-lease-";

/// The owner, in the setup namespace (`oci::Mapping::Setup`), of a lease's overlay
/// directories while its container may run: id 1, the first subordinate id, which the
/// action's namespace maps the container's root to. The container cannot write under a
/// directory whose owner it does not map, so without this the action could write nothing
/// under the exec root (the overlay refuses with `EROFS`).
pub(crate) const CONTAINER_OWNER: &str = "1:1";

/// The owner, in the same namespace, of the overlay directories once the container has
/// stopped: id 0 there is the daemon's own user, so the driver reads every output (an
/// action's `0600` file or `0700` directory among them) and removes the scratch itself.
pub(crate) const DAEMON_OWNER: &str = "0:0";

/// The per-container limits every action's container is created with. Each is written
/// into the bundle explicitly, so no default of the runtime changes what an action gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerLimits {
    /// The container's `pids.max`, its tasks (threads included).
    pub pids: u64,
    /// The size of the container's `/dev/shm` tmpfs, in MiB.
    pub shm_mib: u64,
    /// `RLIMIT_NOFILE`, soft and hard. Rootless, it cannot exceed the daemon's own hard
    /// limit.
    pub nofile: u64,
    /// `RLIMIT_NPROC`, soft and hard. The kernel counts it per user, and every
    /// container's root is the same subordinate id, so it bounds the processes of all the
    /// node's actions together, not one container's.
    pub nproc: u64,
}

impl ContainerLimits {
    /// 8192 pids, a 64 MiB `/dev/shm`, 65,536 open files and 32,768 processes.
    pub const DEFAULT: Self = Self {
        pids: 8192,
        shm_mib: 64,
        nofile: 65_536,
        nproc: 32_768,
    };
}

/// The container to run for one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerSpec {
    /// The container's name, `kbf-lease-<term>-<seq>`.
    pub name: String,
    /// `<repo>@sha256:<digest>`.
    pub image: String,
    /// `sha256:<hex>`: the image's digest alone.
    pub image_digest: String,
    /// The lease's scratch directory.
    pub dir: PathBuf,
    /// The lease cgroup, relative to the cgroup mount.
    pub cgroup_parent: String,
    /// The input root on the host: the overlay's read-only lower layer.
    pub input_root: PathBuf,
    /// The overlay's upper layer: everything the action writes under the exec root.
    pub upper: PathBuf,
    /// The overlay's work directory (empty, on the upper layer's filesystem).
    pub overlay_work: PathBuf,
    /// The working directory, relative to the exec root.
    pub working_directory: String,
    pub env: Vec<(String, String)>,
    pub argv: Vec<String>,
    pub limits: ContainerLimits,
    /// Host directories mounted read-only inside the exec root (shared trees).
    pub shared: Vec<SharedMount>,
}

/// The directories a lease hands between the daemon and the action's root.
pub(crate) struct Dirs<'a> {
    /// The lease's scratch directory.
    pub lease: &'a Path,
    /// The input root (the overlay's lower layer).
    pub root: &'a Path,
    pub upper: &'a Path,
    pub work: &'a Path,
    /// With the input cache, the daemon's binary: the input root is then given by its
    /// directories alone ([`crate::chown_dirs`]), never its files.
    pub helper: Option<&'a Path>,
}

/// How a started container ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// It ran and exited with this code.
    Exited(i32),
    /// It did not run to an exit: `crun` reported an error (`exited <code>`).
    NotRun(String),
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "kbf-lease-1-2".to_owned(),
            image: format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            image_digest: format!("sha256:{}", "a".repeat(64)),
            dir: PathBuf::from("/scratch/kbf-lease-1-2"),
            cgroup_parent: "/kbf.slice/actions/kbf-lease-1-2".to_owned(),
            input_root: PathBuf::from("/scratch/kbf-lease-1-2/root"),
            upper: PathBuf::from("/scratch/kbf-lease-1-2/upper"),
            overlay_work: PathBuf::from("/scratch/kbf-lease-1-2/work"),
            working_directory: "pkg".to_owned(),
            env: vec![("PATH".to_owned(), "/bin".to_owned())],
            argv: vec!["tool".to_owned(), "--flag".to_owned(), "a b".to_owned()],
            limits: ContainerLimits {
                pids: 101,
                shm_mib: 102,
                nofile: 103,
                nproc: 104,
            },
            shared: Vec::new(),
        }
    }
}
