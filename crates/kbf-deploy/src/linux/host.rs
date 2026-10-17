//! What the host-changing `linux` verbs share: the tools they run, why they refuse,
//! and reads of the host made while planning.
//!
//! Every read goes through [`Exec`] (a query, an `inspect` or a `stat`), in a dry run as
//! in an apply, so a dry run refuses what an apply would.

use std::fmt;
use std::io::{self, Write};
use std::process::Output;

use crate::linux::check::Failure;
use crate::linux::config::LinuxError;
use crate::template::TemplateError;
use crate::unit::UnitError;
use kbf_host::exec::{Entry, Exec, Meta, succeeded};
use kbf_host::path::{HostPath, PathError};
use kbf_host::plan::Planner;
use kbf_host::step::{Cmd, Owner, Perms, Step, UrlError};
use kbf_host::user::{self, RunUser, UserError};

use super::check::CheckError;

/// The tools the verbs run, where Debian and Ubuntu put them.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
pub const PODMAN: &str = "/usr/bin/podman";
pub const SWAPON: &str = "/usr/sbin/swapon";
pub const SWAPOFF: &str = "/usr/sbin/swapoff";
pub const MKSWAP: &str = "/usr/sbin/mkswap";
pub const FALLOCATE: &str = "/usr/bin/fallocate";
pub const FINDMNT: &str = "/usr/bin/findmnt";
pub const DF: &str = "/usr/bin/df";
pub const CHMOD: &str = "/usr/bin/chmod";
pub const SS: &str = "/usr/bin/ss";

/// Why a host-changing verb refused, or failed.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error(transparent)]
    Config(#[from] LinuxError),
    #[error(transparent)]
    Template(#[from] TemplateError),
    #[error(transparent)]
    Render(#[from] super::modes::RenderError),
    #[error(transparent)]
    Check(#[from] CheckError),
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    User(#[from] UserError),
    #[error(transparent)]
    Url(#[from] UrlError),
    #[error(transparent)]
    Unit(#[from] UnitError),
    #[error(transparent)]
    Deploy(#[from] kbf_host::HostError),
    #[error("cannot read {path}: {source}")]
    Inspect { path: HostPath, source: io::Error },
    #[error("{cmd}: {source}")]
    Query { cmd: String, source: io::Error },
    #[error("{cmd} printed {out:?}, which is not what this kit reads")]
    Output { cmd: String, out: String },
    #[error("cannot write the plan out: {0}")]
    Write(io::Error),

    // install-units
    #[error(
        "no cgroup v2 memory controller at /sys/fs/cgroup: boot with systemd.unified_cgroup_hierarchy=1"
    )]
    NoMemoryController,
    #[error(
        "{0} is not a systemd unit on this host, so the MinIO drop-in would apply to nothing. If MinIO runs in Kubernetes here, its priority comes from the pod"
    )]
    NoMinioUnit(String),
    #[error("no user {0}: useradd --system --create-home {0}")]
    NoUser(String),
    #[error(
        "{user} has no range in {file} (rootless Podman): usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}"
    )]
    NoSubIds { user: String, file: HostPath },
    #[error(
        "{user} does not linger, so /run/user/{uid} (Podman's run directory) ends with its last session: loginctl enable-linger {user}"
    )]
    NoLinger { user: String, uid: u32 },
    #[error("{0} is missing (docs/deploy/storage-node.md, \"Files the units read\")")]
    Missing(HostPath),
    #[error("{path} is owned by uid {uid}, not {user}: chown {user} {path}")]
    NotOwned {
        path: HostPath,
        uid: u32,
        user: String,
    },
    #[error("{path} has mode {perms}: chmod 600 {path}")]
    Readable { path: HostPath, perms: Perms },
    #[error("{0} needs AWS_ACCESS_KEY_ID= and AWS_SECRET_ACCESS_KEY= lines, each with a value")]
    S3Env(HostPath),
    #[error("port {0} is in use: free it or pick another in the config")]
    PortInUse(u16),
    #[error("the rendered units break the priorities; nothing was written:\n{}", lines(.0))]
    Units(Vec<Failure>),
    #[error("{0} is not an executable (--bin-dir DIR, or install it in KBF_BIN_DIR)")]
    NotExecutable(HostPath),
    #[error("{bin} has no {flag} (its --help does not list it), which its unit passes")]
    NoFlag { bin: HostPath, flag: String },

    // install-units --mode
    #[error("{0} names no User= or root: kbf runs as a user of its own")]
    RunsAsRoot(String),
    #[error(transparent)]
    ModeCheck(#[from] super::modes::ParseError),

    // image-pull
    #[error("no image: set KBF_IMAGES or pass --image <repo>@sha256:<digest>")]
    NoImages,
    #[error(
        "image-pull reads and fills {0}'s Podman store as {0}: run it with sudo, even for a dry run"
    )]
    PullNeedsRoot(String),
    #[error(
        "{image}: the store's copy has digest {got}, not the pinned one (an index digest? pin the {platform} manifest's digest)"
    )]
    WrongDigest {
        image: String,
        got: String,
        platform: String,
    },
    #[error("{image} is {got}, not {want}")]
    WrongPlatform {
        image: String,
        got: String,
        want: String,
    },
    #[error("{0} is still not in the store after its pull")]
    NotPulled(String),
    /// The check after an apply failed. Steps ran, so this is a failed verb (exit 1),
    /// not a refusal.
    #[error("the check after the apply: {0}")]
    AfterApply(Box<HostError>),

    // swap
    #[error("KBF_SWAP_SIZE={0}: give it in whole GiB, as 128G")]
    SwapSize(String),
    #[error(
        "{0} is active: kubelet refuses to start with swap on unless its config sets failSwapOn: false. Check that first, then re-run with --kubelet-allows-swap"
    )]
    Kubelet(&'static str),
    #[error("{0} is there and is not a regular file")]
    NotAFile(HostPath),
    #[error(
        "{path} exists with {len} bytes, not {size} ({want}): resize it by hand (swapoff, remove, re-run)"
    )]
    SwapLen {
        path: HostPath,
        len: u64,
        size: String,
        want: u64,
    },
    #[error("{dir} is on {fs}: this step makes swapfiles on ext4 or xfs only (fallocate)")]
    FsType { dir: String, fs: String },
    #[error("{dir} has {free_gib} GiB free: {size} and 16 GiB to spare are needed")]
    NoSpace {
        dir: String,
        free_gib: u64,
        size: String,
    },
}

