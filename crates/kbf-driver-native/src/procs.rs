//! An action's processes: which processes on the node belong to it, how much memory
//! they hold, and how they are all ended.
//!
//! The action's first process leads a new process group. A process belongs to the
//! action when it is in that group (and no older than its leader, so a recycled group
//! id is never taken for it), when it descends from a process that belongs, or when it
//! was seen belonging earlier and is still the same process (pid and start time): a
//! process that leaves the group (`setsid`) and is orphaned (its parent exits, so it
//! is reparented to `launchd` or `init`) is still the action's once a poll has seen it.
//! One that does both between two polls is not seen; a per-lease user would close
//! that gap, and is a follow-up.
//!
//! The pure part, [`Tracker`] and the text parsers, is tested on every platform. The
//! process table and per-process memory come from `/proc` on Linux and from `libproc`
//! on macOS ([`snapshot`], [`footprint`]).

use std::collections::{BTreeMap, BTreeSet};

/// One process in a snapshot of the process table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// When the process started, in a platform unit that only grows; with `pid` it
    /// names one process for its whole life.
    pub start: u64,
    /// Exited but not yet reaped: holds no memory and cannot be signalled further.
    pub zombie: bool,
}

/// The processes of one action, followed across snapshots.
#[derive(Debug)]
pub struct Tracker {
    /// The leader's pid, which is also the group's id.
    leader: i32,
    /// The leader's start time, once a snapshot has shown it.
    leader_start: Option<u64>,
    /// Every process seen belonging, by pid, with its start time.
    known: BTreeMap<i32, u64>,
    /// Whether a snapshot has shown the group empty. An empty group's id can be
    /// recycled, so from then on membership is by descent and memory only.
    group_gone: bool,
    /// This process: never the action's, whatever a snapshot says.
    me: i32,
}

impl Tracker {
    /// A tracker for the action whose leader is `leader` (the pid of a process spawned
    /// into a new process group). `me` is the daemon's own pid.
    #[must_use]
    pub fn new(leader: i32, me: i32) -> Self {
        Self {
            leader,
            leader_start: None,
            known: BTreeMap::new(),
            group_gone: false,
            me,
        }
    }

    /// A tracker for an action an earlier daemon started, known only by its run record:
    /// the leader's pid and its start time ([`crate::record`]).
    #[must_use]
    pub fn resume(leader: i32, leader_start: u64, me: i32) -> Self {
        Self {
            leader_start: Some(leader_start),
            ..Self::new(leader, me)
        }
    }

    /// The leader's pid and process group id.
    #[must_use]
    pub fn leader(&self) -> i32 {
        self.leader
    }

    /// The live processes of the action in `snapshot`, sorted by pid; every one of
    /// them, and every zombie of the action, is remembered for later snapshots.
    pub fn members(&mut self, snapshot: &[Proc]) -> Vec<Proc> {
        if self.leader_start.is_none() {
            self.leader_start = snapshot
                .iter()
                .find(|p| p.pid == self.leader)
                .map(|p| p.start);
        }
        let floor = self.leader_start.unwrap_or(0);
        // Another process under the leader's pid: the kernel hands out no pid that
        // still names a process group, so the action's group emptied first, and any
        // group of that id now is someone else's.
        if snapshot
            .iter()
            .any(|p| p.pid == self.leader && p.start != floor)
        {
            self.group_gone = true;
        }
        let mut children: BTreeMap<i32, Vec<&Proc>> = BTreeMap::new();
        for p in snapshot {
            children.entry(p.ppid).or_default().push(p);
        }
        let (gone, leader) = (self.group_gone, self.leader);
        let in_group = |p: &&Proc| !gone && p.pgid == leader && p.start >= floor;
        if !snapshot.iter().any(|p| in_group(&p)) {
            self.group_gone = true;
        }
        let mut pending: Vec<&Proc> = snapshot
            .iter()
            .filter(|p| in_group(p) || self.known.get(&p.pid) == Some(&p.start))
            .collect();
        let mut found: BTreeMap<i32, Proc> = BTreeMap::new();
        while let Some(p) = pending.pop() {
            if p.pid == self.me || p.pid <= 1 || found.insert(p.pid, *p).is_some() {
                continue;
            }
            if let Some(kids) = children.get(&p.pid) {
                pending.extend(kids.iter().copied());
            }
        }
        // Forget processes that are gone, so the map stays as small as the action.
        let alive: BTreeSet<(i32, u64)> = snapshot.iter().map(|p| (p.pid, p.start)).collect();
        self.known
            .retain(|pid, start| alive.contains(&(*pid, *start)));
        self.known.extend(found.values().map(|p| (p.pid, p.start)));
        found.into_values().filter(|p| !p.zombie).collect()
    }
}

