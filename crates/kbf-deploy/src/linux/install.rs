//! `linux install-units`: the converged storage node's units on the host.
//!
//! It checks the host first, read only, and each refusal says the fix: a cgroup v2
//! memory controller; `MINIO_UNIT` is a systemd unit here; the two users exist, and
//! the kbf user has subuid and subgid ranges (a line keyed by its name or its uid)
//! and lingers; the key files are their service's alone, the certificates and deny
//! list exist and `s3.env` names a key pair; and, unless kbf-server runs, its three
//! ports are free. A host with no active swap gets a warning. It then renders the
//! units (`render-units` with the kbf user's uid), refuses them unless `check-units
//! --installable` passes, and refuses a binary whose `--help` lacks a flag its unit
//! passes.
//!
//! The plan has each of these only when the host differs:
//! 1. with `--bin-dir`, `kbf-server` and `kbf-daemon` copied into `KBF_BIN_DIR` (0755),
//!    the binary they replace kept as `<name>.previous` (its mode);
//! 2. `kbf.slice`, the MinIO drop-in, `kbf-server.service` and `kbf-daemon.service`
//!    under `/etc/systemd/system` (0644); these and the binaries are root's (uid and
//!    gid 0), so one with the right bytes and mode that another user owns is replaced;
//! 3. the metadata directory (`KBF_META_DIR`, 0700, the server's user's) and the data
//!    directory (`KBF_DATA_DIR`, 0755, the daemon's user's);
//! 4. when a binary or unit changed: `daemon-reload`, then enable and restart
//!    kbf-server and kbf-daemon; otherwise each is enabled and started only if it is
//!    not; after any start, a wait of up to 60 s for kbf-server's `/readyz`.
//!
//! MinIO is never restarted unless `--restart-minio` is given: it serves the farm, and
//! its new priorities take effect only at its next start.
//!
//! `--uninstall` is the rollback: kbf-daemon and kbf-server stopped and disabled
//! (when either is enabled or running), the four files moved to
//! `/var/lib/kbf-linux/removed/<unix seconds>/`, `daemon-reload`.
//! The binaries, the Raft log, the scratch, the bucket and the swapfile stay.

use std::io::Write;
use std::time::Duration;

