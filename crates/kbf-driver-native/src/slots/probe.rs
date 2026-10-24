//! What the Mac's clean reads from the tools it runs and the decisions it makes from it,
//! as plain functions with no Mac in them, so that they run under test on every
//! platform: how a launchd job is told to be launchd's own, which folders the shared
//! sweep covers, which mounts lie under a slot's folders, and how the kill loop decides
//! the uid has no process left. [`super::macos::host`] runs the tools and calls these.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_mac_session::sweep::SweepPlan;

/// The names a `/var/folders` entry keeps, emptied: `0` (the `DARWIN_USER_DIR`), `C`
/// (cache), `T` (temporary) and `X`, where they exist. Anything else in the entry is
/// removed whole: the slot's user can create files there, and nothing reads them back.
pub(crate) const SKELETON: [&str; 4] = ["0", "C", "T", "X"];

/// The folders whose plists are launchd's own: on the sealed system volume, or Apple's
/// under `/Library` (Rosetta's agents), neither of which a pool user can write.
pub(crate) const OWN_PLISTS: [&str; 2] = ["/System/", "/Library/Apple/"];

/// What `launchctl` exits with for a job or domain that is being torn down: "keep
/// waiting", never "failed".
pub(crate) const TEARING_DOWN: i32 = 124;

/// How many scans the kill loop makes at most.
pub(crate) const MAX_SCANS: usize = 100;

/// The wait between two scans of the kill loop, and between the two looks of the
/// second process look.
pub(crate) const SCAN_PAUSE: Duration = Duration::from_millis(200);

/// What the restore of the home has to change to give it `want` (uid, gid, mode), from
/// what it `has`: whether the owner (and group) and whether the mode. Nothing that is
/// right already is touched.
pub(crate) fn ownership_to_change(has: (u32, u32, u32), want: (u32, u32, u32)) -> (bool, bool) {
    (
        (has.0, has.1) != (want.0, want.1),
        has.2 & 0o7777 != want.2 & 0o7777,
    )
}

/// The widest mode the system gives the entry's folder `name`: `0` and `X` are made 0755,
/// `C` and `T` 0700 (measured on a Mac; the system refuses a `chmod` of them, even by the
/// owner, without Full Disk Access). Narrower is within it.
pub(crate) fn folder_ceiling(name: &str) -> u32 {
    match name {
        "0" | "X" => 0o755,
        _ => 0o700,
    }
}

/// What the restore of the entry's folder has to do, from what it `has` (uid, mode), the
/// pool user `uid` and the `ceiling` of its mode ([`folder_ceiling`]): whether the owner
/// is wrong (nothing can fix it), and the mode to `chmod` it to, only when it is wider
/// than the ceiling (the bits beyond it taken away, nothing added). Within the ceiling
/// nothing is touched: the flags, ACLs and attributes are the system's, and the system
/// refuses even the owner's `chmod` of the folder.
pub(crate) fn folder_fix(has: (u32, u32), uid: u32, ceiling: u32) -> (bool, Option<u32>) {
    let mode = has.1 & 0o7777;
    (
        has.0 != uid,
        (mode & !ceiling != 0).then_some(mode & ceiling),
    )
}

/// The places the shared sweep covers: launchd's per-uid files, and every entry the uid
/// owns in the `Shared` folder under `/Users`, `/private/tmp`, `/private/var/tmp` and
/// `/Library/Caches`. Not all of `/private/var/folders`: the uid can write only its own
/// entry there, which the contents step empties.
pub(crate) fn shared_plan() -> SweepPlan {
    let mut plan = SweepPlan::macos();
    plan.owned
        .retain(|place| place != Path::new("/private/var/folders"));
    plan.owned.push(PathBuf::from("/Library/Caches"));
    plan
}

/// Whether the plist at `path` is launchd's own ([`OWN_PLISTS`]).
pub(crate) fn is_own_plist(path: &str) -> bool {
    OWN_PLISTS.iter().any(|folder| path.starts_with(folder))
}

/// The plist path in what `launchctl print user/<uid>/<label>` says: its `path = `
/// line one level in (deeper ones belong to the job's nested dictionaries).
pub(crate) fn plist_path(printed: &str) -> Option<&str> {
    printed
        .lines()
        .find_map(|line| line.strip_prefix("\tpath = "))
        .map(str::trim_end)
}

/// The program in what `launchctl print user/<uid>/<label>` says: its `program = ` line
/// one level in. A job that names its program only in `arguments` has none.
pub(crate) fn program_path(printed: &str) -> Option<&str> {
    printed
        .lines()
        .find_map(|line| line.strip_prefix("\tprogram = "))
        .map(str::trim_end)
}

/// The services `launchctl print user/<uid>` lists: `services = {`, one `<pid, 0 when
/// not running> <last exit or -> <label>` per line, then `}`. A pid of 0 or less is
/// `None`. The label is the rest of the line after the two columns, so a label with a
/// space in it is whole.
pub(crate) fn listed_services(domain: &str) -> Vec<(Option<i32>, String)> {
    domain
        .lines()
        .skip_while(|line| line.trim() != "services = {")
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .filter_map(|line| {
            let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (_, label) = rest.trim_start().split_once(char::is_whitespace)?;
            let label = label.trim();
            let pid = pid.parse().ok().filter(|pid: &i32| *pid > 0);
            (!label.is_empty()).then(|| (pid, label.to_owned()))
        })
        .collect()
}

