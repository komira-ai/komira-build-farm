//! The Mac's part of a slot's clean ([`crate::slots::clean`]): each step for one pool
//! user, and the check of what is left. The order and the rule for a failed step are
//! `clean.rs`'s; the decisions that need no Mac are in [`crate::slots::probe`],
//! [`crate::slots::domain`] and [`crate::slots::crash`], tested on every platform.
//!
//! - C1 is [`domain::end`] over a real `launchctl`: the domains `user/<uid>` and
//!   `gui/<uid>` booted out whole, then `launchctl print user/<uid>` polled until only
//!   launchd's own jobs are left. A job is launchd's own when its plist lies in the
//!   sealed system folders ([`probe::OWN_PLISTS`]), never by its label, which a lease
//!   chooses. The crontab, `at` jobs and the shared places are swept by uid with
//!   `kbf_mac_session::sweep`, which never follows a link and stays on one filesystem.
//! - The check reads, never removes. Processes that launchd lists as running one of
//!   its own agents are not counted ([`leftover_processes`]): launchd starts them again
//!   as the uid on demand after the kill.
//! - Not reached: Background Task Management entries (macOS removes none for one uid).
//! - Disk images: every image `hdiutil info -plist` says the uid attached is detached
//!   (`hdiutil detach -force`), then every mount under the home or the entry is unmounted
//!   (`diskutil unmount force`, then `umount -f`).
//! - System V IPC: every message queue, shared memory segment and semaphore set
//!   `ipcs` lists with the user as owner or creator is removed (`ipcrm`). POSIX named
//!   semaphores and shared memory are not reached: macOS lists them nowhere.
//! - C6 clears the flags on the home **first** (`sweep::clear_flags` opens each root and
//!   clears it before it looks below), removes ACLs of the uid's entries (`find -x ...
//!   -user <uid> -exec chmod -N`, so a hard link to another owner's file keeps its ACL)
//!   and sets the owner's `u+rwx`. C7 then empties the home, whoever owns what is in it.
//! - The `/var/folders` entry is cleaned **as the slot's uid**, never as root
//!   ([`crate::slots::entry`]): the daemon is a launch daemon without Full Disk Access,
//!   and root cannot clear the flags or ACLs of the entry's `0`, `C`, `T` and `X` nor
//!   list them, while their owner can. C6 does for the entry what it does for the home,
//!   by a child that drops to the uid; C7 empties the entry that way, keeping `0`, `C`,
//!   `T` and `X` ([`probe::SKELETON`]); the check looks at it that way too.
//! - C10 gives the home back to the user, mode 0700, no flags (made anew if it is gone),
//!   and the entry's roots what they had when the slot was resolved
//!   ([`super::Entry`]), without a lock or `hidden`.
//!
//! The user is never deleted or made again: a step that fails leaves the user dirty
//! (`clean.rs`).

use std::io::{self, Read as _};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kbf_daemon::uid;
use kbf_mac_session::sweep::{self, SweepPlan, Swept};
use rustix::fs::{CWD, Mode, OFlags};

use super::Ready;
use crate::slots::assertion;
use crate::slots::clean::{Left, Os, Step};
use crate::slots::domain::{self, Launchd, Said};
use crate::slots::{crash, entry, probe};

const HDIUTIL: &str = "/usr/bin/hdiutil";
const DISKUTIL: &str = "/usr/sbin/diskutil";
const UMOUNT: &str = "/sbin/umount";
const MOUNT: &str = "/sbin/mount";
const FIND: &str = "/usr/bin/find";
const CHMOD: &str = "/bin/chmod";
const IPCS: &str = "/usr/bin/ipcs";
const IPCRM: &str = "/usr/bin/ipcrm";

/// How long a `launchctl` that was killed, or has exited, is waited for to be reaped and
/// its output read.
const READ_GRACE: Duration = Duration::from_secs(2);

/// The wait between the check's two looks.
const LOOK_PAUSE: Duration = Duration::from_millis(200);

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// One slot's user, to clean: `within` bounds the kill.
pub struct MacSlot<'a> {
    pub ready: &'a Ready,
    pub within: Duration,
    /// How long C3 gives the processes to end after SIGTERM, before the kill
    /// ([`crate::grace`]); zero kills at once.
    pub grace: Duration,
    /// The uids of the pool's users: C9 does not walk a folder one of them owns a
    /// directory on the path of.
    pub pool: std::ops::RangeInclusive<u32>,
    /// What this clean has judged of launchd's services ([`probe::OwnLabels`]); a clean
    /// reads each service's plist once.
    pub(super) judged: probe::OwnLabels,
    /// The daemon's binary: the `/var/folders` entry is cleaned by it, as the slot's user
    /// ([`entry`]).
    pub(super) program: &'a Path,
}