use super::check::check_texts;
use super::config::LinuxConfig;
use super::host::{
    self, HostError, SS, SYSTEMCTL, binary, inspect, note, parents, stat, tool, under, unit_is,
};
use super::render::{DAEMON, DROP_IN, SERVER, SLICE, Templates, render};
use crate::unit::UnitFile;
use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::plan::{Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::{Cmd, HttpUrl, Owner, Perms};
use kbf_host::user::RunUser;

pub(super) const UNITS: &str = "/etc/systemd/system";
const REMOVED: &str = "/var/lib/kbf-linux/removed";
const CONTROLLERS: &str = "/sys/fs/cgroup/cgroup.controllers";
const LINGER: &str = "/var/lib/systemd/linger";
const SERVICES: [&str; 2] = [SERVER, DAEMON];
const BINARIES: [&str; 2] = ["kbf-server", "kbf-daemon"];
/// Certificates and the deny list: they must exist.
const PUBLIC: [&str; 5] = ["ca.pem", "reapi.pem", "worker.pem", "node.pem", "deny-list"];
/// How long kbf-server has to answer `/readyz` after a start.
pub const READY: Duration = Duration::from_secs(60);
/// Who owns each binary, `.previous` copy and unit install-units writes: root, user and
/// group. The in-place check compares it too, so a right file another user owns (and so
/// could rewrite what systemd runs as root) is replaced.
pub(super) const ROOT: Owner = Owner { uid: 0, gid: 0 };

/// What `install-units` was asked for.
pub struct Install {
    pub config: LinuxConfig,
    /// Where new `kbf-server` and `kbf-daemon` binaries are; none: keep the installed.
    pub bin_dir: Option<HostPath>,
    pub restart_minio: bool,
}

/// Check the host, then plan the binaries, units, directories and service steps.
pub fn plan<E: Exec>(exec: &E, a: &Install, out: &mut dyn Write) -> Result<Plan, HostError> {
    plan_with(exec, a, &Templates::BUILT_IN, ROOT, out)
}

/// [`plan`] with `templates` and with `root` as the owner of what root writes (a scratch
/// test can chown only to its own user).
fn plan_with<E: Exec>(
    exec: &E,
    a: &Install,
    templates: &Templates<'_>,
    root: Owner,
    out: &mut dyn Write,
) -> Result<Plan, HostError> {
    let c = &a.config;
    let (kbf, server) = preflight(exec, c, out)?;
    let units = render(c, Some(kbf.uid), templates)?;
    let report = check_texts(c, units.files(), true)?;
    if !report.failures.is_empty() {
        return Err(HostError::Units(report.failures));
    }
    let bin_from = a.bin_dir.as_ref().unwrap_or(&c.bin_dir);
    let mut new = Vec::new();
    for (bin, text) in BINARIES.into_iter().zip([&units.server, &units.daemon]) {
        new.push(supports(exec, &under(bin_from, bin)?, text)?);
    }

    let mut planner = Planner::new(exec);
    let mut changed = false;
    if a.bin_dir.is_some() {
        for (bin, bytes) in BINARIES.into_iter().zip(new) {
            let dst = under(&c.bin_dir, bin)?;
            changed |= binary(exec, &mut planner, &dst, bytes, root, out)?;
        }
    }
    let dir = HostPath::new(UNITS)?;
    for (name, text) in units.files() {
        let path = under(&dir, &name)?;
        parents(exec, &mut planner, &path)?;
        if planner.file(
            path.clone(),
            Perms::new(0o644),
            Some(root),
            text.as_bytes().to_vec(),
        )? {
            changed = true;
            note(out, format_args!("{path}: writes"))?;
            for line in text.lines() {
                note(out, format_args!("    | {line}"))?;
            }
        } else {
            note(out, format_args!("{path}: unchanged"))?;
        }
    }
    // The metadata directory is the server's alone; the data directory is searchable by
    // others, for a rootless container's root (the daemon makes `leases` and `cache` 0700
    // and its tree store 0711 itself).
    let owned = [(&c.meta_dir, &server, 0o700), (&c.data_dir, &kbf, 0o755)];
    for (path, user, mode) in owned {
        parents(exec, &mut planner, path)?;
        let owner = Owner {
            uid: user.uid,
            gid: user.gid,
        };
        planner.dir(path.clone(), Perms::new(mode), Some(owner))?;
    }

    let mut started = false;
    if changed {
        planner.run(systemctl(&["daemon-reload"]));
        if a.restart_minio {
            planner.run(systemctl(&["restart", &c.minio_unit]));
        } else {
            note(
                out,
                format_args!(
                    "NOTE: {} keeps its old priorities until it restarts; restart it in a quiet window (or re-run with --restart-minio)",
                    c.minio_unit
                ),
            )?;
        }
        planner.run(systemctl(&["enable", SERVER, DAEMON]));
        planner.run(systemctl(&["restart", SERVER, DAEMON]));
        started = true;
    } else {
        note(
            out,
            format_args!("units and binaries unchanged: nothing restarted"),
        )?;
        for unit in SERVICES {
            if !unit_is(exec, "is-enabled", unit)? {
                planner.run(systemctl(&["enable", unit]));
            }
            if !unit_is(exec, "is-active", unit)? {
                planner.run(systemctl(&["start", unit]));
                started = true;
            }
        }
    }
    if started {
        let url = HttpUrl::new(&format!("http://127.0.0.1:{}/readyz", c.api_port))?;
        planner.wait_http(url, READY);
    }
    Ok(planner.finish())
}

/// The host checks, read only; the kbf user and kbf-server's user.
fn preflight<E: Exec>(
    exec: &E,
    c: &LinuxConfig,
    out: &mut dyn Write,
) -> Result<(RunUser, RunUser), HostError> {
    let controllers = text_of(exec, &HostPath::new(CONTROLLERS)?)?;
    if !controllers.is_some_and(|t| t.split_whitespace().any(|w| w == "memory")) {
        return Err(HostError::NoMemoryController);
    }
    if !host::exits_0(exec, &systemctl(&["cat", "--", &c.minio_unit]))? {
        return Err(HostError::NoMinioUnit(c.minio_unit.clone()));
    }
    let kbf = host::user_named(exec, &c.user)?;
    let server = host::user_named(exec, &c.server_user)?;
    rootless(exec, &kbf)?;
    let private = [
        ("s3.env", &server),
        ("api-token", &server),
        ("reapi.key", &server),
        ("worker.key", &server),
        ("node.key", &kbf),
    ];
    for (name, user) in private {
        let path = under(&c.etc, name)?;
        match stat(exec, &path)? {
            Some(m) if m.kind == Kind::File => {
                if m.owner.uid != user.uid {
                    return Err(HostError::NotOwned {
                        path,
                        uid: m.owner.uid,
                        user: user.name.clone(),
                    });
                }
                if m.perms.bits() & 0o077 != 0 {
                    return Err(HostError::Readable {
                        path,
                        perms: m.perms,
                    });
                }
            }
            _ => return Err(HostError::Missing(path)),
        }
    }
    for name in PUBLIC {
        let path = under(&c.etc, name)?;
        if !stat(exec, &path)?.is_some_and(|m| m.kind == Kind::File) {
            return Err(HostError::Missing(path));
        }
    }
    let s3 = under(&c.etc, "s3.env")?;
    let pair = text_of(exec, &s3)?.is_some_and(|t| {
        ["AWS_ACCESS_KEY_ID=", "AWS_SECRET_ACCESS_KEY="]
            .iter()
            .all(|key| {
                t.lines()
                    .any(|l| l.strip_prefix(key).is_some_and(|v| !v.is_empty()))
            })
    });
    if !pair {
        return Err(HostError::S3Env(s3));
    }
    if host::swaps(exec)?.is_empty() {
        note(
            out,
            format_args!(
                "WARN: no swap is active: run `kbf-deploy linux swap` first (builds may swap; MinIO and kbf-server may not)"
            ),
        )?;
    }
    if unit_is(exec, "is-active", SERVER)? {
        note(
            out,
            format_args!("kbf-server is running: its ports are its own"),
        )?;
    } else {
        // `ss -Hltn`: `<state> <recv-q> <send-q> <local address:port> <peer>`.
        let listening = host::checked(exec, &Cmd::new(tool(SS)).arg("-Hltn"))?;
        for port in [c.reapi_port, c.worker_port, c.api_port] {
            let tail = format!(":{port}");
            let held = listening.lines().any(|l| {
                l.split_whitespace()
                    .nth(3)
                    .is_some_and(|local| local.ends_with(&tail))
            });
            if held {
                return Err(HostError::PortInUse(port));
            }
        }
    }
    Ok((kbf, server))
}

/// `kbf` can run rootless Podman: it has subuid and subgid ranges and lingers.
pub(super) fn rootless<E: Exec>(exec: &E, kbf: &RunUser) -> Result<(), HostError> {
    // A range line is `<user>:<first id>:<count>`, the user named or given by uid
    // (subuid(5); newuidmap takes either).
    let uid = kbf.uid.to_string();
    for file in ["/etc/subuid", "/etc/subgid"] {
        let file = HostPath::new(file)?;
        let ranged = text_of(exec, &file)?.is_some_and(|t| {
            t.lines().any(|l| {
                l.split_once(':')
                    .is_some_and(|(who, _)| who == kbf.name || who == uid)
            })
        });
        if !ranged {
            return Err(HostError::NoSubIds {
                user: kbf.name.clone(),
                file,
            });
        }
    }
    if stat(exec, &under(&HostPath::new(LINGER)?, &kbf.name)?)?.is_none() {
        return Err(HostError::NoLinger {
            user: kbf.name.clone(),
            uid: kbf.uid,
        });
    }
    Ok(())
}

/// A file's text, `None` when there is no regular file at `path`.
fn text_of<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<String>, HostError> {
    Ok(match inspect(exec, path)? {
        Some(Entry::File { bytes, .. }) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    })
}

/// The binary at `bin` is an executable whose `--help` lists every `--flag` its unit's
/// `ExecStart=` passes; its bytes.
fn supports<E: Exec>(exec: &E, bin: &HostPath, unit: &str) -> Result<Vec<u8>, HostError> {
    let bytes = match inspect(exec, bin)? {
        Some(Entry::File { perms, bytes, .. }) if perms.bits() & 0o111 != 0 => bytes,
        _ => return Err(HostError::NotExecutable(bin.clone())),
    };
    let help = host::checked(exec, &Cmd::new(bin.clone()).arg("--help"))?;
    let listed: Vec<&str> = help
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .collect();
    let unit = UnitFile::parse(unit)?;
    for word in unit.exec_start() {
        let flag = word.split('=').next().unwrap_or(word);
        if flag.starts_with("--") && !listed.contains(&flag) {
            return Err(HostError::NoFlag {
                bin: bin.clone(),
                flag: flag.to_owned(),
            });
        }
    }
    Ok(bytes)
}

/// Plan the rollback; `stamp` names the directory the files move to.
pub fn plan_uninstall<E: Exec>(
    exec: &E,
    c: &LinuxConfig,
    stamp: &str,
    out: &mut dyn Write,
) -> Result<Plan, HostError> {
    let mut planner = Planner::new(exec);
    let mut on = false;
    for unit in SERVICES {
        on |= unit_is(exec, "is-enabled", unit)? || unit_is(exec, "is-active", unit)?;
    }
    if on {
        planner.run(systemctl(&["disable", "--now", DAEMON, SERVER]));
    }
    let dir = HostPath::new(UNITS)?;
    let to = under(&HostPath::new(REMOVED)?, stamp)?;
    let mut moved = false;
    let names = [
        SLICE.to_owned(),
        format!("{}.d/{DROP_IN}", c.minio_unit),
        SERVER.to_owned(),
        DAEMON.to_owned(),
    ];
    for name in names {
        let path = under(&dir, &name)?;
        if let Some(Entry::File {
            perms,
            owner,
            bytes,
        }) = inspect(exec, &path)?
        {
            let kept = under(&to, &name)?;
            parents(exec, &mut planner, &kept)?;
            planner.file(kept, perms, Some(owner), bytes)?;
            planner.absent(path)?;
            moved = true;
        } else {
            note(out, format_args!("{name}: not installed"))?;
        }
    }
    if moved {
        planner.run(systemctl(&["daemon-reload"]));
    }
    note(
        out,
        format_args!(
            "MinIO keeps running; it returns to its own unit's settings at its next start."
        ),
    )?;
    note(
        out,
        format_args!(
            "kept: the binaries, {}, {}, the bucket {} and the swapfile",
            c.meta_dir, c.data_dir, c.s3_bucket
        ),
    )?;
    Ok(planner.finish())
}

/// Plan, then print or carry the plan out.
pub fn run<E: Exec>(
    exec: &mut E,
    a: &Install,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), HostError> {
    let p = plan(&*exec, a, &mut *out)?;
    Ok(execute(&p, RunAs::Root, mode, exec, &mut out)?)
}

/// [`plan_uninstall`], then print or carry the plan out.
pub fn run_uninstall<E: Exec>(
    exec: &mut E,
    c: &LinuxConfig,
    stamp: &str,
    mode: Mode,
    mut out: &mut dyn Write,
) -> Result<(), HostError> {
    let p = plan_uninstall(&*exec, c, stamp, &mut *out)?;
    Ok(execute(&p, RunAs::Root, mode, exec, &mut out)?)
}

pub(super) fn systemctl(args: &[&str]) -> Cmd {
    args.iter()
        .fold(Cmd::new(tool(SYSTEMCTL)), |cmd, arg| cmd.arg(*arg))
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
