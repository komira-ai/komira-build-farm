//! The verbs: `user-create`, `run`, `kill-uid` and `user-delete` (S4.2), over a
//! [`Host`] that does what only the OS can (user records, launchd, processes), so the
//! rules here are tested on any platform.
//!
//! Rules every verb keeps:
//! - A lease id is parsed strictly and names one user, `kbf-lease-<term>-<seq>`.
//! - The uid comes from the ledger, never from a lookup, and must be in the lease
//!   range; a uid outside it is never touched.
//! - `user-create` is refused for a lease id used before (so names are never reused
//!   and a grant is single-use) and makes an administrator only with a valid grant.
//! - `run`, `kill-uid` and `user-delete` act only on a lease the ledger holds; `run`
//!   and `kill-uid` refuse a deleted one (its uid may belong to a newer lease).
//! - `kill-uid` kills, removes the user's crontab and `at` jobs, and kills again, so
//!   no scheduled job of the user starts after it returns.
//! - `user-delete` refuses while any process of the uid remains, first removes what
//!   can start one (crontab, `at` jobs), then looks again, sweeps the rest, looks a
//!   last time, deletes the record, and records the deletion last: a crash part-way
//!   leaves the lease live, and the delete is simply repeated. At each look it waits,
//!   bounded, for processes of the uid to exit (a cron job that started after
//!   `kill-uid`), and names those still alive when it refuses.
//!
//! The mutating verbs run one at a time (one lock around the ledger), and `run` holds
//! it while it starts the process, so no process starts for a lease being deleted.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use kbf_types::LeaseId;
use rustix::fs::{CWD, Gid, Mode, OFlags, Uid};

use crate::grant::{self, Expect, GrantKeys};
use crate::lease::{UidRange, parse_lease, user_name};
use crate::ledger::{Entry, Ledger};
use crate::proto::{Reply, Request};
use crate::spawn::{self, Spawn};
use crate::sweep::{SweepPlan, sweep};

/// How many times `kill-uid` kills and looks again before it gives up.
pub const KILL_ROUNDS: usize = 10;

/// How many times each of `user-delete`'s looks lists the uid's processes, a
/// [`Host::pause`] apart, before it refuses: about five seconds on macOS, for a job
/// that started after `kill-uid` to exit.
pub const EXIT_LOOKS: usize = 50;

/// How many processes a refusal names; it counts the rest.
const NAMED: usize = 8;

/// What the OS does for the helper. The macOS implementation is in `crate::macos`.
pub trait Host: Send + Sync {
    /// The wall clock, for grant expiry.
    fn now(&self) -> SystemTime;
    /// Whether any user record has `uid`.
    ///
    /// # Errors
    /// The directory could not be asked.
    fn uid_taken(&self, uid: u32) -> io::Result<bool>;
    /// Makes the user's home folder ([`make_home`] on a real host).
    ///
    /// # Errors
    /// The folder exists or could not be made.
    fn make_home(&self, homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()>;
    /// Creates the user record.
    ///
    /// # Errors
    /// The record could not be created.
    fn create_user(&self, user: &NewUser) -> io::Result<()>;
    /// Deletes the user record `name` (and any group membership it has); `false` when
    /// there is none.
    ///
    /// # Errors
    /// The record could not be deleted.
    fn delete_user(&self, name: &str) -> io::Result<bool>;
    /// `launchctl bootout <domain>`. A domain that does not exist is an error the
    /// caller expects and ignores.
    ///
    /// # Errors
    /// The domain could not be booted out.
    fn bootout(&self, domain: &str) -> io::Result<()>;
    /// Sends `SIGKILL` to every process of `uid`.
    ///
    /// # Errors
    /// The signal could not be sent.
    fn kill_all(&self, uid: u32) -> io::Result<()>;
    /// The live (not zombie) processes that have `uid` as their real or effective uid.
    ///
    /// # Errors
    /// The process table could not be read.
    fn live_processes(&self, uid: u32) -> io::Result<Vec<LiveProcess>>;
    /// Waits a moment between kill rounds, and between the listings of a wait.
    fn pause(&self);
}

/// A live process of a lease uid, as a refusal names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: i32,
    /// Its command name as the kernel keeps it (truncated), or `?` if unreadable.
    pub command: String,
}