/// `launchctl`, run for real: each call is killed at its alarm.
struct Launchctl;

impl Launchd for Launchctl {
    fn run(&self, args: &[&str], alarm: Duration) -> Said {
        let child = Command::new(uid::LAUNCHCTL)
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(why) => return Said::Missing(why.to_string()),
        };
        // Read as it is written: a domain's listing is larger than a pipe holds. The reader
        // reports through a channel, so no wait for it is unbounded.
        let stdout = child.stdout.take();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_end(&mut text);
            }
            let _ = sent.send(text);
        });
        let give_up = Instant::now() + alarm;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let text = received.recv_timeout(READ_GRACE).unwrap_or_default();
                    return Said::Exited {
                        code: status.code().unwrap_or(-1),
                        stdout: String::from_utf8_lossy(&text).into_owned(),
                    };
                }
                Ok(None) if Instant::now() < give_up => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    // Reaped within a bound too: a process stuck in the kernel is left to
                    // the OS, and the step reports the timeout.
                    let reaped = Instant::now() + READ_GRACE;
                    while child.try_wait().is_ok_and(|s| s.is_none()) && Instant::now() < reaped {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    return Said::TimedOut;
                }
            }
        }
    }

    fn pause(&self, time: Duration) {
        std::thread::sleep(time);
    }

    fn now(&self) -> Duration {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }
}

impl Os for MacSlot<'_> {
    fn run(&self, step: Step) -> Result<(), String> {
        let ready = self.ready;
        let user = &ready.user;
        match step {
            Step::Bootout => domain::end(&Launchctl, user.uid),
            Step::Schedules => complete(&sweep::sweep(
                &SweepPlan::macos_schedules(),
                &user.name,
                user.uid,
            )),
            Step::Kill => self.kill(),
            Step::Images => {
                let mut failed = Vec::new();
                for device in images_of(user.uid)? {
                    if let Err(why) = run(HDIUTIL, &["detach", "-force", &device]) {
                        failed.push(why);
                    }
                }
                failed.extend(unmount_all(ready));
                joined(failed)
            }
            Step::Ipc => {
                let mut failed = Vec::new();
                for (kind, id) in ipc_of(&user.name, user.uid)? {
                    if let Err(why) = run(IPCRM, &[kind.flag(), &id]) {
                        failed.push(why);
                    }
                }
                joined(failed)
            }
            Step::Flags => entry::flags_step(
                &self.as_uid(),
                (&ready.home, &ready.folders),
                // The home, as root, the root first, then below it.
                &mut |home| {
                    let mut failed = sweep::clear_flags(home, user.uid).errors;
                    failed.extend(remove_acls(home, user.uid).err());
                    failed
                },
                &mut run_child,
            ),
            Step::Contents => entry::contents_step(
                &self.as_uid(),
                (&ready.home, &ready.folders),
                &mut run_child,
            ),
            Step::Shared => complete(&sweep::sweep(&probe::shared_plan(), &user.name, user.uid)),
            Step::Crashes => joined(crash::sweep(&crash_dirs(), &self.crash_scope())),
            Step::Restore => restore(ready, &self.as_uid()),
            Step::Look => {
                for look in 0..2 {
                    if look > 0 {
                        std::thread::sleep(probe::SCAN_PAUSE);
                    }
                    let pids = uid::processes(user.uid).map_err(|e| e.to_string())?;
                    let left = not_launchds_own(pids, user.uid, &self.judged);
                    if !left.is_empty() {
                        return Err(format!("processes {:?}", named(&left)));
                    }
                }
                Ok(())
            }
        }
    }

    fn left(&self) -> Vec<Left> {
        let user = &self.ready.user;
        let mut left = Vec::new();
        let mut note = |step, what: String| left.push(Left { step, what });
        // The processes first, then the services: a process started in between is in
        // neither list, never in the first alone.
        let pids = uid::processes(user.uid);
        let services = services_of(user.uid, &self.judged);
        match pids.map(|pids| not_launchds_own_in(pids, &services)) {
            Ok(pids) => {
                if !pids.is_empty() {
                    note(Step::Kill, format!("processes {:?}", named(&pids)));
                }
            }
            Err(why) => note(Step::Kill, format!("processes: {why}")),
        }
        let jobs: Vec<&str> = (services.iter())
            .filter(|s| !s.own)
            .map(|s| s.label.as_str())
            .collect();
        if !jobs.is_empty() {
            note(
                Step::Bootout,
                format!("jobs in user/{}: {jobs:?}", user.uid),
            );
        }
        for (step, plan) in [
            (Step::Schedules, SweepPlan::macos_schedules()),
            (Step::Shared, probe::shared_plan()),
        ] {
            // Looked at, not swept: a check that removed what it found would hide a
            // skipped step.
            let found = owned_entries(&plan, &user.name, user.uid);
            if !found.is_empty() {
                note(step, format!("{found:?}"));
            }
        }
        match images_of(user.uid) {
            Ok(images) if images.is_empty() => {}
            Ok(images) => note(Step::Images, format!("attached {images:?}")),
            Err(why) => note(Step::Images, why),
        }
        match ipc_of(&user.name, user.uid) {
            Ok(objects) if objects.is_empty() => {}
            Ok(objects) => note(Step::Ipc, format!("{objects:?}")),
            Err(why) => note(Step::Ipc, why),
        }
        if let Err(why) = home_is_empty(self.ready, &self.as_uid()) {
            note(Step::Contents, why);
        }
        // As the uid: root cannot look inside the entry's folders.
        match entry::left(&self.as_uid(), &self.ready.folders, &mut run_child) {
            Ok(lines) => {
                for line in lines {
                    note(Step::Contents, line);
                }
            }
            Err(why) => note(Step::Contents, why),
        }
        let crashes = crash::found(&crash_dirs(), &self.crash_scope());
        if !crashes.is_empty() {
            note(Step::Crashes, format!("{crashes:?}"));
        }
        if let Err(why) = entry_is_restored(self.ready) {
            note(Step::Restore, why);
        }
        left
    }

    fn pause(&self) {
        std::thread::sleep(LOOK_PAUSE);
    }
}

