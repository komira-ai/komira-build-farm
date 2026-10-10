//! The Podman command line: the flags every action's container is created with, and
//! the few Podman verbs the driver uses. The daemon decides; Podman executes (RFC 10.2).
//!
//! Every invocation passes `--cgroup-manager=cgroupfs`: the daemon owns the delegated
//! cgroup subtree, not the user's systemd.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

/// Where the input root appears inside the container.
pub const EXEC_ROOT: &str = "/kbf/root";

/// The label naming the daemon a container belongs to: every container is created with
/// `kbf.owner=<owner>` ([`crate::PodmanConfig::owner`]), so the next daemon on the
/// node finds what its predecessor left.
pub const OWNER_LABEL: &str = "kbf.owner";

/// How every lease's container, lease cgroup and scratch directory are named:
/// `kbf-lease-<term>-<seq>`.
pub(crate) const LEASE_PREFIX: &str = "kbf-lease-";

/// The owner, in Podman's rootless user namespace, of a lease's overlay directories
/// while its container may run: id 1, the first subordinate id, which `--userns=nomap`
/// maps the container's root to. The container cannot write under a directory whose
/// owner it does not map, so without this the action could write nothing under the
/// exec root (the overlay refuses with `EROFS`).
pub(crate) const CONTAINER_OWNER: &str = "1:1";

/// The owner, in the same namespace, of the overlay directories once the container has
/// stopped: id 0 there is the daemon's own user, so the driver reads every output (an
/// action's `0600` file or `0700` directory among them) and removes the scratch itself.
pub(crate) const DAEMON_OWNER: &str = "0:0";

/// The user every action runs as, inside the container: its root. Explicit, so the
/// image's `USER` does not choose it; under `--userns=nomap` it is the daemon user's
/// first subordinate id on the host ([`CONTAINER_OWNER`]).
pub(crate) const CONTAINER_USER: &str = "0:0";

/// The per-container limits every action's container is created with. Each is passed
/// explicitly, so neither Podman's defaults nor a node's `containers.conf` changes what
/// an action gets. (Without `--pids-limit`, rootless Podman 4.9 on the hosted runners
/// left `pids.max` at `max`: no limit.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerLimits {
    /// `--pids-limit`: the container's `pids.max`, its tasks (threads included).
    pub pids: u64,
    /// `--shm-size`, in MiB: the size of the container's `/dev/shm` tmpfs.
    pub shm_mib: u64,
    /// `--ulimit=nofile=<n>:<n>`: `RLIMIT_NOFILE`, soft and hard. Rootless, it cannot
    /// exceed the daemon's own hard limit.
    pub nofile: u64,
    /// `--ulimit=nproc=<n>:<n>`: `RLIMIT_NPROC`, soft and hard. The kernel counts it per
    /// user, and every container's root is the same subordinate id, so it bounds the
    /// processes of all the node's actions together, not one container's.
    pub nproc: u64,
}

impl ContainerLimits {
    /// 8192 pids, a 64 MiB `/dev/shm` (Podman's own default size), 65,536 open files
    /// and 32,768 processes.
    pub const DEFAULT: Self = Self {
        pids: 8192,
        shm_mib: 64,
        nofile: 65_536,
        nproc: 32_768,
    };
}

/// The container to create for one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerSpec {
    /// The container's name, `kbf-lease-<term>-<seq>`.
    pub name: String,
    /// The value of its [`OWNER_LABEL`].
    pub owner: String,
    /// `<repo>@sha256:<digest>`.
    pub image: String,
    /// The lease cgroup, as `--cgroup-parent` names it.
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
}

