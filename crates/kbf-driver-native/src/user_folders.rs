//! The daemon user's own temporary and cache folders on macOS, the few names in them a
//! sandboxed action may write, and the sweep of what actions leave there.
//!
//! Some tools write in the folders `confstr(_CS_DARWIN_USER_TEMP_DIR)` and
//! `confstr(_CS_DARWIN_USER_CACHE_DIR)` name (`/var/folders/<..>/T` and `../C`),
//! whatever `TMPDIR` says, so the lease's own directories (`crate::home`) cannot take
//! those writes (issue #163):
//! - Foundation's atomic saves and item replacement directories go to
//!   `T/TemporaryItems`: `swift build` (SwiftPM saves its files that way) and
//!   `xcodebuild` (its log store) fail without them;
//! - `xcrun`, behind every `/usr/bin` compiler shim (`cc`, `clang`, `swiftc`), keeps its
//!   lookup cache in `T/xcrun_db`, written through a temporary `T/xcrun_db-<random>`;
//!   without it every call prints "couldn't create cache file" and is several times
//!   slower.
//!
//! The sandbox profile ([`UserFolders::rules`]) lets an action write below
//! `T/TemporaryItems` (not the folder itself: an action that could replace it with a
//! symlink would aim the daemon's sweep anywhere) and to those `xcrun` names, and
//! nothing else in the two folders, the folders themselves included. The daemon finds
//! the folders once, at start ([`UserFolders::detect`]), with every link resolved: the
//! sandbox compares the real paths of the files written. The sweep makes
//! `TemporaryItems` (mode 0700) when it is missing, since an action cannot.
//!
//! What this gives up, until each lease runs as its own user (issue #122, which gives
//! each lease its own `/var/folders` entry): leases of the daemon's user share these
//! names, so a lease can see, change or remove the temporary files another lease has
//! open there, and can rewrite `xcrun_db`, which the next lease's compiler shims read
//! to find their tools. The daemon itself trusts nothing there: it removes `xcrun_db`
//! ([`UserFolders::forget_xcrun_cache`]) before it runs any developer tool at start,
//! runs `xcodebuild` from inside the Xcode rather than through its `/usr/bin` shim
//! ([`crate::xcode::discover`]), and sweeps by descriptor.
//!
//! What a lease leaves there (a save it was killed in the middle of, a temporary
//! `xcrun_db-*`) is swept ([`UserFolders::sweep`]) at daemon start and after every
//! lease, once its last change is at least [`LEFTOVER_AGE`] from now either way: a
//! younger one may be another lease's save in progress, which a sweep must not break,
//! and one as far in the future can be no save in progress (the clock was set back,
//! or an action set the time). `xcrun_db` itself is a cache and stays. The sweep
//! opens `T` and `TemporaryItems` with `O_NOFOLLOW`, takes each only when it is a
//! directory of the daemon's user, and removes entries by name relative to it,
//! never following a symlink at any level ([`kbf_outputs::remove_tree_at`]).

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rustix::fs::{AtFlags, CWD, Dir, Mode, OFlags};

/// How old a leftover in the user folders must be before the sweep removes it: older
/// than any save in progress.
pub const LEFTOVER_AGE: Duration = Duration::from_secs(3600);

/// Where in the temporary folder Foundation makes its temporary items.
const TEMPORARY_ITEMS: &str = "TemporaryItems";

/// `xcrun`'s cache in the temporary folder; it writes `<XCRUN_DB>-<random>` first.
const XCRUN_DB: &str = "xcrun_db";

/// How the sweep opens a folder: a directory, never through a symlink.
const FOLDER: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// What removes an entry, by name, from a folder the sweep holds open.
pub type Remove = dyn Fn(&OwnedFd, &OsStr) -> io::Result<()>;

/// The daemon user's temporary and cache folders, by their real paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserFolders {
    temp: PathBuf,
    cache: PathBuf,
}

