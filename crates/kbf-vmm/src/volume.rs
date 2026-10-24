//! Which mounted volume of an attached guest disk is the guest's Data volume.
//!
//! A macOS guest disk mounts several volumes. Two of them hold `private/var/db`: the
//! Data volume, which the guest boots from and writes to, and the sealed System volume,
//! which keeps an empty `private/var/db` stub. A write that lands on the System volume is
//! refused (read-only) or, worse, lands where the booted guest never looks. The choice is
//! therefore made from what each volume is, never from the order the mounts came in.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The file only the System volume carries: the sealed OS's own version record.
pub const SYSTEM_VERSION: &str = "System/Library/CoreServices/SystemVersion.plist";
/// The Setup Assistant marker the offline setup writes into the Data volume.
pub const SETUP_DONE: &str = "private/var/db/.AppleSetupDone";

/// What one mounted volume holds, as far as its role is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub mount: PathBuf,
    /// A real directory `private/var/db` (not a link).
    pub var_db: bool,
    /// The sealed OS's `SystemVersion.plist`: this is the System volume.
    pub system_version: bool,
    /// A real `Users` directory.
    pub users: bool,
    /// The marker the offline setup writes.
    pub setup_done: bool,
}

fn is_real_dir(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn is_real_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

/// Reads the role facts of the volume mounted at `mount`, following no symbolic link
/// at the last step.
#[must_use]
pub fn probe(mount: &Path) -> Facts {
    Facts {
        mount: mount.to_owned(),
        var_db: is_real_dir(&mount.join("private/var/db")),
        system_version: is_real_file(&mount.join(SYSTEM_VERSION)),
        users: is_real_dir(&mount.join("Users")),
        setup_done: is_real_file(&mount.join(SETUP_DONE)),
    }
}

fn describe(all: &[Facts]) -> String {
    let mut text = String::new();
    for f in all {
        let _ = write!(
            text,
            "{} (var/db {}, SystemVersion.plist {}, Users {}, setup marker {}); ",
            f.mount.display(),
            f.var_db,
            f.system_version,
            f.users,
            f.setup_done
        );
    }
    text
}

/// The Data volume among `all`: it holds `private/var/db` and is not the System volume.
/// When several qualify, the one with the setup marker wins, then the one with `Users`;
/// if that still leaves more than one, nothing is chosen.
///
/// # Errors
/// No volume qualifies, or several do and nothing tells them apart. The message lists
/// what each mounted volume holds.
pub fn choose(all: &[Facts]) -> Result<&Facts, String> {
    let mut left: Vec<&Facts> = all
        .iter()
        .filter(|f| f.var_db && !f.system_version)
        .collect();
    for prefer in [
        (|f: &Facts| f.setup_done) as fn(&Facts) -> bool,
        |f: &Facts| f.users,
    ] {
        if left.iter().any(|f| prefer(f)) {
            left.retain(|f| prefer(f));
        }
    }
    match left.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "no mounted volume is the Data volume (one with private/var/db that is not the System volume); mounted: {}",
            describe(all)
        )),
        _ => Err(format!(
            "several mounted volumes could be the Data volume and nothing tells them apart; mounted: {}",
            describe(all)
        )),
    }
}

