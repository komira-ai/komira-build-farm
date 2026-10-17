//! The slots on macOS: each pool user resolved and cleaned at start, a slot taken per
//! lease, the action launched as its user, every process of its uid ended, and the
//! slot cleaned before it goes back ([`crate::slots`] says what a slot gets;
//! [`crate::slots::clean`] how it is cleaned; [`host`] the Mac's part of that).

use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kbf_daemon::RuntimeError;
use kbf_daemon::uid;
use tokio::process::Child;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::clean::{self, Outcome};
use super::{AS_USER_ARG, Pool, SlotUser};
use crate::network::{Isolation, Network};
use crate::procs;

pub mod host;

/// The program that starts a process in a user's bootstrap namespace.
pub const LAUNCHCTL: &str = kbf_mac_session::macos::gui::LAUNCHCTL;

/// The program that names a user's `/var/folders` entry.
const GETCONF: &str = "/usr/bin/getconf";

/// The program that says whether Spotlight indexes a volume.
const MDUTIL: &str = "/usr/bin/mdutil";

/// One pool user, resolved: its real home and its `/var/folders` entry, each by its
/// real path, and the sandbox rule that lets an action write both.
#[derive(Debug)]
pub struct Ready {
    pub user: SlotUser,
    pub home: PathBuf,
    pub folders: PathBuf,
    rules: String,
}

/// How a slot is cleaned: [`host::MacSlot`] through [`clean::clean`], or a test's
/// stand-in.
type Cleaner = Arc<dyn Fn(&Ready) -> Outcome + Send + Sync>;

/// The slots of a root daemon: one per pool user, each free, held by one lease, or
/// quarantined.
pub struct Slots {
    shared: Arc<Shared>,
    permits: Arc<Semaphore>,
    /// This binary, which a launch runs in its `__as-user` mode.
    program: PathBuf,
}

/// What every slot of one [`Slots`] shares.
struct Shared {
    ready: Vec<Ready>,
    free: Mutex<BTreeSet<usize>>,
    quarantined: AtomicUsize,
    cleaner: Cleaner,
}

impl std::fmt::Debug for Slots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slots")
            .field("ready", &self.shared.ready)
            .field("free", &self.shared.free)
            .field("quarantined", &self.shared.quarantined)
            .finish_non_exhaustive()
    }
}

impl Slots {
    /// Resolves every user of `pool` ([`resolve`]), then cleans each slot
    /// ([`clean::clean`], every process of the uid ended `within` each kill): what a
    /// daemon killed before this one left, running or written, is gone before the
    /// daemon says `Hello`, and a slot that cannot be cleaned starts quarantined. No
    /// uid is killed before the user database has said it is the pool user's.
    ///
    /// # Errors
    /// A user cannot be resolved, or its home lies in a place the clean sweeps.
    pub fn start(pool: &Pool, within: Duration) -> io::Result<Self> {
        let program = kbf_daemon::keeper::program()?;
        let mut ready = Vec::new();
        for user in pool.users() {
            ready.push(resolve(user, &program)?);
        }
        for slot in &ready {
            // A home in a place the clean sweeps would be swept away with the uid's files.
            let swept = host::shared().owned;
            if let Some(place) = swept.iter().find(|p| slot.home.starts_with(p)) {
                return Err(io::Error::other(format!(
                    "the home {} of {} is in {}, which the clean sweeps",
                    slot.home.display(),
                    slot.user.name,
                    place.display()
                )));
            }
            warn_if_indexed(&slot.home);
        }
        let cleaner = mac_cleaner(within, program.clone());
        Ok(Self::with(ready, cleaner, program))
    }

    /// Slots over `ready`, each cleaned by `cleaner` first: a slot whose clean
    /// quarantines it is never free.
    fn with(ready: Vec<Ready>, cleaner: Cleaner, program: PathBuf) -> Self {
        let mut free = BTreeSet::new();
        for (index, slot) in ready.iter().enumerate() {
            if settle(&cleaner, slot, index) {
                free.insert(index);
            }
        }
        let quarantined = ready.len() - free.len();
        Self {
            permits: Arc::new(Semaphore::new(free.len())),
            shared: Arc::new(Shared {
                ready,
                free: Mutex::new(free),
                quarantined: AtomicUsize::new(quarantined),
                cleaner,
            }),
            program,
        }
    }

