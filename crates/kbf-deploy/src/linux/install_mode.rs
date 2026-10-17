//! `linux install-units --mode MODE`: one mode's unit of the one `kbf` binary on the
//! host (docs/deploy/kbf-modes.md, "Installing a mode's unit").
//!
//! It checks the host first, read only, and each refusal says the fix:
//! - the unit names a `User=` other than root, and the host has that user; for a mode
//!   that runs builds, the kbf user has subuid and subgid ranges and lingers;
//! - `kbf` is an executable in `--bin-dir` (or, without it, in `KBF_BIN_DIR`);
//! - the rendered unit passes `check-units --mode MODE --installable` on this host
//!   ([`modes::check_on_host`]), its flags parsed by that `kbf`'s own `check-args`;
//! - every file the unit reads is there: each `EnvironmentFile=` (the cold store's key,
//!   the state store's URI), each file a `--*key=` or `--*token-file=` flag names the
//!   unit's user's alone (owner and no group or other bit: kbf reads it as that user),
//!   and each a `--*cert=`, `--*-ca=` or `--*deny-list=` flag names.
//!
//! The plan has each of these only when the host differs:
//! 1. with `--bin-dir`, `kbf` copied into `KBF_BIN_DIR` (0755, root's), the binary it
//!    replaces kept as `kbf.previous`;
//! 2. `kbf.slice` and `kbf-<mode>.service` (the frontend's is the template
//!    `kbf-frontend@.service`) under `/etc/systemd/system` (0644, root's);
//! 3. the directories the unit's flags name, the unit's user's: `--data-dir` (0755),
//!    `--meta-dir`, a `file://` `--hot-store` or `--cold-store`, and the directory of a
//!    `--state-store=sqlite://` file (0700);
//! 4. when a file changed: `daemon-reload`, then enable and restart the unit; otherwise
//!    it is enabled and started only if it is not (the frontend's template is enabled as
//!    such and `kbf-frontend@a` started; a serving instance is never restarted, the plan
//!    says to run `kbf frontend handover`); after any start of a unit with
//!    `--api-listen=127.0.0.1:<port>`, a wait of up to 60 s for its `/readyz`.
//!
//! Without `--apply` every step is a `DRY-RUN:` line: nothing is written, reloaded or
//! started. There is no `--uninstall` for a mode's unit yet (`install-units
//! --uninstall` moves the four files of the converged node's units aside).

use std::io::Write;

use super::LinuxConfig;
use super::host::{self, HostError, binary, inspect, note, parents, stat, under, unit_is};
use super::install::{READY, ROOT, UNITS, rootless, systemctl};
use super::modes::{self, Mode, UnitText};
use super::render::{SLICE, Templates, render};
use crate::unit::UnitFile;
use kbf_host::exec::{Entry, Exec, Kind};
use kbf_host::path::HostPath;
use kbf_host::plan::{self, Plan, Planner, RunAs};
use kbf_host::step::{HttpUrl, Owner, Perms};
use kbf_host::user::RunUser;

/// What `install-units --mode` was asked for.
pub struct InstallMode {
    pub config: LinuxConfig,
    pub mode: Mode,
    /// Where a new `kbf` is; none: the installed one stays.
    pub bin_dir: Option<HostPath>,
}

/// Check the host, then plan the binary, the two units, the directories and the unit's
/// start.
pub fn plan<E: Exec>(exec: &E, a: &InstallMode, out: &mut dyn Write) -> Result<Plan, HostError> {
    plan_with(exec, a, ROOT, out)
}

