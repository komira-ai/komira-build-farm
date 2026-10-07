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

/// The container to create for one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerSpec {
    /// The container's name, `kbf-lease-<term>-<seq>`.
    pub name: String,
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
}

/// The `podman create` arguments for `spec`.
///
/// - **Network:** `--network=none`, loopback only. No REAPI action gets a network.
/// - **Image:** `--pull=never`; nodes never pull at action time (RFC 10.7).
/// - **Entrypoint:** the action's argv as a JSON array, so the image's `ENTRYPOINT`
///   and `CMD` are both ignored (RFC 10.5) and no argument is re-split.
/// - **Files:** the input root as an overlay: the host copy is never written, and
///   every write lands in `upper`, which the driver reads outputs from (no output may
///   already be an input, so each is whole there).
/// - **Memory:** `memory.oom.group=1` on the container, so an OOM kill takes the whole
///   action. Limits live on the lease cgroup (see `cgroup`), never `--memory`.
pub(crate) fn create_args(spec: &ContainerSpec) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "create",
        "--pull=never",
        "--network=none",
        "--hostname=localhost",
        "--cgroup-conf=memory.oom.group=1",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(format!("--name={}", spec.name).into());
    args.push(format!("--cgroup-parent={}", spec.cgroup_parent).into());
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

    /// The exit code of a container that has exited. Read from Podman's record, not
    /// from `podman start`'s own status, which also reports Podman's errors.
    pub(crate) async fn exit_code(&self, name: &str) -> Result<i32, String> {
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
                .map_err(|_| format!("podman inspect: exit code {code:?}")),
            _ => Err(format!(
                "the container did not run to an exit: podman reports {:?}",
                text.trim()
            )),
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

    /// Removes the container, killing it first if it still runs; a container that does
    /// not exist is not an error. Blocking, so a dropped lease can clean up too.
    pub(crate) fn remove_blocking(&self, name: &str) -> Result<(), String> {
        self.blocking(&["rm", "--force", "--ignore", "--time=0", name], "rm")
    }

    /// Removes `dir` inside Podman's user namespace, where files an action created as
    /// another container user can be deleted. Blocking.
    pub(crate) fn unshare_remove_blocking(&self, dir: &Path) -> Result<(), String> {
        let dir = dir.to_string_lossy();
        self.blocking(&["unshare", "rm", "-rf", "--", &dir], "unshare rm")
    }

    fn blocking(&self, args: &[&str], verb: &str) -> Result<(), String> {
        let output = std::process::Command::new(&self.program)
            .arg("--cgroup-manager=cgroupfs")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("run {}: {e}", self.program.display()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(failure(verb, &output))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "kbf-lease-1-2".to_owned(),
            image: format!("docker.io/library/busybox@sha256:{}", "a".repeat(64)),
            cgroup_parent: "/kbf.slice/actions/kbf-lease-1-2".to_owned(),
            input_root: PathBuf::from("/scratch/kbf-lease-1-2/root"),
            upper: PathBuf::from("/scratch/kbf-lease-1-2/upper"),
            overlay_work: PathBuf::from("/scratch/kbf-lease-1-2/work"),
            working_directory: "pkg".to_owned(),
            env: vec![("PATH".to_owned(), "/bin".to_owned())],
            argv: vec!["sh".to_owned(), "-c".to_owned(), "echo \"a b\"".to_owned()],
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
        let mut at_root = spec();
        at_root.working_directory.clear();
        assert!(strings(&create_args(&at_root)).contains(&"--workdir=/kbf/root".to_owned()));
    }
}