impl MacSlot<'_> {
    /// The slot's user, as [`entry`] runs the `/var/folders` entry's clean.
    fn as_uid(&self) -> entry::AsUid<'_> {
        entry::AsUid {
            program: self.program,
            uid: self.ready.user.uid,
            gid: self.ready.user.gid,
        }
    }

    /// C9's scope: this slot's uid, and the pool's.
    fn crash_scope(&self) -> crash::Scope {
        crash::Scope {
            uid: self.ready.user.uid,
            pool: self.pool.clone(),
        }
    }

    /// C3: SIGTERM, `grace` for the uid's processes to end (the ones that ignore it are
    /// named in the log), then SIGKILL by real and effective uid until two scans 200 ms apart find nothing,
    /// within `within` and 100 scans ([`probe::kill_until_quiet`]). A scan does not count
    /// launchd's own agents, which launchd starts again on demand.
    fn kill(&self) -> Result<(), String> {
        let uid = self.ready.user.uid;
        let mut scan = || {
            let pids = uid::processes(uid).map_err(|e| e.to_string())?;
            if pids.is_empty() {
                return Ok(pids);
            }
            Ok(not_launchds_own(pids, uid, &self.judged))
        };
        let round = super::term_uid(
            uid,
            self.grace.min(self.within),
            &mut scan,
            probe::SCAN_PAUSE,
        );
        crate::grace::then_kill(round, "slot clean", |ignored| {
            if let Some(brief) = crate::grace::brief(ignored) {
                tracing::warn!(uid, "slot clean: {brief}");
            }
            // The grace is extra to `within`, which bounds the kill alone.
            let give_up = Instant::now() + self.within;
            probe::kill_until_quiet(
                &mut || Instant::now() >= give_up,
                &mut || uid::kill(uid).map_err(|e| e.to_string()),
                &mut scan,
                &mut || std::thread::sleep(probe::SCAN_PAUSE),
            )
        })
        .map(|_| ())
    }
}

/// C9's folders: the system's, and one per user of `/Users`.
pub(super) fn crash_dirs() -> Vec<PathBuf> {
    let users = list(Path::new("/Users")).unwrap_or_default();
    crash::crash_dirs(&users)
}