/// Failures one to a line.
fn lines(failures: &[Failure]) -> String {
    let lines: Vec<String> = failures.iter().map(ToString::to_string).collect();
    lines.join("\n")
}

/// A tool's path.
pub fn tool(path: &str) -> HostPath {
    HostPath::new(path).expect("a tool's path is absolute")
}

/// Print one line of what the plan found, before its steps.
pub fn note(out: &mut dyn Write, line: fmt::Arguments<'_>) -> Result<(), HostError> {
    writeln!(out, "{line}").map_err(HostError::Write)
}

/// Run `cmd` as a query; its exit status is the caller's to read.
pub fn query<E: Exec>(exec: &E, cmd: &Cmd) -> Result<Output, HostError> {
    exec.query(cmd).map_err(|source| HostError::Query {
        cmd: cmd.to_string(),
        source,
    })
}

/// Run `cmd` as a query; a non-zero exit is an error naming its stderr. Its output.
pub fn checked<E: Exec>(exec: &E, cmd: &Cmd) -> Result<String, HostError> {
    let out = succeeded(cmd, query(exec, cmd)?).map_err(|source| HostError::Query {
        cmd: cmd.to_string(),
        source,
    })?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `cmd` as a query and tell whether it exited 0.
pub fn exits_0<E: Exec>(exec: &E, cmd: &Cmd) -> Result<bool, HostError> {
    Ok(query(exec, cmd)?.status.success())
}

/// `systemctl <verb> --quiet <unit>` exited 0: `is-active`, `is-enabled`.
pub fn unit_is<E: Exec>(exec: &E, verb: &str, unit: &str) -> Result<bool, HostError> {
    let cmd = Cmd::new(tool(SYSTEMCTL)).arg(verb).arg("--quiet").arg(unit);
    exits_0(exec, &cmd)
}

/// The active swap areas, as `swapon --show` names them.
pub fn swaps<E: Exec>(exec: &E) -> Result<Vec<String>, HostError> {
    let cmd = Cmd::new(tool(SWAPON))
        .arg("--show=NAME")
        .arg("--noheadings");
    Ok(checked(exec, &cmd)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn inspect<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Entry>, HostError> {
    exec.inspect(path).map_err(|source| HostError::Inspect {
        path: path.clone(),
        source,
    })
}

pub fn stat<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Meta>, HostError> {
    exec.stat(path).map_err(|source| HostError::Inspect {
        path: path.clone(),
        source,
    })
}

/// The user `name`, or [`HostError::NoUser`].
pub fn user_named<E: Exec>(exec: &E, name: &str) -> Result<RunUser, HostError> {
    user::lookup(exec, name)?.ok_or_else(|| HostError::NoUser(name.to_owned()))
}

/// `path` joined under the directory `dir`.
pub fn under(dir: &HostPath, name: &str) -> Result<HostPath, HostError> {
    Ok(HostPath::new(&format!("{dir}/{name}"))?)
}

/// Plan `bytes` into `dst` (0755, owned by `root`), keeping a different binary there as
/// `<dst>.previous` (its mode, owned by `root`: a copy left as its old owner's, beside
/// root's binaries, is one rename from being run as root); whether it changes. The
/// `linux` and `mdm` kits install their binaries through it.
pub fn binary<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    dst: &HostPath,
    bytes: Vec<u8>,
    root: Owner,
    out: &mut dyn Write,
) -> Result<bool, HostError> {
    parents(exec, planner, dst)?;
    if let Some(Entry::File {
        perms, bytes: old, ..
    }) = inspect(exec, dst)?
        && old != bytes
    {
        let previous = HostPath::new(&format!("{dst}.previous"))?;
        planner.file(previous, perms, Some(root), old)?;
    }
    let changed = planner.file(dst.clone(), Perms::new(0o755), Some(root), bytes)?;
    if !changed {
        note(out, format_args!("{dst}: unchanged"))?;
    }
    Ok(changed)
}

/// Plan a directory for every missing directory above `path` (root's, 0755) that is
/// not planned yet, so a step that makes `path` finds its parent.
pub fn parents<E: Exec>(
    exec: &E,
    planner: &mut Planner<'_, E>,
    path: &HostPath,
) -> Result<(), HostError> {
    let s = path.as_str();
    for (i, _) in s.match_indices('/').skip(1) {
        let dir = HostPath::new(&s[..i])?;
        let planned = planner
            .steps()
            .iter()
            .any(|step| matches!(step, Step::Mkdir { path, .. } if *path == dir));
        if !planned && stat(exec, &dir)?.is_none() {
            planner.dir(dir, Perms::new(0o755), None)?;
        }
    }
    Ok(())
}
