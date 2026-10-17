//! The release verbs: `install`, `stage`, `rollback` and `gc`, on the layout of
//! [`super::layout`].
//!
//! Each reads the deploy state, plans its steps through a [`Planner`] and prints them
//! (`DRY-RUN: <step>`) unless `--apply` is given. With `--apply` it takes the deploy lock
//! first ([`Locked`]) and holds it until the state is written, so two runs cannot both
//! move the links; a dry run takes no lock and writes nothing. Each refuses while a
//! commit-confirm deploy is pending (the state's `pending_confirm`): that deploy owns the
//! links until it is confirmed or rolled back.
//!
//! - `install` copies a release into `releases/<commit>/` (root's, 0755, so the user a
//!   service runs as cannot replace its own binary; units 0644), refuses a release
//!   directory that is there with other content, checks that each copied binary's
//!   sha256 is the source's, then points `previous` at the old current release and
//!   `current` at the new one and writes the state. Only the files the source names are
//!   compared: a file the release directory has beyond them is not noticed. A release
//!   holds the binaries its source directory has: `kbf` (a Linux release), `kbf-daemon`
//!   (a macOS one) or both.
//! - `stage` does the copy and the checks of `install` and no more: no link moves, and
//!   the state's `staged` names the release, which `gc` then keeps. It is what a
//!   release that was verified elsewhere ([`crate::stage`]) goes through, and it
//!   proceeds while a deploy is pending, since it touches nothing that deploy owns.
//! - `rollback` points `current` at the previous release and removes `previous` (the
//!   state keeps no release before it); it refuses when there is no previous release or
//!   its binary is gone.
//! - `gc` removes release directories, never the current, previous or staged one, nor
//!   one the `current` or `previous` link points to.
//!
//! None of them restarts a service: installing and restarting the units is
//! `linux install-units`' (see the SEAM comment in `switch`). An apply ends by saying
//! so.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use kbf_host::exec::{Entry, Exec, Kind, Meta};
use kbf_host::path::HostPath;
use kbf_host::plan::{HostError, Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::{Owner, Perms};
use sha2::{Digest, Sha256};

use super::CommitId;
use super::layout::{self, Layout, Platform, path};
use super::state::{self, Locked, State, StateError};

/// Who owns a release: root, so only root can replace what a service runs.
pub const ROOT: Owner = Owner { uid: 0, gid: 0 };
/// A release directory and the directories above it: root writes, everyone reads.
pub const DIR: Perms = Perms::new(0o755);
/// The release binary: root writes, everyone runs.
pub const BINARY: Perms = Perms::new(0o755);
/// A unit file in a release.
pub const UNIT: Perms = Perms::new(0o644);

/// Why a release verb refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{0:?} is not a UTF-8 name")]
    NotUtf8(PathBuf),
    #[error("a commit-confirm deploy of {0} is pending: confirm it or let it roll back first")]
    Pending(CommitId),
    #[error("{0} is there and is not a directory")]
    NotADirectory(HostPath),
    #[error("{path} differs from the release being installed: a release is never replaced")]
    Differs { path: HostPath },
    #[error("the copy at {path} has sha256 {got}; the source has {want}")]
    Copy {
        path: HostPath,
        want: String,
        got: String,
    },
    #[error("no previous release to roll back to")]
    NoPrevious,
    #[error("the previous release {0} is gone (no binary at its directory)")]
    PreviousGone(CommitId),
    #[error("release {0} is not installed (no binary at its directory): stage it first")]
    NotInstalled(CommitId),
    #[error("the pending swap of {commit} was made by {pending}, not by order {got:?}")]
    OtherOrder {
        commit: CommitId,
        pending: String,
        got: String,
    },
    #[error("no swap is pending, and {0} is not the current release")]
    NotPending(CommitId),
    #[error("a confirm window of {0} s is outside the {min} to {max} s the host allows", min = crate::activate::WINDOW_MIN_SECS, max = crate::activate::WINDOW_MAX_SECS)]
    Window(u64),
}

/// The error closure for a read of `path`.
fn read_failed(path: &Path) -> impl FnOnce(io::Error) -> ReleaseError + '_ {
    move |source| ReleaseError::Read {
        path: path.to_owned(),
        source,
    }
}

