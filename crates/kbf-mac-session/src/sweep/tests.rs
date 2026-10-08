//! The sweep, on a real filesystem. Tests run unprivileged, so every file they make is
//! the test's own: "the departing uid" is the test's uid to see what is removed, and
//! any other uid to see that nothing is.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn other() -> u32 {
    me() + 1
}

fn root() -> bool {
    me() == 0
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kbf-mac-session-sweep-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn owned_plan(base: &Path) -> SweepPlan {
    SweepPlan {
        named: Vec::new(),
        owned: vec![base.to_path_buf()],
    }
}

/// A shared folder holding a file, a directory tree and an empty directory.
fn populate(base: &Path) {
    fs::write(base.join("file"), "x").unwrap();
    fs::create_dir_all(base.join("tree/a/b")).unwrap();
    fs::write(base.join("tree/a/b/deep"), "x").unwrap();
    fs::create_dir(base.join("empty")).unwrap();
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches: a sweep that leaves the departing uid's files in a shared place (the
/// next user of the uid would own them), or one that removes the shared folder itself.
#[test]
fn everything_the_uid_owns_goes_and_the_folder_stays() {
    let base = scratch("owned").join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let swept = sweep(&owned_plan(&base), "kbf-lease-1-1", me());
    assert_eq!(
        swept,
        Swept {
            removed: 3,
            errors: Vec::new()
        }
    );
    assert!(names(&base).is_empty());
    assert!(base.is_dir());
}

/// Catches: a sweep that removes what another uid owns (another lease's files, the
/// system's), or one that never descends into another owner's directory.
#[test]
fn nothing_of_another_uid_goes() {
    let base = scratch("others").join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let swept = sweep(&owned_plan(&base), "kbf-lease-1-1", other());
    assert_eq!(swept, Swept::default());
    assert_eq!(names(&base), ["empty", "file", "tree"]);
    assert!(base.join("tree/a/b/deep").is_file());
}

/// Catches: following a symbolic link in the sweep (S10: "follow links in the
/// sweep"). Another lease user can plant a link in a shared folder to a place root
/// would then empty; the link must go and its target stay.
#[test]
fn a_planted_link_is_removed_and_its_target_kept() {
    let dir = scratch("link");
    let base = dir.join("Shared");
    let outside = dir.join("outside");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious"), "x").unwrap();
    symlink(&outside, base.join("to-outside")).unwrap();
    symlink(outside.join("precious"), base.join("to-file")).unwrap();
    fs::create_dir(base.join("sub")).unwrap();
    symlink(&outside, base.join("sub/nested-link")).unwrap();

    let swept = sweep(&owned_plan(&base), "kbf-lease-1-1", me());
    assert_eq!(
        swept,
        Swept {
            removed: 3,
            errors: Vec::new()
        }
    );
    assert!(names(&base).is_empty());
    assert_eq!(names(&outside), ["precious"]);

    // A plan directory that is itself a link is not entered either.
    symlink(&outside, dir.join("linked-base")).unwrap();
    let swept = sweep(&owned_plan(&dir.join("linked-base")), "u", me());
    assert_eq!(swept.removed, 0);
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert_eq!(names(&outside), ["precious"]);
}

/// Catches: a named entry (home folder, crontab, launchd's per-uid files) left behind,
/// a template not filled in, or a missing one taken for an error.
#[test]
fn named_entries_go_whole_and_absent_ones_are_fine() {
    let dir = scratch("named");
    fs::create_dir_all(dir.join("Users/kbf-lease-2-7/Library/Caches")).unwrap();
    fs::write(dir.join("Users/kbf-lease-2-7/Library/Caches/c"), "x").unwrap();
    fs::create_dir_all(dir.join("Users/kbf-lease-2-8")).unwrap();
    fs::create_dir(dir.join("tabs")).unwrap();
    fs::write(dir.join("tabs/kbf-lease-2-7"), "* * * * * true\n").unwrap();
    fs::create_dir(dir.join("launchd")).unwrap();
    fs::write(dir.join("launchd/disabled.4242.plist"), "x").unwrap();
    fs::write(dir.join("launchd/disabled.4243.plist"), "x").unwrap();
    let at = |rest: &str| format!("{}/{rest}", dir.display());
    let plan = SweepPlan {
        named: vec![
            at("Users/{user}"),
            at("tabs/{user}"),
            at("launchd/disabled.{uid}.plist"),
            at("launchd/loginitems.{uid}.plist"),
            at("no-such-parent/{user}"),
        ],
        owned: vec![dir.join("no-such-folder")],
    };
    let swept = sweep(&plan, "kbf-lease-2-7", 4242);
    assert_eq!(
        swept,
        Swept {
            removed: 3,
            errors: Vec::new()
        }
    );
    assert_eq!(names(&dir.join("Users")), ["kbf-lease-2-8"]);
    assert!(names(&dir.join("tabs")).is_empty());
    assert_eq!(names(&dir.join("launchd")), ["disabled.4243.plist"]);

    let root_path = SweepPlan {
        named: vec!["/".to_owned()],
        owned: Vec::new(),
    };
    let swept = sweep(&root_path, "u", 1);
    assert_eq!(swept.errors, ["/: not a path to an entry"]);
}

/// Catches: an error swallowed (a sweep reported complete with entries left), or a
/// walk that stops at the first stuck entry.
#[test]
fn places_that_cannot_be_read_are_errors_and_the_walk_goes_on() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("errors");
    let locked = dir.join("locked");
    fs::create_dir(&locked).unwrap();
    let base = dir.join("Shared");
    fs::create_dir(&base).unwrap();
    fs::create_dir(base.join("no-search")).unwrap();
    fs::write(base.join("no-search/f"), "x").unwrap();
    fs::create_dir(base.join("no-read")).unwrap();
    fs::write(base.join("file"), "x").unwrap();
    chmod(&base.join("no-search"), 0o400);
    chmod(&base.join("no-read"), 0o000);
    chmod(&locked, 0o000);
    fs::create_dir(dir.join("tabs")).unwrap();
    fs::write(dir.join("tabs/u"), "x").unwrap();
    chmod(&dir.join("tabs"), 0o600);

    let plan = SweepPlan {
        named: vec![format!("{}/tabs/{{user}}", dir.display())],
        owned: vec![locked.clone(), base.clone()],
    };
    // Another uid: the walk descends, so every unreadable directory is an error (a
    // directory without search permission cannot even be listed).
    let swept = sweep(&plan, "u", other());
    let mut errors = swept.errors.clone();
    errors.sort();
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(errors[0].contains("Shared/no-read:"), "{errors:?}");
    assert!(errors[1].contains("Shared/no-search:"), "{errors:?}");
    assert!(errors[2].contains("/locked"), "{errors:?}");
    assert!(errors[3].contains("/tabs/u"), "{errors:?}");
    assert_eq!(swept.removed, 0);

    // The departing uid: what it owns goes where it can, the rest is reported.
    let swept = sweep(&owned_plan(&base), "u", me());
    let mut errors = swept.errors;
    errors.sort();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].contains("Shared/no-read"), "{errors:?}");
    assert!(errors[1].contains("Shared/no-search"), "{errors:?}");
    assert_eq!(swept.removed, 1);
    assert!(!base.join("file").exists());

    for path in [
        &base.join("no-search"),
        &base.join("no-read"),
        &locked,
        &dir.join("tabs"),
    ] {
        chmod(path, 0o700);
    }
}