/// [`plan`] with `root` as the owner of what root writes (a scratch test can chown only
/// to its own user).
fn plan_with<E: Exec>(
    exec: &E,
    a: &InstallMode,
    root: Owner,
    out: &mut dyn Write,
) -> Result<Plan, HostError> {
    let (c, mode) = (&a.config, a.mode);
    let kbf_user = if mode.runs_builds() {
        let kbf = host::user_named(exec, &c.user)?;
        rootless(exec, &kbf)?;
        Some(kbf)
    } else {
        None
    };
    let name = mode.unit_name();
    let text = modes::render(c, kbf_user.as_ref().map(|u| u.uid), mode)?;
    let slice = render(c, None, &Templates::BUILT_IN)?.slice;
    let unit = UnitFile::parse(&text)?;
    let user = host::user_named(exec, unit_user(&unit, &name)?)?;

    let bin = under(a.bin_dir.as_ref().unwrap_or(&c.bin_dir), "kbf")?;
    let bytes = match inspect(exec, &bin)? {
        Some(Entry::File { perms, bytes, .. }) if perms.bits() & 0o111 != 0 => bytes,
        _ => return Err(HostError::NotExecutable(bin)),
    };
    let report = modes::check_on_host(
        exec,
        c,
        mode,
        &UnitText::new(name.clone(), text.clone()),
        &UnitText::new(SLICE, slice.clone()),
        true,
        Some(&bin),
    )?;
    if !report.failures.is_empty() {
        return Err(HostError::Units(report.failures));
    }
    for line in &report.notes {
        note(out, format_args!("{line}"))?;
    }
    files_read(exec, &unit, &user)?;

    let mut planner = Planner::new(exec);
    let mut changed = false;
    if a.bin_dir.is_some() {
        let dst = under(&c.bin_dir, "kbf")?;
        changed |= binary(exec, &mut planner, &dst, bytes, root, out)?;
    }
    let units = HostPath::new(UNITS)?;
    for (file, text) in [(SLICE, &slice), (name.as_str(), &text)] {
        let path = under(&units, file)?;
        parents(exec, &mut planner, &path)?;
        let wrote = planner.file(
            path.clone(),
            Perms::new(0o644),
            Some(root),
            text.as_bytes().to_vec(),
        )?;
        changed |= wrote;
        let what = if wrote { "writes" } else { "unchanged" };
        note(out, format_args!("{path}: {what}"))?;
    }
    let owner = Owner {
        uid: user.uid,
        gid: user.gid,
    };
    for (path, bits) in directories(&unit)? {
        parents(exec, &mut planner, &path)?;
        planner.dir(path, Perms::new(bits), Some(owner))?;
    }

    let mut started = false;
    // The frontend's unit is a template: it is enabled by that name (DefaultInstance=
    // makes it the first instance) and started as an instance. A serving instance is
    // never restarted: a restart drains it and refuses every new build until the next
    // start, which is what `kbf frontend handover` exists not to do.
    let instances = mode.instances();
    let first = instances[0].clone();
    let serving = || -> Result<Option<String>, HostError> {
        for instance in &instances {
            if unit_is(exec, "is-active", instance)? {
                return Ok(Some(instance.clone()));
            }
        }
        Ok(None)
    };
    if changed {
        planner.run(systemctl(&["daemon-reload"]));
        planner.run(systemctl(&["enable", &name]));
        if mode != Mode::Frontend {
            planner.run(systemctl(&["restart", &name]));
            started = true;
        } else if let Some(serving) = serving()? {
            note(
                out,
                format_args!(
                    "{serving} is serving and is not restarted: run `kbf frontend handover` \
                     to start the other instance on the new unit and binary"
                ),
            )?;
        } else {
            planner.run(systemctl(&["start", &first]));
            started = true;
        }
    } else {
        note(
            out,
            format_args!("unit and binary unchanged: nothing restarted"),
        )?;
        if !unit_is(exec, "is-enabled", &first)? {
            planner.run(systemctl(&["enable", &name]));
        }
        if serving()?.is_none() {
            planner.run(systemctl(&["start", &first]));
            started = true;
        }
    }
    let api = unit
        .flag("api-listen")
        .and_then(|a| a.strip_prefix("127.0.0.1:"));
    if let (true, Some(port)) = (started, api) {
        planner.wait_http(
            HttpUrl::new(&format!("http://127.0.0.1:{port}/readyz"))?,
            READY,
        );
    }
    Ok(planner.finish())
}