impl UserFolders {
    /// The folders at `temp` and `cache`, if both paths can be written into a sandbox
    /// profile: absolute, UTF-8, and free of `"`, `\` and control characters (which
    /// `confstr` never returns). `None` otherwise.
    #[must_use]
    pub fn new(temp: PathBuf, cache: PathBuf) -> Option<Self> {
        let fits = |path: &Path| {
            path.is_absolute()
                && path
                    .to_str()
                    .is_some_and(|s| !s.contains(['"', '\\']) && !s.contains(char::is_control))
        };
        (fits(&temp) && fits(&cache)).then_some(Self { temp, cache })
    }

    /// This user's folders, from `confstr` (which makes them if they are missing),
    /// every link resolved; `None`, logged, when either cannot be found.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn detect() -> Option<Self> {
        let found = |name| {
            let path = confstr(name)?;
            std::fs::canonicalize(&path)
                .map_err(|e| tracing::warn!(path = %path.display(), "user folder: {e}"))
                .ok()
        };
        let folders = Self::new(
            found(libc::_CS_DARWIN_USER_TEMP_DIR)?,
            found(libc::_CS_DARWIN_USER_CACHE_DIR)?,
        );
        if folders.is_none() {
            tracing::warn!("the user folders' paths cannot go into a sandbox profile");
        }
        folders
    }

    /// None: only macOS has these folders.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub fn detect() -> Option<Self> {
        None
    }

    /// The temporary folder.
    #[must_use]
    pub fn temp(&self) -> &Path {
        &self.temp
    }

    /// The cache folder.
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// The sandbox profile rules that let an action write the names the module lists
    /// (below `TemporaryItems`, never the folder itself) and nothing else in these
    /// folders; they follow the profile's `(deny file-write*)`, which they narrow.
    #[must_use]
    pub fn rules(&self) -> String {
        let temp = regex_quote(&self.temp.to_string_lossy());
        format!(
            "(allow file-write*\n  (regex #\"^{temp}/{TEMPORARY_ITEMS}/\")\n  (regex #\"^{temp}/{XCRUN_DB}(-[^/]*)?$\"))\n",
        )
    }

    /// Removes `xcrun`'s cache, `T/xcrun_db`, which any lease can rewrite: the daemon
    /// calls this before it runs a developer tool of its own, so no tool it runs outside
    /// the sandbox finds its tools through what a lease wrote. Logged when it fails.
    pub fn forget_xcrun_cache(&self) {
        self.forget_xcrun_cache_as(rustix::process::geteuid().as_raw());
    }

    /// [`Self::forget_xcrun_cache`] as if the daemon ran as `uid`.
    fn forget_xcrun_cache_as(&self, uid: u32) {
        let removed = open_own(CWD, &self.temp, uid)
            .and_then(|temp| kbf_outputs::remove_tree_at(&temp, OsStr::new(XCRUN_DB)));
        if let Err(e) = removed {
            tracing::warn!(temp = %self.temp.display(), "xcrun cache not removed: {e}");
        }
    }

    /// Removes, with `remove` (by name, from the folder it is in), every leftover the
    /// module lists whose last change is at least `age` from `now`, and makes
    /// `TemporaryItems` if it is missing. A folder that is not a directory of the
    /// daemon's user (a symlink included) is logged and not swept; what cannot be read
    /// or removed is logged and left.
    pub fn sweep(&self, age: Duration, now: SystemTime, remove: &Remove) {
        self.sweep_as(rustix::process::geteuid().as_raw(), age, now, remove);
    }

    /// [`Self::sweep`] as if the daemon ran as `uid`.
    fn sweep_as(&self, uid: u32, age: Duration, now: SystemTime, remove: &Remove) {
        let temp = match open_own(CWD, &self.temp, uid) {
            Ok(temp) => temp,
            Err(e) => {
                tracing::warn!(temp = %self.temp.display(), "not swept: {e}");
                return;
            }
        };
        // An error shows when the folder is opened next.
        let _ = rustix::fs::mkdirat(&temp, TEMPORARY_ITEMS, Mode::RWXU);
        match open_own(temp.as_fd(), Path::new(TEMPORARY_ITEMS), uid) {
            Ok(items) => sweep_in(&items, b"", age, now, remove),
            Err(e) => {
                tracing::warn!(temp = %self.temp.display(), "{TEMPORARY_ITEMS} not swept: {e}")
            }
        }
        sweep_in(&temp, format!("{XCRUN_DB}-").as_bytes(), age, now, remove);
    }
}