/// The `podman create` arguments for `spec`.
///
/// - **Owner:** `--label=kbf.owner=<owner>`, which a restarted daemon's sweep looks
///   for ([`OWNER_LABEL`]).
/// - **Network:** `--network=none`, loopback only. No REAPI action gets a network.
/// - **Image:** `--pull=never`; nodes never pull at action time (RFC 10.7).
/// - **Entrypoint:** the action's argv as a JSON array, so the image's `ENTRYPOINT`
///   and `CMD` are both ignored (RFC 10.5) and no argument is re-split.
/// - **Files:** the input root as an overlay: the host copy is never written, and
///   every write lands in `upper`, which the driver reads outputs from (no output may
///   already be an input, so each is whole there).
/// - **Memory:** `memory.oom.group=1` on the container, so an OOM kill takes the whole
///   action. Limits live on the lease cgroup (see `cgroup`), never `--memory`.
/// - **Users:** `--userns=nomap`, so no container uid or gid maps to the daemon's user:
///   container id 0 is the daemon user's first subordinate id, and so on up. Rootless
///   Podman's default makes the container's root the daemon's own uid on the host,
///   with only the mount and pid namespaces between an action and what that uid can
///   reach (fleet-updates-security S4.3). Not `--userns=auto`: rootless, it gives the
///   first container 65,535 ids of a standard 65,536-id range and refuses a second
///   container while the first exists ("not enough unused IDs in user namespace").
///   `--user=0:0` ([`CONTAINER_USER`]) whatever the image's `USER` says.
/// - **Environment:** `--unsetenv-all`, then one `--env` per `Command` variable, so
///   neither the image's `ENV`, Podman's defaults (`PATH`, `container`; `TERM` only
///   with a tty, which an action never has) nor a node's `containers.conf` `env`
///   reaches the action. Podman (4.9) still adds two
///   variables when the `Command` sets neither: `HOSTNAME=localhost` (the hostname
///   above) and `HOME`, uid 0's home in the image's `/etc/passwd`. Both follow from
///   the image digest, so they are the same on every node; a `Command` that sets
///   either gets its own value.
/// - **Limits:** `--pids-limit`, `--shm-size` and `--ulimit` for `nofile` and `nproc`
///   from [`ContainerLimits`], so a node's `containers.conf` cannot change them.
pub(crate) fn create_args(spec: &ContainerSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "create",
        "--pull=never",
        "--network=none",
        "--userns=nomap",
        "--hostname=localhost",
        "--unsetenv-all",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(format!("--name={}", spec.name).into());
    args.push(format!("--label={OWNER_LABEL}={}", spec.owner).into());
    args.push(format!("--cgroup-parent={}", spec.cgroup_parent).into());
    args.push(format!("--user={CONTAINER_USER}").into());
    let limits = spec.limits;
    args.push(format!("--pids-limit={}", limits.pids).into());
    args.push(format!("--shm-size={}m", limits.shm_mib).into());
    args.push(format!("--ulimit=nofile={0}:{0}", limits.nofile).into());
    args.push(format!("--ulimit=nproc={0}:{0}", limits.nproc).into());
    let entrypoint = serde_json::Value::from(spec.argv.clone());
    args.push(format!("--entrypoint={entrypoint}").into());
    let workdir = if spec.working_directory.is_empty() {
        EXEC_ROOT.to_owned()
    } else {
        format!("{EXEC_ROOT}/{}", spec.working_directory)
    };
    args.push(format!("--workdir={workdir}").into());
    for (name, value) in &spec.env {
        args.push(format!("--env={name}={value}").into());
    }
    let mut volume = OsString::from("--volume=");
    volume.push(&spec.input_root);
    volume.push(format!(":{EXEC_ROOT}:O,upperdir="));
    volume.push(&spec.upper);
    volume.push(",workdir=");
    volume.push(&spec.overlay_work);
    args.push(volume);
    args.push(spec.image.clone().into());
    args
}

/// How a started container ended ([`Podman::ended`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// It ran and exited with this code.
    Exited(i32),
    /// It did not run to an exit: Podman's `<status> <exit code>` for it.
    NotRun(String),
}

/// A Podman program.
#[derive(Clone, Debug)]
pub(crate) struct Podman {
    program: PathBuf,
}