/// What `install --from <dir>` copies: each of `<dir>/kbf` and `<dir>/kbf-daemon` that
/// exists (at least one must), and each file of `<dir>/units/` (sorted by name) when that
/// directory exists.
#[derive(Debug)]
pub struct Source {
    /// The binaries by name, in the order of [`layout::RELEASE_BINARIES`].
    pub binaries: Vec<(&'static str, Vec<u8>)>,
    pub units: Vec<(String, Vec<u8>)>,
}

impl Source {
    pub fn read(from: &Path) -> Result<Source, ReleaseError> {
        let mut binaries = Vec::new();
        let mut missing = None;
        for name in layout::RELEASE_BINARIES {
            let bin = from.join(name);
            match fs::read(&bin) {
                Ok(bytes) => binaries.push((name, bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    missing.get_or_insert((bin, e));
                }
                Err(e) => return Err(read_failed(&bin)(e)),
            }
        }
        // With none, the error is the first one's: the source has no `kbf`.
        if let Some((bin, e)) = missing.filter(|_| binaries.is_empty()) {
            return Err(read_failed(&bin)(e));
        }
        let dir = from.join(layout::RELEASE_UNITS);
        let items = match fs::read_dir(&dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            listed => listed
                .and_then(Iterator::collect::<io::Result<Vec<_>>>)
                .map_err(read_failed(&dir))?,
        };
        let mut units = Vec::new();
        for item in items {
            let file = item.path();
            let name = utf8(&item.file_name(), &file)?.to_owned();
            units.push((name, fs::read(&file).map_err(read_failed(&file))?));
        }
        units.sort();
        Ok(Source { binaries, units })
    }
}

fn utf8<'a>(name: &'a OsStr, full: &Path) -> Result<&'a str, ReleaseError> {
    name.to_str()
        .ok_or_else(|| ReleaseError::NotUtf8(full.to_owned()))
}

