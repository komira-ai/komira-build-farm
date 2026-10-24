//! The assertion that a pool user holds nothing a lease left: what it reads, how it
//! compares, and what a red result is. The tools it reads with are a [`Reader`]'s (the
//! Mac's, [`super::macos`]); the parsers of what they print and the comparison are here,
//! tested on every platform with fixtures and a fake.
//!
//! A **snapshot** is what a [`Reader`] says of the user for each [`Probe`], as sorted
//! lines. The **baseline** is the snapshot of a user that has run one command through
//! the launch an action uses (`/usr/bin/true`) and been cleaned: a user that was never
//! launched differs from it by launchd's own per-user services and by the group of its
//! home, so the target is a used-once user, recorded at the daemon's start
//! ([`record`]).
//!
//! | probe | the user is clean when |
//! |---|---|
//! | [`Probe::Processes`] | no process has it as its real, effective or saved uid, but launchd's own |
//! | [`Probe::Domain`] | `launchctl print user/<uid>` lists no job but launchd's own |
//! | [`Probe::Ipc`] | `ipcs -a` lists no object it owns or made |
//! | [`Probe::Mounts`] | nothing is mounted or attached under its home, its `/var/folders` entry or the lease directory, no mount anywhere is marked as mounted by it, and it attached no image |
//! | [`Probe::Home`] | its home is an empty directory, mode 0700, its uid and gid, with no flags, and the ACL and extended attributes (names and values) it had in the baseline |
//! | [`Probe::Entry`] | its `/var/folders` entry is as recorded (root, and the mode and extended attributes, names and values, of each folder), with every directory empty |
//! | [`Probe::Shared`] | it owns nothing in the shared places and the crash folders |
//! | [`Probe::Schedules`] | it has no cron or `at` job |
//! | [`Probe::Record`] | its directory record and `id` equal the baseline's |
//!
//! Launchd's own services differ from run to run (a service loaded on demand, a pid), so
//! the baseline notes each with its plist path and program ([`Probe::Services`]) and
//! never compares the list. A job is launchd's own by the file its plist names
//! ([`own_plist`]: under the system folders, root's, nobody else can write it, the same
//! label), and, if the baseline noted a service of its label, only with the plist path
//! and program it noted ([`is_own_job`]): never by its label or a line of text that a
//! lease can choose. Every job is printed and judged at every look.
//!
//! Only the `user/<uid>` domain is read for jobs; a job loaded into another domain of
//! the uid (`gui/<uid>`) runs as a process of the uid while it runs, which
//! [`Probe::Processes`] sees, and is not seen while it is idle.
//!
//! A probe that cannot run is red, never green; so is a probe that needs a baseline and
//! has none.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::probe;

/// One thing the assertion reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Probe {
    Processes,
    Domain,
    Ipc,
    Mounts,
    Home,
    Entry,
    Shared,
    Schedules,
    Record,
    /// Launchd's own per-user services: recorded, never compared.
    Services,
}

/// Every probe, in the order they are read.
pub(crate) const ALL: [Probe; 10] = [
    Probe::Processes,
    Probe::Services,
    Probe::Domain,
    Probe::Ipc,
    Probe::Mounts,
    Probe::Home,
    Probe::Entry,
    Probe::Shared,
    Probe::Schedules,
    Probe::Record,
];

/// The cheap probes, run again when a slot is handed to a lease.
pub(crate) const CHEAP: [Probe; 6] = [
    Probe::Processes,
    Probe::Domain,
    Probe::Ipc,
    Probe::Mounts,
    Probe::Home,
    Probe::Entry,
];

/// How a probe's lines are judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Every line is something that must not be there.
    Absent,
    /// The lines must equal the baseline's. A baseline holds only the lines
    /// [`baseline_defects`] does not find wrong.
    Recorded,
    /// The lines are kept in the baseline and not compared.
    Noted,
}