/// What a Podman verb printed when it failed.
fn failure(verb: &str, output: &std::process::Output) -> String {
    format!(
        "podman {verb}: {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

impl Podman {
    pub(crate) fn new(program: PathBuf) -> Self {
        Self { program }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg("--cgroup-manager=cgroupfs")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn output(&self, args: &[OsString]) -> Result<std::process::Output, String> {
        self.command()
            .args(args)
            .output()
            .await
            .map_err(|e| format!("run {}: {e}", self.program.display()))
    }

    /// The id of the local image `reference`, or `None` if this node's image store does
    /// not hold it.
    pub(crate) async fn image_id(&self, reference: &str) -> Result<Option<String>, String> {
        let args = ["image", "inspect", "--format={{.Id}}", reference].map(OsString::from);
        let output = self.output(&args).await?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    }

    /// The directory of the image store's per-image directories:
    /// `<graph root>/<driver>-images`.
    pub(crate) async fn images_dir(&self) -> Result<PathBuf, String> {
        let args = [
            "info",
            "--format={{.Store.GraphRoot}}/{{.Store.GraphDriverName}}-images",
        ]
        .map(OsString::from);
        let output = self.output(&args).await?;
        if !output.status.success() {
            return Err(failure("info", &output));
        }
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    }

    /// Creates the container (it does not start).
    pub(crate) async fn create(&self, spec: &ContainerSpec) -> Result<(), String> {
        let output = self.output(&create_args(spec)).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(failure("create", &output))
        }
    }

    /// Starts the container attached: the returned child ends when the container does,
    /// with the container's stdout and stderr written to the given files.
    pub(crate) fn start(
        &self,
        name: &str,
        stdout: std::fs::File,
        stderr: std::fs::File,
    ) -> Result<Child, String> {
        self.command()
            .args(["start", "--attach", name])
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("run {}: {e}", self.program.display()))
    }

    /// How the container ended, from Podman's record, not from `podman start`'s own
    /// status, which also reports Podman's errors: its exit code, or the state of a
    /// container that did not run to an exit.
    pub(crate) async fn ended(&self, name: &str) -> Result<Ended, String> {
        let args = [
            "inspect",
            "--type=container",
            "--format={{.State.Status}} {{.State.ExitCode}}",
            name,
        ]
        .map(OsString::from);
        let output = self.output(&args).await?;
        if !output.status.success() {
            return Err(failure("inspect", &output));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        match text.split_whitespace().collect::<Vec<_>>()[..] {
            ["exited", code] => code
                .parse()
                .map(Ended::Exited)
                .map_err(|_| format!("podman inspect: exit code {code:?}")),
            _ => Ok(Ended::NotRun(text.trim().to_owned())),
        }
    }

    /// Sends `signal` to the container's main process.
    pub(crate) async fn signal(&self, name: &str, signal: &str) -> Result<(), String> {
        let args = ["kill", &format!("--signal={signal}"), name].map(OsString::from);
        let output = self.output(&args).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(failure("kill", &output))
        }
    }

    /// The names of the lease containers labelled as `owner`'s, in any state. Blocking.
    pub(crate) fn owned(&self, owner: &str) -> Result<Vec<String>, String> {
        let filter = format!("--filter=label={OWNER_LABEL}={owner}");
        let format = format!("--format={{{{index .Labels \"{OWNER_LABEL}\"}}}} {{{{.Names}}}}");
        let output = self.blocking_output(&["ps", "--all", &filter, &format], "ps")?;
        // The label is checked again, exactly: the filter's matching is Podman's.
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(text
            .lines()
            .filter_map(|line| line.rsplit_once(' '))
            .filter(|(label, name)| *label == owner && name.starts_with(LEASE_PREFIX))
            .map(|(_, name)| name.to_owned())
            .collect())
    }

    /// Removes the container, killing it first if it still runs; a container that does
    /// not exist is not an error. Blocking, so a dropped lease can clean up too.
    pub(crate) fn remove_blocking(&self, name: &str) -> Result<(), String> {
        self.blocking(&["rm", "--force", "--ignore", "--time=0", name], "rm")
    }

    /// Gives `paths`, and everything below them, to `owner` (`uid:gid` in Podman's user
    /// namespace). A symlink is changed itself, never followed (`-h`; `-R` traverses
    /// none), so a link an action left cannot hand a host file to anyone.
    pub(crate) async fn chown(&self, owner: &str, paths: &[&Path]) -> Result<(), String> {
        let mut args: Vec<OsString> = ["unshare", "chown", "-hR", owner, "--"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(paths.iter().map(|p| p.as_os_str().to_owned()));
        let output = self.output(&args).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(failure("unshare chown", &output))
        }
    }

    /// Removes `dir` inside Podman's user namespace, where files an action created as
    /// another container user can be deleted. Blocking.
    pub(crate) fn unshare_remove_blocking(&self, dir: &Path) -> Result<(), String> {
        let dir = dir.to_string_lossy();
        self.blocking(&["unshare", "rm", "-rf", "--", &dir], "unshare rm")
    }

    fn blocking(&self, args: &[&str], verb: &str) -> Result<(), String> {
        self.blocking_output(args, verb).map(drop)
    }

    /// Runs Podman with `args`, blocking; its output if it succeeded.
    fn blocking_output(&self, args: &[&str], verb: &str) -> Result<std::process::Output, String> {
        let output = std::process::Command::new(&self.program)
            .arg("--cgroup-manager=cgroupfs")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("run {}: {e}", self.program.display()))?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(failure(verb, &output))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "kbf-lease-1-2".to_owned(),
            owner: "node-1".to_owned(),
            image: format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            cgroup_parent: "/kbf.slice/actions/kbf-lease-1-2".to_owned(),
            input_root: PathBuf::from("/scratch/kbf-lease-1-2/root"),
            upper: PathBuf::from("/scratch/kbf-lease-1-2/upper"),
            overlay_work: PathBuf::from("/scratch/kbf-lease-1-2/work"),
            working_directory: "pkg".to_owned(),
            env: vec![("PATH".to_owned(), "/bin".to_owned())],
            argv: vec!["sh".to_owned(), "-c".to_owned(), "echo \"a b\"".to_owned()],
            limits: ContainerLimits {
                pids: 101,
                shm_mib: 102,
                nofile: 103,
                nproc: 104,
            },
        }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Catches the network being left on by default (the "network on" mutant): without
    /// `--network=none` rootless Podman gives the container slirp4netns, which reaches
    /// the host's ports and DNS.
    #[test]
    fn the_network_is_off() {
        let args = strings(&create_args(&spec()));
        assert!(args.contains(&"--network=none".to_owned()), "{args:?}");
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--network=") && a != "--network=none")
        );
    }

    /// Catches the "drop `--userns`" mutant (fleet-updates-security S10) at the
    /// command line: rootless Podman's default maps the container's root to the
    /// daemon's own uid. `tests/podman.rs` checks the mapping on real Podman.
    #[test]
    fn no_container_id_is_the_daemons() {
        let args = strings(&create_args(&spec()));
        let userns: Vec<_> = args.iter().filter(|a| a.starts_with("--userns")).collect();
        assert_eq!(userns, ["--userns=nomap"], "{args:?}");
    }

    /// Catches a container that could pull a different image at action time, or run
    /// the image's entrypoint around the action's argv.
    #[test]
    fn the_image_is_local_and_the_argv_is_exact() {
        let args = strings(&create_args(&spec()));
        assert_eq!(args[0], "create");
        assert!(args.contains(&"--pull=never".to_owned()));
        assert!(args.contains(&r#"--entrypoint=["sh","-c","echo \"a b\""]"#.to_owned()));
        assert_eq!(args.last(), Some(&spec().image));
    }

    /// Catches a hard per-lease memory cap or a missing OOM group: the policy is soft
    /// limits on the lease cgroup, and one OOM kill ends the whole action.
    #[test]
    fn no_hard_memory_cap_and_one_oom_group() {
        let args = strings(&create_args(&spec()));
        assert!(args.contains(&"--cgroup-conf=memory.oom.group=1".to_owned()));
        assert!(args.contains(&"--cgroup-parent=/kbf.slice/actions/kbf-lease-1-2".to_owned()));
        for banned in ["--memory", "--memory-swap", "--cpus", "--cpu-quota"] {
            assert!(
                !args.iter().any(|a| a.starts_with(banned)),
                "{banned} in {args:?}"
            );
        }
    }

    /// Catches the input root mounted writable (an action could change the inputs the
    /// next reader of this directory sees) and a wrong working directory.
    #[test]
    fn the_input_root_is_an_overlay_and_the_workdir_is_inside_it() {
        let args = strings(&create_args(&spec()));
        assert!(args.contains(
            &"--volume=/scratch/kbf-lease-1-2/root:/kbf/root:O,upperdir=/scratch/kbf-lease-1-2/upper,workdir=/scratch/kbf-lease-1-2/work"
                .to_owned()
        ));
        assert!(args.contains(&"--workdir=/kbf/root/pkg".to_owned()));
        assert!(args.contains(&"--env=PATH=/bin".to_owned()));
        assert!(args.contains(&"--hostname=localhost".to_owned()));
        // What a restarted daemon's sweep finds the container by.
        assert!(args.contains(&"--label=kbf.owner=node-1".to_owned()));
        let mut at_root = spec();
        at_root.working_directory.clear();
        assert!(strings(&create_args(&at_root)).contains(&"--workdir=/kbf/root".to_owned()));
    }

    /// Catches the image's `ENV`, Podman's default variables or a node's
    /// `containers.conf` `env` reaching the action (the "drop `--unsetenv-all`"
    /// mutant), and the image's `USER` choosing who the action runs as.
    /// `tests/podman_env.rs` checks the environment inside on real Podman.
    #[test]
    fn only_the_commands_environment_and_a_fixed_user() {
        let args = strings(&create_args(&spec()));
        assert!(args.contains(&"--unsetenv-all".to_owned()), "{args:?}");
        let env: Vec<_> = args.iter().filter(|a| a.starts_with("--env")).collect();
        assert_eq!(env, ["--env=PATH=/bin"], "{args:?}");
        let users: Vec<_> = args.iter().filter(|a| a.starts_with("--user=")).collect();
        assert_eq!(users, ["--user=0:0"], "{args:?}");
    }

    /// Catches a limit left to Podman's defaults or a node's `containers.conf` (the
    /// "drop `--pids-limit`" mutant, and the same for `/dev/shm` and each ulimit), and a
    /// limit that is not the configured one.
    #[test]
    fn every_limit_is_explicit() {
        let args = strings(&create_args(&spec()));
        let named = |prefix: &str| -> Vec<&String> {
            args.iter().filter(|a| a.starts_with(prefix)).collect()
        };
        assert_eq!(named("--pids-limit"), ["--pids-limit=101"], "{args:?}");
        assert_eq!(named("--shm-size"), ["--shm-size=102m"], "{args:?}");
        assert_eq!(
            named("--ulimit"),
            ["--ulimit=nofile=103:103", "--ulimit=nproc=104:104"],
            "{args:?}"
        );
    }
}
