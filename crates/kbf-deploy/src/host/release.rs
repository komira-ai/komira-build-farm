//! The manual release verbs: `install`, `rollback` and `gc`, on the layout of
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
//!   directory that is there with other content, checks that the copied binary's sha256
//!   is the source's, then points `previous` at the old current release and `current`
//!   at the new one and writes the state. Only the files the source names are compared:
//!   a file the release directory has beyond them is not noticed.
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
use kbf_host::path::{HostPath, Root};
use kbf_host::plan::{HostError, Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::{Owner, Perms};
use sha2::{Digest, Sha256};

use super::CommitId;
use super::layout::{self, path};
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
}

/// The error closure for a read of `path`.
fn read_failed(path: &Path) -> impl FnOnce(io::Error) -> ReleaseError + '_ {
    move |source| ReleaseError::Read {
        path: path.to_owned(),
        source,
    }
}

/// What `install --from <dir>` copies: `<dir>/kbf`, and each file of `<dir>/units/`
/// (sorted by name) when that directory exists.
#[derive(Debug)]
pub struct Source {
    pub binary: Vec<u8>,
    pub units: Vec<(String, Vec<u8>)>,
}

impl Source {
    pub fn read(from: &Path) -> Result<Source, ReleaseError> {
        let bin = from.join(layout::RELEASE_BINARY);
        let binary = fs::read(&bin).map_err(read_failed(&bin))?;
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
        Ok(Source { binary, units })
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
fn begin(root: &Root, mode: Mode) -> Result<(Option<Locked>, State), ReleaseError> {
    let lock = match mode {
        Mode::Apply => Some(Locked::acquire(root)?),
        Mode::DryRun => None,
    };
    let state = match &lock {
        Some(lock) => lock.read()?,
        None => state::read_unlocked(root)?,
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
        let _ = writeln!(out, "state: written to {}", layout::STATE_FILE);
        // SEAM(unit install): no verb here installs a release's units or restarts a
        // service; `linux install-units` does both. When it takes a release directory,
        // the restart goes here, after the switch.
        let _ = writeln!(
            out,
            "next, by hand: restart the units that run {}/{} (systemctl restart <unit>), \
             then check `kbf-deploy host status`",
            layout::CURRENT,
            layout::RELEASE_BINARY
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
    root: &Root,
    owner: Owner,
    a: &Install,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let source = Source::read(&a.from)?;
    let (lock, state) = begin(root, mode)?;
    let copy = plan_copy(exec, owner, &a.commit, &source)?;
    let _ = writeln!(
        out,
        "release {}: {} ({} bytes, sha256 {}), {} unit(s)",
        a.commit,
        layout::release_dir(&a.commit),
        source.binary.len(),
        sha256(&source.binary),
        source.units.len()
    );
    execute(&copy, RunAs::Root, mode, exec, &mut &mut *out)?;
    if mode == Mode::Apply {
        let bin = layout::release_file(&a.commit, layout::RELEASE_BINARY);
        verify(inspect(exec, &bin)?, bin, &source.binary)?;
    }
    // Installing the current release again keeps its previous one.
    let previous = if state.current.as_ref() == Some(&a.commit) {
        state.previous.clone()
    } else {
        state.current.clone()
    };
    switch(exec, lock, &state, (a.commit.clone(), previous), out)
}

/// The directories and files of the release, root's; a release directory already there
/// must hold the same files (its modes and owner are then set again, nothing else).
fn plan_copy<E: Exec>(
    exec: &E,
    owner: Owner,
    commit: &CommitId,
    source: &Source,
) -> Result<Plan, ReleaseError> {
    let dir = layout::release_dir(commit);
    let units = layout::release_file(commit, layout::RELEASE_UNITS);
    let mut files = vec![(
        layout::release_file(commit, layout::RELEASE_BINARY),
        BINARY,
        &source.binary,
    )];
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
    let mut dirs = vec![
        path(layout::STATE_DIR),
        path(layout::KBF_DIR),
        path(layout::RELEASES),
        dir,
    ];
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
    root: &Root,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let (lock, state) = begin(root, mode)?;
    let Some(previous) = state.previous.clone() else {
        return Err(ReleaseError::NoPrevious);
    };
    let bin = layout::release_file(&previous, layout::RELEASE_BINARY);
    let there = matches!(
        exec.stat(&bin),
        Ok(Some(Meta {
            kind: Kind::File,
            ..
        }))
    );
    if !there {
        return Err(ReleaseError::PreviousGone(previous));
    }
    // The state records no release before the previous one, so `previous` goes.
    switch(exec, lock, &state, (previous, None), out)
}

/// `host gc --keep N`: remove each release directory but the kept ones: the state's
/// current, previous and staged, what the `current` and `previous` links point to, and
/// the `keep` newest others (by the directory's modification time). An entry of
/// `releases/` that is not a directory named by a commit id is left alone.
pub fn gc<E: Exec>(
    exec: &mut E,
    root: &Root,
    keep: usize,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    // Held through the removals, so an install cannot make a release current meanwhile.
    let (_lock, state) = begin(root, mode)?;
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
    for (commit, modified) in releases(root, out)? {
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
        contents(root, &dir, &mut paths)?;
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
fn releases(root: &Root, out: &mut dyn Write) -> Result<Vec<(CommitId, SystemTime)>, ReleaseError> {
    let dir = root.join(&path(layout::RELEASES));
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
fn contents(root: &Root, dir: &HostPath, paths: &mut Vec<HostPath>) -> Result<(), ReleaseError> {
    let full = root.join(dir);
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
            contents(root, &p, paths)?;
        }
        paths.push(p);
    }
    Ok(())
}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
