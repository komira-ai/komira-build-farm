//! `cargo xtask podman-tests`: the container driver's real-Podman tests
//! (`crates/kbf-driver-container/tests/podman*.rs` and `oci_crun.rs`, every test
//! `#[ignore]` with the reason) on a GitHub-hosted Linux runner, set up the way the T4
//! spike found works (`docs/spikes/hosted-runners.md`):
//!
//! - [`runner_setup`]: lingering on for the runner user, so its systemd user manager
//!   runs and rootless Podman has a runtime directory (`/run/user/<uid>`);
//! - busybox pulled once by its image index digest, so the store holds both the index
//!   and this architecture's manifest. The manifest's digest is read from the index
//!   ([`manifest_for`]; Podman's own `.Digest` is the digest of the first pull, here the
//!   index's), for this machine's architecture ([`arch_for`]);
//! - the test targets built as the runner user ([`sudo_test::build`]), before any unit
//!   starts, so a unit only runs a binary;
//! - each target's binary run in its own system unit with `Delegate=yes`, as the runner
//!   user ([`systemd_run_args`]): the user manager alone gets no io controller, and a
//!   daemon must own its subtree. The tests set the unit's cgroup up with kbf-daemon's
//!   own code (a `supervisor` leaf, then `actions` with cpu, memory and pids). The unit
//!   lowers its OOM score as the daemon's does (`docs/deploy/linux-build-host.md`), so
//!   the tests see what an action inherits from it.
//!
//! Every target runs even when one before it failed; the verb fails if any did.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use serde_json::Value;

use crate::Error;
use crate::sudo_test::{self, Built};

/// The busybox image the tests run, without its digest.
pub const BUSYBOX_NAME: &str = "docker.io/library/busybox";
/// busybox 1.37.0, the digest of its image index.
pub const BUSYBOX_INDEX: &str =
    "sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";
/// The package the tests are in.
pub const PACKAGE: &str = "kbf-driver-container";
/// The containers.conf the `podman_env` target runs under, from its package's directory:
/// a node whose containers.conf sets its own environment and limits, which the driver's
/// flags must win over.
pub const CONTAINERS_CONF: &str = "tests/fixtures/containers-override.conf";

/// One integration test target and how it runs.
#[derive(Debug)]
pub struct Target {
    /// The file in the package's `tests/`, without `.rs`.
    pub name: &'static str,
    /// Run with `--nocapture`, so its timings show in the log.
    pub nocapture: bool,
    /// Run with `CONTAINERS_CONF_OVERRIDE` naming [`CONTAINERS_CONF`].
    pub containers_conf: bool,
}

/// The real-Podman test targets, in the order they run.
pub const TARGETS: &[Target] = &[
    Target {
        name: "podman",
        nocapture: false,
        containers_conf: false,
    },
    Target {
        name: "podman_memory",
        nocapture: false,
        containers_conf: false,
    },
    Target {
        name: "podman_killed",
        nocapture: false,
        containers_conf: false,
    },
    Target {
        name: "podman_cache",
        nocapture: true,
        containers_conf: false,
    },
    Target {
        name: "podman_env",
        nocapture: false,
        containers_conf: true,
    },
    // Writes and names its own containers.conf: a node that hands its files and its
    // environment to every container, which the action must still not read the cold
    // store key through.
    Target {
        name: "podman_credential",
        nocapture: false,
        containers_conf: false,
    },
    // The OCI backend on real `crun` (hosted runners carry crun 1.14.1 beside Podman
    // 4.9.3); Podman only pulls and exports the image there.
    Target {
        name: "oci_crun",
        nocapture: true,
        containers_conf: false,
    },
];

/// The OCI architecture of the machine `uname -m` names.
///
/// # Errors
/// A machine with no busybox architecture here.
pub fn arch_for(machine: &str) -> Result<&'static str, Error> {
    match machine {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        _ => Err(Error::Failed(format!(
            "no busybox architecture for {machine}"
        ))),
    }
}

/// The digest of the `linux/<arch>` manifest in `index_json`, an image index as `podman
/// manifest inspect` prints it: the first such entry, as a pull resolves it.
///
/// # Errors
/// The text is not JSON, no entry is `linux/<arch>` with a `sha256:` digest, or the
/// digest is `index_digest` (the index's own): the test of an image named by its index
/// would then mean nothing.
pub fn manifest_for(index_json: &str, index_digest: &str, arch: &str) -> Result<String, Error> {
    let index: Value = serde_json::from_str(index_json)
        .map_err(|e| Error::Failed(format!("the image index is not JSON: {e}")))?;
    let digest = index["manifests"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| (m["platform"]["os"] == "linux") & (m["platform"]["architecture"] == arch))
        .and_then(|m| m["digest"].as_str())
        .filter(|d| d.starts_with("sha256:"));
    let Some(digest) = digest else {
        return Err(Error::Failed(format!(
            "no linux/{arch} manifest with a sha256 digest in {BUSYBOX_NAME}@{index_digest}"
        )));
    };
    if digest == index_digest {
        return Err(Error::Failed(format!(
            "the linux/{arch} manifest's digest is the index's ({digest}); the index test \
             would mean nothing"
        )));
    }
    Ok(digest.to_owned())
}