/// `dir/name`, for a name read from a directory (never empty, `.`, `..` or with a `/`).
fn child(dir: &HostPath, name: &str) -> HostPath {
    HostPath::new(&format!("{dir}/{name}")).expect("a directory entry's name is one component")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn inspect<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Entry>, ReleaseError> {
    let fail = |source| HostError::Inspect {
        path: path.clone(),
        source,
    };
    Ok(exec.inspect(path).map_err(fail)?)
}

/// The lock (only with `--apply`) and the state, refused while a deploy is pending.
fn begin(host: &Layout, mode: Mode) -> Result<(Option<Locked>, State), ReleaseError> {
    let lock = match mode {
        Mode::Apply => Some(Locked::acquire(host)?),
        Mode::DryRun => None,
    };
    let state = match &lock {
        Some(lock) => lock.read()?,
        None => state::read_unlocked(host)?,
    };
    let state = state.unwrap_or_default();
    if let Some(p) = &state.pending_confirm {
        return Err(ReleaseError::Pending(p.commit.clone()));
    }
    Ok((lock, state))
}

/// `none`, or the commit.
fn or_none(commit: &Option<CommitId>) -> String {
    commit
        .as_ref()
        .map_or("none".to_owned(), ToString::to_string)
}

/// Point `previous` at `previous` (or remove it), then `current` at `current`: until
/// `current` moves, both name whole releases. Then, with the lock (an apply), write the
/// state with those two and say what is left to do by hand.
fn switch<E: Exec>(
    exec: &mut E,
    host: &Layout,
    lock: Option<Locked>,
    was: &State,
    (current, previous): (CommitId, Option<CommitId>),
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let mut planner = Planner::new(&*exec);
    match &previous {
        Some(c) => planner.symlink(path(layout::PREVIOUS), layout::release_link(c))?,
        None => planner.absent(path(layout::PREVIOUS))?,
    };
    planner.symlink(path(layout::CURRENT), layout::release_link(&current))?;
    let plan = planner.finish();
    let _ = writeln!(
        out,
        "links: current {} -> {current}, previous {} -> {}",
        or_none(&was.current),
        or_none(&was.previous),
        or_none(&previous)
    );
    let mode = if lock.is_some() {
        Mode::Apply
    } else {
        Mode::DryRun
    };
    execute(&plan, RunAs::Root, mode, exec, &mut &mut *out)?;
    if let Some(lock) = lock {
        let next = State {
            current: Some(current),
            previous,
            ..was.clone()
        };
        lock.write(&next)?;
        let _ = writeln!(out, "state: written to {}", host.state_file());
        // SEAM(unit install): no verb here installs a release's units or restarts a
        // service; `linux install-units` does both. When it takes a release directory,
        // the restart goes here, after the switch.
        let (binary, restart) = match host.platform() {
            Platform::Linux => (layout::RELEASE_BINARY, "systemctl restart <unit>"),
            Platform::MacOs => (
                layout::RELEASE_DAEMON,
                "launchctl kickstart -k system/<label>",
            ),
        };
        let _ = writeln!(
            out,
            "next, by hand: restart the units that run {}/{binary} ({restart}), \
             then check `kbf-deploy host status`",
            layout::CURRENT,
        );
    }
    Ok(())
}

/// What `install` was asked for.
#[derive(Debug)]
pub struct Install {
    pub commit: CommitId,
    pub from: PathBuf,
}

/// `host install`: copy, check, switch. `owner` is [`ROOT`] on a host (a scratch test
/// can chown only to its own user).
pub fn install<E: Exec>(
    exec: &mut E,
    host: &Layout,
    owner: Owner,
    a: &Install,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let source = Source::read(&a.from)?;
    let (lock, state) = begin(host, mode)?;
    copy_release(exec, host, owner, &a.commit, &source, mode, out)?;
    // Installing the current release again keeps its previous one.
    let previous = if state.current.as_ref() == Some(&a.commit) {
        state.previous.clone()
    } else {
        state.current.clone()
    };
    switch(exec, host, lock, &state, (a.commit.clone(), previous), out)
}

/// `host stage`: copy and check as `install` does, move no link, and record the release
/// as the state's `staged` one (with `Mode::Apply`; a dry run prints the plan).
pub fn stage<E: Exec>(
    exec: &mut E,
    host: &Layout,
    owner: Owner,
    a: &Install,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let source = Source::read(&a.from)?;
    // Held through the copy, so a `gc` cannot remove the release between the two.
    let lock = match mode {
        Mode::Apply => Some(Locked::acquire(host)?),
        Mode::DryRun => None,
    };
    copy_release(exec, host, owner, &a.commit, &source, mode, out)?;
    if let Some(lock) = lock {
        let state = lock.read()?.unwrap_or_default();
        lock.write(&State {
            staged: Some(a.commit.clone()),
            ..state
        })?;
        let _ = writeln!(out, "state: {} staged", a.commit);
    }
    Ok(())
}

/// The plan of the release's directories and files, printed and (with `Mode::Apply`)
/// carried out, and then each binary read back from the host and compared with the source.
fn copy_release<E: Exec>(
    exec: &mut E,
    host: &Layout,
    owner: Owner,
    commit: &CommitId,
    source: &Source,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let copy = plan_copy(exec, host, owner, commit, source)?;
    let dir = layout::release_dir(commit);
    let existed = inspect(exec, &dir)?.is_some();
    // The first binary is named in the line, as it was when a release held only `kbf`;
    // each other one gets a line of its own.
    let ((_, first), others) = source
        .binaries
        .split_first()
        .expect("a source holds at least one binary");
    let _ = writeln!(
        out,
        "release {commit}: {} ({} bytes, sha256 {}), {} unit(s)",
        layout::release_dir(commit),
        first.len(),
        sha256(first),
        source.units.len()
    );
    for (name, bytes) in others {
        let _ = writeln!(
            out,
            "also {name} ({} bytes, sha256 {})",
            bytes.len(),
            sha256(bytes)
        );
    }
    let copied = execute(&copy, RunAs::Root, mode, exec, &mut &mut *out)
        .map_err(ReleaseError::from)
        .and_then(|()| {
            if mode == Mode::Apply {
                for (name, bytes) in &source.binaries {
                    let bin = layout::release_file(commit, name);
                    verify(inspect(exec, &bin)?, bin, bytes)?;
                }
            }
            Ok(())
        });
    // A dry run changes nothing, so a failed copy is always an apply.
    if copied.is_err() && !existed {
        // Best effort: the error returned is the copy's.
        let _ = remove_partial(exec, host, &dir);
    }
    copied
}

/// Remove the release directory a failed copy made (a full disk is the usual cause), so
/// that a retry finds nothing there. A directory left half written would be refused for
/// ever: it is there with other content, and a release is never replaced.
fn remove_partial<E: Exec>(
    exec: &mut E,
    host: &Layout,
    dir: &HostPath,
) -> Result<(), ReleaseError> {
    let mut paths = Vec::new();
    contents(host, dir, &mut paths)?;
    paths.push(dir.clone());
    let mut planner = Planner::new(&*exec);
    for p in paths {
        planner.absent(p)?;
    }
    execute(
        &planner.finish(),
        RunAs::Root,
        Mode::Apply,
        exec,
        &mut io::sink(),
    )?;
    Ok(())
}

/// The directories and files of the release, root's; a release directory already there
/// must hold the same files (its modes and owner are then set again, nothing else).
/// `/usr/local` is made when the host has none, and left as it is when it has.
fn plan_copy<E: Exec>(
    exec: &E,
    host: &Layout,
    owner: Owner,
    commit: &CommitId,
    source: &Source,
) -> Result<Plan, ReleaseError> {
    let dir = layout::release_dir(commit);
    let units = layout::release_file(commit, layout::RELEASE_UNITS);
    let mut files = Vec::new();
    for (name, bytes) in &source.binaries {
        files.push((layout::release_file(commit, name), BINARY, bytes));
    }
    for (name, bytes) in &source.units {
        files.push((child(&units, name), UNIT, bytes));
    }
    match inspect(exec, &dir)? {
        None => {}
        Some(Entry::Dir { .. }) => {
            for (path, _, want) in &files {
                let same = match inspect(exec, path)? {
                    Some(Entry::File { bytes, .. }) => bytes == **want,
                    _ => false,
                };
                if !same {
                    return Err(ReleaseError::Differs { path: path.clone() });
                }
            }
        }
        Some(_) => return Err(ReleaseError::NotADirectory(dir)),
    }
    let mut planner = Planner::new(exec);
    let mut dirs = Vec::new();
    // A fresh macOS has no `/usr/local`. An existing one is not touched: its owner and
    // mode are the host's, and the trust check (crate::trust) refuses one others write to.
    let usr_local = path(layout::USR_LOCAL);
    if inspect(exec, &usr_local)?.is_none() {
        dirs.push(usr_local);
    }
    dirs.extend([
        host.state_dir(),
        path(layout::KBF_DIR),
        path(layout::RELEASES),
        dir,
    ]);
    if !source.units.is_empty() {
        dirs.push(units);
    }
    for d in dirs {
        planner.dir(d, DIR, Some(owner))?;
    }
    for (path, perms, bytes) in files {
        planner.file(path, perms, Some(owner), bytes.clone())?;
    }
    Ok(planner.finish())
}

/// The copied binary at `path` (`copied`, read back from the host after the copy and
/// before any link names it) has the source's sha256. Not generic over the host, so one
/// copy of it is measured for coverage, whichever host a test reads it from.
fn verify(copied: Option<Entry>, path: HostPath, source: &[u8]) -> Result<(), ReleaseError> {
    let got = match copied {
        Some(Entry::File { bytes, .. }) => sha256(&bytes),
        _ => "none (no file)".to_owned(),
    };
    let want = sha256(source);
    if got != want {
        return Err(ReleaseError::Copy { path, want, got });
    }
    Ok(())
}

/// `host rollback`: `current` back to the previous release; `previous` removed.
pub fn rollback<E: Exec>(
    exec: &mut E,
    host: &Layout,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let (lock, state) = begin(host, mode)?;
    let Some(previous) = state.previous.clone() else {
        return Err(ReleaseError::NoPrevious);
    };
    if !has_binary(exec, &previous) {
        return Err(ReleaseError::PreviousGone(previous));
    }
    // The state records no release before the previous one, so `previous` goes.
    switch(exec, host, lock, &state, (previous, None), out)
}

/// Whether the release `commit` has a binary in its directory. Any will do: a Linux
/// release holds `kbf`, a macOS one `kbf-daemon`.
pub(crate) fn has_binary<E: Exec>(exec: &E, commit: &CommitId) -> bool {
    layout::RELEASE_BINARIES.iter().any(|name| {
        matches!(
            exec.stat(&layout::release_file(commit, name)),
            Ok(Some(Meta {
                kind: Kind::File,
                ..
            }))
        )
    })
}

/// `host gc --keep N`: remove each release directory but the kept ones: the state's
/// current, previous and staged, what the `current` and `previous` links point to, and
/// the `keep` newest others (by the directory's modification time). An entry of
/// `releases/` that is not a directory named by a commit id is left alone.
pub fn gc<E: Exec>(
    exec: &mut E,
    host: &Layout,
    keep: usize,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    // Held through the removals, so an install cannot make a release current meanwhile.
    let (_lock, state) = begin(host, mode)?;
    let mut kept: BTreeSet<CommitId> = [state.current, state.previous, state.staged]
        .into_iter()
        .flatten()
        .collect();
    for link in [layout::CURRENT, layout::PREVIOUS] {
        if let Some(Entry::Link { target }) = inspect(exec, &path(link))? {
            kept.extend(linked(&target));
        }
    }
    let mut old = Vec::new();
    for (commit, modified) in releases(host, out)? {
        if !kept.contains(&commit) {
            old.push((modified, commit));
        }
    }
    // Newest first; the first `keep` stay.
    old.sort_by(|a, b| b.cmp(a));
    let gone: Vec<CommitId> = old.into_iter().skip(keep).map(|(_, c)| c).collect();
    let mut paths = Vec::new();
    for commit in &gone {
        let dir = layout::release_dir(commit);
        contents(host, &dir, &mut paths)?;
        paths.push(dir);
    }
    let mut planner = Planner::new(&*exec);
    for p in paths {
        planner.absent(p)?;
    }
    let kept: Vec<String> = kept.iter().map(ToString::to_string).collect();
    let _ = writeln!(
        out,
        "keeping: {}; removing: {}",
        list(&kept),
        list(&gone.iter().map(ToString::to_string).collect::<Vec<_>>())
    );
    execute(&planner.finish(), RunAs::Root, mode, exec, &mut &mut *out)?;
    Ok(())
}

fn list(items: &[String]) -> String {
    match items {
        [] => "none".to_owned(),
        _ => items.join(" "),
    }
}

/// The commit a link target `releases/<commit>` names.
fn linked(target: &Path) -> Option<CommitId> {
    let name = target.strip_prefix("releases").ok()?.to_str()?;
    CommitId::new(name).ok()
}

/// Each directory in `releases/` named by a commit id, with its modification time.
fn releases(
    host: &Layout,
    out: &mut dyn Write,
) -> Result<Vec<(CommitId, SystemTime)>, ReleaseError> {
    let dir = host.join(&path(layout::RELEASES));
    let items = match fs::read_dir(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        listed => listed
            .and_then(Iterator::collect::<io::Result<Vec<_>>>)
            .map_err(read_failed(&dir))?,
    };
    let mut found = Vec::new();
    for item in items {
        let full = item.path();
        let meta = fs::symlink_metadata(&full).map_err(read_failed(&full))?;
        let commit = item
            .file_name()
            .to_str()
            .and_then(|n| CommitId::new(n).ok());
        match commit {
            Some(c) if meta.is_dir() => {
                found.push((c, meta.modified().map_err(read_failed(&full))?));
            }
            _ => {
                let _ = writeln!(out, "left alone (not a release): {}", full.display());
            }
        }
    }
    Ok(found)
}

/// Everything under `dir`, deepest first, so each removal finds its directory empty.
/// Links are removed, never followed.
fn contents(host: &Layout, dir: &HostPath, paths: &mut Vec<HostPath>) -> Result<(), ReleaseError> {
    let full = host.join(dir);
    let items = fs::read_dir(&full)
        .and_then(Iterator::collect::<io::Result<Vec<_>>>)
        .map_err(read_failed(&full))?;
    for item in items {
        let name = item.file_name();
        let p = child(dir, utf8(&name, &item.path())?);
        if item
            .file_type()
            .map_err(read_failed(&item.path()))?
            .is_dir()
        {
            contents(host, &p, paths)?;
        }
        paths.push(p);
    }
    Ok(())
}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