impl Probe {
    pub(crate) const fn kind(self) -> Kind {
        match self {
            Self::Home | Self::Entry | Self::Record => Kind::Recorded,
            Self::Services => Kind::Noted,
            _ => Kind::Absent,
        }
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Processes => "processes",
            Self::Domain => "launchd",
            Self::Ipc => "ipc",
            Self::Mounts => "mounts",
            Self::Home => "home",
            Self::Entry => "entry",
            Self::Shared => "shared",
            Self::Schedules => "schedules",
            Self::Record => "record",
            Self::Services => "services",
        })
    }
}

/// What a probe read of the user: sorted, without repeats.
pub(crate) type Lines = Vec<String>;

/// What the assertion reads, for one user.
pub(crate) trait Reader {
    /// The lines of `probe`.
    ///
    /// # Errors
    /// The probe could not run, or what it printed could not be read.
    fn read(&self, probe: Probe) -> Result<Lines, String>;
}

/// What a [`Reader`] said of a user, by probe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot(BTreeMap<Probe, Lines>);

impl Snapshot {
    /// The lines of `probe`, if it was read.
    pub(crate) fn lines(&self, probe: Probe) -> Option<&Lines> {
        self.0.get(&probe)
    }
}

fn tidy(mut lines: Lines) -> Lines {
    lines.sort();
    lines.dedup();
    lines
}

/// Reads every probe of a user that has just been cleaned after its first launch: the
/// baseline. It is only a baseline if it is itself clean.
///
/// # Errors
/// What is wrong with the user (a line of each): a probe that could not run, or a
/// probe that must find nothing and found something.
pub(crate) fn record(reader: &dyn Reader) -> Result<Snapshot, Lines> {
    let mut snapshot = Snapshot::default();
    let mut wrong = Vec::new();
    for probe in ALL {
        match reader.read(probe) {
            Ok(lines) => {
                let lines = tidy(lines);
                wrong.extend(baseline_defects(probe, &lines));
                snapshot.0.insert(probe, lines);
            }
            Err(why) => wrong.push(cannot_run(probe, &why)),
        }
    }
    if wrong.is_empty() {
        Ok(snapshot)
    } else {
        Err(wrong)
    }
}

/// What a baseline of `probe` may not hold, as red lines: any line of a probe that must
/// find nothing, and for a recorded one the lines that are a wrong state and not a
/// recorded fact (the home's owner, mode, flags and names; a name in the entry that is not
/// one of its folders).
fn baseline_defects(probe: Probe, lines: &[String]) -> Lines {
    let recorded = |l: &&String| match probe {
        Probe::Home => l.starts_with("acl ") || l.starts_with("xattr "),
        Probe::Entry => l.starts_with(". "),
        _ => true,
    };
    match probe.kind() {
        Kind::Absent => lines.iter().map(|l| format!("{probe}: + {l}")).collect(),
        Kind::Recorded => lines
            .iter()
            .filter(|l| !recorded(l))
            .map(|l| format!("{probe}: + {l}"))
            .collect(),
        Kind::Noted => Vec::new(),
    }
}

/// Reads `probes` and compares them with `baseline`: the lines that differ, `probe: +
/// line` for what is there and should not be, `probe: - line` for what the baseline has
/// and is gone. Empty means the user is as the baseline says. `stopped` is asked before
/// each probe: when it says so (the lease was killed) the rest are not read, and the
/// result says that. `until` is the time the check may take in all: asked before each
/// probe (one that has begun is not cut short; the tools it runs have their own limit),
/// and a probe not begun by then is not read, which is red.
pub(crate) fn assert(
    reader: &dyn Reader,
    baseline: Option<&Snapshot>,
    probes: &[Probe],
    (stopped, until): (&dyn Fn() -> bool, Option<std::time::Instant>),
) -> Lines {
    let mut wrong = Vec::new();
    for &probe in probes {
        if stopped() {
            wrong.push(format!("{probe}: not read, the check was stopped"));
            break;
        }
        if until.is_some_and(|t| std::time::Instant::now() >= t) {
            wrong.push(format!("{probe}: not read, the check ran out of time"));
            break;
        }
        let now = match reader.read(probe) {
            Ok(lines) => tidy(lines),
            Err(why) => {
                wrong.push(cannot_run(probe, &why));
                continue;
            }
        };
        let before = baseline.and_then(|b| b.lines(probe));
        if probe.kind() == Kind::Recorded && before.is_none() {
            wrong.push(format!("{probe}: no baseline to compare with"));
            continue;
        }
        wrong.extend(differences(probe, &now, before.map(Vec::as_slice)));
    }
    wrong
}

