//! Action slots: the lease-user pool a root daemon runs actions as
//! (docs/design/macos-end-state.md, steps 3 and 4).
//!
//! On a Mac whose daemon runs as root, every action runs as a pool user of its own,
//! one per slot: `kbf-pool-0` .. `kbf-pool-<N-1>`, uids `first` .. `first + N - 1`,
//! primary group id `first`, as kbf-mac-provision makes them (`pool_users`,
//! `pool_first_uid`; the daemon's `--pool-users` and `--pool-first-uid`). What one slot
//! gets, and how its processes are ended, is macOS's part ([`macos`]):
//!
//! - a lease takes a free slot, waiting for one while all are busy, and gives it back
//!   once the lease has ended and the clean of its directory has run; the slot is
//!   cleaned first ([`clean`]: its launchd domain, crontab, `at` jobs, processes, disk
//!   images, System V IPC objects, home, `/var/folders` entry and files in the shared
//!   temporary places, crash reports), the clean is checked, and a clean that fails
//!   leaves the slot dirty: not offered to a lease, the node reports one fewer, and it
//!   is cleaned again before it is offered ([`book`]); the user is never made again;
//! - its lease directory is the slot user's alone (mode 0700, from the moment it is made:
//!   another pool user cannot read, list or connect into it, and the root of the leases is
//!   searchable by others, `LEASES_ROOT_MODE`, so the slot user can reach it), and its
//!   action is started through
//!   `launchctl asuser <uid>`, then this binary's `__as-user` mode, which drops to the
//!   slot's uid and group (no supplementary groups) and executes it: so it runs in
//!   `user/<uid>`'s bootstrap namespace, with the per-user services (`cfprefsd`,
//!   the keychain) of its own user;
//! - the sandbox lets it write its lease directory, `/dev`, the slot user's real home
//!   and its `/var/folders` entry, and no longer denies `defaults write`, `open` or
//!   handing launchd a job: what those protected was a uid every lease shared;
//! - its memory is the sum over the processes whose effective uid is the slot's;
//! - its end and the keeper's are one rule, every process of the uid
//!   ([`kbf_daemon::uid::end`]); the start cleans every slot before the daemon says
//!   `Hello`.
//!
//! No action runs as root: a pool user below [`MIN_UID`] (root's 0 among them) is
//! refused here, and a root daemon without a pool refuses to start
//! ([`check_daemon_user`]). Linux native keeps its process-group tracking
//! ([`crate::procs`]); a pool there is refused.
//!
//! The scheduler holds a node to its `action_slots` count: it places no lease on the node
//! once it holds that many, so a lease waits here for a slot only while the directory of
//! the one that just ended is still being cleaned. Not yet: nothing drains or reboots a
//! node with a dirty slot. A dirty slot lowers the runtime's `action_slots` entry at
//! once, through its own part of the driver watch ([`SOURCE`]), and a recovery thread
//! cleans it again ([`book`]) with no Xcode survey and no lease needed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kbf_daemon::RuntimeError;

use crate::network::{Isolation, Network};
use crate::record::KeeperFor;

pub mod canary;
// The bookkeeping and the clean's rules over text and a fake `launchctl` run on every
// platform; only a Mac uses them outside tests.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod assertion;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod book;
pub mod clean;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod crash;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod domain;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod entry;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod keychain;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod probe;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod tool;

/// The lowest uid a pool user may have: every uid below it is root's, a system
/// account's, or (from 501) one a Mac gives its first human users. The pool's uids
/// start at 600 or above in provisioning too.
pub const MIN_UID: u32 = 600;

/// The most users a pool may have, as in provisioning.
pub const MAX_USERS: u32 = 100;

/// The mode of the root of the lease directories (`--data-dir`'s `leases`) when a pool
/// runs actions: the daemon's (root's), searchable but not listable by others, so that a
/// slot user reaches its own lease directory through it and no other. A mode of 0700
/// there stops the first lease ("working directory: Permission denied"). The node makes
/// the directory; a test that runs a pool over a scratch directory makes the same mode.
pub const LEASES_ROOT_MODE: u32 = 0o711;

/// The mode of a slot's lease directory: its user's alone. The pool users share a
/// primary group and a Mac's default is 0755, so any other mode lets another slot read
/// the lease's files, list them and connect to a socket in them.
pub const LEASE_DIR_MODE: u32 = 0o700;

