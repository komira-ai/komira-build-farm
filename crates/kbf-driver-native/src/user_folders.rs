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
//! The sandbox profile ([`UserFolders::rules`]) lets an action write those names and
//! nothing else in the two folders, the folders themselves included. The daemon finds
//! the folders once, at start ([`UserFolders::detect`]), with every link resolved: the
//! sandbox compares the real paths of the files written.
//!
//! What this gives up, until each lease runs as its own user (issue #122, which gives
//! each lease its own `/var/folders` entry): leases of the daemon's user share these
//! names, so a lease can see, change or remove the temporary files another lease has
//! open there, and can rewrite `xcrun_db`, which the next lease's compiler shims read
//! to find their tools.
//!
//! What a lease leaves there (a save it was killed in the middle of, a temporary
//! `xcrun_db-*`) is swept ([`UserFolders::sweep`]) at daemon start and after every
//! lease, once it is older than [`LEFTOVER_AGE`]: a younger one may be another lease's
//! save in progress, which a sweep must not break. `xcrun_db` itself is a cache and
//! stays.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How old a leftover in the user folders must be before the sweep removes it: older
/// than any save in progress.
pub const LEFTOVER_AGE: Duration = Duration::from_secs(3600);

/// Where in the temporary folder Foundation makes its temporary items.
const TEMPORARY_ITEMS: &str = "TemporaryItems";

/// `xcrun`'s cache in the temporary folder; it writes `<XCRUN_DB>-<random>` first.
const XCRUN_DB: &str = "xcrun_db";

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
    /// and nothing else in these folders; they follow the profile's
    /// `(deny file-write*)`, which they narrow.
    #[must_use]
    pub fn rules(&self) -> String {
        let temp = self.temp.to_string_lossy();
        let _ = TEMPORARY_ITEMS;
        format!(
            "(allow file-write*\n  (regex #\"^{}/{XCRUN_DB}(-[^/]*)?\"))\n",
            regex_quote(&temp)
        )
    }

    /// Removes, with `remove`, every leftover the module lists that was last changed
    /// at least `age` before `now`. What cannot be read or removed is logged and left.
    pub fn sweep(&self, age: Duration, now: SystemTime, remove: &dyn Fn(&Path) -> io::Result<()>) {
        let items = self.temp.join(TEMPORARY_ITEMS);
        // Each folder, and the start of the names that are leftovers in it.
        let xcrun_temp = format!("{XCRUN_DB}-");
        for (dir, leftover) in [(&items, ""), (&self.temp, xcrun_temp.as_str())] {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), "sweep: {e}");
                    continue;
                }
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let old = entry.file_name().to_string_lossy().starts_with(leftover)
                    && path
                        .symlink_metadata()
                        .and_then(|m| m.modified())
                        .is_ok_and(|changed| changed + age <= now);
                if !old {
                    continue;
                }
                match remove(&path) {
                    Ok(()) => tracing::info!(path = %path.display(), "swept a leftover"),
                    Err(e) => tracing::warn!(path = %path.display(), "sweep: {e}"),
                }
            }
        }
    }
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
        let full = base.join(path.trim_end_matches('/'));
        if path.ends_with('/') {
            std::fs::create_dir_all(full.join("inside")).expect("dir");
        } else {
            std::fs::write(&full, b"x").expect("file");
        }
        let when = SystemTime::now() - ago;
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

    /// Catches rules that open the folders whole (any file in `T` or `C`), miss one of
    /// the names the tools write, or let a regular-expression character in the path
    /// (`+` and `.` are common in `/var/folders` names) match other paths.
    #[test]
    fn the_rules_name_only_temporary_items_and_the_xcrun_cache() {
        let folders = UserFolders::new(
            PathBuf::from("/private/var/folders/ab/c+d.e/T"),
            PathBuf::from("/private/var/folders/ab/c+d.e/C"),
        )
        .expect("fits");
        assert_eq!(
            folders.rules(),
            "(allow file-write*\n  \
             (subpath \"/private/var/folders/ab/c+d.e/T/TemporaryItems\")\n  \
             (regex #\"^/private/var/folders/ab/c\\+d\\.e/T/xcrun_db(-[^/]*)?$\"))\n"
        );
        assert_eq!(
            regex_quote("a.^$*+?()[]{}|b/-"),
            "a\\.\\^\\$\\*\\+\\?\\(\\)\\[\\]\\{\\}\\|b/-"
        );
    }

    /// Catches a sweep that removes a young leftover (another lease's save in
    /// progress), `xcrun_db` itself (the cache), or anything in the folders but the
    /// leftovers the module lists; that leaves an old one; or that stops at the first
    /// entry it cannot remove.
    #[test]
    fn the_sweep_removes_only_old_leftovers() {
        let dir = scratch("sweep");
        let f = folders(&dir);
        let old = LEFTOVER_AGE + Duration::from_secs(60);
        let young = Duration::from_secs(60);
        let items = f.temp().join(TEMPORARY_ITEMS);
        let gone = [
            made("NSIRD_swift-build_old/", &items, old),
            made("old-file", &items, old),
            made("xcrun_db-AbC123", f.temp(), old),
        ];
        let kept = [
            made("NSIRD_xcodebuild_young/", &items, young),
            made("xcrun_db-young", f.temp(), young),
            made("xcrun_db", f.temp(), old),
            made("xcrun_dbx", f.temp(), old),
            made("someone-elses", f.temp(), old),
            made("com.apple.something/", f.cache(), old),
        ];
        let stuck = made("NSIRD_stuck/", &items, old);
        let remove = |path: &Path| {
            if path == stuck {
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            kbf_outputs::remove_tree(path)
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
        kbf_outputs::remove_tree(&dir).expect("clean");
        f.sweep(Duration::ZERO, SystemTime::now(), &remove);
    }
}