/// One file under `root` (relative), trimmed, or `n/a` when it cannot be read.
fn read_or_na(root: &Path, file: &str) -> String {
    match std::fs::read_to_string(root.join(file)) {
        Ok(text) => text.trim().to_owned(),
        Err(_) => "n/a".to_owned(),
    }
}

/// How the kernel under `root` (`/` on a runner) reclaims into swap, which the swap
/// tests in `podman_memory.rs` depend on, and its swap areas: lines for the log.
pub fn host_report(root: &Path) -> String {
    format!(
        "kernel {}; vm.swappiness {}; zswap {}; lru_gen {}; THP {}\nswap areas:\n{}",
        read_or_na(root, "proc/sys/kernel/osrelease"),
        read_or_na(root, "proc/sys/vm/swappiness"),
        read_or_na(root, "sys/module/zswap/parameters/enabled"),
        read_or_na(root, "sys/kernel/mm/lru_gen/enabled"),
        read_or_na(root, "sys/kernel/mm/transparent_hugepage/enabled"),
        read_or_na(root, "proc/swaps"),
    )
}

/// Waits for a socket at `path`: `tries` looks, `every` apart.
///
/// # Errors
/// No socket there after the last look (nothing there, or a file of another type).
pub fn wait_for_socket(path: &Path, tries: u32, every: Duration) -> Result<(), Error> {
    for _ in 0..tries {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
            return Ok(());
        }
        std::thread::sleep(every);
    }
    Err(Error::Failed(format!(
        "no socket at {} after {tries} looks {every:?} apart",
        path.display()
    )))
}

/// The runtime directory systemd gives the user `uid` (its `XDG_RUNTIME_DIR`), where
/// its user manager's bus is.
pub fn default_runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

/// `sudo -n <args>`: its exit status. Its output is this process's.
fn sudo(program: &Path, args: &[OsString]) -> Result<ExitStatus, Error> {
    Command::new(program)
        .arg("-n")
        .args(args)
        .status()
        .map_err(Error::spawn(program.display().to_string()))
}

/// Readies a hosted runner's user for rootless Podman: lingering on (`sudo -n loginctl
/// enable-linger <uid>`, a no-op when it is on already), so its systemd user manager
/// runs and `runtime_dir`, its `XDG_RUNTIME_DIR`, holds the manager's bus. Every verb
/// that runs rootless Podman on a runner starts with it: `podman-tests` here, and
/// `kbf-cell m2` (crates/kbf-it/src/m2.rs).
///
/// # Errors
/// sudo could not be started or failed, or the bus did not appear within 10 s.
pub fn runner_setup(sudo_program: &Path, uid: u32, runtime_dir: &Path) -> Result<(), Error> {
    let args = ["loginctl", "enable-linger", &uid.to_string()].map(OsString::from);
    let status = sudo(sudo_program, &args)?;
    if !status.success() {
        return Err(Error::Failed(format!(
            "sudo -n loginctl enable-linger {uid} failed: {status}"
        )));
    }
    wait_for_socket(&runtime_dir.join("bus"), 100, Duration::from_millis(100))
}

