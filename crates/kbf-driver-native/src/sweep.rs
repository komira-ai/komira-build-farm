//! The start-up sweep of the scratch root: every lease directory a previous daemon
//! left behind is removed.
//!
//! A directory the sweep cannot remove does not stop the daemon from starting: an
//! action decides what its lease directory holds, so a failure here is one build step
//! away, and refusing to start would let that step take the node out of the farm. The
//! directory is moved aside into [`QUARANTINE`] instead (out of the way of the lease
//! names), logged as an error, and tried again at every start. One that cannot even be
//! moved stays where it is, also logged; a later lease with the same name then fails
//! loudly rather than silently.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The directory under the scratch root that holds lease directories the sweep could
/// not remove.
pub const QUARANTINE: &str = "quarantine";

/// Removes, with `remove`, every entry of [`QUARANTINE`] and every `lease-*` entry of
/// `scratch`, moving a lease directory that will not go into [`QUARANTINE`].
///
/// # Errors
/// The scratch root cannot be read. A directory that cannot be removed is not an
/// error.
pub(crate) fn sweep(scratch: &Path, remove: &dyn Fn(&Path) -> io::Result<()>) -> io::Result<()> {
    let quarantine = scratch.join(QUARANTINE);
    // Absent until a sweep first needs it.
    if let Ok(entries) = std::fs::read_dir(&quarantine) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Err(why) = remove(&path) {
                tracing::error!(dir = %path.display(), "a quarantined lease directory still cannot be removed: {why}");
            }
        }
    }
    for entry in std::fs::read_dir(scratch)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("lease-") {
            continue;
        }
        let path = entry.path();
        tracing::warn!(dir = %path.display(), "removing a lease directory left behind");
        let Err(why) = remove(&path) else {
            continue;
        };
        match move_aside(&path, &quarantine, &name) {
            Ok(to) => tracing::error!(
                dir = %path.display(),
                to = %to.display(),
                "a lease directory left behind cannot be removed ({why}); moved it aside"
            ),
            Err(moved) => tracing::error!(
                dir = %path.display(),
                "a lease directory left behind cannot be removed ({why}) or moved aside ({moved}); left in place"
            ),
        }
    }
    Ok(())
}

/// Moves `path` into `quarantine` under its `name` and a suffix no earlier start used.
fn move_aside(path: &Path, quarantine: &Path, name: &OsStr) -> io::Result<PathBuf> {
    std::fs::create_dir_all(quarantine)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut aside = name.to_owned();
    aside.push(format!(".{nanos}"));
    let to = quarantine.join(aside);
    std::fs::rename(path, &to)?;
    Ok(to)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("sweep-{name}-{}", std::process::id()));
        // Absent unless a run with this pid left it.
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        // Logging on, so the sweep's messages are written (and their arguments run).
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        dir
    }

    /// Fails for any path whose name is `lease-stuck`, as a directory the action
    /// locked would; removes everything else.
    fn stuck(path: &Path) -> io::Result<()> {
        if path.file_name().is_some_and(|n| n == "lease-stuck") {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        kbf_outputs::remove_tree(path)
    }

    fn quarantined(scratch: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(scratch.join(QUARANTINE))
            .map(|entries| {
                entries
                    .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Catches one lease directory that cannot be removed stopping the daemon from
    /// starting (one build step could brick the node), the stuck directory left where
    /// the next lease could collide with it, and the sweep giving up on the others.
    #[test]
    fn a_directory_that_cannot_be_removed_is_moved_aside() {
        let dir = scratch("aside");
        std::fs::create_dir_all(dir.join("lease-stuck/root")).expect("mkdir");
        std::fs::write(dir.join("lease-stuck/root/out"), b"x").expect("write");
        std::fs::create_dir_all(dir.join("lease-1-1/root")).expect("mkdir");
        std::fs::write(dir.join("keep"), b"not a lease").expect("write");

        sweep(&dir, &stuck).expect("the sweep goes on past a failure");
        assert!(!dir.join("lease-stuck").exists(), "moved out of the way");
        assert!(!dir.join("lease-1-1").exists(), "the others still go");
        assert!(dir.join("keep").exists(), "not a lease directory");
        let aside = quarantined(&dir);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(aside[0].starts_with("lease-stuck."), "{aside:?}");
        let kept = dir.join(QUARANTINE).join(&aside[0]).join("root/out");
        assert_eq!(std::fs::read(kept).expect("moved whole"), b"x");

        // The next start tries again; what still fails stays in quarantine, and the
        // start goes on.
        let fail_all = |_: &Path| Err(io::Error::from_raw_os_error(libc::EPERM));
        sweep(&dir, &fail_all).expect("a stuck quarantine does not stop a start");
        assert_eq!(quarantined(&dir), aside);
        sweep(&dir, &kbf_outputs::remove_tree).expect("sweep");
        assert_eq!(
            quarantined(&dir),
            Vec::<String>::new(),
            "removed once it can be"
        );
    }

    /// Catches a start refused because the stuck directory could not be moved aside
    /// either (the quarantine path taken by a file): the directory stays, logged, and
    /// the start goes on.
    #[test]
    fn a_directory_that_cannot_be_moved_aside_stays() {
        let dir = scratch("stays");
        std::fs::create_dir_all(dir.join("lease-stuck")).expect("mkdir");
        std::fs::write(dir.join(QUARANTINE), b"in the way").expect("write");
        sweep(&dir, &stuck).expect("the start goes on");
        assert!(dir.join("lease-stuck").is_dir());
    }
}