/// The name of the slots' part of the daemon's driver watch: the `action_slots` entry,
/// pushed whenever the number of usable (not dirty) slots changes.
pub const SOURCE: &str = "slots";

/// The node report entry that says how many actions run at once.
pub const CAPABILITY: &str = "action_slots";

/// The argument after argv0 that selects this binary's `as-user` mode.
pub const AS_USER_ARG: &str = "__as-user";

/// One pool user an action can run as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotUser {
    /// Its name in the user database; the daemon checks that the uid has it.
    pub name: String,
    pub uid: u32,
    /// Its primary group.
    pub gid: u32,
}

/// The pool users actions run as, one per slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    users: Vec<SlotUser>,
}

/// Why a pool was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    #[error("a pool needs 1 to {MAX_USERS} users, not {0}")]
    Size(usize),
    #[error("pool user {name} has uid 0: no action runs as root")]
    Root { name: String },
    #[error("pool user {name} has uid {uid}, below {MIN_UID}: a system account's or a person's")]
    LowUid { name: String, uid: u32 },
    #[error("pool user {name} has group id 0")]
    RootGroup { name: String },
    #[error("uid {0} is in the pool twice")]
    Twice(u32),
    #[error("a pool of {count} users from uid {first} runs past the last uid")]
    Overflow { count: u32, first: u32 },
}

impl Pool {
    /// The pool `users`, slot `i` being `users[i]`.
    ///
    /// # Errors
    /// No users or more than [`MAX_USERS`], a uid below [`MIN_UID`] (root's 0
    /// among them), group id 0, or a uid twice.
    pub fn new(users: Vec<SlotUser>) -> Result<Self, PoolError> {
        if users.is_empty() || users.len() > MAX_USERS as usize {
            return Err(PoolError::Size(users.len()));
        }
        let mut seen = BTreeSet::new();
        for user in &users {
            let name = user.name.clone();
            if user.uid == 0 {
                return Err(PoolError::Root { name });
            }
            if user.uid < MIN_UID {
                let uid = user.uid;
                return Err(PoolError::LowUid { name, uid });
            }
            if user.gid == 0 {
                return Err(PoolError::RootGroup { name });
            }
            if !seen.insert(user.uid) {
                return Err(PoolError::Twice(user.uid));
            }
        }
        Ok(Self { users })
    }

    /// The pool kbf-mac-provision makes: `kbf-pool-<i>` for `i < count`, uid
    /// `first + i`, primary group id `first`.
    ///
    /// # Errors
    /// As [`Pool::new`], and a pool whose last uid would not fit.
    pub fn provisioned(count: u32, first: u32) -> Result<Self, PoolError> {
        let last = first.checked_add(count.saturating_sub(1));
        if last.is_none() {
            return Err(PoolError::Overflow { count, first });
        }
        Self::new(
            (0..count)
                .map(|i| SlotUser {
                    name: format!("kbf-pool-{i}"),
                    uid: first + i,
                    gid: first,
                })
                .collect(),
        )
    }

    /// The users, slot by slot.
    #[must_use]
    pub fn users(&self) -> &[SlotUser] {
        &self.users
    }
}

/// Why a root daemon without a pool does not start.
const ROOT_WITHOUT_POOL: &str = "the daemon runs as root and has no lease-user pool \
     (--pool-users): it runs no action as root";

/// Whether the daemon may run actions as it is: a root daemon only with a pool (it
/// would otherwise run every action as root), a pool only with a root daemon (only
/// root can start a process as another uid and kill all of a uid's).
///
/// # Errors
/// What is refused, and why.
#[cfg(target_os = "macos")]
pub fn check_daemon_user(root: bool, pool: Option<&Pool>) -> Result<(), String> {
    match (root, pool) {
        (true, None) => Err(ROOT_WITHOUT_POOL.to_owned()),
        (false, Some(_)) => Err("a lease-user pool needs a daemon running as root".to_owned()),
        _ => Ok(()),
    }
}

/// Whether the daemon may run actions as it is: off macOS, never with a pool, and never
/// as root (it would run every action as root).
///
/// # Errors
/// What is refused, and why.
#[cfg(not(target_os = "macos"))]
pub fn check_daemon_user(root: bool, pool: Option<&Pool>) -> Result<(), String> {
    match (root, pool) {
        (_, Some(_)) => Err("a lease-user pool runs on macOS only".to_owned()),
        (true, None) => Err(ROOT_WITHOUT_POOL.to_owned()),
        (false, None) => Ok(()),
    }
}