fn cannot_run(probe: Probe, why: &str) -> String {
    format!("{probe}: cannot run, so it cannot say the user is clean: {why}")
}

/// How `now` differs from `before` for `probe` (see [`Kind`]).
pub(crate) fn differences(probe: Probe, now: &[String], before: Option<&[String]>) -> Lines {
    match probe.kind() {
        Kind::Noted => Vec::new(),
        Kind::Absent => now.iter().map(|l| format!("{probe}: + {l}")).collect(),
        Kind::Recorded => {
            let before = before.unwrap_or_default();
            let added = now.iter().filter(|l| !before.contains(l));
            let gone = before.iter().filter(|l| !now.contains(l));
            added
                .map(|l| format!("{probe}: + {l}"))
                .chain(gone.map(|l| format!("{probe}: - {l}")))
                .collect()
        }
    }
}

/// One process of `ps -axo pid=,ruid=,uid=,svuid=,stat=,comm=`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: i32,
    pub real: u32,
    pub effective: u32,
    pub saved: u32,
    pub zombie: bool,
    pub command: String,
}

/// The processes `ps` printed.
///
/// # Errors
/// A line is not `pid ruid uid svuid stat command`, or there is none (a table always
/// has launchd).
pub(crate) fn parse_ps(text: &str) -> Result<Vec<Proc>, String> {
    let mut procs = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut words = line.split_whitespace();
        let mut number = |what: &str| -> Result<u32, String> {
            let word = words
                .next()
                .ok_or_else(|| format!("ps: no {what}: {line:?}"))?;
            word.parse()
                .map_err(|_| format!("ps: {what} {word:?} is not a number: {line:?}"))
        };
        let pid = number("pid")?;
        let (real, effective, saved) = (number("ruid")?, number("uid")?, number("svuid")?);
        let stat = words
            .next()
            .ok_or_else(|| format!("ps: no state: {line:?}"))?;
        let command = words.collect::<Vec<_>>().join(" ");
        procs.push(Proc {
            pid: i32::try_from(pid).map_err(|_| format!("ps: pid {pid} too large"))?,
            real,
            effective,
            saved,
            zombie: stat.starts_with('Z'),
            command,
        });
    }
    if procs.is_empty() {
        return Err("ps listed no process".to_owned());
    }
    Ok(procs)
}

/// The live processes with `uid` as real, effective or saved uid, other than the pids in
/// `own` (launchd's own agents): `pid command` each.
pub(crate) fn uid_processes(procs: &[Proc], uid: u32, own: &[i32]) -> Lines {
    let ours = |p: &&Proc| p.real == uid || p.effective == uid || p.saved == uid;
    procs
        .iter()
        .filter(ours)
        .filter(|p| !p.zombie && !own.contains(&p.pid))
        .map(|p| format!("{} {}", p.pid, p.command))
        .collect()
}

/// Whether the plist the job `label` says it came from is launchd's own, judged on the
/// file and not on a line of text that a job can write (a submitted job has no plist, and
/// an argument can carry a line that looks like the plist line): the real path (links and
/// `..` resolved) lies under one of `roots`, `owner` owns the file and no one else can
/// write it, and its `Label` is the job's.
pub(crate) fn own_plist(path: &str, label: &str, roots: &[&str], owner: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    // The real path, and its metadata (a file that goes between the two is not launchd's).
    let Some((real, meta)) = std::fs::canonicalize(path)
        .ok()
        .and_then(|real| std::fs::metadata(&real).ok().map(|meta| (real, meta)))
    else {
        return false;
    };
    let under = |root: &&str| real.to_string_lossy().starts_with(*root);
    if !roots.iter().any(under) || meta.uid() != owner || meta.mode() & 0o022 != 0 {
        return false;
    }
    plist::Value::from_file(&real)
        .ok()
        .and_then(|v| {
            v.as_dictionary()?
                .get("Label")?
                .as_string()
                .map(|l| l == label)
        })
        .unwrap_or(false)
}