/// Catches: unbounded recursion, by which a lease user could exhaust the helper's
/// stack or descriptors with a deep tree. The departing user's own deep tree fails
/// its deletion closed; another user's deep tree is not searched below the limit, so
/// it cannot stop every other lease's deletion (a CI run showed one doing just that).
#[test]
fn deep_trees_are_bounded_without_blocking_other_users() {
    let base = scratch("deep").join("Shared");
    let mut deep = base.clone();
    for _ in 0..=MAX_DEPTH + 1 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("f"), "x").unwrap();
    // Another uid's view: the tree is someone else's; searched to the limit, no error.
    let swept = sweep(&owned_plan(&base), "u", other());
    assert_eq!(swept, Swept::default());
    // The departing uid's own tree: removing it would go below the limit.
    let swept = sweep(&owned_plan(&base), "u", me());
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert!(
        swept.errors[0].contains("nested deeper than 256"),
        "{:?}",
        swept.errors
    );
    assert!(deep.join("f").is_file());
    fs::remove_dir_all(base.parent().unwrap()).unwrap();
}

/// Catches: a walk that crosses into another mounted filesystem, or that removes a
/// directory other than the one it examined (one swapped in by a rename).
#[test]
fn another_filesystem_or_a_swapped_directory_is_not_entered() {
    let base = scratch("dev").join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let dir = open_base(&base).unwrap().unwrap();
    let dev = rustix::fs::fstat(&dir).unwrap().st_dev;
    let mut swept = Swept::default();
    // Every entry looks as if it were on another filesystem: none is touched.
    walk_owned(&dir, &base, me(), dev + 1, 0, &mut swept).unwrap();
    assert_eq!(swept, Swept::default());
    assert_eq!(names(&base), ["empty", "file", "tree"]);

    let stat = rustix::fs::statat(&dir, "tree", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let error = remove_entry(&dir, OsStr::new("tree"), &stat, me(), dev + 1, 1).unwrap_err();
    assert!(error.to_string().contains("another filesystem"), "{error}");

    let empty = rustix::fs::statat(&dir, "empty", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let error = open_child(&dir, OsStr::new("tree"), &empty).unwrap_err();
    assert!(error.to_string().contains("replaced"), "{error}");
    assert_eq!(names(&base), ["empty", "file", "tree"]);
}

#[test]
fn the_macos_plan_names_the_places_of_the_design() {
    let plan = SweepPlan::macos();
    assert!(
        plan.named
            .contains(&"/private/var/at/tabs/{user}".to_owned())
    );
    assert!(
        plan.named
            .contains(&"/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist".to_owned())
    );
    assert!(plan.owned.contains(&Path::new("/Users").join("Shared")));
    assert!(plan.owned.contains(&PathBuf::from("/private/var/folders")));
}
