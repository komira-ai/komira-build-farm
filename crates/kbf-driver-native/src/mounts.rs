//! Volumes mounted below a lease directory, and their unmounting before the directory
//! is removed.
//!
//! A system service can mount a volume on an action's behalf, below a path the action
//! named, without a write the sandbox profile checks: `xcodebuild` run with
//! `CFFIXED_USER_HOME` set to a directory of its lease has the asset service mount the
//! Metal toolchain below `<home>/Library/Developer/DVTDownloads/MetalToolchain/mounts`
//! (issue #178). The mounted volume is read-only, so removing the lease directory
//! fails (`EROFS`) and the lease is quarantined with an image still mounted. The
//! volume is mounted for the action's user, which may unmount it, so the driver
//! unmounts every volume at or below the lease directory first, deepest first.
//!
//! Only macOS lists mounts here; elsewhere there is nothing to unmount.

use std::path::{Path, PathBuf};

/// The mount points in `mounts` that are `dir` or below it, deepest first (an inner
/// mount must go before the one it is in). Compared by path component, so `lease-1`
/// does not take `lease-10`'s mounts.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn below(mounts: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = mounts
        .iter()
        .filter(|mount| mount.starts_with(dir))
        .cloned()
        .collect();
    found.sort_by_key(|mount| std::cmp::Reverse(mount.components().count()));
    found
}

/// Unmounts every volume mounted at or below `dir`, deepest first, trying every one
/// before it gives up. The mounts that were unmounted.
///
/// # Errors
/// What could not be listed or unmounted, each named.
#[cfg(target_os = "macos")]
pub fn unmount_below(dir: &Path) -> Result<Vec<PathBuf>, String> {
    // The mount table holds real paths.
    let Ok(dir) = std::fs::canonicalize(dir) else {
        return Ok(Vec::new());
    };
    let mounts = mounted().map_err(|e| format!("list the mounts: {e}"))?;
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for mount in below(&mounts, &dir) {
        match unmount(&mount) {
            Ok(()) => done.push(mount),
            Err(why) => failed.push(format!("unmount {}: {why}", mount.display())),
        }
    }
    if failed.is_empty() {
        Ok(done)
    } else {
        Err(failed.join("; "))
    }
}

/// Removes the lease directory (or quarantined entry) `dir`: unmounts what a system
/// service mounted below it, then removes the tree (`kbf_outputs::remove_tree`). A
/// mount that cannot be unmounted is logged and the removal tried anyway, which fails
/// and quarantines the directory as any other it cannot remove.
///
/// # Errors
/// The tree cannot be removed.
pub fn remove_lease_dir(dir: &Path) -> std::io::Result<()> {
    unmount_logged(dir);
    kbf_outputs::remove_tree(dir)
}

/// [`unmount_below`], what it did and what failed logged.
#[cfg(target_os = "macos")]
pub(crate) fn unmount_logged(dir: &Path) {
    match unmount_below(dir) {
        Ok(done) => {
            for mount in done {
                tracing::warn!(mount = %mount.display(), "unmounted a volume mounted below a lease directory");
            }
        }
        Err(why) => {
            tracing::error!(dir = %dir.display(), "a mount below the directory stays: {why}")
        }
    }
}

/// Nothing: only macOS lists mounts here.
#[cfg(not(target_os = "macos"))]
pub(crate) fn unmount_logged(_dir: &Path) {}

/// Unmounts `mount` by the `unmount` system call, forced, and if the kernel refuses
/// by `diskutil unmount force`, which asks `diskarbitrationd` to do it for the user
/// that mounted it.
#[cfg(target_os = "macos")]
fn unmount(mount: &Path) -> Result<(), String> {
    by_syscall(mount).or_else(|syscall| {
        by_diskutil(mount).map_err(|diskutil| format!("{syscall}; diskutil: {diskutil}"))
    })
}

/// [`unmount`] by the system call alone.
///
/// # Errors
/// The kernel refused, or the path has a NUL.
#[cfg(target_os = "macos")]
pub fn by_syscall(mount: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(mount.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let rc = unsafe { libc::unmount(path.as_ptr(), libc::MNT_FORCE) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("unmount: {}", std::io::Error::last_os_error()))
    }
}

/// [`unmount`] by `diskutil unmount force` alone.
///
/// # Errors
/// `diskutil` could not run or said no.
#[cfg(target_os = "macos")]
pub fn by_diskutil(mount: &Path) -> Result<(), String> {
    let out = std::process::Command::new("/usr/sbin/diskutil")
        .args(["unmount", "force"])
        .arg(mount)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The mount points of every mounted volume, by their real paths.
#[cfg(target_os = "macos")]
fn mounted() -> std::io::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt as _;

    // The table can change between asking its size and reading it: ask again with room
    // to spare, and retry if it still grew.
    for _ in 0..5 {
        // SAFETY: a null buffer asks only for the number of mounted volumes.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        let count = usize::try_from(count).map_err(|_| std::io::Error::last_os_error())?;
        let room = count + 8;
        // SAFETY: `statfs` is plain data; all-zero is a valid value.
        let mut table: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; room];
        let bytes = libc::c_int::try_from(room * std::mem::size_of::<libc::statfs>())
            .map_err(std::io::Error::other)?;
        // SAFETY: `table` holds `bytes` bytes, the size `getfsstat` writes at most.
        let wrote = unsafe { libc::getfsstat(table.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        let wrote = usize::try_from(wrote).map_err(|_| std::io::Error::last_os_error())?;
        if wrote >= room {
            continue;
        }
        return Ok(table[..wrote]
            .iter()
            .map(|fs| {
                let name = &fs.f_mntonname;
                let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                // SAFETY: `c_char` and `u8` have the same size; the slice is the
                // bytes of the name up to its NUL.
                let bytes = unsafe { std::slice::from_raw_parts(name.as_ptr().cast::<u8>(), len) };
                PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
            })
            .collect());
    }
    Err(std::io::Error::other("the mount table kept growing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    /// Catches an unmount list that takes a sibling directory sharing a name prefix
    /// (`lease-10` for `lease-1`), leaves one below the directory, forgets one at the
    /// directory itself, or puts an outer mount before the inner one it holds (the
    /// outer cannot go while the inner is mounted).
    #[test]
    fn the_mounts_below_a_directory_come_deepest_first() {
        let mounts = paths(&[
            "/",
            "/scratch/lease-10/m",
            "/scratch/lease-1/home/a",
            "/scratch/lease-1",
            "/scratch/lease-1/home/a/b/c",
            "/scratch/other",
        ]);
        assert_eq!(
            below(&mounts, Path::new("/scratch/lease-1")),
            paths(&[
                "/scratch/lease-1/home/a/b/c",
                "/scratch/lease-1/home/a",
                "/scratch/lease-1"
            ])
        );
        assert_eq!(below(&mounts, Path::new("/nowhere")), Vec::<PathBuf>::new());
    }
}