/// `<count> processes of uid <uid> remain <when>: <pid> (<command>), ...`.
fn remain(uid: u32, when: &str, left: &[LiveProcess]) -> String {
    let mut named: Vec<String> = left
        .iter()
        .take(NAMED)
        .map(|p| format!("{} ({})", p.pid, p.command))
        .collect();
    if left.len() > NAMED {
        named.push(format!("and {} more", left.len() - NAMED));
    }
    format!(
        "{} processes of uid {uid} remain {when}: {}",
        left.len(),
        named.join(", ")
    )
}

/// A user record to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub admin: bool,
}

/// The helper's fixed configuration.
#[derive(Debug)]
pub struct Settings {
    pub range: UidRange,
    /// Every lease user's primary group.
    pub gid: u32,
    /// Where home folders are made (`/Users` on macOS).
    pub homes: PathBuf,
    /// What `user-delete` sweeps first: where a process of the user can be started
    /// with no session (crontab, `at` jobs).
    pub schedules: SweepPlan,
    /// What `user-delete` sweeps then, besides the home folder.
    pub sweep: SweepPlan,
    /// The gate's grant keys; without them no administrator is ever made.
    pub grant_keys: Option<GrantKeys>,
    /// This Mac's serial number, which a grant must name.
    pub serial: String,
}

/// What a request comes to.
#[derive(Debug)]
pub enum Outcome {
    /// One reply, and the request is done.
    Reply(Reply),
    /// A process started; the caller reports its exit.
    Running(Child),
}

/// The helper: settings, the host, and the ledger behind one lock.
pub struct Helper {
    host: Box<dyn Host>,
    settings: Settings,
    ledger: Mutex<Ledger>,
}