    /// How many slots can run a lease: all but the quarantined.
    #[must_use]
    pub fn count(&self) -> usize {
        let quarantined = self.shared.quarantined.load(Ordering::SeqCst);
        self.shared.ready.len().saturating_sub(quarantined)
    }

    /// A free slot, waiting for one while every slot is held.
    ///
    /// # Errors
    /// None in practice: the semaphore is never closed, and a permit always finds a
    /// free slot.
    pub async fn take(&self) -> Result<Slot, RuntimeError> {
        let permit = Arc::clone(&self.permits).acquire_owned().await;
        let permit = permit.map_err(|e| RuntimeError::Failed(format!("slots: {e}")))?;
        let index = lock(&self.shared.free).pop_first();
        let index = index.ok_or_else(|| RuntimeError::Failed("slots: none free".to_owned()))?;
        Ok(Slot {
            shared: Arc::clone(&self.shared),
            index,
            permit: Some(permit),
            program: self.program.clone(),
        })
    }
}

/// The cleaner of a root daemon's slots: [`host::MacSlot`] through [`clean::clean`].
fn mac_cleaner(within: Duration, program: PathBuf) -> Cleaner {
    Arc::new(move |ready: &Ready| {
        let slot = host::MacSlot {
            ready,
            within,
            program: &program,
        };
        clean::clean(&slot)
    })
}

/// Cleans slot `index` (`slot`) with `cleaner`, logs how that went, and says whether
/// the slot may run a lease.
fn settle(cleaner: &Cleaner, slot: &Ready, index: usize) -> bool {
    let started = Instant::now();
    let outcome = cleaner(slot);
    let (uid, took) = (slot.user.uid, started.elapsed());
    match &outcome {
        Outcome::Clean => tracing::debug!(index, uid, ?took, "slot cleaned"),
        Outcome::Recreated { why } => {
            tracing::warn!(index, uid, ?took, %why, "slot cleaned after its user was made again");
        }
        Outcome::Quarantined { why } => {
            tracing::error!(index, uid, ?took, %why, "slot quarantined: its clean failed");
        }
    }
    !matches!(outcome, Outcome::Quarantined { .. })
}

/// Logs a warning when Spotlight indexes the volume `path` is on: `mdworker` would
/// start as the slot's user to index what a lease wrote, and the kill would race it.
/// Provisioning turns indexing off on the pool's volume (`kbf_volume`).
fn warn_if_indexed(path: &Path) {
    let said = Command::new(MDUTIL)
        .arg("-s")
        .arg(path)
        .env_clear()
        .stdin(Stdio::null())
        .output();
    let said = said.map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    if !said.as_deref().is_ok_and(|text| text.contains("disabled")) {
        tracing::warn!(
            path = %path.display(),
            ?said,
            "Spotlight may index this pool home's volume"
        );
    }
}

/// One lease's slot. Given back ([`Slot::release`]) or dropped, it is cleaned first and
/// free again only if the clean passed ([`clean::clean`]); a quarantined slot stays out
/// of the pool until the daemon starts again, and the node reports one slot fewer.
pub struct Slot {
    shared: Arc<Shared>,
    index: usize,
    /// `None` once the slot has been given back.
    permit: Option<OwnedSemaphorePermit>,
    program: PathBuf,
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("index", &self.index)
            .field("ready", self.ready())
            .finish_non_exhaustive()
    }
}

/// A held slot, to clean and give back. Dropped before its clean passed (the clean's
/// thread could not start, its task never ran, or the clean panicked), it is counted
/// quarantined: never free again, and never reported.
struct Back {
    shared: Arc<Shared>,
    index: usize,
    /// `None` once the slot is free again.
    permit: Option<OwnedSemaphorePermit>,
}

impl std::fmt::Debug for Back {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Back")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Back {
    /// Cleans the slot; frees it, or quarantines it (on drop).
    fn give(mut self) {
        if settle(
            &self.shared.cleaner,
            &self.shared.ready[self.index],
            self.index,
        ) {
            lock(&self.shared.free).insert(self.index);
            drop(self.permit.take());
        }
    }
}

impl Drop for Back {
    /// Quarantines the slot unless [`Back::give`] freed it: its permit is forgotten.
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            self.shared.quarantined.fetch_add(1, Ordering::SeqCst);
            permit.forget();
        }
    }
}

