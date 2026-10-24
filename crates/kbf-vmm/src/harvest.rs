//! `image harvest`: copy a fixed allowlist of files out of a stopped guest's Data volume.
//!
//! The volume belongs to a guest that ran code we did not write, and the copy runs as
//! root, so nothing here follows a link the guest could have planted and nothing outside
//! the allowlist is read. Every ancestor of an allowlisted path is checked to be a real
//! directory, every file is opened without following a link and must be a regular file,
//! and the number of entries, the depth, each file and the total are capped. What is
//! refused or too large is listed in the manifest, not dropped silently.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

use serde_json::{Value, json};

/// One allowlisted source, relative to the Data volume's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// One regular file.
    File(&'static str),
    /// The regular files directly in `dir` whose names start with `prefix` and end with
    /// `suffix`.
    Matching {
        dir: &'static str,
        prefix: &'static str,
        suffix: &'static str,
    },
    /// A directory and everything below it (regular files and directories only).
    Tree(&'static str),
}

/// Everything harvest copies. Adding to this list is the only way to read more.
pub const ALLOWLIST: [Item; 6] = [
    Item::File("private/var/log/kbf-probe-local"),
    Item::Matching {
        dir: "private/var/log",
        prefix: "com.kbf.probe.",
        suffix: ".log",
    },
    Item::Tree("private/var/db/diagnostics"),
    Item::Tree("private/var/db/uuidtext"),
    Item::Tree("private/var/db/timesync"),
    Item::Tree("Library/Logs/DiagnosticReports"),
];

/// Paths whose existence and times are recorded without copying them.
pub const STATS: [&str; 2] = ["private/var/db/.AppleSetupDone", "private/var/log"];

/// The caps on one harvest.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_entries: usize,
    pub max_depth: usize,
}

impl Limits {
    /// Sized for a unified-log archive of a short boot: a tracev3 file is a few MiB.
    pub const DEFAULT: Self = Self {
        max_file_bytes: 256 << 20,
        max_total_bytes: 2 << 30,
        max_entries: 50_000,
        max_depth: 8,
    };
}

/// What the walk has done so far.
struct Run<'a> {
    volume: &'a Path,
    out: &'a Path,
    limits: &'a Limits,
    copied: Vec<Value>,
    skipped: Vec<Value>,
    entries: usize,
    total: u64,
}

impl Run<'_> {
    fn skip(&mut self, rel: &Path, why: impl Into<String>) {
        self.skipped
            .push(json!({"path": rel.display().to_string(), "reason": why.into()}));
    }

    /// Whether another entry may be looked at; records the refusal once it is over.
    fn count_entry(&mut self, rel: &Path) -> bool {
        self.entries += 1;
        if self.entries > self.limits.max_entries {
            if self.entries == self.limits.max_entries + 1 {
                self.skip(rel, "entry limit reached; the rest is not looked at");
            }
            return false;
        }
        true
    }

    /// Copies the regular file at `rel` to the same path under the output directory.
    fn copy_file(&mut self, rel: &Path) {
        let src = self.volume.join(rel);
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&src);
        let file = match opened {
            Ok(f) => f,
            Err(e) => return self.skip(rel, format!("open: {e}")),
        };
        let Some(meta) = file.metadata().ok().filter(|m| m.is_file()) else {
            return self.skip(rel, "not a regular file");
        };
        let (size, mtime) = (meta.len(), meta.mtime());
        if size > self.limits.max_file_bytes {
            return self.skip(
                rel,
                format!("larger than {} bytes", self.limits.max_file_bytes),
            );
        }
        if self.total + size > self.limits.max_total_bytes {
            return self.skip(
                rel,
                format!(
                    "total limit of {} bytes reached",
                    self.limits.max_total_bytes
                ),
            );
        }
        let dst = self.out.join(rel);
        let written = dst
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let mut to = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .open(&dst)?;
                // The file may grow while it is read: never copy more than was checked.
                std::io::copy(&mut file.take(size), &mut to)
            });
        match written {
            Ok(n) => {
                self.total += n;
                self.copied.push(
                    json!({"path": rel.display().to_string(), "bytes": n, "mtime_unix": mtime}),
                );
            }
            Err(e) => self.skip(rel, format!("copy: {e}")),
        }
    }

    /// Looks at the entry `rel` without following a link, and copies or descends.
    fn visit(&mut self, rel: &Path, depth: usize) {
        if !self.count_entry(rel) {
            return;
        }
        let meta = match std::fs::symlink_metadata(self.volume.join(rel)) {
            Ok(m) => m,
            Err(e) => return self.skip(rel, format!("stat: {e}")),
        };
        if meta.file_type().is_symlink() {
            self.skip(rel, "symbolic link, not followed");
        } else if meta.is_file() {
            self.copy_file(rel);
        } else if meta.is_dir() {
            if depth >= self.limits.max_depth {
                self.skip(rel, format!("deeper than {} levels", self.limits.max_depth));
                return;
            }
            self.descend(rel, depth);
        } else {
            self.skip(rel, "not a regular file or directory");
        }
    }

    fn descend(&mut self, rel: &Path, depth: usize) {
        let names = match list(&self.volume.join(rel)) {
            Ok(n) => n,
            Err(e) => return self.skip(rel, format!("read directory: {e}")),
        };
        for name in names {
            self.visit(&rel.join(name), depth + 1);
        }
    }
}