impl Helper {
    #[must_use]
    pub fn new(host: Box<dyn Host>, settings: Settings, ledger: Ledger) -> Self {
        Self {
            host,
            settings,
            ledger: Mutex::new(ledger),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Carries out `request`. Only `run` takes descriptors.
    pub fn handle(&self, request: Request, fds: Vec<OwnedFd>) -> Outcome {
        let refused = |reason: String| Outcome::Reply(Reply::Refused { reason });
        let result = match request {
            Request::Run { lease, argv, env } => {
                return self
                    .run(&lease, argv, env, fds)
                    .map_or_else(refused, Outcome::Running);
            }
            _ if !fds.is_empty() => Err("only run takes descriptors".to_owned()),
            Request::UserCreate { lease, grant } => self
                .user_create(&lease, grant.as_deref())
                .map(|uid| Reply::Created { uid }),
            Request::KillUid { lease } => self.kill_uid(&lease).map(|()| Reply::Killed),
            Request::UserDelete { lease } => self
                .user_delete(&lease)
                .map(|existed| Reply::Deleted { existed }),
        };
        result.map_or_else(refused, Outcome::Reply)
    }

    /// `user-create <lease> [grant]`: a fresh, non-admin user (an administrator with a
    /// valid grant), with an empty home folder only it can enter. Returns its uid.
    ///
    /// # Errors
    /// Why it was refused or failed.
    pub fn user_create(&self, lease: &str, grant: Option<&str>) -> Result<u32, String> {
        let lease = parse_lease(lease)?;
        let admin = match grant {
            None => false,
            Some(grant) => {
                let keys = self.settings.grant_keys.as_ref().ok_or(
                    "this Mac holds no gate key, so no lease user is ever an administrator",
                )?;
                let expect = Expect {
                    serial: &self.settings.serial,
                    lease: &lease.to_string(),
                    now: self.host.now(),
                };
                grant::verify(grant, keys, expect)?;
                true
            }
        };
        let name = user_name(lease);
        let mut ledger = self.lock();
        if ledger.get(lease).is_some() {
            return Err(format!(
                "lease {lease} was used before; a lease user's name is never reused"
            ));
        }
        let uid = self.free_uid(&ledger)?;
        ledger
            .record_created(lease, uid)
            .map_err(|why| format!("recording lease {lease}: {why}"))?;
        let home = self.settings.homes.join(&name);
        self.host
            .make_home(&self.settings.homes, &name, uid, self.settings.gid)
            .map_err(|why| format!("making {}: {why}", home.display()))?;
        let user = NewUser {
            name,
            uid,
            gid: self.settings.gid,
            home,
            admin,
        };
        self.host
            .create_user(&user)
            .map_err(|why| format!("creating user {}: {why}", user.name))?;
        tracing::info!(lease = %lease, uid, admin, "created lease user {}", user.name);
        Ok(uid)
    }

    /// The first uid after the one handed out last that no live lease holds, no user
    /// record has and no process runs as.
    fn free_uid(&self, ledger: &Ledger) -> Result<u32, String> {
        let held = ledger.held_uids();
        let range = self.settings.range;
        for uid in range.after(ledger.last_uid()) {
            if held.contains(&uid) {
                continue;
            }
            let taken = self.host.uid_taken(uid).map_err(|why| why.to_string())?;
            if !taken && self.live(uid)?.is_empty() {
                return Ok(uid);
            }
        }
        Err(format!("no free uid in {range}"))
    }

    fn live(&self, uid: u32) -> Result<Vec<LiveProcess>, String> {
        self.host
            .live_processes(uid)
            .map_err(|why| format!("listing the processes of uid {uid}: {why}"))
    }

    /// The uid of a live lease, in range.
    fn live_uid(&self, ledger: &Ledger, lease: LeaseId) -> Result<u32, String> {
        match ledger.get(lease) {
            None => Err(format!("lease {lease} has no user")),
            Some(Entry { deleted: true, .. }) => Err(format!("lease {lease}'s user was deleted")),
            Some(Entry { uid, .. }) if !self.settings.range.contains(uid) => Err(format!(
                "lease {lease}'s uid {uid} is outside {}",
                self.settings.range
            )),
            Some(Entry { uid, .. }) => Ok(uid),
        }
    }

    /// `run <lease> <fd-passed argv>`: starts the process as the lease's user. The
    /// descriptors are stdin, stdout, stderr and the lease directory.
    ///
    /// # Errors
    /// Why it was refused, or why the process did not start.
    pub fn run(
        &self,
        lease: &str,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        fds: Vec<OwnedFd>,
    ) -> Result<Child, String> {
        let lease = parse_lease(lease)?;
        spawn::check(&argv, &env)?;
        let count = fds.len();
        let [stdin, stdout, stderr, cwd]: [OwnedFd; 4] = fds.try_into().map_err(|_| {
            format!(
                "run takes stdin, stdout, stderr and the lease directory; got {count} descriptors"
            )
        })?;
        let ledger = self.lock();
        let uid = self.live_uid(&ledger, lease)?;
        let user = user_name(lease);
        let program = argv[0].clone();
        let child = spawn::spawn(Spawn {
            uid,
            gid: self.settings.gid,
            home: self.settings.homes.join(&user),
            user,
            argv,
            env,
            stdin,
            stdout,
            stderr,
            cwd,
        })
        .map_err(|why| format!("starting {program}: {why}"))?;
        drop(ledger);
        tracing::info!(lease = %lease, uid, pid = child.id(), "started {program}");
        Ok(child)
    }

    /// `kill-uid <lease>`: boots out the uid's GUI and user launchd domains (so
    /// launchd restarts none of its jobs), kills every process of the uid until none
    /// is left, removes the user's crontab and `at` jobs (so no job of the user starts
    /// after kill-uid returns: one would start a process `user-delete` refuses, and
    /// launchd per-user agents such as `distnoted` with it), then kills again, for a
    /// job that fired before they went.
    ///
    /// # Errors
    /// Why it was refused, processes that survived [`KILL_ROUNDS`] rounds, or a
    /// crontab or `at` job that could not be removed (after the second kill).
    pub fn kill_uid(&self, lease: &str) -> Result<(), String> {
        let lease = parse_lease(lease)?;
        let ledger = self.lock();
        let uid = self.live_uid(&ledger, lease)?;
        for domain in [format!("gui/{uid}"), format!("user/{uid}")] {
            if let Err(why) = self.host.bootout(&domain) {
                tracing::debug!(lease = %lease, "launchctl bootout {domain}: {why}");
            }
        }
        self.kill_rounds(uid)?;
        // Swept only once nothing of the user runs (the sweep's contract).
        let schedules = Self::sweep_all(&self.settings.schedules, &user_name(lease), uid);
        self.kill_rounds(uid)?;
        let removed = schedules?;
        tracing::info!(lease = %lease, uid, removed, "no process of the lease user is left");
        Ok(())
    }

    /// Kills every process of `uid` until none is left, for at most [`KILL_ROUNDS`].
    fn kill_rounds(&self, uid: u32) -> Result<(), String> {
        let mut left = Vec::new();
        for _ in 0..KILL_ROUNDS {
            self.host
                .kill_all(uid)
                .map_err(|why| format!("killing the processes of uid {uid}: {why}"))?;
            left = self.live(uid)?;
            if left.is_empty() {
                return Ok(());
            }
            self.host.pause();
        }
        Err(remain(uid, &format!("after {KILL_ROUNDS} rounds"), &left))
    }

    /// `user-delete <lease>`: sweeps what the user leaves, deletes its record and
    /// records the deletion. Returns whether a record was there to delete; a lease
    /// already deleted returns `false` and touches nothing.
    ///
    /// No process of the uid may be left at any of three looks: before anything is
    /// removed (so nothing of the user races the walk), once the crontab and `at` jobs
    /// are gone (a job that started after `kill-uid` is found here, and none can start
    /// later), and after the whole sweep, just before the record goes (so no process
    /// outlives its user as an orphan uid that later leases' files are open to). Each
    /// look waits up to [`EXIT_LOOKS`] listings for the processes it finds to exit: a
    /// cron job that started between `kill-uid` and this delete ends by itself, and
    /// one that does not is named in the refusal.
    ///
    /// # Errors
    /// Why it was refused: a process of the uid remains, or the sweep or the deletion
    /// failed (the lease stays live, so the delete can be repeated).
    pub fn user_delete(&self, lease: &str) -> Result<bool, String> {
        let lease = parse_lease(lease)?;
        let mut ledger = self.lock();
        if ledger.get(lease).is_some_and(|entry| entry.deleted) {
            return Ok(false);
        }
        let uid = self.live_uid(&ledger, lease)?;
        self.none_left(uid)?;
        let name = user_name(lease);
        let mut removed = Self::sweep_all(&self.settings.schedules, &name, uid)?;
        self.none_left(uid)?;
        let mut plan = self.settings.sweep.clone();
        plan.named
            .push(format!("{}/{{user}}", self.settings.homes.display()));
        removed += Self::sweep_all(&plan, &name, uid)?;
        self.none_left(uid)?;
        let existed = self
            .host
            .delete_user(&name)
            .map_err(|why| format!("deleting user {name}: {why}"))?;
        ledger
            .record_deleted(lease)
            .map_err(|why| format!("recording the deletion of lease {lease}: {why}"))?;
        tracing::info!(lease = %lease, uid, removed, existed, "deleted lease user {name}");
        Ok(existed)
    }

    /// Waits, for at most [`EXIT_LOOKS`] listings, until no process of `uid` is
    /// alive; refuses, naming the processes, if some still are.
    fn none_left(&self, uid: u32) -> Result<(), String> {
        let mut left = self.live(uid)?;
        let mut looks = 1;
        while !left.is_empty() && looks < EXIT_LOOKS {
            self.host.pause();
            left = self.live(uid)?;
            looks += 1;
        }
        if left.is_empty() {
            return Ok(());
        }
        Err(format!(
            "{}; kill-uid first",
            remain(uid, &format!("after {EXIT_LOOKS} looks"), &left)
        ))
    }

    /// Sweeps `plan` for the user; how many entries went, or why it is incomplete.
    fn sweep_all(plan: &SweepPlan, name: &str, uid: u32) -> Result<usize, String> {
        let swept = sweep(plan, name, uid);
        if swept.errors.is_empty() {
            Ok(swept.removed)
        } else {
            Err(format!(
                "the sweep for {name} is incomplete: {}",
                swept.errors.join("; ")
            ))
        }
    }
}

/// Makes the home folder `name` in `homes`: new (an existing one is an error), owned
/// by the user, mode 0700. Never through a link. `homes` is root's to write, so the
/// directory just made is still the one opened.
///
/// # Errors
/// `homes` cannot be opened, the folder exists, or it could not be given away.
pub fn make_home(homes: &Path, name: &str, uid: u32, gid: u32) -> io::Result<()> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let parent = rustix::fs::openat(CWD, homes, flags, Mode::empty())?;
    rustix::fs::mkdirat(&parent, name, Mode::from_raw_mode(0o700))?;
    let home = rustix::fs::openat(&parent, name, flags, Mode::empty())?;
    rustix::fs::fchown(&home, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
    rustix::fs::fchmod(&home, Mode::from_raw_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
mod tests;