/// `path` in `dir`, opened as a directory without following a symlink, if `uid` owns
/// it.
fn open_own(dir: BorrowedFd<'_>, path: &Path, uid: u32) -> io::Result<OwnedFd> {
    let fd = rustix::fs::openat(dir, path, FOLDER, Mode::empty())?;
    let owner = rustix::fs::fstat(&fd)?.st_uid;
    if owner != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("owned by uid {owner}, not {uid}"),
        ));
    }
    Ok(fd)
}

/// Removes, with `remove`, every entry of `dir` whose name starts with `leftover` and
/// whose last change is at least `age` from `now`.
fn sweep_in(dir: &OwnedFd, leftover: &[u8], age: Duration, now: SystemTime, remove: &Remove) {
    // Listed whole before any is removed: removing while reading skips entries on
    // some filesystems.
    let names: Vec<Vec<u8>> = Dir::read_from(dir)
        .inspect_err(|e| tracing::warn!("sweep: {e}"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_bytes().to_vec())
        .filter(|name| name.starts_with(leftover) && !matches!(&name[..], b"." | b".."))
        .collect();
    for name in names {
        let name = OsStr::from_bytes(&name);
        let old = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| is_old(changed(&stat), age, now));
        if !old {
            continue;
        }
        match remove(dir, name) {
            Ok(()) => tracing::info!(?name, "swept a leftover"),
            Err(e) => tracing::warn!(?name, "sweep: {e}"),
        }
    }
}

/// When `stat`'s file was last changed.
#[allow(clippy::unnecessary_cast)] // the field types differ between Linux and macOS
fn changed(stat: &rustix::fs::Stat) -> SystemTime {
    time_of(stat.st_mtime as i64, stat.st_mtime_nsec as i64)
}

/// The time `secs` and `nanos` after the epoch; the epoch for a time before it or
/// nanoseconds out of range.
fn time_of(secs: i64, nanos: i64) -> SystemTime {
    match (u64::try_from(secs), u32::try_from(nanos)) {
        (Ok(secs), Ok(nanos)) if nanos < 1_000_000_000 => {
            SystemTime::UNIX_EPOCH + Duration::new(secs, nanos)
        }
        _ => SystemTime::UNIX_EPOCH,
    }
}

/// Whether something last changed at `changed` is at least `age` from `now`, before
/// or after it.
fn is_old(changed: SystemTime, age: Duration, now: SystemTime) -> bool {
    let apart = now
        .duration_since(changed)
        .unwrap_or_else(|future| future.duration());
    apart >= age
}

