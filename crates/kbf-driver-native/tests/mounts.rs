//! On macOS, a volume mounted below a lease directory does not keep the directory from
//! being removed (issue #178): a system service mounts the Metal toolchain below the
//! home `xcodebuild` is given, read-only, and removing the lease fails under the mount.

#![cfg(target_os = "macos")]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use kbf_driver_native::mounts;
use support::scratch;

/// The mount points of `/sbin/mount` that are below `dir`.
fn mounted_below(dir: &Path) -> Vec<PathBuf> {
    let out = Command::new("/sbin/mount").output().expect("mount");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            line.split_once(" on ")?
                .1
                .split_once(" (")
                .map(|(on, _)| on)
        })
        .map(PathBuf::from)
        .filter(|on| on.starts_with(dir))
        .collect()
}

/// A small disk image mounted at `at`, detached when dropped if it is still there.
struct Image(PathBuf);

impl Image {
    fn attach(image: &Path, at: &Path) -> Self {
        let made = Command::new("/usr/bin/hdiutil")
            .args([
                "create", "-size", "8m", "-fs", "HFS+", "-volname", "kbftest",
            ])
            .arg(image)
            .output()
            .expect("hdiutil create");
        assert!(made.status.success(), "{made:?}");
        std::fs::create_dir_all(at).expect("mount point");
        let attached = Command::new("/usr/bin/hdiutil")
            .arg("attach")
            .arg(image)
            .arg("-mountpoint")
            .arg(at)
            .args(["-nobrowse", "-noverify", "-noautoopen"])
            .output()
            .expect("hdiutil attach");
        assert!(attached.status.success(), "{attached:?}");
        Self(at.to_owned())
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/hdiutil")
            .args(["detach", "-force"])
            .arg(&self.0)
            .output();
    }
}

/// Catches a lease directory that cannot be removed while a volume is mounted below it
/// (the removal fails, as it does under the Metal toolchain `xcodebuild` has mounted
/// in the lease's home), a volume left mounted after the removal, one mounted below
/// another not unmounted first, and a neighbouring lease's volume (`lease-1` and
/// `lease-10`) unmounted with this one's.
#[test]
fn a_volume_mounted_below_a_lease_does_not_keep_it_from_being_removed() {
    let dir = scratch("mounts");
    let lease = dir.join("lease-1");
    let neighbour = dir.join("lease-10");
    // One volume mounted below another: `nested` in the volume `outer` shows.
    let outer = Image::attach(&dir.join("outer.dmg"), &lease.join("home"));
    let nested = Image::attach(&dir.join("nested.dmg"), &lease.join("home/nested"));
    let other = Image::attach(&dir.join("other.dmg"), &neighbour.join("m"));
    assert_eq!(mounted_below(&lease).len(), 2, "{:?}", mounted_below(&dir));

    let removed = mounts::remove_lease_dir(&lease);
    assert!(
        removed.is_ok() && !lease.exists(),
        "{removed:?}; still mounted: {:?}",
        mounted_below(&lease)
    );
    assert_eq!(mounted_below(&lease), Vec::<PathBuf>::new());
    assert_eq!(
        mounted_below(&neighbour),
        vec![neighbour.join("m")],
        "the neighbour's volume stays"
    );
    drop((nested, outer, other));
    kbf_outputs::remove_tree(&dir).expect("clean");
}