/// The Data volume among the volumes mounted at `mounts`.
///
/// # Errors
/// As [`choose`].
pub fn data_volume(mounts: &[PathBuf]) -> Result<PathBuf, String> {
    let facts: Vec<Facts> = mounts.iter().map(|m| probe(m)).collect();
    choose(&facts).map(|f| f.mount.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{holds, tmp};

    fn facts(mount: &str, var_db: bool, system_version: bool, users: bool, done: bool) -> Facts {
        Facts {
            mount: mount.into(),
            var_db,
            system_version,
            users,
            setup_done: done,
        }
    }

    /// Catches a chooser that takes the first volume holding `private/var/db`: the sealed
    /// System volume has that directory too, and in a real run it was listed first.
    #[test]
    fn the_system_volume_is_never_chosen_even_when_it_comes_first() {
        let all = [
            facts("/Volumes/Macintosh HD 1", true, true, false, false),
            facts("/Volumes/Data", true, false, true, false),
        ];
        assert_eq!(
            choose(&all).expect("data").mount,
            Path::new("/Volumes/Data")
        );
        let reversed = [all[1].clone(), all[0].clone()];
        assert_eq!(
            choose(&reversed).expect("data").mount,
            Path::new("/Volumes/Data")
        );
    }

    /// Catches a chooser that falls back to the System volume when nothing else
    /// qualifies, or that picks a volume with no `private/var/db`, and an error that does
    /// not say what each volume held.
    #[test]
    fn with_no_data_volume_the_choice_fails_and_lists_the_mounts() {
        let only_system = [
            facts("/Volumes/Preboot", false, false, false, false),
            facts("/Volumes/Macintosh HD 1", true, true, false, false),
        ];
        let e = choose(&only_system).unwrap_err();
        assert!(e.contains("no mounted volume is the Data volume"), "{e}");
        assert!(e.contains("/Volumes/Macintosh HD 1 (var/db true, SystemVersion.plist true"));
        assert!(choose(&[]).is_err());
    }

    /// Catches a chooser that lets the first of two candidates win: with two volumes that
    /// qualify, the setup marker decides, then `Users`, and otherwise it refuses.
    #[test]
    fn two_candidates_are_told_apart_by_the_setup_marker_then_users() {
        let marked = [
            facts("/a", true, false, true, false),
            facts("/b", true, false, false, true),
        ];
        assert_eq!(choose(&marked).expect("b").mount, Path::new("/b"));
        let users = [
            facts("/a", true, false, false, false),
            facts("/b", true, false, true, false),
        ];
        assert_eq!(choose(&users).expect("b").mount, Path::new("/b"));
        let tie = [
            facts("/a", true, false, true, false),
            facts("/b", true, false, true, false),
        ];
        let e = choose(&tie).unwrap_err();
        assert!(e.contains("several mounted volumes"), "{e}");
        assert!(holds(&[e.contains("/a"), e.contains("/b")]), "{e}");
    }

    /// Catches a probe that reads the wrong thing as a role fact: it builds a System
    /// volume, a Data volume and a volume whose `private/var/db` is a link, on disk, and
    /// checks what is read and chosen from the mount paths alone.
    #[test]
    fn the_role_facts_are_read_from_the_mounted_directories() {
        let root = tmp("volume-probe");
        let system = root.join("system");
        let data = root.join("data");
        let linked = root.join("linked");
        std::fs::create_dir_all(system.join("private/var/db")).expect("system db");
        std::fs::create_dir_all(system.join("System/Library/CoreServices")).expect("cs");
        std::fs::write(system.join(SYSTEM_VERSION), "plist").expect("version");
        std::fs::create_dir_all(data.join("private/var/db")).expect("data db");
        std::fs::create_dir_all(data.join("Users")).expect("users");
        std::fs::write(data.join(SETUP_DONE), "").expect("marker");
        std::fs::create_dir_all(linked.join("private/var")).expect("linked");
        std::os::unix::fs::symlink(data.join("private/var/db"), linked.join("private/var/db"))
            .expect("link");
        let s = probe(&system);
        assert!(holds(&[
            s.var_db,
            s.system_version,
            !s.users,
            !s.setup_done
        ]));
        let d = probe(&data);
        assert!(holds(&[d.var_db, !d.system_version, d.users, d.setup_done]));
        let l = probe(&linked);
        assert!(!l.var_db, "a linked private/var/db is not a Data volume");
        let mounts = [system, linked, data.clone()];
        assert_eq!(data_volume(&mounts), Ok(data));
        assert!(data_volume(&mounts[..2]).is_err());
    }
}