/// Whether a pool may run under `isolation`: a pool's slot profile is what keeps a lease
/// from writing outside its own directories and its canaries ([`canary`]) run under it,
/// so a node whose isolation is [`Isolation::None`] (no `sandbox-exec`) with a pool is
/// refused at start, not at every lease.
///
/// # Errors
/// What is refused, and why.
pub fn check_pool_isolation(pool: Option<&Pool>, isolation: &Isolation) -> Result<(), String> {
    match (pool, isolation) {
        (Some(_), Isolation::None) => Err(
            "a lease-user pool needs sandbox-exec, which this node lacks: the slot profile \
             and its canaries cannot run"
                .to_owned(),
        ),
        _ => Ok(()),
    }
}

/// Makes the lease directory `dir` ([`LEASE_DIR_MODE`] for a slot user's, otherwise the
/// process's default for a directory) and nothing above it.
///
/// # Errors
/// The directory exists, or its parent does not.
pub(crate) async fn make_lease_dir(dir: &Path, private: bool) -> std::io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    if private {
        builder.mode(LEASE_DIR_MODE);
    }
    builder.create(dir).await
}

/// Gives `root` and everything under it to `uid`:`gid`, never following a link, then
/// closes `root` itself to everyone else ([`LEASE_DIR_MODE`], whatever its mode was),
/// which is what keeps another slot out of the tree. A file with more than one name is
/// not given (its owner is the owner of every name): the result counts those. Stops at
/// the next entry, with [`std::io::ErrorKind::Interrupted`], once `cancel` is set, so
/// that nothing is given after the caller has been told it stopped.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn hand_tree(
    root: &Path,
    (uid, gid): (u32, u32),
    cancel: &std::sync::atomic::AtomicBool,
) -> std::io::Result<usize> {
    hand_tree_with(root, cancel, &mut |path| {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))
    })
}