/// One service of the uid's launchd domain, as the assertion needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Job {
    pub pid: Option<i32>,
    pub label: String,
    /// The plist and the program `launchctl print` says it has.
    pub plist: Option<String>,
    pub program: Option<String>,
    /// Its plist is launchd's own ([`is_own_job`]).
    pub own: bool,
}

/// The line the baseline keeps of a service: `label`, plist path and program, apart by
/// tabs (a missing plist or program is empty).
pub(crate) fn service_line(label: &str, plist: Option<&str>, program: Option<&str>) -> String {
    format!(
        "{label}\t{}\t{}",
        plist.unwrap_or_default(),
        program.unwrap_or_default()
    )
}

/// Whether a job is launchd's own: it has a plist, `on_file` (the check of the file,
/// [`own_plist`]) accepts it for the job's label, and if the baseline (`known`, the lines
/// of [`service_line`]) noted a service of that label, the plist path and the program are
/// the ones it noted. A label alone never makes a job launchd's own: a lease that boots
/// out a service and submits its own job under the label has a different plist (none) or
/// program.
pub(crate) fn is_own_job(
    (label, plist, program): (&str, Option<&str>, Option<&str>),
    known: &[String],
    on_file: &dyn Fn(&str) -> bool,
) -> bool {
    let Some(path) = plist else {
        return false;
    };
    let prefix = format!("{label}\t");
    let mut noted = known.iter().filter(|k| k.starts_with(&prefix)).peekable();
    if noted.peek().is_some() {
        let now = service_line(label, plist, program);
        if !noted.any(|k| *k == now) {
            return false;
        }
    }
    on_file(path)
}

/// The pids of launchd's own running services.
pub(crate) fn own_pids(jobs: &[Job]) -> Vec<i32> {
    jobs.iter()
        .filter(|j| j.own)
        .filter_map(|j| j.pid)
        .collect()
}

/// The jobs that are not launchd's own: `job <label>` each, with the pid while running.
pub(crate) fn foreign_jobs(jobs: &[Job]) -> Lines {
    jobs.iter()
        .filter(|j| !j.own)
        .map(|j| match j.pid {
            Some(pid) => format!("job {} (pid {pid})", j.label),
            None => format!("job {}", j.label),
        })
        .collect()
}

/// The label, plist path and program of launchd's own services: what the baseline notes.
pub(crate) fn own_services(jobs: &[Job]) -> Lines {
    jobs.iter()
        .filter(|j| j.own)
        .map(|j| service_line(&j.label, j.plist.as_deref(), j.program.as_deref()))
        .collect()
}

/// A System V IPC object `ipcs -a` listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IpcRow {
    /// `q`, `m` or `s`.
    pub kind: char,
    pub id: String,
}

/// The objects of `ipcs -a` (`T ID KEY MODE OWNER GROUP CREATOR CGROUP ...` per line)
/// that `name` or `uid` (for a name the user database does not give) owns or created.
pub(crate) fn ipc_rows(text: &str, name: &str, uid: u32) -> Vec<IpcRow> {
    let uid = uid.to_string();
    let ours = |who: Option<&str>| who.is_some_and(|w| w == name || w == uid);
    let mut rows = Vec::new();
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let kind = match words.first() {
            Some(&"q") => 'q',
            Some(&"m") => 'm',
            Some(&"s") => 's',
            _ => continue,
        };
        let Some(id) = words.get(1).filter(|id| id.parse::<i64>().is_ok()) else {
            continue;
        };
        if ours(words.get(4).copied()) || ours(words.get(6).copied()) {
            rows.push(IpcRow {
                kind,
                id: (*id).to_owned(),
            });
        }
    }
    rows
}

/// A disk image `hdiutil info -plist` listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Image {
    /// The whole device (`/dev/diskN`).
    pub device: Option<String>,
    /// The uid that attached it.
    pub owner: Option<u64>,
    pub path: Option<String>,
    /// Where its volumes are mounted.
    pub mounts: Vec<String>,
}