/// Runs podman with `XDG_RUNTIME_DIR` set: its stdout. Its stderr is this process's.
fn podman(program: &Path, runtime_dir: &Path, args: &[&str]) -> Result<String, Error> {
    let out = Command::new(program)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .map_err(Error::spawn(program.display().to_string()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "podman {} failed: {}",
            args.join(" "),
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What one target's unit is given.
pub struct Unit<'a> {
    pub target: &'a Target,
    pub built: &'a Built,
    /// The runner user's uid and gid, which the unit runs as.
    pub uid: u32,
    pub gid: u32,
    /// The runner user's `HOME` and `PATH`.
    pub home: &'a OsStr,
    pub path: &'a OsStr,
    /// The runner user's `XDG_RUNTIME_DIR`.
    pub runtime_dir: &'a Path,
    /// The digest of this architecture's busybox manifest ([`manifest_for`]).
    pub manifest: &'a str,
}

/// `<prefix><value>`, for a value that need not be UTF-8.
fn joined(prefix: &str, value: &OsStr) -> OsString {
    let mut out = OsString::from(prefix);
    out.push(value);
    out
}

/// The arguments for `sudo -n` that run one target's binary in its own system unit:
/// `systemd-run`, waiting for it and passing its output and exit status through, in
/// `kbf-daemon.slice` with `Delegate=yes` and the daemon's `OOMScoreAdjust=-900`, as
/// the runner user, from the package's directory (where `cargo test` runs a binary),
/// with the binary's ignored tests included.
///
/// The environment is the runner user's `HOME`, `PATH` and `XDG_RUNTIME_DIR`, the
/// manifest as `KBF_TEST_IMAGE` and the index as `KBF_TEST_INDEX_IMAGE`, and for a
/// [`Target::containers_conf`] target `CONTAINERS_CONF_OVERRIDE`.
pub fn systemd_run_args(u: &Unit<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["systemd-run", "--quiet", "--wait", "--collect", "--pipe"]
        .map(OsString::from)
        .into();
    args.push(format!("--unit=kbf-podman-tests-{}", u.target.name).into());
    args.extend(
        [
            "--slice=kbf-daemon.slice",
            "-p",
            "Delegate=yes",
            "-p",
            "OOMScoreAdjust=-900",
        ]
        .map(OsString::from),
    );
    args.push(format!("--uid={}", u.uid).into());
    args.push(format!("--gid={}", u.gid).into());
    args.push(joined("--working-directory=", u.built.dir.as_os_str()));
    let mut env = vec![
        joined("HOME=", u.home),
        joined("PATH=", u.path),
        joined("XDG_RUNTIME_DIR=", u.runtime_dir.as_os_str()),
        format!("KBF_TEST_IMAGE=docker://{BUSYBOX_NAME}@{}", u.manifest).into(),
        format!("KBF_TEST_INDEX_IMAGE=docker://{BUSYBOX_NAME}@{BUSYBOX_INDEX}").into(),
    ];
    if u.target.containers_conf {
        env.push(joined(
            "CONTAINERS_CONF_OVERRIDE=",
            u.built.dir.join(CONTAINERS_CONF).as_os_str(),
        ));
    }
    for e in env {
        args.push("-E".into());
        args.push(e);
    }
    args.push(u.built.executable.clone().into());
    args.push("--include-ignored".into());
    if u.target.nocapture {
        args.push("--nocapture".into());
    }
    args
}

/// What `podman-tests` runs and where.
pub struct PodmanTests<'a> {
    /// The cargo program (on `cargo xtask`, the `CARGO` cargo sets).
    pub cargo: &'a OsStr,
    /// The sudo program; it is run with `-n`, so it never prompts.
    pub sudo: &'a Path,
    /// The podman program.
    pub podman: &'a Path,
    /// The repository root, where cargo runs.
    pub root: &'a Path,
    /// The runner user's runtime directory (`None`: `/run/user/<uid>`).
    pub runtime_dir: Option<&'a Path>,
}

/// Sets the runner up, pulls busybox, builds the test targets and runs each in its
/// own unit: the line to print when every target passed. The steps' output and the
/// tests' go to this process's stdout and stderr as they run.
///
/// # Errors
/// A step failed (the first one's error), or one or more targets failed (one line
/// each, after every target ran).
pub fn run(p: &PodmanTests<'_>) -> Result<Vec<String>, Error> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let default_dir = default_runtime_dir(uid);
    let runtime_dir = p.runtime_dir.unwrap_or(default_dir.as_path());
    runner_setup(p.sudo, uid, runtime_dir)?;
    print!("{}", podman(p.podman, runtime_dir, &["--version"])?);
    println!("{}", host_report(Path::new("/")));
    let index = format!("{BUSYBOX_NAME}@{BUSYBOX_INDEX}");
    podman(p.podman, runtime_dir, &["pull", "-q", &index])?;
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    let arch = arch_for(&machine)?;
    let inspect = podman(p.podman, runtime_dir, &["manifest", "inspect", &index])?;
    let manifest = manifest_for(&inspect, BUSYBOX_INDEX, arch)?;
    println!("busybox manifest for {arch}: {manifest}");

    let names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
    let messages = sudo_test::build(p.cargo, p.root, PACKAGE, &names)?;
    let mut built = Vec::new();
    for name in &names {
        built.push(sudo_test::find(&messages, name)?);
    }

    let home = std::env::var_os("HOME").unwrap_or_default();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut failed = Vec::new();
    for (target, built) in TARGETS.iter().zip(&built) {
        let unit = Unit {
            target,
            built,
            uid,
            gid,
            home: &home,
            path: &path,
            runtime_dir,
            manifest: &manifest,
        };
        let status = sudo(p.sudo, &systemd_run_args(&unit))?;
        if !status.success() {
            failed.push(format!(
                "the tests of -p {PACKAGE} --test {} failed in their unit ({}): {status}",
                target.name,
                built.executable.display()
            ));
        }
    }
    if failed.is_empty() {
        return Ok(vec![format!(
            "ok: the real-Podman tests of -p {PACKAGE} passed, each target in its own \
             Delegate=yes unit"
        )]);
    }
    Err(Error::Refused(failed))
}

#[cfg(test)]
mod tests;