/// [`hand_tree`] with the owner change made by `chown`, so a test sees which entries it
/// is made for.
#[cfg(any(target_os = "macos", test))]
fn hand_tree_with(
    root: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    chown: &mut dyn FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<usize> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::sync::atomic::Ordering;
    /// How many names a log line carries.
    const NAMED: usize = 5;
    let mut pending: Vec<PathBuf> = vec![root.to_owned()];
    let mut skipped: Vec<PathBuf> = Vec::new();
    while let Some(path) = pending.pop() {
        if cancel.load(Ordering::SeqCst) {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let meta = std::fs::symlink_metadata(&path)?;
        // A file with another name (a hard link) is not given: changing its owner would
        // change the owner of the file under the other name, which may be a file of
        // the daemon's or of another user's.
        if !meta.is_dir() && meta.nlink() > 1 {
            skipped.push(path);
            continue;
        }
        chown(&path)?;
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(LEASE_DIR_MODE))?;
    if !skipped.is_empty() {
        let names: Vec<_> = skipped
            .iter()
            .take(NAMED)
            .map(|p| p.display().to_string())
            .collect();
        tracing::warn!(
            count = skipped.len(),
            ?names,
            "not handed over: these files have another name"
        );
    }
    Ok(skipped.len())
}

/// A lease's directory and the runner whose slot's user owns it. Fields drop in the
/// order they are declared, so the directory goes first and the slot after, whether the
/// run ends, is dropped or panics: a slot is cleaned and offered again as soon as it
/// drops, and must not be while the directory still exists. Everything that holds the
/// slot during the run borrows it from here and owns none. Reordering the fields breaks
/// that (`the_directory_drops_before_the_slot_even_on_a_panic`); there is no `Drop` impl
/// so that `run` can move each out to clean and release it in turn.
pub(crate) struct Scope<D, S> {
    pub dir: D,
    pub runner: S,
}

/// How a lease's action runs: as the daemon's own user, its processes followed by
/// process group ([`crate::procs`]), or as the pool user of the slot it took (macOS).
#[derive(Debug, Default)]
pub(crate) enum Runner {
    #[default]
    Daemon,
    #[cfg(target_os = "macos")]
    Slot(macos::Slot),
}

impl Runner {
    /// The program and arguments that run `program` with `args` under `network` in
    /// `lease`: the sandbox's wrap ([`Isolation::wrap`], with `rules`), or a slot's
    /// launch ([`macos::Slot::launch`], with its own rules).
    ///
    /// # Errors
    /// A slot refused to run the action (its uid is 0).
    pub fn wrap(
        &self,
        isolation: &Isolation,
        (network, lease, rules): (Network, &Path, &str),
        program: PathBuf,
        args: &[String],
    ) -> Result<(PathBuf, Vec<String>), RuntimeError> {
        match self {
            Self::Daemon => Ok(isolation.wrap(network, lease, rules, program, args)),
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => slot.launch(isolation, network, lease, program, args),
        }
    }

    /// Whether the lease directory is made closed to others at once (a slot user's, which
    /// the pool's other users could otherwise list while its inputs are written).
    pub fn private_lease(&self) -> bool {
        match self {
            Self::Daemon => false,
            #[cfg(target_os = "macos")]
            Self::Slot(_) => true,
        }
    }

    /// Makes the lease directory `dir` the slot user's ([`macos::Slot::hand_over_until`]);
    /// nothing for the daemon's user, whose it is. Stops once `cancel` is set.
    ///
    /// # Errors
    /// It could not be handed over.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(unused_variables, reason = "no slots off macOS")
    )]
    pub async fn hand_over(
        &self,
        dir: &Path,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<(), RuntimeError> {
        match self {
            Self::Daemon => Ok(()),
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => slot.hand_over_until(dir, cancel).await,
        }
    }

    /// Runs the profile canaries as the slot's user under the profile of the lease in
    /// `lease` ([`macos::Slot::canary`]); none for the daemon's user, which has no slot
    /// profile.
    ///
    /// # Errors
    /// A canary could not be started.
    #[cfg(target_os = "macos")]
    pub async fn canary(
        &self,
        isolation: &Isolation,
        network: Network,
        lease: &Path,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<canary::Report, RuntimeError> {
        match self {
            Self::Daemon => Ok(canary::Report::default()),
            Self::Slot(slot) => slot.canary_until(isolation, network, lease, cancel).await,
        }
    }

    /// The cheap probes of the assertion for the slot about to run the lease in `lease`
    /// ([`macos::Slot::assert_ready`]): the lines that differ from the slot's baseline,
    /// none for the daemon's user, which has no baseline.
    ///
    /// # Errors
    /// The check did not finish.
    #[cfg(target_os = "macos")]
    pub async fn assert_ready(
        &self,
        lease: &Path,
        cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Vec<String>, RuntimeError> {
        match self {
            Self::Daemon => Ok(Vec::new()),
            Self::Slot(slot) => slot.assert_ready_until(lease, cancel).await,
        }
    }

    /// Takes the lease directory `dir` back from the slot's user ([`macos::Slot::reclaim`]).
    #[cfg(target_os = "macos")]
    ///
    /// # Errors
    /// The directory could not be taken back.
    pub async fn reclaim(&self, dir: &Path) -> Result<(), RuntimeError> {
        match self {
            Self::Daemon => Ok(()),
            Self::Slot(slot) => slot.reclaim(dir).await,
        }
    }

    /// Marks the slot dirty after a failed canary ([`macos::Slot::set_aside`]): not
    /// offered, not counted, cleaned again by the recovery thread.
    #[cfg(target_os = "macos")]
    pub fn set_aside(self, why: &str) {
        match self {
            Self::Daemon => {}
            Self::Slot(slot) => slot.set_aside(why),
        }
    }

    /// The keeper the action gets.
    pub fn keeper(&self) -> KeeperFor {
        match self {
            Self::Daemon => KeeperFor::Group,
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => KeeperFor::Uid(slot.ready().user.uid),
        }
    }

    /// Refuses a simulator to an action that runs as a slot user: `simctl` runs as the
    /// daemon, and its devices would be in root's device set, which the action does not
    /// see (step 6 of docs/design/macos-end-state.md moves them into the slot user's).
    ///
    /// # Errors
    /// The lease asked for a simulator and runs in a slot.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(unused_variables, reason = "no slots off macOS")
    )]
    pub fn allows_simulator(&self, asked: bool) -> Result<(), RuntimeError> {
        match self {
            Self::Daemon => Ok(()),
            #[cfg(target_os = "macos")]
            Self::Slot(_) if asked => Err(RuntimeError::Failed(
                "simulator: not yet for an action run as a pool user".to_owned(),
            )),
            #[cfg(target_os = "macos")]
            Self::Slot(_) => Ok(()),
        }
    }

    /// Cleans a slot and gives it back once the lease has ended and the clean of its
    /// directory has run ([`macos::Slot::release`]).
    pub async fn release(self) {
        match self {
            Self::Daemon => {}
            #[cfg(target_os = "macos")]
            Self::Slot(slot) => slot.release().await,
        }
    }
}

