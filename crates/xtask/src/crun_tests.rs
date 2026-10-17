//! `cargo xtask crun-tests`: the container driver's real-`crun` tests
//! (`crates/kbf-driver-container/tests/crun*.rs` and `oci_crun.rs`, every test
//! `#[ignore]` with the reason) on a GitHub-hosted Linux runner. kbf runs containers
//! with `crun` only, and each test's image is fetched from its registry by the driver's
//! own image store (`kbf-image`), digest-verified.
//!
//! - this architecture's busybox manifest is one of two pinned digests ([`manifest_for`]),
//!   so no index is resolved here and the tests name the image as a daemon is told to: by
//!   the per-architecture manifest digest ([`arch_for`] maps the machine);
//! - the test targets are built as the runner user ([`sudo_test::build`]), before any unit
//!   starts, so a unit only runs a binary;
//! - each target's binary is run in its own system unit with `Delegate=yes`, as the runner
//!   user ([`systemd_run_args`]): the user manager alone gets no io controller, and a
//!   daemon must own its subtree. The tests set the unit's cgroup up with kbf-daemon's
//!   own code (a `supervisor` leaf, then `actions` with cpu, memory and pids). The unit
//!   lowers its OOM score as the daemon's does (`docs/deploy/linux-build-host.md`), so
//!   the tests see what an action inherits from it.
//!
//! Every target runs even when one before it failed; the verb fails if any did.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, ExitStatus};

use crate::Error;
use crate::sudo_test::{self, Built};

/// The busybox image the tests run, without its digest.
pub const BUSYBOX_NAME: &str = "docker.io/library/busybox";
/// busybox 1.37.0, the digest of its image index: what the tests that must see an index
/// refused name.
pub const BUSYBOX_INDEX: &str =
    "sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";
/// The `linux/amd64` manifest of that index.
pub const BUSYBOX_AMD64: &str =
    "sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4";
/// The `linux/arm64` manifest of that index.
pub const BUSYBOX_ARM64: &str =
    "sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c";
/// The package the tests are in.
pub const PACKAGE: &str = "kbf-driver-container";

/// One integration test target and how it runs.
#[derive(Debug)]
pub struct Target {
    /// The file in the package's `tests/`, without `.rs`.
    pub name: &'static str,
    /// Run with `--nocapture`, so its timings show in the log.
    pub nocapture: bool,
}

/// The real-`crun` test targets, in the order they run.
pub const TARGETS: &[Target] = &[
    Target {
        name: "crun",
        nocapture: false,
    },
    Target {
        name: "crun_memory",
        nocapture: false,
    },
    Target {
        name: "crun_killed",
        nocapture: false,
    },
    Target {
        name: "crun_cache",
        nocapture: true,
    },
    Target {
        name: "crun_env",
        nocapture: false,
    },
    // The node's cold-store key reaches no action through the runtime's environment or
    // its files.
    Target {
        name: "crun_credential",
        nocapture: false,
    },
    // The sandbox as `config.json` says it, read from inside.
    Target {
        name: "oci_crun",
        nocapture: true,
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

/// The pinned busybox manifest of `arch` (`amd64` or `arm64`, [`arch_for`]'s answers).
///
/// # Errors
/// An architecture with no pin.
pub fn manifest_for(arch: &str) -> Result<&'static str, Error> {
    match arch {
        "amd64" => Ok(BUSYBOX_AMD64),
        "arm64" => Ok(BUSYBOX_ARM64),
        _ => Err(Error::Failed(format!(
            "no pinned {BUSYBOX_NAME} manifest for linux/{arch}"
        ))),
    }
}

/// One file under `root` (relative), trimmed, or `n/a` when it cannot be read.
fn read_or_na(root: &Path, file: &str) -> String {
    match std::fs::read_to_string(root.join(file)) {
        Ok(text) => text.trim().to_owned(),
        Err(_) => "n/a".to_owned(),
    }
}

/// How the kernel under `root` (`/` on a runner) reclaims into swap, which the swap
/// tests in `crun_memory.rs` depend on, and its swap areas: lines for the log.
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

/// `sudo -n <args>`: its exit status. Its output is this process's.
fn sudo(program: &Path, args: &[OsString]) -> Result<ExitStatus, Error> {
    Command::new(program)
        .arg("-n")
        .args(args)
        .status()
        .map_err(Error::spawn(program.display().to_string()))
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
/// The environment is the runner user's `HOME` and `PATH`, the manifest as
/// `KBF_TEST_IMAGE` and the index as `KBF_TEST_INDEX_IMAGE`.
pub fn systemd_run_args(u: &Unit<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["systemd-run", "--quiet", "--wait", "--collect", "--pipe"]
        .map(OsString::from)
        .into();
    args.push(format!("--unit=kbf-crun-tests-{}", u.target.name).into());
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
    let env = [
        joined("HOME=", u.home),
        joined("PATH=", u.path),
        format!("KBF_TEST_IMAGE=docker://{BUSYBOX_NAME}@{}", u.manifest).into(),
        format!("KBF_TEST_INDEX_IMAGE=docker://{BUSYBOX_NAME}@{BUSYBOX_INDEX}").into(),
    ];
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

/// What `crun-tests` runs and where.
pub struct CrunTests<'a> {
    /// The cargo program (on `cargo xtask`, the `CARGO` cargo sets).
    pub cargo: &'a OsStr,
    /// The sudo program; it is run with `-n`, so it never prompts.
    pub sudo: &'a Path,
    /// The repository root, where cargo runs.
    pub root: &'a Path,
}

/// Reports the host, builds the test targets and runs each in its own unit: the line to
/// print when every target passed. The steps' output and the tests' go to this process's
/// stdout and stderr as they run.
///
/// # Errors
/// A step failed (the first one's error), or one or more targets failed (one line
/// each, after every target ran).
pub fn run(p: &CrunTests<'_>) -> Result<Vec<String>, Error> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    println!("{}", host_report(Path::new("/")));
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    let arch = arch_for(&machine)?;
    let manifest = manifest_for(arch)?;
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
            manifest,
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
            "ok: the real-crun tests of -p {PACKAGE} passed, each target in its own \
             Delegate=yes unit"
        )]);
    }
    Err(Error::Refused(failed))
}

#[cfg(test)]
mod tests;