/// The images in `hdiutil info -plist`'s output.
///
/// # Errors
/// It is not a property list.
pub(crate) fn hdiutil_images(xml: &[u8]) -> Result<Vec<Image>, String> {
    let info = plist::Value::from_reader_xml(xml).map_err(|e| format!("hdiutil info: {e}"))?;
    let images = info
        .as_dictionary()
        .and_then(|d| d.get("images"))
        .and_then(plist::Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut found = Vec::new();
    for image in images.iter().filter_map(plist::Value::as_dictionary) {
        let entities: Vec<&plist::Dictionary> = image
            .get("system-entities")
            .and_then(plist::Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter_map(plist::Value::as_dictionary)
            .collect();
        let text = |d: &plist::Dictionary, key: &str| {
            d.get(key)
                .and_then(plist::Value::as_string)
                .map(str::to_owned)
        };
        found.push(Image {
            device: entities
                .iter()
                .filter_map(|e| text(e, "dev-entry"))
                .min_by_key(String::len),
            owner: image
                .get("owner-uid")
                .and_then(plist::Value::as_unsigned_integer),
            path: text(image, "image-path"),
            mounts: entities
                .iter()
                .filter_map(|e| text(e, "mount-point"))
                .collect(),
        });
    }
    Ok(found)
}

/// The mount points in `mount`'s output whose options say `mounted by <name>`, which
/// macOS prints for a filesystem a user mounted (an nfs or smb share) wherever it is.
pub(crate) fn mounted_by(mount: &str, name: &str) -> Vec<PathBuf> {
    let marker = format!("mounted by {name}");
    mount
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(" on ")?;
            let (point, options) = rest.rsplit_once(" (")?;
            let options = options.trim_end().trim_end_matches(')');
            options
                .split(',')
                .any(|o| o.trim() == marker)
                .then(|| PathBuf::from(point))
        })
        .collect()
}

/// What is mounted or attached that the user's state must not hold: mounts in `mount`'s
/// output at or under one of `roots`, mounts marked as mounted by `name` anywhere, images
/// attached by `uid`, and images that lie at or mount under one of `roots`.
pub(crate) fn mount_lines(
    mount: &str,
    images: &[Image],
    (uid, name): (u32, &str),
    roots: &[&Path],
) -> Lines {
    let under_roots = probe::mounts_under(mount, roots);
    let mut lines: Lines = under_roots
        .iter()
        .map(|p| format!("mounted {}", p.display()))
        .collect();
    lines.extend(
        mounted_by(mount, name)
            .iter()
            .filter(|p| !under_roots.contains(p))
            .map(|p| format!("mounted {} by the user", p.display())),
    );
    let under = |p: &str| roots.iter().any(|r| Path::new(p).starts_with(r));
    for image in images {
        let device = image.device.as_deref().unwrap_or("?");
        if image.owner == Some(u64::from(uid)) {
            lines.push(format!("image {device} attached by the uid"));
        } else if image.path.as_deref().is_some_and(under) || image.mounts.iter().any(|m| under(m))
        {
            lines.push(format!("image {device} under the user's folders"));
        }
    }
    lines
}

/// The ACL entries `ls -led <path>` printed: the lines ` 0: user:... allow ...` after the
/// first.
pub(crate) fn acl_entries(ls: &str) -> Lines {
    ls.lines()
        .skip(1)
        .filter(|line| {
            let line = line.trim_start();
            let (index, rest) = line.split_once(": ").unwrap_or(("", ""));
            !rest.is_empty() && index.chars().all(|c| c.is_ascii_digit()) && !index.is_empty()
        })
        .map(|l| l.trim().to_owned())
        .collect()
}

/// What is read of a directory the user must find empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub uid: u32,
    pub gid: u32,
    /// The permission bits (`0o700`).
    pub mode: u32,
    /// `st_flags`.
    pub flags: u32,
    pub acl: Lines,
    /// `name=value` (the value in hex) each.
    pub xattrs: Lines,
    /// The names in it.
    pub entries: Lines,
}