/// C10: the home the user's (made anew where it is gone), mode 0700, no flags; the
/// entry's four folders narrowed to the system's modes, as the uid (see [`entry::restore`]). The flags
/// and ACLs of the entry and its folders are the system's and are never touched.
fn restore(ready: &Ready, me: &entry::AsUid<'_>) -> Result<(), String> {
    let (uid, gid) = (ready.user.uid, ready.user.gid);
    let mut failed = Vec::new();
    let home = match open_dir(&ready.home) {
        Ok(dir) => own(&dir, (uid, gid, 0o700), 0),
        Err(e) if e.kind() == io::ErrorKind::NotFound => make_home(&ready.home, uid, gid),
        Err(e) => Err(e),
    };
    if let Err(why) = home {
        failed.push(format!("{}: {why}", ready.home.display()));
    }
    failed.extend(entry::restore(me, &ready.folders, &mut run_child).err());
    joined(failed)
}

/// Whether the entry's four folders are as C10 leaves them: the pool user's, and no wider
/// than the system makes them (`0` and `X` 0755, `C` and `T` 0700).
fn entry_is_restored(ready: &Ready) -> Result<(), String> {
    let mut wrong = Vec::new();
    for name in probe::SKELETON {
        let path = ready.folders.join(name);
        // One the system has not made yet is made, as the user, when it is asked for.
        let Ok(meta) = rustix::fs::lstat(&path) else {
            continue;
        };
        let has = (meta.st_uid, u32::from(meta.st_mode));
        if probe::folder_fix(has, ready.user.uid, probe::folder_ceiling(name)) != (false, None) {
            wrong.push(format!(
                "{}: owner {} mode {:o}",
                path.display(),
                has.0,
                has.1 & 0o7777
            ));
        }
    }
    joined(wrong)
}

/// The mounts under the home or the entry, unmounted (`diskutil unmount force`, then
/// `umount -f`), deepest first; the failures.
fn unmount_all(ready: &Ready) -> Vec<String> {
    let roots = [ready.home.as_path(), ready.folders.as_path()];
    let listed = |roots: &[&Path]| match output(MOUNT, &[]) {
        Ok(text) => Ok(probe::mounts_under(&String::from_utf8_lossy(&text), roots)),
        Err(why) => Err(why),
    };
    let mut failed = Vec::new();
    let points = match listed(&roots) {
        Ok(points) => points,
        Err(why) => return vec![why],
    };
    for point in &points {
        let path = point.to_string_lossy();
        if run(DISKUTIL, &["unmount", "force", &path]).is_err()
            && let Err(why) = run(UMOUNT, &["-f", &path])
        {
            failed.push(why);
        }
    }
    match listed(&roots) {
        Ok(left) if left.is_empty() => {}
        Ok(left) => failed.push(format!("still mounted: {left:?}")),
        Err(why) => failed.push(why),
    }
    failed
}

/// Removes the ACLs of every entry `uid` owns at or below `root` (`chmod -N`, in batches
/// through `find`, never through a link and on one filesystem): an entry of another
/// owner, such as a hard link to a system file, keeps its own.
fn remove_acls(root: &Path, uid: u32) -> Result<(), String> {
    // A root that is not there holds no ACL.
    if matches!(std::fs::symlink_metadata(root), Err(e) if e.kind() == io::ErrorKind::NotFound) {
        return Ok(());
    }
    let root = root.to_string_lossy();
    let uid = uid.to_string();
    run(
        FIND,
        &[
            "-x", &root, "-user", &uid, "!", "-type", "l", "-exec", CHMOD, "-N", "{}", "+",
        ],
    )
}

/// [`entry::Run`], for real: `program` with `args`, a plain environment and no input;
/// what it printed, or why it failed (its status and what it said, the first 4000
/// characters of it).
pub(super) fn run_child(program: &Path, args: &[std::ffi::OsString]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    Err(format!(
        "{}: {}: {}",
        program.display(),
        out.status,
        said.trim().chars().take(4000).collect::<String>()
    ))
}

/// Whether a sweep was complete.
fn complete(swept: &Swept) -> Result<(), String> {
    joined(swept.errors.clone())
}

fn joined(failed: Vec<String>) -> Result<(), String> {
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

/// Runs `program` with `args` and a plain environment; why it failed, if it did.
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    output(program, args).map(drop)
}