impl Slot {
    /// The slot's user, home and folders.
    #[must_use]
    pub fn ready(&self) -> &Ready {
        &self.shared.ready[self.index]
    }

    /// The program and arguments that run `program` with `args` as the slot's user:
    /// `launchctl asuser <uid> <this binary> __as-user <uid> <gid> -- ` and then the
    /// sandbox's wrap ([`Isolation::wrap_slot`]), whose write rules add the slot's home
    /// and folders.
    ///
    /// # Errors
    /// The slot's uid is 0 (never, past [`Pool::new`]: checked again here, where the
    /// action would start).
    pub fn launch(
        &self,
        isolation: &Isolation,
        network: Network,
        lease: &Path,
        program: PathBuf,
        args: &[String],
    ) -> Result<(PathBuf, Vec<String>), RuntimeError> {
        let ready = self.ready();
        let (uid, gid) = (ready.user.uid, ready.user.gid);
        if uid == 0 {
            return Err(RuntimeError::Failed(
                "refusing to run an action as root".to_owned(),
            ));
        }
        let (program, args) = isolation.wrap_slot(network, lease, &ready.rules, program, args);
        let mut launch = vec![
            "asuser".to_owned(),
            uid.to_string(),
            self.program.to_string_lossy().into_owned(),
            AS_USER_ARG.to_owned(),
            uid.to_string(),
            gid.to_string(),
            "--".to_owned(),
            program.to_string_lossy().into_owned(),
        ];
        launch.extend(args);
        Ok((PathBuf::from(LAUNCHCTL), launch))
    }

    /// Makes the lease directory `dir`, and everything in it, the slot user's (no link
    /// followed), so the action can write its inputs, outputs and own directories.
    ///
    /// # Errors
    /// An entry could not be read or given to the user.
    pub async fn hand_over(&self, dir: &Path) -> Result<(), RuntimeError> {
        let (uid, gid) = (self.ready().user.uid, self.ready().user.gid);
        let path = dir.to_owned();
        let handed = tokio::task::spawn_blocking(move || chown_tree(&path, uid, gid)).await;
        handed
            .map_err(io::Error::other)
            .and_then(|done| done)
            .map_err(|e| RuntimeError::Failed(format!("hand {} over: {e}", dir.display())))
    }

    /// The watch over the action's processes: every process of the slot's uid.
    #[must_use]
    pub fn watch(&self, record: PathBuf) -> UidWatch {
        UidWatch {
            uid: self.ready().user.uid,
            record,
            seen: Arc::default(),
            armed: true,
        }
    }

    /// Cleans the slot and gives it back ([`clean::clean`]: free again if the clean
    /// passed, quarantined if not), once its lease has ended and the clean of its lease
    /// directory has run, whether or not that removed everything (the shared sweep
    /// removes what of the uid is left there, and the lease reports the failed clean).
    pub async fn release(mut self) {
        let Some(back) = self.back() else {
            return;
        };
        if let Err(why) = tokio::task::spawn_blocking(move || back.give()).await {
            tracing::error!(%why, "the clean of a slot did not finish: it is quarantined");
        }
    }

    /// What giving the slot back needs, once.
    fn back(&mut self) -> Option<Back> {
        let permit = self.permit.take()?;
        Some(Back {
            shared: Arc::clone(&self.shared),
            index: self.index,
            permit: Some(permit),
        })
    }
}

impl Drop for Slot {
    /// A slot dropped without [`Slot::release`] (the daemon dropped the run) is cleaned
    /// and given back on a thread of its own.
    fn drop(&mut self) {
        let Some(back) = self.back() else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("kbf-slot-clean".to_owned())
            .spawn(move || back.give());
        if let Err(why) = spawned {
            tracing::error!(%why, "no thread to clean a dropped slot: it is quarantined");
        }
    }
}

/// The processes of an action that runs as a slot's user: every process of the uid.
/// Dropped while armed (the daemon dropped the run), it ends them, blocking.
#[derive(Debug)]
pub struct UidWatch {
    uid: u32,
    record: PathBuf,
    /// Every pid of the uid any measurement saw: whose a sandbox denial is.
    seen: Arc<Mutex<BTreeSet<i32>>>,
    armed: bool,
}