/// [`AS_USER_ARG`]'s mode, when this process's arguments ask for it: drops to the uid
/// and group they name and executes the program (`kbf-mac-session`'s `as-user`, which
/// returns only on failure). `None` otherwise, and always off macOS. A binary that
/// starts actions as pool users (`kbf-daemon`) calls this first in `main`.
#[must_use]
pub fn dispatch() -> Option<std::process::ExitCode> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    canary_exit(&args)
        .or_else(|| entry_exit(&args))
        .map(|code| std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)))
        .or_else(|| as_user_mode(&args))
}

/// [`AS_USER_ARG`]'s mode for `args`, on macOS.
#[cfg(target_os = "macos")]
fn as_user_mode(args: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
    if args.get(1).is_some_and(|a| a == AS_USER_ARG) {
        return Some(kbf_mac_session::macos::gui::as_user(&args[2..]));
    }
    None
}

/// [`AS_USER_ARG`]'s mode: none off macOS.
#[cfg(not(target_os = "macos"))]
fn as_user_mode(_args: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
    None
}

/// The profile canary's probe mode ([`canary::ARG`]) for `args`, printing to stdout:
/// its exit code when they select it.
fn canary_exit(args: &[std::ffi::OsString]) -> Option<i32> {
    canary::dispatch(args, &canary::RealFs, &mut std::io::stdout())
}