/// The line for an extended attribute: `name=value`, the value in hex, so that a lease
/// that changes a value and keeps the name is a difference.
pub(crate) fn xattr_line(name: &str, value: &[u8]) -> String {
    let hex: String = value.iter().map(|b| format!("{b:02x}")).collect();
    format!("{name}={hex}")
}

/// How the home `facts` differ from an empty directory of `uid`:`gid`, mode 0700, with no
/// flags. Its ACL and extended attributes are lines too (`acl ...`, `xattr ...`): a
/// home made by the system's tools can carry some, which the baseline then records.
pub(crate) fn home_lines(facts: &Facts, (uid, gid): (u32, u32)) -> Lines {
    let mut lines = Vec::new();
    if (facts.uid, facts.gid) != (uid, gid) {
        lines.push(format!(
            "owned by {}:{}, not {uid}:{gid}",
            facts.uid, facts.gid
        ));
    }
    if facts.mode != 0o700 {
        lines.push(format!("mode {:o}, not 700", facts.mode));
    }
    if facts.flags != 0 {
        lines.push(format!("flags {:#x}", facts.flags));
    }
    lines.extend(facts.acl.iter().map(|a| format!("acl {a}")));
    lines.extend(facts.xattrs.iter().map(|x| format!("xattr {x}")));
    lines.extend(facts.entries.iter().map(|e| format!("entry {e}")));
    lines
}

/// The lines of the `/var/folders` entry: its root as found (recorded), the mode and
/// extended attributes of each folder it keeps (recorded), and everything
/// below it that is not an empty folder the user owns (`skeleton`: the names the clean
/// keeps, emptied; the others must not exist).
///
/// `root` is the entry's root, `children` its names with what is read of each (a folder
/// or not, and the same facts; `entries` for a folder).
pub(crate) fn entry_lines(
    root: &Facts,
    children: &[(String, bool, Facts)],
    skeleton: &[&str],
    uid: u32,
) -> Lines {
    let mut lines = vec![format!(
        ". owned by {}:{} mode {:o} flags {:#x}",
        root.uid, root.gid, root.mode, root.flags
    )];
    lines.extend(root.acl.iter().map(|a| format!(". acl {a}")));
    lines.extend(root.xattrs.iter().map(|x| format!(". xattr {x}")));
    for (name, is_dir, facts) in children {
        let kept = skeleton.contains(&name.as_str()) && *is_dir;
        if !kept {
            lines.push(format!("{name}: not part of the entry"));
            continue;
        }
        // What a folder is when the entry is clean is recorded with the root.
        lines.push(format!(". {name} mode {:o}", facts.mode));
        lines.extend(facts.xattrs.iter().map(|x| format!(". {name} xattr {x}")));
        if facts.uid != uid {
            lines.push(format!("{name}: owned by {}", facts.uid));
        }
        // The flags and ACLs of a folder are the system's (root without Full Disk Access
        // cannot change them, nor the user): recorded like the mode, not asked to be none.
        lines.push(format!(". {name} flags {:#x}", facts.flags));
        lines.extend(facts.acl.iter().map(|a| format!(". {name} acl {a}")));
        lines.extend(facts.entries.iter().map(|e| format!("{name}: entry {e}")));
    }
    lines
}

/// `Attribute: value` pairs of `dscl . -read <record> <attributes>`, as `attribute=value`
/// lines in the order of `wanted`; an attribute not printed is `attribute=` (missing is a
/// difference too). A value on the line after `Attribute:` is read as well.
pub(crate) fn dscl_lines(text: &str, wanted: &[&str]) -> Lines {
    let mut found: BTreeMap<&str, String> = BTreeMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if line.starts_with(' ') || !wanted.contains(&key) {
            continue;
        }
        let mut value = value.trim().to_owned();
        if value.is_empty()
            && let Some(next) = lines.next_if(|l| l.starts_with(' '))
        {
            value = next.trim().to_owned();
        }
        found.insert(key, value);
    }
    wanted
        .iter()
        .map(|k| format!("{k}={}", found.get(k).map_or("", String::as_str)))
        .collect()
}

#[cfg(test)]
mod tests;