/// The names in `dir`, sorted.
fn list(dir: &Path) -> std::io::Result<Vec<std::ffi::OsString>> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    names.sort();
    Ok(names)
}

/// Whether every directory on the way to `rel` (not `rel` itself) is a real directory of
/// `volume`, and `rel` has only plain components.
fn ancestors_are_real(volume: &Path, rel: &Path) -> Result<(), String> {
    let mut at = volume.to_owned();
    let parts: Vec<Component<'_>> = rel.components().collect();
    for (i, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err("not a plain relative path".to_owned());
        };
        if i + 1 == parts.len() {
            break;
        }
        at.push(name);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a symbolic link", rel_of(volume, &at)));
            }
            Ok(_) => return Err(format!("{} is not a directory", rel_of(volume, &at))),
            // Nothing there: nothing to follow, and the path itself will be absent.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", rel_of(volume, &at))),
        }
    }
    Ok(())
}

fn rel_of(volume: &Path, p: &Path) -> String {
    p.strip_prefix(volume).unwrap_or(p).display().to_string()
}

fn stat_json(volume: &Path, rel: &str) -> Value {
    if let Err(e) = ancestors_are_real(volume, Path::new(rel)) {
        return json!({"path": rel, "present": false, "reason": e});
    }
    match std::fs::symlink_metadata(volume.join(rel)) {
        Ok(m) => json!({
            "path": rel,
            "present": true,
            "kind": if m.file_type().is_symlink() { "symlink" } else if m.is_dir() { "directory" } else { "file" },
            "mtime_unix": m.mtime(),
            "ctime_unix": m.ctime(),
            "bytes": m.len(),
            "uid": m.uid(),
            "gid": m.gid(),
            "mode": format!("{:o}", m.mode() & 0o7777),
        }),
        Err(e) => json!({"path": rel, "present": false, "reason": e.to_string()}),
    }
}