impl UidWatch {
    /// Every pid of the uid seen so far.
    #[must_use]
    pub fn seen(&self) -> BTreeSet<i32> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The physical footprint of every process whose effective uid is the slot's, in
    /// bytes.
    ///
    /// # Errors
    /// The process table could not be read.
    pub async fn measure(&self) -> Result<u64, RuntimeError> {
        let (uid, seen) = (self.uid, Arc::clone(&self.seen));
        let used = tokio::task::spawn_blocking(move || {
            let pids = uid::effective(uid)?;
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(pids.iter().copied());
            Ok::<_, io::Error>(
                pids.into_iter()
                    .filter_map(procs::footprint)
                    .fold(0_u64, u64::saturating_add),
            )
        })
        .await;
        used.map_err(io::Error::other)
            .and_then(|used| used)
            .map_err(|e| RuntimeError::Failed(format!("memory of uid {uid}: {e}")))
    }

    /// Ends every process of the uid ([`uid::end`], `wait` at most), then reaps the
    /// leader, `child`: once nothing of the uid is left the leader has exited too, or
    /// it is `launchctl`, which exits once its child has; one still there after `wait`
    /// is SIGKILLed by pid, safe because it is this process's unreaped child. Removes
    /// the run record. Survivors fail the lease and keep their record; the slot's clean
    /// kills again, and quarantines the slot if they still survive.
    ///
    /// # Errors
    /// Processes of the uid survived, or the leader could not be reaped.
    pub async fn end(&mut self, child: &mut Child, wait: Duration) -> Result<(), RuntimeError> {
        let uid = self.uid;
        let ended = tokio::task::spawn_blocking(move || uid::end(uid, wait)).await;
        let ended = ended
            .map_err(|e| e.to_string())
            .and_then(|e| e.map_err(|e| e.to_string()));
        // Ended or not, the drop does not kill again: the slot's clean does.
        self.armed = false;
        if let Err(why) = ended {
            return Err(RuntimeError::Failed(why));
        }
        let _ = std::fs::remove_file(&self.record);
        if tokio::time::timeout(wait, child.wait()).await.is_err() {
            tracing::warn!(
                uid,
                "the action's leader outlived its uid's processes; killing it"
            );
            let _ = child.start_kill();
        }
        child
            .wait()
            .await
            .map(drop)
            .map_err(|e| RuntimeError::Failed(format!("reap the action: {e}")))
    }
}

impl Drop for UidWatch {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let ended = uid::end(self.uid, kbf_daemon::keeper::UID_WITHIN);
        if ended.is_ok() {
            let _ = std::fs::remove_file(&self.record);
        }
        tracing::warn!(
            uid = self.uid,
            ?ended,
            "run dropped; ended the slot's processes"
        );
    }
}