/// `program`'s stdout, if it exited 0.
pub(super) fn output(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(format!(
        "{program} {args:?}: {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

/// The devices (`/dev/diskN`) of the disk images `uid` attached, from
/// `hdiutil info -plist`: each image's own entry, whose detach detaches its partitions.
fn images_of(uid: u32) -> Result<Vec<String>, String> {
    let out = output(HDIUTIL, &["info", "-plist"])?;
    let images = assertion::hdiutil_images(&out)?;
    Ok(images
        .into_iter()
        .filter(|image| image.owner == Some(u64::from(uid)))
        .filter_map(|image| image.device)
        .collect())
}

/// A System V IPC object's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ipc {
    Queue,
    Memory,
    Semaphores,
}

impl Ipc {
    fn flag(self) -> &'static str {
        match self {
            Self::Queue => "-q",
            Self::Memory => "-m",
            Self::Semaphores => "-s",
        }
    }
}

/// The System V IPC objects `ipcs -a` lists with `name` (or `uid`, for a name the
/// user database does not give) as owner or creator ([`assertion::ipc_rows`]).
fn ipc_of(name: &str, uid: u32) -> Result<Vec<(Ipc, String)>, String> {
    let out = output(IPCS, &["-a"])?;
    let rows = assertion::ipc_rows(&String::from_utf8_lossy(&out), name, uid);
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let kind = match row.kind {
                'q' => Ipc::Queue,
                'm' => Ipc::Memory,
                's' => Ipc::Semaphores,
                _ => return None,
            };
            Some((kind, row.id))
        })
        .collect())
}

/// Whether the home is an empty directory of the user's, mode 0700, without flags.
fn home_is_empty(ready: &Ready, me: &entry::AsUid<'_>) -> Result<(), String> {
    let home = &ready.home;
    let want = (ready.user.uid, ready.user.gid, 0o700, 0);
    let found = rustix::fs::lstat(home)
        .map(|s| {
            (
                s.st_uid,
                s.st_gid,
                u32::from(s.st_mode) & 0o7777,
                s.st_flags,
            )
        })
        .map_err(|e| format!("{}: {e}", home.display()))?;
    if found != want {
        return Err(format!(
            "{}: owner, group, mode and flags {found:?}, not {want:?}",
            home.display()
        ));
    }
    // As the uid: root without Full Disk Access cannot look inside the home's `Library`.
    match entry::left_home(me, home, &mut run_child) {
        Ok(lines) if lines.is_empty() => Ok(()),
        Ok(lines) => Err(format!("{}: {lines:?}", home.display())),
        Err(why) => Err(why),
    }
}

/// The names in the directory `path`.
fn list(path: &Path) -> io::Result<Vec<String>> {
    std::fs::read_dir(path)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect()
}

/// `path`, a directory, opened without following a link.
fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(CWD, path, DIRECTORY, Mode::empty())?)
}

/// Gives the open directory `dir` to `uid`:`gid` with `mode` and the user flags `flags`
/// (all it has otherwise are cleared), and takes every extended attribute off it: the
/// assertion compares them with the baseline's, which is read after a clean.
fn own(dir: &OwnedFd, (uid, gid, mode): (u32, u32, u32), flags: u32) -> io::Result<()> {
    set_flags(dir, flags)?;
    strip_xattrs(dir)?;
    // Only what differs is changed: the entry's folders are the user's already after the
    // uid-side clean, and root, with no Full Disk Access, may not change them.
    let now = rustix::fs::fstat(dir)?;
    let has = (now.st_uid, now.st_gid, u32::from(now.st_mode));
    let (owner, mode_differs) = probe::ownership_to_change(has, (uid, gid, mode));
    if owner {
        rustix::fs::fchown(
            dir,
            Some(rustix::fs::Uid::from_raw(uid)),
            Some(rustix::fs::Gid::from_raw(gid)),
        )?;
    }
    if mode_differs {
        rustix::fs::fchmod(dir, Mode::from_raw_mode(mode as _))?;
    }
    Ok(())
}