/// The `/var/folders` entry mode ([`entry::ARG`]) for `args`: its exit code when they
/// select it. It drops to the pool user inside this process.
fn entry_exit(args: &[std::ffi::OsString]) -> Option<i32> {
    entry::dispatch(
        args,
        &mut drop_to_pool_user,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

/// Drops this process to the pool user `uid`:`gid`, with no supplementary groups.
#[cfg(target_os = "macos")]
fn drop_to_pool_user(uid: u32, gid: u32) -> Result<(), String> {
    kbf_mac_session::macos::gui::drop_to(uid, gid)
}

/// Drops this process to the pool user: only macOS has pool users.
#[cfg(not(target_os = "macos"))]
fn drop_to_pool_user(_uid: u32, _gid: u32) -> Result<(), String> {
    Err("pool users exist on macOS only".to_owned())
}

/// [`dispatch`] for a test binary, which has no `main` of its own: when the arguments
/// ask for the mode, runs it and exits (with 127 whatever went wrong: the mode returns
/// only when it did not execute the program, and has said why on stderr). Called by
/// [`as_user_test_hook!`](crate::as_user_test_hook) before the test harness starts.
#[doc(hidden)]
pub fn hook() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let _ = canary_exit(&args).map(|code| std::process::exit(code));
    let _ = entry_exit(&args).map(|code| std::process::exit(code));
    let _ = as_user_mode(&args).map(|_| std::process::exit(127));
}

/// Makes the test binary it is expanded in answer [`AS_USER_ARG`] as `kbf-daemon`
/// does, so the tests that start actions as pool users start them through it. It runs
/// [`hook`] before the test harness reads its arguments, from the platform's list of
/// start-up functions.
#[macro_export]
macro_rules! as_user_test_hook {
    () => {
        const _: () = {
            extern "C" fn kbf_as_user_hook() {
                $crate::slots::hook();
            }
            #[used]
            #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
            #[cfg_attr(
                target_vendor = "apple",
                unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")
            )]
            static KBF_AS_USER_HOOK: extern "C" fn() = kbf_as_user_hook;
        };
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off macOS the entry mode is selected and the drop to a pool user refuses.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_entry_mode_refuses_to_drop_to_a_pool_user_off_macos() {
        let args: Vec<std::ffi::OsString> = ["kbf-daemon", entry::ARG, "empty", "5", "5", "/x"]
            .iter()
            .map(std::ffi::OsString::from)
            .collect();
        assert_eq!(entry_exit(&args), Some(126));
        assert_eq!(entry_exit(&args[..1]), None);
    }

    fn user(name: &str, uid: u32, gid: u32) -> SlotUser {
        SlotUser {
            name: name.to_owned(),
            uid,
            gid,
        }
    }

    /// Catches a pool that lets an action run as root (uid 0), as a system account or
    /// a person (a uid below 600, whose every process the kill would end), with group
    /// 0, or as one uid in two slots (two leases sharing a user); and a provisioned
    /// pool whose names, uids or group differ from kbf-mac-provision's.
    #[test]
    fn a_pool_never_holds_root_a_low_uid_or_a_uid_twice() {
        let refused = [
            (vec![], PoolError::Size(0)),
            (
                vec![user("a", 0, 700)],
                PoolError::Root {
                    name: "a".to_owned(),
                },
            ),
            (
                vec![user("b", 501, 700)],
                PoolError::LowUid {
                    name: "b".to_owned(),
                    uid: 501,
                },
            ),
            (
                vec![user("c", 700, 0)],
                PoolError::RootGroup {
                    name: "c".to_owned(),
                },
            ),
            (
                vec![user("d", 700, 700), user("e", 700, 700)],
                PoolError::Twice(700),
            ),
            (vec![user("f", 700, 700); 101], PoolError::Size(101)),
        ];
        for (users, want) in refused {
            assert_eq!(Pool::new(users), Err(want.clone()), "{want}");
        }
        let edge = Pool::new(vec![user("g", MIN_UID, 1)]).expect("the lowest uid");
        assert_eq!(edge.users(), [user("g", 600, 1)]);
        let full = Pool::provisioned(MAX_USERS, 61000).expect("a full pool");
        assert_eq!(full.users().len(), 100);
        assert_eq!(full.users()[99], user("kbf-pool-99", 61099, 61000));
        assert_eq!(
            Pool::provisioned(MAX_USERS + 1, 61000),
            Err(PoolError::Size(101))
        );
        let pool = Pool::provisioned(2, 700).expect("pool");
        assert_eq!(
            pool.users(),
            [user("kbf-pool-0", 700, 700), user("kbf-pool-1", 701, 700)]
        );
        assert_eq!(
            Pool::provisioned(2, u32::MAX),
            Err(PoolError::Overflow {
                count: 2,
                first: u32::MAX
            })
        );
        assert_eq!(Pool::provisioned(0, 700), Err(PoolError::Size(0)));
        assert!(matches!(
            Pool::provisioned(1, 0),
            Err(PoolError::Root { .. })
        ));
    }

    /// Catches a root daemon that starts without a pool (every action would run as
    /// root), a pool taken by a daemon that is not root (it cannot start anything as
    /// another uid) or off macOS, and a refusal of the ordinary daemon (no pool, not
    /// root).
    #[test]
    fn only_a_root_daemon_on_macos_runs_a_pool_and_it_must() {
        let pool = Pool::provisioned(1, 700).expect("pool");
        assert_eq!(check_daemon_user(false, None), Ok(()));
        let root = check_daemon_user(true, None).expect_err("root without a pool");
        assert!(root.contains("no action as root"), "{root}");
        let as_user = check_daemon_user(false, Some(&pool)).expect_err("a pool, not root");
        let as_root = check_daemon_user(true, Some(&pool));
        #[cfg(target_os = "macos")]
        {
            assert!(as_user.contains("as root"), "{as_user}");
            assert_eq!(as_root, Ok(()));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(as_user.contains("macOS only"), "{as_user}");
            assert!(as_root.expect_err("off macOS").contains("macOS only"));
        }
    }

    /// Catches a node with a pool and no `sandbox-exec` starting (every lease would then
    /// fail its canaries, or worse run unsandboxed): refused once at start, with the
    /// reason; a pool with a sandbox, and no pool with none, are fine.
    #[test]
    fn a_pool_without_sandbox_exec_is_refused_at_start() {
        let pool = Pool::provisioned(1, 700).expect("pool");
        let sandbox = Isolation::Sandbox(PathBuf::from("/usr/bin/sandbox-exec"));
        let refused = check_pool_isolation(Some(&pool), &Isolation::None).expect_err("refused");
        assert!(refused.contains("needs sandbox-exec"), "{refused}");
        assert_eq!(check_pool_isolation(Some(&pool), &sandbox), Ok(()));
        assert_eq!(check_pool_isolation(None, &Isolation::None), Ok(()));
        assert_eq!(check_pool_isolation(None, &sandbox), Ok(()));
    }

    /// Catches the `as-user` mode taken for arguments that do not ask for it (the test
    /// harness's own run would end in it).
    #[test]
    fn only_the_as_user_argument_selects_the_mode() {
        assert!(dispatch().is_none());
        hook();
        let args = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(canary_exit(&args(&["kbf", AS_USER_ARG])), None);
        assert_eq!(
            canary_exit(&args(&["kbf", canary::ARG])),
            Some(canary::USAGE),
            "the canary mode answers for its own argument"
        );
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    }

    /// Catches a slot's lease directory left readable by the other pool users (0755 is
    /// what the daemon makes it and what a hand-over by `chown` alone keeps; the pool
    /// users share a primary group, so 0770 would leak too): after the hand-over the
    /// lease directory is 0700 and its user's, whatever its mode was; what is inside
    /// keeps its own modes and is the user's too, a link is given and not followed, and
    /// a tree that cannot be given (here, one that does not exist) fails and closes
    /// nothing.
    #[test]
    fn handing_over_a_lease_directory_closes_it_to_everyone_else() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let base = crate::unit_dir("slots-hand-tree");
        let lease = base.join("lease-1-1");
        std::fs::create_dir_all(lease.join("root/sub")).expect("tree");
        std::fs::write(lease.join("root/sub/file"), b"x").expect("file");
        std::os::unix::fs::symlink("/", lease.join("root/link")).expect("link");
        for (path, mode) in [
            (lease.clone(), 0o775),
            (lease.join("root"), 0o755),
            (lease.join("root/sub/file"), 0o644),
        ] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        }
        let me = std::fs::metadata(&lease).expect("lease");
        hand_tree(&lease, (me.uid(), me.gid()), &no_cancel()).expect("handed over");
        assert_eq!(mode_of(&lease), LEASE_DIR_MODE, "the lease directory");
        assert_eq!(mode_of(&lease), 0o700, "the documented mode");
        assert_eq!(mode_of(&lease.join("root")), 0o755, "inside keeps its mode");
        assert_eq!(mode_of(&lease.join("root/sub/file")), 0o644);
        assert!(
            std::fs::symlink_metadata(lease.join("root/link"))
                .expect("link")
                .is_symlink(),
            "the link was followed or replaced"
        );
        let missing = base.join("missing");
        assert!(hand_tree(&missing, (me.uid(), me.gid()), &no_cancel()).is_err());
        assert!(!missing.exists(), "a failed hand-over made something");
    }

    /// Records its name when dropped.
    struct Recorded(
        &'static str,
        std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    );

    impl Drop for Recorded {
        fn drop(&mut self) {
            self.1.lock().expect("log").push(self.0);
        }
    }

    /// Catches a lease scope that frees the slot before the lease directory is removed
    /// (a slot dropped first is cleaned and offered again while its user still owns the
    /// directory): a scope that ends, one dropped on a panic, and one where the runner
    /// is replaced in the middle all drop the directory first. Catches: the fields
    /// of `Scope` swapped.
    #[test]
    fn the_directory_drops_before_the_slot_even_on_a_panic() {
        use std::sync::{Arc, Mutex};
        let log = Arc::new(Mutex::new(Vec::new()));
        let scope = || Scope {
            dir: Recorded("dir", Arc::clone(&log)),
            runner: Recorded("slot", Arc::clone(&log)),
        };
        drop(scope());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _scope = scope();
            panic!("a run that panics");
        }));
        assert!(panicked.is_err());
        let Scope { dir, runner } = scope();
        drop(dir);
        drop(runner);
        assert_eq!(
            *log.lock().expect("log"),
            ["dir", "slot", "dir", "slot", "dir", "slot"]
        );
    }

    fn no_cancel() -> std::sync::atomic::AtomicBool {
        std::sync::atomic::AtomicBool::new(false)
    }

    /// Catches an owner change made for a hard-linked file (deleting the skip leaves
    /// the count right and the file given, which a hand-over to the same user does not
    /// show): the names of the entries `chown` is called for are recorded, and the
    /// linked file is not among them while the plain file and the directories are.
    #[test]
    fn the_owner_of_a_file_with_another_name_is_never_changed() {
        let base = crate::unit_dir("slots-hand-tree-seam");
        let lease = base.join("lease");
        std::fs::create_dir_all(lease.join("sub")).expect("tree");
        std::fs::write(lease.join("plain"), b"x").expect("plain");
        let outside = base.join("outside");
        std::fs::write(&outside, b"x").expect("outside");
        std::fs::hard_link(&outside, lease.join("sub/linked")).expect("link");
        let mut given = Vec::new();
        let skipped = hand_tree_with(&lease, &no_cancel(), &mut |path| {
            given.push(path.strip_prefix(&lease).expect("inside").to_owned());
            Ok(())
        })
        .expect("handed over");
        given.sort();
        let want: Vec<PathBuf> = ["", "plain", "sub"].map(PathBuf::from).to_vec();
        assert_eq!((skipped, given), (1, want));
    }

    /// Catches a hand-over that goes on after it was told to stop (a stray chown after
    /// the lease was answered as killed): with the flag set after the second entry no
    /// third is given, the root is not closed, and the call says it was interrupted.
    #[test]
    fn a_hand_over_stops_at_the_next_entry_once_cancelled() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = crate::unit_dir("slots-hand-tree-cancel");
        let lease = base.join("lease");
        std::fs::create_dir_all(&lease).expect("lease");
        for name in ["a", "b", "c", "d"] {
            std::fs::write(lease.join(name), b"x").expect("file");
        }
        std::fs::set_permissions(&lease, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let cancel = no_cancel();
        let mut given = 0;
        let stopped = hand_tree_with(&lease, &cancel, &mut |_| {
            given += 1;
            if given == 2 {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        });
        assert_eq!(
            stopped.expect_err("stopped").kind(),
            std::io::ErrorKind::Interrupted
        );
        assert_eq!(given, 2, "nothing was given after the flag was set");
        assert_eq!(mode_of(&lease), 0o755, "the root was not closed");
    }

    /// Catches a hand-over that changes the owner of a file that has another name (a
    /// hard link to a file outside the lease would make that file the slot user's):
    /// both names are skipped and counted, a file with one name and a directory are
    /// given, and the link's other name is still there.
    #[test]
    fn a_file_with_another_name_is_not_handed_over() {
        let base = crate::unit_dir("slots-hand-tree-links");
        let lease = base.join("lease");
        std::fs::create_dir_all(lease.join("sub")).expect("tree");
        std::fs::write(lease.join("plain"), b"x").expect("plain");
        let outside = base.join("outside");
        std::fs::write(&outside, b"x").expect("outside");
        std::fs::hard_link(&outside, lease.join("sub/linked")).expect("link");
        let me = std::fs::metadata(&lease).expect("lease");
        use std::os::unix::fs::MetadataExt as _;
        let skipped = hand_tree(&lease, (me.uid(), me.gid()), &no_cancel()).expect("handed over");
        assert_eq!(skipped, 1, "the name in the lease");
        assert_eq!(
            [outside.exists(), lease.join("sub/linked").exists()],
            [true, true]
        );
        std::fs::remove_file(lease.join("sub/linked")).expect("unlink");
        assert_eq!(
            hand_tree(&lease, (me.uid(), me.gid()), &no_cancel()).expect("again"),
            0
        );
    }

    /// Catches a slot's lease directory made with the process's default mode, which
    /// the other pool users can list while its inputs are written (before the
    /// hand-over): made for a slot it is 0700 at once; made for the daemon's own user
    /// it is made as it always was; neither replaces a directory that exists.
    #[tokio::test]
    async fn a_slots_lease_directory_is_closed_when_it_is_made() {
        let base = crate::unit_dir("slots-make-lease-dir");
        let private = base.join("lease-1-1");
        make_lease_dir(&private, true).await.expect("made");
        assert_eq!(mode_of(&private), 0o700);
        let plain = base.join("lease-1-2");
        make_lease_dir(&plain, false).await.expect("made");
        assert!(plain.is_dir());
        assert!(make_lease_dir(&private, true).await.is_err(), "twice");
        assert!(
            make_lease_dir(&base.join("a/b"), true).await.is_err(),
            "no parents made"
        );
        assert!(!Runner::Daemon.private_lease());
    }
}