/// The unit's `User=`, unless it names none or root.
fn unit_user<'a>(unit: &'a UnitFile, name: &str) -> Result<&'a str, HostError> {
    unit.get_in("Service", "User")
        .filter(|u| !u.is_empty() && *u != "root" && *u != "0")
        .ok_or_else(|| HostError::RunsAsRoot(name.to_owned()))
}

/// Which files a flag names: a private one (a key, a token) or one that must only be
/// there (a certificate, a CA, a deny list). `None`: not a file this check reads.
fn file_flag(name: &str) -> Option<bool> {
    if name == "key" || name.ends_with("-key") || name.ends_with("token-file") {
        Some(true)
    } else if name == "cert"
        || name.ends_with("-cert")
        || name.ends_with("-ca")
        || name.ends_with("deny-list")
    {
        Some(false)
    } else {
        None
    }
}

/// Every file the unit reads at start is there, and a private one is `user`'s alone.
fn files_read<E: Exec>(exec: &E, unit: &UnitFile, user: &RunUser) -> Result<(), HostError> {
    let mut files = Vec::new();
    for d in unit.directives() {
        // A leading `-` makes the file optional to systemd: not this kit's.
        if d.key == "EnvironmentFile" && !d.value.is_empty() && !d.value.starts_with('-') {
            files.push((d.value.clone(), false));
        }
    }
    for word in unit.exec_start() {
        let Some((flag, value)) = word.strip_prefix("--").and_then(|w| w.split_once('=')) else {
            continue;
        };
        if let (Some(private), true) = (file_flag(flag), value.starts_with('/')) {
            files.push((value.to_owned(), private));
        }
    }
    for (path, private) in files {
        let path = HostPath::new(&path)?;
        let meta = match stat(exec, &path)? {
            Some(m) if m.kind == Kind::File => m,
            _ => return Err(HostError::Missing(path)),
        };
        if private && meta.owner.uid != user.uid {
            return Err(HostError::NotOwned {
                path,
                uid: meta.owner.uid,
                user: user.name.clone(),
            });
        }
        if private && meta.perms.bits() & 0o077 != 0 {
            return Err(HostError::Readable {
                path,
                perms: meta.perms,
            });
        }
    }
    Ok(())
}

/// The directories the unit's flags name, each with its mode: `--data-dir` first (a
/// `file://` cold store may be under it).
fn directories(unit: &UnitFile) -> Result<Vec<(HostPath, u32)>, HostError> {
    let file = |flag: &str| unit.flag(flag).and_then(|v| v.strip_prefix("file://"));
    let wanted = [
        (unit.flag("data-dir"), 0o755),
        (unit.flag("meta-dir"), 0o700),
        (file("hot-store"), 0o700),
        (file("cold-store"), 0o700),
    ];
    let mut dirs = Vec::new();
    for (path, bits) in wanted {
        if let Some(path) = path {
            dirs.push((HostPath::new(path)?, bits));
        }
    }
    // The SQLite state store's file is made by kbf, in a directory that must be there and
    // the unit's user's.
    if let Some(file) = unit
        .flag("state-store")
        .and_then(|v| v.strip_prefix("sqlite://"))
    {
        let file = HostPath::new(file)?;
        let dir = file.as_str().rsplit_once('/').map_or("", |(dir, _)| dir);
        dirs.push((HostPath::new(dir)?, 0o700));
    }
    Ok(dirs)
}

/// Plan, then print or carry the plan out.
pub fn run<E: Exec>(
    exec: &mut E,
    a: &InstallMode,
    mode: plan::Mode,
    mut out: &mut dyn Write,
) -> Result<(), HostError> {
    let p = plan(&*exec, a, &mut *out)?;
    Ok(plan::execute(&p, RunAs::Root, mode, exec, &mut out)?)
}

#[cfg(test)]
#[path = "install_mode_tests.rs"]
mod tests;