/// The fields of a Linux `/proc/<pid>/stat` line this module reads, or `None` when it
/// does not parse. The command name is in parentheses and may hold anything, so the
/// fields are counted from the last `)`.
#[must_use]
pub fn parse_linux_stat(text: &str) -> Option<Proc> {
    let (head, rest) = text.rsplit_once(')')?;
    let pid = head.split_once(" (")?.0.trim().parse().ok()?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state (3), ppid (4), pgrp (5), ... starttime (22).
    let state = *fields.first()?;
    Some(Proc {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        pgid: fields.get(2)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
        zombie: matches!(state, "Z" | "X"),
    })
}

/// The memory a Linux process holds that it alone pays for, from its
/// `/proc/<pid>/status`: anonymous and shared-memory pages resident, plus what is
/// swapped out (`RssAnon + RssShmem + VmSwap`), in bytes. The nearest Linux measure
/// to macOS's physical footprint. `None` without those lines (a kernel thread, a
/// zombie).
#[must_use]
pub fn parse_linux_status(text: &str) -> Option<u64> {
    let kib = |key: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?.strip_prefix(':')?;
            rest.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
        })
    };
    let anon = kib("RssAnon")?;
    let total = anon + kib("RssShmem").unwrap_or(0) + kib("VmSwap").unwrap_or(0);
    Some(total * 1024)
}

pub use platform::{footprint, process, snapshot};

#[cfg(target_os = "linux")]
mod platform {
    use super::{Proc, parse_linux_stat, parse_linux_status};

    /// Every process in `/proc` that could be read. One that exits while the table
    /// is read is left out.
    pub fn snapshot() -> std::io::Result<Vec<Proc>> {
        let mut procs = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let name = entry?.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            if let Some(p) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .as_deref()
                .and_then(parse_linux_stat)
            {
                procs.push(p);
            }
        }
        Ok(procs)
    }

    /// Process `pid`, as a snapshot would show it; `None` once it is gone.
    #[must_use]
    pub fn process(pid: i32) -> Option<Proc> {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .as_deref()
            .and_then(parse_linux_stat)
    }

    /// What `pid` holds, in bytes; `None` once it is gone.
    #[must_use]
    pub fn footprint(pid: i32) -> Option<u64> {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .as_deref()
            .and_then(parse_linux_status)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Proc;

    /// `pbi_status` of a process that has exited and waits to be reaped.
    const SZOMB: u32 = 5;

    /// Every process `libproc` lists whose BSD info could be read. One that exits
    /// while the table is read is left out.
    pub fn snapshot() -> std::io::Result<Vec<Proc>> {
        // Room for more pids than the kernel reports, so a few new ones still fit.
        // SAFETY: a null buffer asks only for the count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut pids = vec![0 as libc::pid_t; count.unsigned_abs() as usize + 64];
        let bytes =
            i32::try_from(std::mem::size_of_val(pids.as_slice())).map_err(std::io::Error::other)?;
        // SAFETY: the buffer holds `bytes` bytes of pids and outlives the call.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        pids.truncate(n.unsigned_abs() as usize);
        Ok(pids.into_iter().filter_map(process).collect())
    }

    /// Process `pid`, as a snapshot would show it; `None` once it is gone.
    #[must_use]
    pub fn process(pid: libc::pid_t) -> Option<Proc> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes; the call writes
        // at most that many and returns how many it wrote.
        let wrote = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if wrote != size {
            return None;
        }
        // SAFETY: the call filled the whole struct (checked above).
        let info = unsafe { info.assume_init() };
        Some(Proc {
            pid,
            ppid: i32::try_from(info.pbi_ppid).ok()?,
            pgid: i32::try_from(info.pbi_pgid).ok()?,
            start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
            zombie: info.pbi_status == SZOMB,
        })
    }

    /// The physical footprint of `pid` (what Activity Monitor calls its memory), in
    /// bytes; `None` once it is gone.
    #[must_use]
    pub fn footprint(pid: i32) -> Option<u64> {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        // SAFETY: `info` is a writable rusage_info_v2, the struct RUSAGE_INFO_V2
        // fills; the pointer outlives the call.
        let done =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast()) };
        if done != 0 {
            return None;
        }
        // SAFETY: the call succeeded, so it filled the struct.
        Some(unsafe { info.assume_init() }.ri_phys_footprint)
    }
}