/// `user` resolved: its uid must carry its name in the user database, its home must
/// be a directory of its own, and `getconf DARWIN_USER_DIR`, run as the user through
/// the same launch as an action ([`program`] in its `__as-user` mode), names its
/// `/var/folders` entry (`.../0/`, whose parent holds `0`, `C` and `T`).
///
/// # Errors
/// Any of that fails, or a path holds a character the sandbox profile cannot quote.
fn resolve(user: &SlotUser, program: &Path) -> io::Result<Ready> {
    let (name, home) = passwd(user.uid)?;
    if name != user.name {
        return Err(io::Error::other(format!(
            "uid {} is {name:?} in the user database, not the pool user {}",
            user.uid, user.name
        )));
    }
    let home = std::fs::canonicalize(&home)
        .map_err(|e| io::Error::other(format!("home {} of {name}: {e}", home.display())))?;
    let owner = std::fs::symlink_metadata(&home)?;
    if !owner.is_dir() || owner.uid() != user.uid {
        return Err(io::Error::other(format!(
            "home {} of {name} is not a directory of uid {}",
            home.display(),
            user.uid
        )));
    }
    let out = Command::new(LAUNCHCTL)
        .arg("asuser")
        .arg(user.uid.to_string())
        .arg(program)
        .arg(AS_USER_ARG)
        .arg(user.uid.to_string())
        .arg(user.gid.to_string())
        .args(["--", GETCONF, "DARWIN_USER_DIR"])
        .env_clear()
        .stdin(Stdio::null())
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let dir = Path::new(text.trim());
    let parent = dir
        .parent()
        .filter(|_| out.status.success() && dir.is_absolute());
    let folders = parent
        .map(std::fs::canonicalize)
        .transpose()?
        .ok_or_else(|| {
            io::Error::other(format!(
                "{GETCONF} DARWIN_USER_DIR as {name}: {} {:?} {}",
                out.status,
                text.trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        })?;
    let rules = format!(
        "(allow file-write* (subpath {}) (subpath {}))\n",
        quoted(&home)?,
        quoted(&folders)?
    );
    Ok(Ready {
        user: user.clone(),
        home,
        folders,
        rules,
    })
}

/// `path` as a sandbox profile string literal.
fn quoted(path: &Path) -> io::Result<String> {
    let text = path.to_str().unwrap_or("\"");
    if text.contains(['"', '\\']) {
        return Err(io::Error::other(format!(
            "{} cannot be named in a sandbox profile",
            path.display()
        )));
    }
    Ok(format!("\"{text}\""))
}

/// The name and home directory the user database gives `uid`.
fn passwd(uid: u32) -> io::Result<(String, PathBuf)> {
    // SAFETY: a zeroed passwd is a valid value for getpwuid_r to overwrite.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 1 << 16];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live, writable value of the right type, and
    // `buffer.len()` is the buffer's size.
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &raw mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut found,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    if found.is_null() || entry.pw_name.is_null() || entry.pw_dir.is_null() {
        return Err(io::Error::other(format!("no user has uid {uid}")));
    }
    // SAFETY: getpwuid_r found the entry: both are NUL-terminated strings in `buffer`,
    // which lives until the copies are made.
    let (name, dir) = unsafe { (CStr::from_ptr(entry.pw_name), CStr::from_ptr(entry.pw_dir)) };
    Ok((
        name.to_string_lossy().into_owned(),
        PathBuf::from(OsStr::from_bytes(dir.to_bytes())),
    ))
}

/// Gives `root` and everything under it to `uid`:`gid`, never following a link.
fn chown_tree(root: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let mut pending: Vec<PathBuf> = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        std::os::unix::fs::lchown(&path, Some(uid), Some(gid))?;
        if std::fs::symlink_metadata(&path)?.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn lock(free: &Mutex<BTreeSet<usize>>) -> std::sync::MutexGuard<'_, BTreeSet<usize>> {
    // Each update is one set operation, so the set is consistent after a panic.
    free.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool users of [`bookkeeping`]: no user database behind them.
    fn ready(count: usize) -> Vec<Ready> {
        (0..count)
            .map(|i| Ready {
                user: SlotUser {
                    name: format!("kbf-unit-slot-{i}"),
                    uid: 700 + u32::try_from(i).expect("small"),
                    gid: 700,
                },
                home: PathBuf::from("/nonexistent"),
                folders: PathBuf::from("/nonexistent"),
                rules: String::new(),
            })
            .collect()
    }

    /// Slots over `count` users that no test resolves or kills, cleaned by a stand-in
    /// that answers `outcomes` in turn (then `Clean`) and counts its calls: the slot
    /// bookkeeping alone (which slot is free, how many permits, how many reported).
    fn bookkeeping(count: usize, outcomes: Vec<Outcome>) -> (Slots, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let queue = Mutex::new(std::collections::VecDeque::from(outcomes));
        let counted = Arc::clone(&calls);
        let cleaner: Cleaner = Arc::new(move |_: &Ready| {
            counted.fetch_add(1, Ordering::SeqCst);
            let next = queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            next.unwrap_or(Outcome::Clean)
        });
        let slots = Slots::with(ready(count), cleaner, PathBuf::from("/nonexistent"));
        (slots, calls)
    }

    fn quarantined() -> Outcome {
        Outcome::Quarantined {
            why: "test".to_owned(),
        }
    }

    /// Catches a slot that goes back to the pool uncleaned or although its clean
    /// quarantined it (the next lease would find the last one's state, or run beside
    /// its survivors), a quarantine the node does not report, and a cleaned slot that
    /// does not go back: a clean slot (or one whose user was made again) is taken again
    /// at once; a quarantined one never, and is reported one fewer.
    #[tokio::test]
    async fn a_quarantined_slot_stays_out_of_the_pool_and_is_not_reported() {
        let recreated = Outcome::Recreated {
            why: "test".to_owned(),
        };
        let (slots, calls) = bookkeeping(1, vec![Outcome::Clean, recreated, quarantined()]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the start cleans each slot"
        );
        assert_eq!(slots.count(), 1);
        for (cleans, _) in [(2, "recreated"), (3, "quarantined")] {
            let slot = tokio::time::timeout(Duration::from_secs(5), slots.take())
                .await
                .expect("a cleaned slot goes back")
                .expect("taken");
            assert_eq!(slot.ready().user.uid, 700);
            slot.release().await;
            assert_eq!(calls.load(Ordering::SeqCst), cleans, "release cleans");
        }
        assert_eq!(slots.count(), 0, "a quarantined slot is reported");
        let again = tokio::time::timeout(Duration::from_millis(300), slots.take()).await;
        assert!(again.is_err(), "a quarantined slot was taken again");
        assert!(
            lock(&slots.shared.free).is_empty(),
            "it is back in the free set"
        );
    }

    /// Catches a slot whose clean at the start failed taken by a lease, and one dropped
    /// mid-run (the daemon dropped the run) that goes back uncleaned or never: of two
    /// slots, the quarantined one is never handed out, and the other, dropped, is
    /// cleaned and free again.
    #[tokio::test]
    async fn a_slot_quarantined_at_the_start_or_dropped_is_handled() {
        let (slots, calls) = bookkeeping(2, vec![quarantined()]);
        assert_eq!(slots.count(), 1);
        let slot = slots.take().await.expect("the other slot");
        assert_eq!(slot.ready().user.uid, 701);
        assert!(format!("{slot:?}").contains("kbf-unit-slot-1"), "{slot:?}");
        assert!(format!("{slots:?}").contains("quarantined: 1"), "{slots:?}");
        drop(slot);
        let slot = tokio::time::timeout(Duration::from_secs(5), slots.take())
            .await
            .expect("a dropped slot goes back once cleaned")
            .expect("taken");
        assert_eq!(slot.ready().user.uid, 701);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        slot.release().await;
    }

    /// Catches a slot whose clean did not finish (the clean panicked, or its thread or
    /// task never ran) left counted as able to run a lease, with a permit that finds
    /// no free slot (`take` would fail with "none free"): it is counted quarantined,
    /// and no lease waits on it.
    #[tokio::test]
    async fn a_slot_whose_clean_did_not_finish_is_quarantined() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let cleaner: Cleaner = Arc::new(move |_: &Ready| {
            if counted.fetch_add(1, Ordering::SeqCst) > 0 {
                panic!("the clean of the hand-back panics");
            }
            Outcome::Clean
        });
        let slots = Slots::with(ready(1), cleaner, PathBuf::from("/nonexistent"));
        slots.take().await.expect("taken").release().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            slots.count(),
            0,
            "a slot whose clean did not finish is reported"
        );
        let again = tokio::time::timeout(Duration::from_millis(300), slots.take()).await;
        assert!(again.is_err(), "a permit was left for it: {again:?}");
    }

    /// Catches a slot lease handed a simulator (it would be made in root's device set,
    /// which the slot user does not see), and a slot lease refused although it asked
    /// for none.
    #[tokio::test]
    async fn a_slot_lease_is_refused_a_simulator() {
        let (slots, _) = bookkeeping(1, Vec::new());
        let runner = crate::slots::Runner::Slot(slots.take().await.expect("slot"));
        let refused = runner.allows_simulator(true).expect_err("a simulator");
        assert!(refused.to_string().contains("pool user"), "{refused}");
        assert!(runner.allows_simulator(false).is_ok());
        runner.release().await;
    }

    /// Catches a memory watch that measures nothing (a lease past its memory limit
    /// would never be stopped): over this process's own uid, which runs this test, it
    /// measures more than nothing.
    #[tokio::test]
    async fn the_watch_measures_the_uids_processes() {
        let watch = UidWatch {
            uid: rustix::process::geteuid().as_raw(),
            record: PathBuf::from("/nonexistent"),
            seen: Arc::default(),
            armed: false,
        };
        let used = watch.measure().await.expect("measured");
        assert!(used > 1 << 20, "this uid's processes use {used} bytes");
        assert!(
            watch
                .seen()
                .contains(&i32::try_from(std::process::id()).expect("pid")),
            "the measurement saw this process"
        );
    }
}