/// Removes the extended attributes of the open directory `dir` that can be removed.
fn strip_xattrs(dir: &OwnedFd) -> io::Result<()> {
    for name in super::assert::xattrs_of(dir)? {
        let name = std::ffi::CString::new(name).map_err(io::Error::other)?;
        // SAFETY: `dir` is a descriptor this function borrows and `name` a NUL-terminated
        // attribute name.
        if unsafe { libc::fremovexattr(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let why = io::Error::last_os_error();
            // One that went since the listing is removed. One the system protects (the
            // entry's folders carry some) cannot be, and stays: the baseline records it,
            // and the assertion finds any other that stays.
            if !matches!(why.raw_os_error(), Some(libc::ENOATTR | libc::EPERM)) {
                return Err(why);
            }
        }
    }
    Ok(())
}

/// Sets the user flags of the open directory `dir` to `flags`.
fn set_flags(dir: &OwnedFd, flags: u32) -> io::Result<()> {
    if rustix::fs::fstat(dir)?.st_flags == flags {
        return Ok(());
    }
    // SAFETY: a plain call on a descriptor this function borrows.
    if unsafe { libc::fchflags(dir.as_raw_fd(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Makes the home `home`, empty, the user's, mode 0700, in its root-owned parent.
fn make_home(home: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let (Some(parent), Some(name)) = (home.parent(), home.file_name().and_then(|n| n.to_str()))
    else {
        return Err(io::Error::other("not a path to a home"));
    };
    kbf_mac_session::helper::make_home(parent, name, uid, gid)
}

/// The processes of `uid`, by effective or real uid ([`uid::processes`]), that a lease
/// could have left: all but those launchd lists as running one of its own agents in
/// `user/<uid>` ([`Service::own`]). launchd starts those again on demand after the kill
/// (`distnoted`, measured on the CI runner), as the uid: fresh processes, holding
/// nothing of a lease.
///
/// # Errors
/// The process table could not be read.
pub fn leftover_processes(uid: u32) -> io::Result<Vec<i32>> {
    let pids = uid::processes(uid)?;
    Ok(not_launchds_own(pids, uid, &probe::OwnLabels::default()))
}

/// The processes of `uid` that `launchctl print user/<uid>` lists as running one of
/// launchd's own agents ([`Service::own`]); read before a domain's bootout, which
/// empties the list. Nothing is listed when no process of the uid is alive.
pub(crate) fn launchds_own(uid: u32) -> Vec<i32> {
    let judged = probe::OwnLabels::default();
    probe::own_of_uid(
        uid::processes(uid),
        |pids| own_pids(uid, pids, &judged),
        || {
            // The process table could not be read: every service launchd lists as
            // running its own agent, as before the table was read first.
            (services_of(uid, &judged).into_iter())
                .filter(|s| s.own)
                .filter_map(|s| s.pid)
                .collect()
        },
    )
}

/// The members of `pids` that launchd lists as running one of its own agents. The
/// domain is not printed for no pids, and only the services that run one of `pids` are
/// printed for their plists, each once for `judged`.
fn own_pids(uid: u32, pids: &[i32], judged: &probe::OwnLabels) -> Vec<i32> {
    let give_up = Instant::now() + SERVICES_WITHIN;
    probe::own_among(
        pids,
        || {
            launchctl_print(&format!("user/{uid}"), PRINT_WITHIN)
                .map(|domain| probe::listed_services(&domain))
                .unwrap_or_default()
        },
        |pid, label| judged.is_own(label, Some(pid), || is_own_service(uid, label, give_up)),
    )
}

/// `pids` without those launchd lists as running one of its own agents.
fn not_launchds_own(mut pids: Vec<i32>, uid: u32, judged: &probe::OwnLabels) -> Vec<i32> {
    let own = own_pids(uid, &pids, judged);
    pids.retain(|pid| !own.contains(pid));
    pids
}

/// `pids` without those `services` lists as running one of launchd's own agents.
fn not_launchds_own_in(mut pids: Vec<i32>, services: &[Service]) -> Vec<i32> {
    pids.retain(|pid| !services.iter().any(|s| s.own && s.pid == Some(*pid)));
    pids
}

/// One service of a launchd domain, as `launchctl print` lists it.
struct Service {
    /// Its process, while it runs.
    pid: Option<i32>,
    label: String,
    /// Whether it is one of launchd's own per-user agents, which launchd loads into
    /// every user domain, which the domain's bootout leaves loaded, and which it starts
    /// again on demand: its plist is in one of [`probe::OWN_PLISTS`]. A job a lease
    /// bootstrapped or submitted is not, whatever its label: its plist is the lease's
    /// (or it has none).
    own: bool,
}

/// The longest one `launchctl print` is waited for.
const PRINT_WITHIN: Duration = Duration::from_secs(5);

/// The longest all the prints of one [`services_of`] are waited for together.
const SERVICES_WITHIN: Duration = Duration::from_secs(15);

/// The services `launchctl print user/<uid>` lists: none when the domain is gone (or
/// cannot be printed, or the print does not answer in time, which the kill and the other
/// checks then stand in for). Each is printed on its own for its plist's path
/// ([`Service::own`]), once for `judged`; one not printed in time is not launchd's own.
fn services_of(uid: u32, judged: &probe::OwnLabels) -> Vec<Service> {
    let give_up = Instant::now() + SERVICES_WITHIN;
    let Some(domain) = launchctl_print(&format!("user/{uid}"), PRINT_WITHIN) else {
        return Vec::new();
    };
    probe::listed_services(&domain)
        .into_iter()
        .map(|(pid, label)| {
            let own = judged.is_own(&label, pid, || is_own_service(uid, &label, give_up));
            Service { pid, label, own }
        })
        .collect()
}

/// Whether `user/<uid>/<label>`'s plist is launchd's own, from its own print, which is
/// waited for until `give_up` (and [`PRINT_WITHIN`] at most); `None` when it was not
/// printed.
fn is_own_service(uid: u32, label: &str, give_up: Instant) -> Option<bool> {
    let left = give_up.saturating_duration_since(Instant::now());
    let printed = launchctl_print(&format!("user/{uid}/{label}"), left.min(PRINT_WITHIN))?;
    Some(probe::plist_path(&printed).is_some_and(probe::is_own_plist))
}

/// What `launchctl print <target>` says, if it succeeds within `within`; a print that
/// does not answer is killed and logged at WARN.
fn launchctl_print(target: &str, within: Duration) -> Option<String> {
    let mut command = Command::new(uid::LAUNCHCTL);
    command.args(["print", target]).env_clear();
    match crate::bounded::stdout_within(&mut command, within) {
        Ok(out) => Some(String::from_utf8_lossy(&out).into_owned()),
        Err(true) => {
            tracing::warn!(
                target,
                ?within,
                "launchctl print did not answer; no services listed"
            );
            None
        }
        Err(false) => None,
    }
}

/// What a sweep of `plan` for `name` and `uid` would remove, without removing it: the
/// named entries that exist, and each entry `uid` owns below the plan's folders (not
/// looked into: the sweep removes it whole), found as the sweep walks, never through
/// a link, on one filesystem and to [`sweep::MAX_DEPTH`]. A place that cannot be read
/// is listed too, as the sweep would fail there.
pub(super) fn owned_entries(plan: &SweepPlan, name: &str, uid: u32) -> Vec<String> {
    let mut found = Vec::new();
    for template in &plan.named {
        let path = template
            .replace("{user}", name)
            .replace("{uid}", &uid.to_string());
        if std::fs::symlink_metadata(&path).is_ok() {
            found.push(path);
        }
    }
    for base in &plan.owned {
        if let Ok(meta) = std::fs::symlink_metadata(base)
            && meta.is_dir()
        {
            look(base, meta.dev(), uid, 0, &mut found);
        }
    }
    found
}

/// [`owned_entries`] below the directory `dir`, on device `dev`.
fn look(dir: &Path, dev: u64, uid: u32, depth: usize, found: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(why) => return found.push(format!("{}: {why}", dir.display())),
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(why) => return found.push(format!("{}: {why}", dir.display())),
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            // Gone since the listing.
            continue;
        };
        if meta.dev() != dev {
            continue;
        }
        if meta.uid() == uid {
            found.push(path.display().to_string());
        } else if meta.is_dir() && depth < sweep::MAX_DEPTH {
            look(&path, dev, uid, depth + 1, found);
        }
    }
}

/// Each of `pids` with its name, as libproc gives it (`?` for one that is gone).
fn named(pids: &[i32]) -> Vec<String> {
    pids.iter()
        .map(|&pid| {
            let mut name = [0_u8; 64];
            let size = u32::try_from(name.len()).unwrap_or(0);
            // SAFETY: `name` is a writable buffer of `size` bytes; the call writes at
            // most that many and returns how many it wrote.
            let wrote = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), size) };
            let len = usize::try_from(wrote).unwrap_or(0).min(name.len());
            let text = String::from_utf8_lossy(&name[..len]);
            format!("{pid} {}", if text.is_empty() { "?" } else { &text })
        })
        .collect()
}