/// What one clean (or one lease end) has judged of launchd's services, by label and the
/// process it runs: whether a service's plist is launchd's own ([`is_own_plist`]). A
/// service is printed once for it, however many scans of the clean ask. A job
/// registered again under a label runs another process, so it is judged again. Scoped
/// to one clean: a later clean judges again, as a lease may have submitted a job under
/// any label in between.
#[derive(Default)]
pub(crate) struct OwnLabels {
    known: std::sync::Mutex<std::collections::HashMap<(String, Option<i32>), bool>>,
}

impl OwnLabels {
    /// Whether the service `label`, running `pid`, is launchd's own: what was judged before, else what
    /// `judge` says (called without the lock held, so a slow print blocks no other). A
    /// `judge` that could not tell (`None`: the print failed or timed out) counts as not
    /// own, and is not kept: the next ask judges again.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn is_own(
        &self,
        label: &str,
        pid: Option<i32>,
        judge: impl FnOnce() -> Option<bool>,
    ) -> bool {
        let lock = || {
            self.known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let key = (label.to_owned(), pid);
        if let Some(own) = lock().get(&key) {
            return *own;
        }
        let Some(own) = judge() else {
            return false;
        };
        lock().insert(key, own);
        own
    }
}

/// The members of `pids` that launchd lists as running one of its own agents. `list`
/// (the domain's services) is not called when `pids` is empty, and `own` (is this label
/// launchd's own, given its pid) is asked only of the services that run one of `pids`: a service with
/// no process of the uid cannot be one of them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn own_among(
    pids: &[i32],
    list: impl FnOnce() -> Vec<(Option<i32>, String)>,
    mut own: impl FnMut(i32, &str) -> bool,
) -> Vec<i32> {
    if pids.is_empty() {
        return Vec::new();
    }
    list()
        .into_iter()
        .filter_map(|(pid, label)| Some((pid.filter(|pid| pids.contains(pid))?, label)))
        .filter(|(pid, label)| own(*pid, label))
        .map(|(pid, _)| pid)
        .collect()
}

/// The pids of launchd's own agents among the processes of a uid whose process table was
/// read as `pids`: those `among` finds in the table, or, when the table could not be
/// read, all that `all` lists (the own services launchd lists as running), so that a
/// failed read never leaves an own agent out of the list.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn own_of_uid<E>(
    pids: Result<Vec<i32>, E>,
    among: impl FnOnce(&[i32]) -> Vec<i32>,
    all: impl FnOnce() -> Vec<i32>,
) -> Vec<i32> {
    match pids {
        Ok(pids) => among(&pids),
        Err(_) => all(),
    }
}

/// The mount points in `mount`'s output (`<device> on <path> (<options>)` per line)
/// that are one of `roots` or lie under one, the deepest first, so that a mount inside
/// a mount comes off before the one it is in.
pub(crate) fn mounts_under(mount: &str, roots: &[&Path]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = mount
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(" on ")?;
            let (point, _) = rest.rsplit_once(" (")?;
            Some(PathBuf::from(point))
        })
        .filter(|point| roots.iter().any(|root| point.starts_with(root)))
        .collect();
    found.sort_by_key(|point| std::cmp::Reverse(point.components().count()));
    found
}

/// The kill loop: `kill`, then `scan` the uid's processes, then `pause`, until two scans
/// in a row find none, for at most [`MAX_SCANS`] scans and until `expired` says so. A
/// deadline that passes after one quiet scan allows one more kill and scan, so a uid that
/// has just gone quiet is not failed for the second look it had no time for. The
/// first scan after a kill can be empty while a process the kill raced with starts one
/// more (a respawn, a fork in flight); the second, [`SCAN_PAUSE`] later, sees it.
///
/// # Errors
/// `kill` or `scan` failed, or processes were left at the bound (their pids).
pub(crate) fn kill_until_quiet(
    expired: &mut dyn FnMut() -> bool,
    kill: &mut dyn FnMut() -> Result<(), String>,
    scan: &mut dyn FnMut() -> Result<Vec<i32>, String>,
    pause: &mut dyn FnMut(),
) -> Result<(), String> {
    let mut quiet = 0;
    let mut left = Vec::new();
    for _ in 0..MAX_SCANS {
        kill()?;
        left = scan()?;
        if left.is_empty() {
            quiet += 1;
            if quiet == 2 {
                return Ok(());
            }
        } else {
            quiet = 0;
        }
        // After the one more scan the count is 2 (returned) or 0, so this allows only one.
        if expired() && quiet != 1 {
            break;
        }
        pause();
    }
    Err(if left.is_empty() {
        "the uid was not quiet for two scans".to_owned()
    } else {
        format!("processes {left:?} survived SIGKILL")
    })
}

#[cfg(test)]
mod tests;