/// Copies the allowlist from the volume mounted at `volume` into the new directory
/// `out` (which must not exist), and returns the manifest.
///
/// # Errors
/// `out` exists or cannot be made.
pub fn copy_allowlist(volume: &Path, out: &Path, limits: &Limits) -> Result<Value, String> {
    out.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .map_err(|e| format!("create the parent of {}: {e}", out.display()))?;
    std::fs::create_dir(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let mut run = Run {
        volume,
        out,
        limits,
        copied: Vec::new(),
        skipped: Vec::new(),
        entries: 0,
        total: 0,
    };
    let mut absent = Vec::new();
    for item in ALLOWLIST {
        let root = Path::new(match item {
            Item::File(p) | Item::Tree(p) => p,
            Item::Matching { dir, .. } => dir,
        });
        if let Err(e) = ancestors_are_real(volume, root) {
            run.skip(root, e);
            continue;
        }
        match (item, std::fs::symlink_metadata(volume.join(root))) {
            (_, Err(_)) => absent.push(root.display().to_string()),
            (Item::File(_), Ok(_)) => run.visit_file_only(root),
            (Item::Tree(_), Ok(_)) => run.visit(root, 0),
            (Item::Matching { prefix, suffix, .. }, Ok(m)) if m.is_dir() => {
                run.matching(root, prefix, suffix);
            }
            (Item::Matching { .. }, Ok(_)) => run.skip(root, "not a real directory"),
        }
    }
    Ok(json!({
        "copied": run.copied,
        "skipped": run.skipped,
        "absent": absent,
        "stats": STATS.iter().map(|s| stat_json(volume, s)).collect::<Vec<_>>(),
        "total_bytes": run.total,
        "limits": {
            "max_file_bytes": limits.max_file_bytes,
            "max_total_bytes": limits.max_total_bytes,
            "max_entries": limits.max_entries,
            "max_depth": limits.max_depth,
        },
    }))
}

impl Run<'_> {
    /// The regular files directly in `dir` named `prefix*suffix`.
    fn matching(&mut self, dir: &Path, prefix: &str, suffix: &str) {
        let names = match list(&self.volume.join(dir)) {
            Ok(n) => n,
            Err(e) => return self.skip(dir, format!("read directory: {e}")),
        };
        for name in names {
            let text = name.to_string_lossy();
            if text.len() > prefix.len() + suffix.len()
                && text.starts_with(prefix)
                && text.ends_with(suffix)
            {
                self.visit_file_only(&dir.join(&name));
            }
        }
    }

    /// Like [`visit`](Self::visit) for a name that must be a regular file: a directory
    /// that happens to match is not descended into.
    fn visit_file_only(&mut self, rel: &Path) {
        if !self.count_entry(rel) {
            return;
        }
        match std::fs::symlink_metadata(self.volume.join(rel)) {
            Ok(m) if m.is_file() => self.copy_file(rel),
            Ok(m) if m.file_type().is_symlink() => self.skip(rel, "symbolic link, not followed"),
            Ok(_) => self.skip(rel, "not a regular file"),
            Err(e) => self.skip(rel, format!("stat: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::tmp;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        std::fs::write(p, body).expect("file");
    }

    /// Every file under `dir`, relative, sorted.
    fn tree(dir: &Path) -> BTreeSet<String> {
        fn walk(base: &Path, at: &Path, into: &mut BTreeSet<String>) {
            for e in std::fs::read_dir(at).expect("dir") {
                let p = e.expect("entry").path();
                if p.is_dir() {
                    walk(base, &p, into);
                } else {
                    into.insert(p.strip_prefix(base).expect("under").display().to_string());
                }
            }
        }
        let mut into = BTreeSet::new();
        walk(dir, dir, &mut into);
        into
    }

    fn strings(v: &Value, key: &str, field: &str) -> Vec<String> {
        v[key]
            .as_array()
            .expect("array")
            .iter()
            .map(|e| e[field].as_str().expect("text").to_owned())
            .collect()
    }

    fn all() -> Limits {
        Limits::DEFAULT
    }

    /// A Data volume holding every allowlisted thing and some that are not on the list.
    fn full_volume(name: &str) -> (kbf_testkit::dir::TestDir, PathBuf) {
        let root = tmp(name);
        let v = root.join("vol");
        put(&v, "private/var/log/kbf-probe-local", "local marker");
        put(&v, "private/var/log/com.kbf.probe.mount.log", "mount log");
        put(&v, "private/var/log/com.kbf.probe.system.log", "system log");
        put(&v, "private/var/log/com.kbf.other.log", "not ours");
        put(&v, "private/var/log/com.kbf.probe.log", "no middle part");
        put(&v, "private/var/log/system.log", "not on the list");
        put(&v, "private/var/log/unrelated-daemon-file.log", "not ours");
        put(&v, "private/var/log/com.kbf.probe.mount.old", "not a log");
        put(&v, "private/var/db/diagnostics/Persist/0001.tracev3", "t1");
        put(&v, "private/var/db/diagnostics/timesync/a.timesync", "ts");
        put(&v, "private/var/db/uuidtext/AB/CDEF", "uu");
        put(&v, "private/var/db/timesync/x.timesync", "tx");
        put(
            &v,
            "private/var/db/dslocal/nodes/Default/users/root.plist",
            "SECRET",
        );
        put(&v, "private/var/db/.AppleSetupDone", "");
        put(&v, "Library/Logs/DiagnosticReports/crash.ips", "ips");
        put(&v, "Library/Keychains/login.keychain-db", "SECRET");
        put(&v, "Users/someone/Documents/secret.txt", "SECRET");
        (root, v)
    }

    /// Catches a harvest that copies more than the allowlist (system log, the user
    /// database, keychains, home directories), a name pattern that is too loose (a
    /// `com.kbf.probe.log` has no middle part) or too tight, and a copy that changes
    /// contents or paths.
    #[test]
    fn exactly_the_allowlist_is_copied_with_its_contents() {
        let (root, v) = full_volume("harvest-allow");
        let out = root.join("out");
        let m = copy_allowlist(&v, &out, &all()).expect("harvest");
        let want: BTreeSet<String> = [
            "private/var/log/kbf-probe-local",
            "private/var/log/com.kbf.probe.mount.log",
            "private/var/log/com.kbf.probe.system.log",
            "private/var/db/diagnostics/Persist/0001.tracev3",
            "private/var/db/diagnostics/timesync/a.timesync",
            "private/var/db/uuidtext/AB/CDEF",
            "private/var/db/timesync/x.timesync",
            "Library/Logs/DiagnosticReports/crash.ips",
        ]
        .map(str::to_owned)
        .into();
        assert_eq!(tree(&out), want);
        assert_eq!(
            std::fs::read_to_string(out.join("private/var/log/com.kbf.probe.mount.log"))
                .expect("read"),
            "mount log"
        );
        let copied: BTreeSet<String> = strings(&m, "copied", "path").into_iter().collect();
        assert_eq!(copied, want);
        assert_eq!(m["total_bytes"], 42);
        assert_eq!(m["skipped"], json!([]));
        assert_eq!(m["absent"], json!([]));
        // The guest's file times are kept in the manifest: they date the daemon's run.
        assert!(m["copied"][0]["mtime_unix"].as_i64().expect("mtime") > 0);
    }

    /// Catches a harvest that follows a link a guest planted at a file, a directory
    /// inside a tree, or a matching name: the target outside the volume must not be read.
    #[test]
    fn a_symbolic_link_inside_the_volume_is_listed_and_not_followed() {
        let (root, v) = full_volume("harvest-links");
        let outside = root.join("outside");
        put(&outside, "secret", "OUTSIDE");
        put(&outside, "dir/inner", "OUTSIDE");
        let link = |at: &str, to: &Path| {
            let p = v.join(at);
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_dir_all(&p);
            std::os::unix::fs::symlink(to, p).expect("link");
        };
        link("private/var/log/kbf-probe-local", &outside.join("secret"));
        link(
            "private/var/log/com.kbf.probe.mount.log",
            &outside.join("secret"),
        );
        link("private/var/db/uuidtext/AB", &outside.join("dir"));
        link(
            "private/var/db/timesync/x.timesync",
            &outside.join("secret"),
        );
        let out = root.join("out");
        let m = copy_allowlist(&v, &out, &all()).expect("harvest");
        let got = tree(&out);
        assert!(!got.iter().any(|p| p.contains("AB")), "{got:?}");
        assert!(!got.iter().any(|p| p.contains("x.timesync")), "{got:?}");
        assert!(!got.contains("private/var/log/kbf-probe-local"));
        assert!(!got.contains("private/var/log/com.kbf.probe.mount.log"));
        for p in &got {
            assert_ne!(
                std::fs::read_to_string(out.join(p)).expect("read"),
                "OUTSIDE"
            );
        }
        let skipped = strings(&m, "skipped", "path");
        for want in [
            "private/var/log/kbf-probe-local",
            "private/var/log/com.kbf.probe.mount.log",
            "private/var/db/uuidtext/AB",
            "private/var/db/timesync/x.timesync",
        ] {
            assert!(skipped.iter().any(|s| s == want), "{want}: {skipped:?}");
        }
        assert!(
            strings(&m, "skipped", "reason")
                .iter()
                .all(|r| r.contains("symbolic link"))
        );
    }

    /// Catches a harvest that walks through a symbolic link on the way to an allowlisted
    /// path (`private`, `var`, `Library`, the log directory itself): the whole item must
    /// be refused, naming the link, and nothing outside the volume read.
    #[test]
    fn a_symbolic_link_on_the_way_to_an_allowlisted_path_refuses_that_item() {
        let (root, v) = full_volume("harvest-ancestors");
        let outside = root.join("outside");
        put(&outside, "log/kbf-probe-local", "OUTSIDE");
        put(&outside, "Logs/DiagnosticReports/x.ips", "OUTSIDE");
        let swap = |at: &str, to: &Path| {
            std::fs::remove_dir_all(v.join(at)).expect("rm");
            std::os::unix::fs::symlink(to, v.join(at)).expect("link");
        };
        swap("private/var/log", &outside.join("log"));
        swap("Library", &outside);
        let out = root.join("out");
        let m = copy_allowlist(&v, &out, &all()).expect("harvest");
        let got = tree(&out);
        assert!(
            got.iter().all(|p| p.starts_with("private/var/db/")),
            "{got:?}"
        );
        for p in &got {
            assert_ne!(
                std::fs::read_to_string(out.join(p)).expect("read"),
                "OUTSIDE"
            );
        }
        let reasons = strings(&m, "skipped", "reason");
        assert!(
            reasons
                .iter()
                .any(|r| r == "private/var/log is a symbolic link"),
            "{reasons:?}"
        );
        assert!(
            reasons.iter().any(|r| r == "Library is a symbolic link"),
            "{reasons:?}"
        );
        // The log directory itself is reported as what it is: a link.
        let stat = m["stats"].as_array().expect("stats");
        assert_eq!(stat[1]["kind"], "symlink");
        // With `private` itself a link, the marker's stat is refused and names the link.
        swap("private", &outside);
        let m = copy_allowlist(&v, &root.join("out2"), &all()).expect("harvest");
        assert_eq!(m["stats"][0]["present"], false);
        assert!(
            m["stats"][0]["reason"]
                .as_str()
                .expect("why")
                .contains("symbolic link")
        );
    }

    /// Catches an entry cap that is not applied to the single files and the matching logs
    /// (only to tree walks), and a directory that is opened and read as if it were a file.
    #[test]
    fn the_entry_cap_covers_single_files_and_a_directory_is_not_a_file() {
        let (root, v) = full_volume("harvest-entries");
        let one = Limits {
            max_entries: 1,
            ..Limits::DEFAULT
        };
        let m = copy_allowlist(&v, &root.join("out"), &one).expect("harvest");
        assert_eq!(
            strings(&m, "copied", "path"),
            ["private/var/log/kbf-probe-local"]
        );
        let reasons = strings(&m, "skipped", "reason");
        assert_eq!(
            reasons.iter().filter(|r| r.contains("entry limit")).count(),
            1
        );
        let limits = Limits::DEFAULT;
        let out = root.join("out2");
        std::fs::create_dir_all(&out).expect("out");
        let mut run = Run {
            volume: &v,
            out: &out,
            limits: &limits,
            copied: Vec::new(),
            skipped: Vec::new(),
            entries: 0,
            total: 0,
        };
        run.copy_file(Path::new("private/var/db"));
        assert_eq!(run.skipped[0]["reason"], "not a regular file");
        assert!(run.copied.is_empty());
    }

    /// Catches the ancestor check accepting a path that is not plain (`..`, an absolute
    /// path), a file where a directory must be, or a missing parent.
    #[test]
    fn the_ancestor_check_wants_plain_relative_paths_through_real_directories() {
        let root = tmp("harvest-ancestor-check");
        put(&root, "a/b/c", "x");
        assert!(ancestors_are_real(&root, Path::new("a/b/c")).is_ok());
        assert!(ancestors_are_real(&root, Path::new("a/b/missing")).is_ok());
        assert!(ancestors_are_real(&root, Path::new("a/../etc/passwd")).is_err());
        assert!(ancestors_are_real(&root, Path::new("/etc/passwd")).is_err());
        assert!(
            ancestors_are_real(&root, Path::new("a/b/c/d"))
                .unwrap_err()
                .contains("not a directory")
        );
        assert!(
            ancestors_are_real(&root, Path::new("a/zz/c")).is_ok(),
            "absent is not a link"
        );
        let long = format!("{}/x", "n".repeat(300));
        assert!(ancestors_are_real(&root, Path::new(&format!("{long}/y"))).is_err());
        assert!(ancestors_are_real(&root, Path::new("")).is_ok());
    }

    /// Catches a harvest that reads a pipe (it would block on open) or a directory named
    /// like a log as a file, or that leaves a directory in a log place unreported.
    #[test]
    fn what_is_not_a_regular_file_is_refused() {
        let (root, v) = full_volume("harvest-special");
        let fifo = std::ffi::CString::new(
            v.join("private/var/db/uuidtext/pipe")
                .as_os_str()
                .as_encoded_bytes(),
        )
        .expect("c");
        // SAFETY: a NUL-terminated path in a directory this test owns.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        std::fs::create_dir_all(v.join("private/var/log/com.kbf.probe.dir.log")).expect("dir");
        std::fs::remove_file(v.join("private/var/log/kbf-probe-local")).expect("rm");
        std::fs::create_dir_all(v.join("private/var/log/kbf-probe-local")).expect("dir");
        let out = root.join("out");
        let m = copy_allowlist(&v, &out, &all()).expect("harvest");
        let got = tree(&out);
        assert!(!got.iter().any(|p| p.contains("pipe")), "{got:?}");
        assert!(!got.iter().any(|p| p.contains("dir.log")), "{got:?}");
        let skipped = strings(&m, "skipped", "path");
        for want in [
            "private/var/db/uuidtext/pipe",
            "private/var/log/com.kbf.probe.dir.log",
            "private/var/log/kbf-probe-local",
        ] {
            assert!(skipped.iter().any(|s| s == want), "{want}: {skipped:?}");
        }
        // A log directory that is not a directory at all.
        std::fs::remove_dir_all(v.join("private/var/log")).expect("rm log");
        std::fs::write(v.join("private/var/log"), "file").expect("file");
        let m = copy_allowlist(&v, &root.join("out2"), &all()).expect("harvest");
        let reasons = strings(&m, "skipped", "reason");
        assert!(
            reasons.iter().any(|r| r == "not a real directory"),
            "{reasons:?}"
        );
    }

    /// Catches caps that do not hold: a file over the per-file cap, a file that would
    /// take the total over its cap, more entries than allowed, and a tree deeper than
    /// allowed must each be listed as skipped and not copied.
    #[test]
    fn the_size_entry_and_depth_caps_hold_and_are_reported() {
        let root = tmp("harvest-caps");
        let v = root.join("vol");
        put(&v, "private/var/db/uuidtext/small", "12345");
        put(&v, "private/var/db/uuidtext/big", "123456789012");
        put(&v, "private/var/db/uuidtext/other", "123456");
        let limits = Limits {
            max_file_bytes: 10,
            max_total_bytes: 8,
            ..Limits::DEFAULT
        };
        let m = copy_allowlist(&v, &root.join("o1"), &limits).expect("harvest");
        assert_eq!(
            strings(&m, "copied", "path"),
            ["private/var/db/uuidtext/other"]
        );
        let skipped = strings(&m, "skipped", "reason");
        assert_eq!(
            skipped,
            [
                "larger than 10 bytes".to_owned(),
                "total limit of 8 bytes reached".to_owned()
            ]
        );
        assert_eq!(m["total_bytes"], 6);
        // Entries: the root, one file, then the limit is passed by two more.
        let few = Limits {
            max_entries: 2,
            ..Limits::DEFAULT
        };
        let m = copy_allowlist(&v, &root.join("o2"), &few).expect("harvest");
        assert_eq!(m["copied"].as_array().expect("copied").len(), 1);
        let reasons = strings(&m, "skipped", "reason");
        assert_eq!(
            reasons.iter().filter(|r| r.contains("entry limit")).count(),
            1,
            "{reasons:?}"
        );
        // Depth.
        put(&v, "private/var/db/timesync/a/b/c/deep", "d");
        let shallow = Limits {
            max_depth: 2,
            ..Limits::DEFAULT
        };
        let m = copy_allowlist(&v, &root.join("o3"), &shallow).expect("harvest");
        let paths = strings(&m, "skipped", "path");
        assert!(
            paths.iter().any(|p| p == "private/var/db/timesync/a/b"),
            "{paths:?}"
        );
        assert!(!tree(&root.join("o3")).iter().any(|p| p.ends_with("deep")));
    }

    /// Catches a manifest that hides what the guest did not produce, and stats that miss
    /// the setup marker and the log directory.
    #[test]
    fn missing_items_are_listed_and_the_marker_times_are_recorded() {
        let root = tmp("harvest-absent");
        let v = root.join("vol");
        put(&v, "private/var/db/.AppleSetupDone", "");
        std::fs::create_dir_all(v.join("private/var/log")).expect("log");
        let m = copy_allowlist(&v, &root.join("out"), &all()).expect("harvest");
        assert_eq!(m["copied"], json!([]));
        let absent: Vec<String> = m["absent"]
            .as_array()
            .expect("absent")
            .iter()
            .map(|p| p.as_str().expect("path").to_owned())
            .collect();
        assert!(absent.contains(&"private/var/log/kbf-probe-local".to_owned()));
        assert!(absent.contains(&"Library/Logs/DiagnosticReports".to_owned()));
        let stats = m["stats"].as_array().expect("stats");
        assert_eq!(stats[0]["path"], "private/var/db/.AppleSetupDone");
        assert_eq!(stats[0]["present"], true);
        assert_eq!(stats[0]["kind"], "file");
        assert!(stats[0]["mtime_unix"].as_i64().expect("mtime") > 0);
        assert_eq!(stats[1]["kind"], "directory");
        let none =
            copy_allowlist(&root.join("empty"), &root.join("out2"), &all()).expect("harvest");
        assert_eq!(none["stats"][0]["present"], false);
        assert!(none["stats"][0]["reason"].is_string());
    }

    /// Catches an output directory that is reused (a second harvest would mix two
    /// guests), and the refusal to make it.
    #[test]
    fn the_output_directory_must_be_new() {
        let root = tmp("harvest-out");
        let v = root.join("vol");
        std::fs::create_dir_all(&v).expect("vol");
        let out = root.join("deep/er/out");
        copy_allowlist(&v, &out, &all()).expect("first");
        assert!(out.is_dir());
        let e = copy_allowlist(&v, &out, &all()).unwrap_err();
        assert!(e.contains("create"), "{e}");
        std::fs::write(root.join("file"), "x").expect("file");
        assert!(copy_allowlist(&v, &root.join("file/out"), &all()).is_err());
    }

    /// Catches walks that let one unreadable entry stop the harvest: each failing step
    /// (open, stat, read a directory, create the output) is recorded as skipped.
    #[test]
    fn a_step_that_fails_is_recorded_and_the_walk_goes_on() {
        let root = tmp("harvest-fail");
        let v = root.join("vol");
        let out = root.join("out");
        std::fs::create_dir_all(&v).expect("vol");
        std::fs::create_dir_all(&out).expect("out");
        let limits = Limits::DEFAULT;
        let mut run = Run {
            volume: &v,
            out: &out,
            limits: &limits,
            copied: Vec::new(),
            skipped: Vec::new(),
            entries: 0,
            total: 0,
        };
        run.copy_file(Path::new("gone"));
        run.visit(Path::new("gone"), 0);
        run.visit_file_only(Path::new("gone"));
        run.descend(Path::new("gone"), 0);
        run.matching(Path::new("gone"), "a", "b");
        let reasons: Vec<String> = run
            .skipped
            .iter()
            .map(|s| s["reason"].as_str().expect("why").to_owned())
            .collect();
        for (r, want) in reasons.iter().zip([
            "open:",
            "stat:",
            "stat:",
            "read directory:",
            "read directory:",
        ]) {
            assert!(r.starts_with(want), "{r}");
        }
        assert_eq!(reasons.len(), 5);
        // A destination that cannot be made: a path longer than the system allows.
        put(&v, "f", "x");
        let long = (0..20).fold(out.clone(), |p, _| p.join("n".repeat(250)));
        let mut run = Run {
            volume: &v,
            out: &long,
            limits: &limits,
            copied: Vec::new(),
            skipped: Vec::new(),
            entries: 0,
            total: 0,
        };
        run.copy_file(Path::new("f"));
        assert!(
            run.skipped[0]["reason"]
                .as_str()
                .expect("why")
                .starts_with("copy:")
        );
        assert!(run.copied.is_empty());
    }

    /// Catches an allowlist that grew without review: it is pinned here, entry by entry.
    #[test]
    fn the_allowlist_is_the_reviewed_list() {
        assert_eq!(ALLOWLIST.len(), 6);
        assert_eq!(ALLOWLIST[0], Item::File("private/var/log/kbf-probe-local"));
        assert_eq!(
            ALLOWLIST[1],
            Item::Matching {
                dir: "private/var/log",
                prefix: "com.kbf.probe.",
                suffix: ".log"
            }
        );
        assert_eq!(
            ALLOWLIST[2..],
            [
                Item::Tree("private/var/db/diagnostics"),
                Item::Tree("private/var/db/uuidtext"),
                Item::Tree("private/var/db/timesync"),
                Item::Tree("Library/Logs/DiagnosticReports"),
            ]
        );
        assert_eq!(STATS, ["private/var/db/.AppleSetupDone", "private/var/log"]);
    }
}