/// Sends SIGKILL to the action's process group, unless a snapshot has shown it empty,
/// and to each of `members`. A process that is already gone is not an error.
///
/// Known race: a pid (or the group id) is checked in a snapshot and signalled a moment
/// later by number. A member that exits in between and whose pid the kernel hands to
/// a new process of the same user gets that process killed instead; the daemon's user
/// only, since `kill` cannot reach another user's processes. Closing it needs a
/// handle that cannot be recycled (`pidfd_send_signal` on Linux; macOS has none for
/// arbitrary processes) or a per-lease user whose every process may be killed. The
/// window is the time between the snapshot and the signal.
pub fn kill_all(tracker: &Tracker, members: &[Proc]) {
    if !tracker.group_gone {
        // SAFETY: kill(2) takes plain integers. The group id is the leader's pid, and
        // the last snapshot showed the group still had members.
        unsafe {
            libc::kill(-tracker.leader(), libc::SIGKILL);
        }
    }
    for p in members {
        // SAFETY: as above; `p` was in the snapshot just taken, pid and start time.
        unsafe {
            libc::kill(p.pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, pgid: i32, start: u64) -> Proc {
        Proc {
            pid,
            ppid,
            pgid,
            start,
            zombie: false,
        }
    }

    fn pids(procs: &[Proc]) -> Vec<i32> {
        procs.iter().map(|p| p.pid).collect()
    }

    /// Catches: a child that left the group (`setsid`) not followed, a grandchild
    /// reparented to init once seen not followed, a recycled pid or group id taken for
    /// the action's, the daemon itself or init counted, and zombies counted as live.
    #[test]
    fn the_tree_is_the_group_its_descendants_and_what_was_seen() {
        let me = 10;
        let mut t = Tracker::new(100, me);
        assert_eq!(t.leader(), 100);
        let first = [
            p(1, 0, 1, 0),
            p(me, 1, me, 5),
            p(100, me, 100, 50),
            p(101, 100, 100, 51),
            // Left the group with setsid, still the leader's child.
            p(102, 100, 102, 52),
            p(103, 102, 102, 53),
            // Unrelated, older, in a group of its own.
            p(200, 1, 200, 1),
        ];
        assert_eq!(pids(&t.members(&first)), [100, 101, 102, 103]);
        // The leader and 102 exit; 103 is reparented to init; 104 is a fresh process
        // that happens to reuse pid 101 (another start time) outside the tree.
        let later = [
            p(1, 0, 1, 0),
            p(me, 1, me, 5),
            p(101, 1, 101, 90),
            p(103, 1, 102, 53),
            p(105, 103, 102, 91),
            Proc {
                zombie: true,
                ..p(106, 103, 102, 92)
            },
            p(200, 1, 200, 1),
        ];
        assert_eq!(pids(&t.members(&later)), [103, 105]);
        // A group id recycled by an older process is not the action's group.
        let mut t = Tracker::new(300, me);
        let recycled = [p(300, me, 300, 70), p(301, 1, 300, 60)];
        assert_eq!(pids(&t.members(&recycled)), [300]);
        // Before the leader is seen, the group is taken at its word; the daemon and
        // init never belong, whatever group they claim.
        let mut t = Tracker::new(400, me);
        let odd = [p(401, 1, 400, 3), p(me, 1, 400, 5), p(1, 0, 400, 0)];
        assert_eq!(pids(&t.members(&odd)), [401]);
        // Once the group has been seen empty, a process that later claims its id is
        // not taken for the action's.
        assert!(t.members(&[p(1, 0, 1, 0)]).is_empty());
        assert!(t.group_gone);
        assert!(t.members(&[p(400, 1, 400, 99)]).is_empty());
    }

    /// Catches: a stat line misread when the command name holds spaces or a `)`, the
    /// wrong field taken for ppid, pgrp or start time, or a zombie read as live.
    #[test]
    fn linux_stat_lines_parse_from_the_last_parenthesis() {
        let line = "4242 (we) ird (name) S 4241 4240 4240 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(
            parse_linux_stat(line),
            Some(Proc {
                pid: 4242,
                ppid: 4241,
                pgid: 4240,
                start: 987_654,
                zombie: false
            })
        );
        let zombie = "7 (z) Z 1 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55 0 0";
        assert!(parse_linux_stat(zombie).expect("zombie").zombie);
        for bad in [
            "",
            "7 (z) Z",
            "x (z) S 1 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55",
            "7 z S 1",
        ] {
            assert_eq!(parse_linux_stat(bad), None, "{bad:?}");
        }
    }

    /// Catches: memory read from VmRSS (file pages the action does not pay for) or in
    /// kB instead of bytes, swap left out, or a process without RssAnon counted as 0.
    #[test]
    fn linux_status_counts_anonymous_shared_and_swapped_memory() {
        let status = "Name:\tperl\nVmRSS:\t  9000 kB\nRssAnon:\t  100 kB\nRssFile:\t 8000 kB\nRssShmem:\t 20 kB\nVmSwap:\t 3 kB\n";
        assert_eq!(parse_linux_status(status), Some(123 * 1024));
        assert_eq!(parse_linux_status("RssAnon:\t5 kB\n"), Some(5 * 1024));
        assert_eq!(parse_linux_status("Name:\tkthreadd\n"), None);
    }

    /// Catches: a snapshot that misses this very process or reads its parent wrong,
    /// and a footprint that is zero or missing for a live process.
    #[test]
    fn this_process_is_in_the_snapshot_with_a_footprint() {
        let me = i32::try_from(std::process::id()).expect("pid");
        let snapshot = snapshot().expect("snapshot");
        let mine = snapshot.iter().find(|p| p.pid == me).expect("this process");
        // SAFETY: getppid takes nothing and cannot fail.
        assert_eq!(mine.ppid, unsafe { libc::getppid() });
        assert!(!mine.zombie);
        assert!(footprint(me).expect("footprint") > 0);
        assert_eq!(footprint(-5), None);
    }
}