/// `text` with every character a regular expression gives a meaning to escaped.
fn regex_quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len());
    for c in text.chars() {
        if ".^$*+?()[]{}|".contains(c) {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted
}

/// The string `confstr` returns for `name`, if any.
#[cfg(target_os = "macos")]
fn confstr(name: libc::c_int) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    // SAFETY: a null buffer of length 0 asks only for the length, NUL included.
    let len = unsafe { libc::confstr(name, std::ptr::null_mut(), 0) };
    if len == 0 {
        tracing::warn!(name, "confstr: no value");
        return None;
    }
    let mut buf = vec![0_u8; len];
    // SAFETY: `buf` holds `len` bytes, the length `confstr` writes at most.
    let wrote = unsafe { libc::confstr(name, buf.as_mut_ptr().cast(), len) };
    if wrote == 0 || wrote > len {
        tracing::warn!(name, "confstr: the value changed");
        return None;
    }
    buf.truncate(wrote - 1);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("user-folders-{name}-{}", std::process::id()));
        // Absent unless a run with this pid left it.
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        dir
    }

    fn folders(dir: &Path) -> UserFolders {
        let (temp, cache) = (dir.join("T"), dir.join("C"));
        std::fs::create_dir_all(temp.join(TEMPORARY_ITEMS)).expect("T");
        std::fs::create_dir_all(&cache).expect("C");
        UserFolders::new(temp, cache).expect("fits")
    }

    /// Makes `path` (a directory when it ends in `/`) last changed `ago` before now.
    fn made(path: &str, base: &Path, ago: Duration) -> PathBuf {
        made_at(path, base, SystemTime::now() - ago)
    }

    /// Makes `path` (a directory when it ends in `/`) last changed `when`.
    fn made_at(path: &str, base: &Path, when: SystemTime) -> PathBuf {
        let full = base.join(path.trim_end_matches('/'));
        if path.ends_with('/') {
            std::fs::create_dir_all(full.join("inside")).expect("dir");
        } else {
            std::fs::write(&full, b"x").expect("file");
        }
        File::open(&full)
            .expect("open")
            .set_modified(when)
            .expect("mtime");
        full
    }

    /// Catches a path the profile cannot hold taken anyway (a `"` would end the
    /// profile's string and let the rest of the path write rules), and a good one
    /// refused.
    #[test]
    fn only_paths_a_profile_can_hold_are_taken() {
        let good = PathBuf::from("/private/var/folders/ab/c+d_e/T");
        let cache = PathBuf::from("/private/var/folders/ab/c+d_e/C");
        let folders = UserFolders::new(good.clone(), cache.clone()).expect("fits");
        assert_eq!((folders.temp(), folders.cache()), (&*good, &*cache));
        for bad in ["T", "/a\"b", "/a\\b", "/a\nb"] {
            assert_eq!(
                UserFolders::new(PathBuf::from(bad), cache.clone()),
                None,
                "{bad:?}"
            );
            assert_eq!(
                UserFolders::new(good.clone(), PathBuf::from(bad)),
                None,
                "{bad:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let not_utf8 = PathBuf::from(std::ffi::OsStr::from_bytes(b"/a\xff"));
            assert_eq!(UserFolders::new(not_utf8, cache), None);
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(UserFolders::detect(), None);
    }

    /// Catches rules that open the folders whole (any file in `T` or `C`), open
    /// `TemporaryItems` itself (an action could swap it for a symlink the daemon's
    /// sweep then follows), miss one of the names the tools write, or let a
    /// regular-expression character in the path (`+` and `.` are common in
    /// `/var/folders` names) match other paths.
    #[test]
    fn the_rules_name_only_what_is_below_temporary_items_and_the_xcrun_cache() {
        let folders = UserFolders::new(
            PathBuf::from("/private/var/folders/ab/c+d.e/T"),
            PathBuf::from("/private/var/folders/ab/c+d.e/C"),
        )
        .expect("fits");
        assert_eq!(
            folders.rules(),
            "(allow file-write*\n  \
             (regex #\"^/private/var/folders/ab/c\\+d\\.e/T/TemporaryItems/\")\n  \
             (regex #\"^/private/var/folders/ab/c\\+d\\.e/T/xcrun_db(-[^/]*)?$\"))\n"
        );
        assert_eq!(
            regex_quote("a.^$*+?()[]{}|b/-"),
            "a\\.\\^\\$\\*\\+\\?\\(\\)\\[\\]\\{\\}\\|b/-"
        );
    }

    /// Catches a sweep that removes a young leftover (another lease's save in
    /// progress, one changed a minute from now included), `xcrun_db` itself (the
    /// cache), or anything in the folders but the leftovers the module lists; that
    /// leaves an old one, or one changed further in the future than the age (issue
    /// #163's review: such a one was never swept); or that stops at the first entry it
    /// cannot remove.
    #[test]
    fn the_sweep_removes_only_old_leftovers() {
        let dir = scratch("sweep");
        let f = folders(&dir);
        let old = LEFTOVER_AGE + Duration::from_secs(60);
        let young = Duration::from_secs(60);
        let items = f.temp().join(TEMPORARY_ITEMS);
        let future = |by: Duration| SystemTime::now() + by;
        let gone = [
            made("NSIRD_swift-build_old/", &items, old),
            made("old-file", &items, old),
            made("xcrun_db-AbC123", f.temp(), old),
            made_at("NSIRD_far_future/", &items, future(old)),
            made_at("xcrun_db-far-future", f.temp(), future(old)),
        ];
        let kept = [
            made("NSIRD_xcodebuild_young/", &items, young),
            made_at("NSIRD_near_future/", &items, future(young)),
            made("xcrun_db-young", f.temp(), young),
            made("xcrun_db", f.temp(), old),
            made("xcrun_dbx", f.temp(), old),
            made("someone-elses", f.temp(), old),
            made("com.apple.something/", f.cache(), old),
        ];
        let stuck = made("NSIRD_stuck/", &items, old);
        let remove = |dir: &OwnedFd, name: &OsStr| {
            if name == "NSIRD_stuck" {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            kbf_outputs::remove_tree_at(dir, name)
        };
        f.sweep(LEFTOVER_AGE, SystemTime::now(), &remove);
        for path in &gone {
            assert!(!path.exists(), "left {}", path.display());
        }
        for path in kept.iter().chain([&stuck]) {
            assert!(path.exists(), "removed {}", path.display());
        }

        // A sweep with nothing to sweep, and one whose folders are gone or not
        // folders, does nothing and does not fail.
        f.sweep(LEFTOVER_AGE, SystemTime::now(), &remove);
        kbf_outputs::remove_tree(&items).expect("remove");
        std::fs::write(&items, b"not a folder").expect("file");
        f.sweep(Duration::ZERO, SystemTime::now(), &remove);
        assert!(items.is_file());
        // A folder that cannot be listed (here a file) is logged and left.
        let not_listed = OwnedFd::from(File::open(&items).expect("open"));
        sweep_in(&not_listed, b"", Duration::ZERO, SystemTime::now(), &remove);
        assert!(items.is_file());
        kbf_outputs::remove_tree(&dir).expect("clean");
        f.sweep(Duration::ZERO, SystemTime::now(), &remove);
    }

    /// Catches the sweep following a symlink a lease left (found by the review of
    /// issue #163, whose profile let an action replace `TemporaryItems` itself):
    /// `TemporaryItems` swapped for a link to a directory of the daemon user's (the
    /// review's own test, on the new remover), an old `xcrun_db-*` link to one, and `T`
    /// itself a link. Each link must be left or unlinked, never followed, and what it
    /// names kept.
    #[test]
    fn the_sweep_never_follows_a_symlink() {
        let dir = scratch("symlink");
        let f = folders(&dir);
        let victim = dir.join("victim");
        std::fs::create_dir_all(&victim).expect("victim");
        let old = LEFTOVER_AGE + Duration::from_secs(60);
        let precious = made("precious", &victim, old);
        let items = f.temp().join(TEMPORARY_ITEMS);
        std::fs::remove_dir_all(&items).expect("rmdir");
        std::os::unix::fs::symlink(&victim, &items).expect("symlink");
        let xcrun_link = f.temp().join("xcrun_db-link");
        std::os::unix::fs::symlink(&victim, &xcrun_link).expect("symlink");
        f.sweep(
            LEFTOVER_AGE,
            SystemTime::now(),
            &kbf_outputs::remove_tree_at,
        );
        assert!(
            precious.exists(),
            "the daemon's sweep removed {} through a symlink",
            precious.display()
        );
        f.sweep(
            Duration::ZERO,
            SystemTime::now(),
            &kbf_outputs::remove_tree_at,
        );
        assert!(precious.exists(), "removed through a symlink");
        assert!(items.is_symlink(), "the TemporaryItems link was not left");
        assert!(
            std::fs::symlink_metadata(&xcrun_link).is_err(),
            "an old xcrun link stays"
        );

        // `T` itself a link: nothing is swept and no `TemporaryItems` made through it.
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        let left = made("xcrun_db-left", &elsewhere, old);
        let linked = UserFolders::new(dir.join("T-link"), f.cache().to_owned()).expect("fits");
        std::os::unix::fs::symlink(&elsewhere, linked.temp()).expect("symlink");
        linked.sweep(
            Duration::ZERO,
            SystemTime::now(),
            &kbf_outputs::remove_tree_at,
        );
        assert!(left.exists(), "swept through a linked T");
        assert!(
            !elsewhere.join(TEMPORARY_ITEMS).exists(),
            "made through a linked T"
        );
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches a sweep that takes folders another user owns (a lease cannot make one,
    /// but the check is what keeps the sweep to the daemon's own), and one that does
    /// not make a missing `TemporaryItems`, private to the daemon's user.
    #[test]
    fn the_sweep_takes_only_the_daemon_users_folders_and_makes_temporary_items() {
        let dir = scratch("owner");
        let f = folders(&dir);
        let items = f.temp().join(TEMPORARY_ITEMS);
        let old = LEFTOVER_AGE + Duration::from_secs(60);
        let left = made("xcrun_db-left", f.temp(), old);
        std::fs::remove_dir_all(&items).expect("rmdir");
        let me = rustix::process::geteuid().as_raw();
        f.sweep_as(
            me + 1,
            Duration::ZERO,
            SystemTime::now(),
            &kbf_outputs::remove_tree_at,
        );
        assert!(left.exists(), "swept another user's folder");
        assert!(!items.exists(), "made a folder in another user's folder");
        f.sweep(
            Duration::ZERO,
            SystemTime::now(),
            &kbf_outputs::remove_tree_at,
        );
        assert!(
            !left.exists(),
            "kept a leftover in the daemon user's folder"
        );
        let mode = std::fs::symlink_metadata(&items)
            .expect("made")
            .permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o700
        );
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches `xcrun`'s cache kept for the daemon's own tools, whether a lease wrote
    /// it as a file or made it a directory, a removal through a linked `T`, and one in
    /// a folder another user owns; and a missing cache, or `T`, taken as a failure
    /// worth more than a log line.
    #[test]
    fn the_xcrun_cache_is_forgotten() {
        let dir = scratch("forget");
        let f = folders(&dir);
        let cache = f.temp().join(XCRUN_DB);
        std::fs::write(&cache, b"lease-written").expect("cache");
        let me = rustix::process::geteuid().as_raw();
        f.forget_xcrun_cache_as(me + 1);
        assert!(cache.exists(), "removed from another user's folder");
        f.forget_xcrun_cache();
        assert!(!cache.exists(), "the cache file stays");
        std::fs::create_dir_all(cache.join("deep")).expect("a directory");
        f.forget_xcrun_cache();
        assert!(!cache.exists(), "the cache directory stays");
        f.forget_xcrun_cache();

        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        let kept = made(XCRUN_DB, &elsewhere, Duration::ZERO);
        let linked = UserFolders::new(dir.join("T-link"), f.cache().to_owned()).expect("fits");
        std::os::unix::fs::symlink(&elsewhere, linked.temp()).expect("symlink");
        linked.forget_xcrun_cache();
        assert!(kept.exists(), "removed through a linked T");
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches a last change read wrongly from the file's times (seconds or
    /// nanoseconds), a time before the epoch or out of range taken as anything but
    /// the epoch, and an age that counts only one way: a change as far in the future
    /// is as old, one nearer either way is not.
    #[test]
    fn old_is_far_from_now_either_way() {
        let epoch = SystemTime::UNIX_EPOCH;
        assert_eq!(time_of(5, 7), epoch + Duration::new(5, 7));
        for (secs, nanos) in [(-5, 0), (5, -1), (5, 1_000_000_000)] {
            assert_eq!(time_of(secs, nanos), epoch, "{secs} {nanos}");
        }
        let now = epoch + Duration::from_secs(10_000);
        let age = Duration::from_secs(100);
        for (changed, old) in [
            (9_900, true),
            (9_901, false),
            (10_000, false),
            (10_099, false),
            (10_100, true),
        ] {
            let changed = epoch + Duration::from_secs(changed);
            assert_eq!(is_old(changed, age, now), old, "{changed:?}");
        }
    }
}
