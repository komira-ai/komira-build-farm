//! The sweep, on a real filesystem. Tests run unprivileged, so every file they make is
//! the test's own: "the departing uid" is the test's uid to see what is removed, and
//! any other uid to see that nothing is.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};

use super::*;
use kbf_testkit::dir::TestDir;

/// A point of a sweep at which a test makes entries vanish (see [`vanishing`]).
pub(super) enum Point<'a> {
    /// A directory was listed; the walk goes on with the names it read.
    Listed,
    /// The entry of this name was examined; the walk goes on with what it found.
    Examined(&'a OsStr),
}

thread_local! {
    /// Whether the sweep gives an owned directory with no permission back its `rwx`.
    static UNLOCKING: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

pub(super) fn unlocking() -> bool {
    UNLOCKING.with(std::cell::Cell::get)
}

/// A test's hook: the point reached, and how many listings the sweep has made so far.
type Hook = Box<dyn FnMut(&Point<'_>, usize)>;

thread_local! {
    /// This thread's hook and its count of listings.
    static HOOK: RefCell<Option<(usize, Hook)>> = const { RefCell::new(None) };
}

/// Called by the sweep at each [`Point`] (see `super::listed` and `super::examine`).
pub(super) fn reached(point: Point<'_>) {
    // Taken out while it runs, so a hook that walks cannot re-enter itself.
    let Some((mut listings, mut hook)) = HOOK.with_borrow_mut(Option::take) else {
        return;
    };
    listings += usize::from(matches!(point, Point::Listed));
    hook(&point, listings);
    HOOK.with_borrow_mut(|slot| *slot = Some((listings, hook)));
}

/// Runs `run` with `hook` called at each point its sweeps reach.
fn vanishing<T>(hook: impl FnMut(&Point<'_>, usize) + 'static, run: impl FnOnce() -> T) -> T {
    HOOK.with_borrow_mut(|slot| *slot = Some((0, Box::new(hook))));
    let out = run();
    HOOK.with_borrow_mut(|slot| *slot = None);
    out
}

/// A hook that runs `act` right after the sweep's `n`th listing.
fn after_listing(n: usize, mut act: impl FnMut() + 'static) -> impl FnMut(&Point<'_>, usize) {
    move |point, listings| {
        if matches!(point, Point::Listed) && listings == n {
            act();
        }
    }
}

/// A hook that removes `path` right after the sweep examined an entry named `name`.
fn after_examining(name: &'static str, path: PathBuf) -> impl FnMut(&Point<'_>, usize) {
    move |point, _| {
        if matches!(point, Point::Examined(seen) if *seen == name) {
            remove(&path);
        }
    }
}

/// Removes `path`, a file or a whole directory, if it is there.
fn remove(path: &Path) {
    let _ = fs::remove_file(path).or_else(|_| fs::remove_dir_all(path));
}

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn other() -> u32 {
    me() + 1
}

fn root() -> bool {
    me() == 0
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-mac-session-sweep-{name}"),
    )
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
    let _base_own = scratch("owned");
    let base = _base_own.join("Shared");
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
    let _base_own = scratch("others");
    let base = _base_own.join("Shared");
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

/// Catches: an error swallowed (a sweep reported complete with entries left), a walk that
/// stops at the first stuck entry, and another owner's unreadable directory failing this
/// uid's sweep (the shared places hold other leases' directories).
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
    // Another uid: the walk descends, and a directory it cannot search below the plan's
    // folders belongs to someone else (another lease may be flipping it): logged and
    // skipped, never this uid's failure. The plan's own places are still errors.
    let swept = sweep(&plan, "u", other());
    let mut errors = swept.errors.clone();
    errors.sort();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors[0].contains("/locked"), "{errors:?}");
    assert!(errors[1].contains("/tabs/u"), "{errors:?}");
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

/// Catches a deep tree that makes the sweep fail (a lease builds 300 directories to cost
/// its slot for good), a search of another owner's deep tree that fails or blocks, and a
/// removal that recurses without a bound: another uid's view of the tree is searched to
/// the limit with no error and no removal; the departing uid's own tree is removed whole.
#[test]
fn deep_trees_are_removed_and_other_owners_are_searched_to_a_limit() {
    let _base_own = scratch("deep");
    let base = _base_own.join("Shared");
    let mut deep = base.clone();
    for _ in 0..=MAX_DEPTH + 1 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("f"), "x").unwrap();
    // Another uid's view: the tree is someone else's; searched to the limit, no error.
    let swept = sweep(&owned_plan(&base), "u", other());
    assert_eq!(swept, Swept::default());
    // The departing uid's own tree: removed, whatever its depth.
    let swept = sweep(&owned_plan(&base), "u", me());
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(names(&base), Vec::<String>::new());
}

/// Makes `levels` directories below `top`, one inside the next, with a file in the last,
/// through descriptors (a path that long does not fit in one call).
fn build_deep(top: &Path, levels: usize) {
    let mut fd = rustix::fs::openat(rustix::fs::CWD, top, DIRECTORY, Mode::empty()).unwrap();
    for _ in 0..levels {
        rustix::fs::mkdirat(&fd, "d", Mode::from_raw_mode(0o700)).unwrap();
        fd = rustix::fs::openat(&fd, "d", DIRECTORY, Mode::empty()).unwrap();
    }
    let leaf = rustix::fs::openat(
        &fd,
        "leaf",
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    );
    drop(leaf.unwrap());
}

/// Catches a removal that cannot take a tree deeper than its recursion bound and one that
/// loses part of it: a tree 1000 directories
/// deep, with a file at every hundredth level, is removed by `empty`, by `sweep`, by a
/// named entry, and by a kept folder, leaving nothing and nothing parked behind.
#[test]
fn a_tree_a_thousand_deep_is_removed_by_every_walk() {
    for how in ["empty", "sweep", "named", "kept"] {
        let dir = scratch(&format!("thousand-{how}"));
        let top = dir.join("top");
        fs::create_dir(&top).unwrap();
        build_deep(&top, 1000);
        let swept = match how {
            "empty" => empty(&top, me()),
            "sweep" => sweep(&owned_plan(&dir), "u", me()),
            "named" => {
                let plan = SweepPlan {
                    named: vec![format!("{}/d", top.display())],
                    owned: Vec::new(),
                };
                sweep(&plan, "u", me())
            }
            _ => empty_keeping(&dir, me(), &["top"]),
        };
        assert_eq!(swept.errors, Vec::<String>::new(), "{how}");
        let left = match how {
            "named" => names(&top),
            "sweep" => names(&dir),
            "kept" => names(&top),
            _ => names(&top),
        };
        assert_eq!(
            left,
            Vec::<String>::new(),
            "{how}: nothing is left or parked"
        );
    }
}

/// Catches: a walk that crosses into another mounted filesystem, or that removes a
/// directory other than the one it examined (one swapped in by a rename).
#[test]
fn another_filesystem_or_a_swapped_directory_is_not_entered() {
    let _base_own = scratch("dev");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    populate(&base);
    let dir = open_base(&base).unwrap().unwrap();
    let dev = rustix::fs::fstat(&dir).unwrap().st_dev;
    let mut swept = Swept::default();
    // Every entry looks as if it were on another filesystem: none is touched.
    walk_owned(
        &dir,
        &base,
        (me(), dev + 1),
        (0, &Park::new(&dir)),
        &mut swept,
    )
    .unwrap();
    assert_eq!(swept, Swept::default());
    assert_eq!(names(&base), ["empty", "file", "tree"]);

    let stat = rustix::fs::statat(&dir, "tree", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let error = remove_entry(
        &dir,
        OsStr::new("tree"),
        &base.join("tree"),
        &stat,
        (me(), dev + 1, 1),
        &Park::new(&dir),
    )
    .unwrap_err();
    assert!(error.to_string().contains("another filesystem"), "{error}");

    let empty = rustix::fs::statat(&dir, "empty", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    let error = open_child(&dir, OsStr::new("tree"), &empty).unwrap_err();
    assert!(error.to_string().contains("replaced"), "{error}");
    assert_eq!(names(&base), ["empty", "file", "tree"]);
}

#[test]
fn the_macos_plan_names_the_places_of_the_design() {
    let schedules = SweepPlan::macos_schedules();
    assert!(
        schedules
            .named
            .contains(&"/private/var/at/tabs/{user}".to_owned())
    );
    assert!(
        schedules
            .owned
            .contains(&PathBuf::from("/private/var/at/jobs"))
    );
    let plan = SweepPlan::macos();
    assert!(
        plan.named
            .contains(&"/private/var/db/com.apple.xpc.launchd/loginitems.{uid}.plist".to_owned())
    );
    assert!(plan.owned.contains(&Path::new("/Users").join("Shared")));
    assert!(plan.owned.contains(&PathBuf::from("/private/var/folders")));
}

/// Catches: an emptied folder that keeps an entry because the uid does not own it (a
/// reused user's home would keep what root or another uid left in it), one that
/// removes the folder itself or follows a link out of it, an absent folder taken for
/// an error, and an entry that cannot be removed swallowed or stopping the rest.
#[test]
fn empty_removes_every_entry_whoever_owns_it_and_keeps_the_folder() {
    let dir = scratch("empty");
    let home = dir.join("home");
    fs::create_dir(&home).unwrap();
    populate(&home);
    let outside = dir.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "x").unwrap();
    symlink(&outside, home.join("link")).unwrap();
    // The entries are the test's, not `other()`'s: what is emptied goes whoever owns it.
    let swept = empty(&home, other());
    assert_eq!(
        swept,
        Swept {
            removed: 4,
            errors: Vec::new()
        }
    );
    assert!(names(&home).is_empty());
    assert!(home.is_dir());
    assert!(outside.join("kept").exists(), "the link was followed");
    assert_eq!(empty(&dir.join("absent"), me()), Swept::default());
    if root() {
        eprintln!("skipped the error half: permission bits do not stop root");
        return;
    }
    fs::create_dir(home.join("locked")).unwrap();
    fs::write(home.join("locked/f"), "x").unwrap();
    fs::write(home.join("file"), "x").unwrap();
    // Open to the user, but nothing in it can be removed.
    chmod(&home.join("locked"), 0o500);
    let swept = empty(&home, me());
    chmod(&home.join("locked"), 0o700);
    assert_eq!(swept.removed, 1, "{swept:?}");
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("home/locked:"), "{swept:?}");
    assert_eq!(names(&home), ["locked"]);
}

/// Catches (kbf#652): a sweep that records an error for an entry which
/// vanished between the listing of its folder and its examination, so `user-delete`
/// refused with "the sweep for ... is incomplete: ...: No such file or directory".
/// First the departing uid's own entries (gone before the walk examines them; the one
/// still there is removed), then the case of the issue: a file of another owner deep
/// in another owner's folders (`triald`'s temporary file under the lease user's
/// `/private/var/folders/.../T`), which the walk only examines.
#[test]
fn an_entry_that_vanishes_after_the_listing_is_not_an_error() {
    let _base_own = scratch("vanish-listed");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    fs::write(base.join("daemon-temp"), "x").unwrap();
    fs::create_dir_all(base.join("daemon-dir/inner")).unwrap();
    fs::write(base.join("kept-until-swept"), "x").unwrap();
    let (temp, dir) = (base.join("daemon-temp"), base.join("daemon-dir"));
    let swept = vanishing(
        after_listing(1, move || {
            remove(&temp);
            remove(&dir);
        }),
        || sweep(&owned_plan(&base), "kbf-lease-10-2", me()),
    );
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(swept.removed, 1, "{swept:?}");
    assert!(names(&base).is_empty());

    // `Shared` is listing 1, `T` 2, `com.apple.triald` 3.
    let triald = base.join("T/com.apple.triald");
    fs::create_dir_all(&triald).unwrap();
    fs::write(triald.join("TRIFileBackedMutableStringArray"), "x").unwrap();
    fs::write(triald.join("kept"), "x").unwrap();
    let gone = triald.join("TRIFileBackedMutableStringArray");
    let swept = vanishing(after_listing(3, move || remove(&gone)), || {
        sweep(&owned_plan(&base), "kbf-lease-10-2", other())
    });
    assert_eq!(swept, Swept::default());
    assert_eq!(names(&triald), ["kept"]);
}

/// Catches (kbf#652): a directory being emptied whose entries vanish
/// between its listing and their examination (the same daemon, one level down) that
/// fails the sweep, or one that leaves the directory behind because of it; and the
/// same for `empty`, which clears the lease user's home. The vanished entries are the
/// second listing's, the one of the directory being removed.
#[test]
fn entries_that_vanish_while_a_directory_is_emptied_are_not_an_error() {
    let _base_own = scratch("vanish-emptied");
    let base = _base_own.join("Shared");
    let tree = base.join("tree");
    for (run, uid) in [("sweep", me()), ("empty", other())] {
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/f"), "x").unwrap();
        fs::write(tree.join("g"), "x").unwrap();
        let gone = tree.clone();
        // Both list `Shared` (1), then `tree` (2): the entries go in between.
        let hook = after_listing(2, move || {
            remove(&gone.join("g"));
            remove(&gone.join("sub"));
        });
        let swept = vanishing(hook, || {
            if run == "sweep" {
                sweep(&owned_plan(&base), "u", uid)
            } else {
                empty(&base, uid)
            }
        });
        assert_eq!(swept.errors, Vec::<String>::new(), "{run}");
        assert_eq!(swept.removed, 1, "{run}: {swept:?}");
        assert!(
            names(&base).is_empty(),
            "{run}: the emptied directory stayed"
        );
    }
}

/// Catches (kbf#652): `empty` (the home folder) recording an error for an
/// entry gone since the listing, or counting it as removed.
#[test]
fn empty_passes_over_an_entry_gone_since_the_listing() {
    let _home_own = scratch("vanish-home");
    let home = _home_own.join("home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("gone"), "x").unwrap();
    fs::write(home.join("file"), "x").unwrap();
    let gone = home.join("gone");
    let swept = vanishing(after_listing(1, move || remove(&gone)), || {
        empty(&home, other())
    });
    assert_eq!(
        swept,
        Swept {
            removed: 1,
            errors: Vec::new()
        }
    );
    assert!(names(&home).is_empty());
}

/// Catches (kbf#652): a directory gone between its examination and its
/// opening that fails the sweep: another owner's directory the walk searches, and one
/// of the departing uid it removes.
#[test]
fn a_directory_gone_since_it_was_examined_holds_nothing() {
    let _base_own = scratch("vanish-open");
    let base = _base_own.join("Shared");
    for (what, uid) in [("searched", other()), ("removed", me())] {
        fs::create_dir_all(base.join("dir/sub")).unwrap();
        fs::write(base.join("kept"), "x").unwrap();
        let swept = vanishing(after_examining("dir", base.join("dir")), || {
            sweep(&owned_plan(&base), "u", uid)
        });
        assert_eq!(swept.errors, Vec::<String>::new(), "{what}");
        assert!(!base.join("dir").exists(), "{what}");
        let _ = fs::remove_file(base.join("kept"));
    }
}

/// Catches (kbf#652): a removal step that fails on an entry gone since
/// it was examined (the unlink or the directory's opening answers "no such file").
/// The entry is examined, then removed by someone else, then removal runs on the stale
/// examination: no listing is involved, so this is the narrowest window.
#[test]
fn an_entry_gone_since_it_was_examined_is_removed_already() {
    let _base_own = scratch("vanish-stale");
    let base = _base_own.join("Shared");
    fs::create_dir_all(base.join("dir")).unwrap();
    fs::write(base.join("file"), "x").unwrap();
    let dir = open_base(&base).unwrap().expect("the folder exists");
    let dev = rustix::fs::fstat(&dir).unwrap().st_dev;
    for name in ["file", "dir"] {
        let stat = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        remove(&base.join(name));
        remove_entry(
            &dir,
            OsStr::new(name),
            &base.join(name),
            &stat,
            (me(), dev, 1),
            &Park::new(&dir),
        )
        .unwrap_or_else(|why| panic!("{name}: {why}"));
    }
}

/// Catches (kbf#652): a directory removed after the walk opened it that
/// fails the walk when listed. The walk relies on rustix's `Dir`, which reads a removed
/// directory as empty (its own fallback for the "no such file or directory" the kernel
/// answers), so this pins that.
#[test]
fn listing_a_directory_removed_since_it_was_opened_finds_nothing() {
    let _dir_own = scratch("vanish-listing");
    let dir = _dir_own.join("dir");
    fs::create_dir(&dir).unwrap();
    let opened = open_base(&dir).unwrap().expect("the directory exists");
    fs::remove_dir(&dir).unwrap();
    assert_eq!(listed(&opened).unwrap(), Vec::<OsString>::new());
}

/// Catches (kbf#652): a fix that swallows more than "no such file": a
/// folder the sweep cannot read (permission denied) must still leave the sweep
/// incomplete when another entry vanishes in the same walk, and the vanished entry
/// must not add an error of its own.
#[test]
fn a_vanished_entry_does_not_hide_a_real_error() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let _base_own = scratch("vanish-real");
    let base = _base_own.join("Shared");
    fs::create_dir(&base).unwrap();
    fs::write(base.join("daemon-temp"), "x").unwrap();
    fs::create_dir(base.join("locked")).unwrap();
    fs::write(base.join("locked/f"), "x").unwrap();
    chmod(&base.join("locked"), 0o000);
    let temp = base.join("daemon-temp");
    let swept = vanishing(after_listing(1, move || remove(&temp)), || {
        sweep(&owned_plan(&base), "u", me())
    });
    chmod(&base.join("locked"), 0o700);
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("Shared/locked: "), "{swept:?}");
    assert!(!swept.errors[0].contains("No such file"), "{swept:?}");
}

/// A `/var/folders` entry as macOS makes it: the kept folders `0`, `C`, `T` and `X`,
/// each holding a file and a tree, and the stray `junk` file, `stray` directory and
/// `T`-named link the entry root must not keep.
fn entry_root(dir: &Path) -> PathBuf {
    let entry = dir.join("entry");
    fs::create_dir(&entry).unwrap();
    for kept in ["0", "C", "T", "X"] {
        fs::create_dir_all(entry.join(kept).join("sub/deeper")).unwrap();
        fs::write(entry.join(kept).join("file"), "x").unwrap();
        fs::write(entry.join(kept).join("sub/deeper/leaf"), "x").unwrap();
    }
    fs::write(entry.join("junk"), "x").unwrap();
    fs::create_dir_all(entry.join("stray/inside")).unwrap();
    fs::write(entry.join("stray/inside/f"), "x").unwrap();
    entry
}

/// Catches the entry sweep that removes the kept folders themselves (the next lease's
/// `getconf DARWIN_USER_TEMP_DIR` would name a folder that is gone), the one that
/// empties only the kept folders and leaves a file or folder of another name in the
/// entry root (a pool user's file there survived the clean: `junk` and `stray` here), one that keeps a folder that is
/// not named, and one that walks through a link planted under a kept name.
#[test]
fn empty_keeping_empties_the_named_folders_and_removes_everything_else() {
    let dir = scratch("keeping");
    let entry = entry_root(&dir);
    let outside = dir.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "x").unwrap();
    // `Y` is not named: a link under a kept name is not a folder either.
    fs::remove_dir_all(entry.join("X")).unwrap();
    symlink(&outside, entry.join("X")).unwrap();
    let swept = empty_keeping(&entry, me(), &["0", "C", "T", "X"]);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(names(&entry), ["0", "C", "T"]);
    for kept in ["0", "C", "T"] {
        assert_eq!(names(&entry.join(kept)), Vec::<String>::new(), "{kept}");
    }
    assert!(outside.join("kept").exists(), "the link was followed");
    // Nothing named: the entry is emptied whole.
    let swept = empty_keeping(&entry, me(), &[]);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert!(names(&entry).is_empty());
}

/// Catches a kept folder whose contents the sweep cannot remove hiding behind the
/// removal of the rest: the error names the kept folder's entry and the other entries
/// still go.
#[test]
fn a_kept_folder_that_cannot_be_emptied_is_an_error_and_the_rest_goes() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("keeping-error");
    let entry = entry_root(&dir);
    fs::create_dir(entry.join("T/locked")).unwrap();
    fs::write(entry.join("T/locked/f"), "x").unwrap();
    chmod(&entry.join("T/locked"), 0o500);
    let swept = empty_keeping(&entry, me(), &["0", "C", "T", "X"]);
    chmod(&entry.join("T/locked"), 0o700);
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("entry/T/locked:"), "{swept:?}");
    assert_eq!(names(&entry), ["0", "C", "T", "X"]);
    assert_eq!(names(&entry.join("T")), ["locked"]);
    assert!(names(&entry.join("C")).is_empty());
}

/// Catches a kept folder removed under the sweep (gone since it was examined) read as an
/// error, as the plain walk treats a vanished entry.
#[test]
fn a_kept_folder_gone_since_it_was_examined_holds_nothing() {
    let dir = scratch("keeping-gone");
    let entry = entry_root(&dir);
    let gone = entry.join("C");
    let swept = vanishing(after_examining("C", gone), || {
        empty_keeping(&entry, me(), &["C"])
    });
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert!(names(&entry).is_empty());
}

/// Catches a mode that stays 0 (the owner's `u+rwx` is not set on what the departing
/// uid owns, so the removal that follows could not enter it as that uid), a clear that
/// changes what another uid owns (a hard link to a system file keeps its mode: here an
/// entry the walk does not count as the uid's), one that follows a link out of the
/// tree, one that walks into another mode's folder without listing it, and a walk that
/// stops at the first error.
#[test]
fn clear_flags_makes_the_uids_entries_rwx_and_leaves_other_owners_alone() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("clear");
    let home = dir.join("home");
    fs::create_dir_all(home.join("a/b")).unwrap();
    fs::write(home.join("a/b/f"), "x").unwrap();
    fs::write(home.join("g"), "x").unwrap();
    let outside = dir.join("outside");
    fs::write(&outside, "x").unwrap();
    chmod(&outside, 0o600);
    symlink(&outside, home.join("link")).unwrap();
    chmod(&home.join("a/b/f"), 0o400);
    chmod(&home.join("g"), 0o400);
    chmod(&home.join("a/b"), 0o500);
    chmod(&home, 0o500);
    let swept = clear_flags(&home, me());
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let modes = [
        mode(&home.join("a/b/f")),
        mode(&home.join("g")),
        mode(&home.join("a/b")),
        mode(&outside),
    ];
    // Another uid's entries: counted nowhere, changed nowhere.
    let others = clear_flags(&home, other());
    chmod(&home, 0o700);
    chmod(&home.join("a/b"), 0o700);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(modes, [0o700, 0o700, 0o700, 0o600]);
    assert_eq!(swept.removed, 5, "a, b, f, g and the link: {swept:?}");
    assert_eq!(others.removed, 0, "{others:?}");
    assert_eq!(clear_flags(&dir.join("absent"), me()), Swept::default());
}

/// A directory its owner left with no permission at all cannot be opened by the owner,
/// and a clean that runs as the uid must still give it `u+rwx` and look below it.
/// Catches a clear that skips what it cannot open: the directory stays at 0, and the
/// walk never sees the locked file below it.
#[test]
fn clear_flags_opens_a_directory_its_owner_left_without_permissions() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("clear-mode-zero");
    let home = dir.join("home");
    fs::create_dir_all(home.join("closed/inner")).unwrap();
    fs::write(home.join("closed/inner/f"), "x").unwrap();
    chmod(&home.join("closed/inner"), 0o000);
    chmod(&home.join("closed"), 0o000);
    let swept = clear_flags(&home, me());
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let modes = [mode(&home.join("closed")), mode(&home.join("closed/inner"))];
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(modes, [0o700, 0o700]);
}

/// Catches a flag clear that fails on a deep tree (a lease that builds one would cost its
/// slot for good: the removal that follows clears the locks itself) or that recurses
/// without a bound: past the limit it stops looking, with no error, for either owner.
#[test]
fn clear_flags_stops_at_its_depth_limit_without_an_error() {
    let dir = scratch("clear-deep");
    let home = dir.join("home");
    fs::create_dir(&home).unwrap();
    let mut deep = home.clone();
    for _ in 0..=MAX_DEPTH + 1 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    let swept = clear_flags(&home, me());
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    // Another owner's deep tree is nobody's to fail on: searched to the limit, no error.
    assert_eq!(clear_flags(&home, other()), Swept::default());
}

/// Catches a kept folder that another process swaps for a link between the examination and
/// the open being followed, or passing as emptied: the open refuses the link (it is
/// opened without following) and the entry is an error naming the folder, the link's
/// target untouched.
#[test]
fn a_kept_folder_swapped_for_a_link_since_it_was_examined_is_an_error() {
    let dir = scratch("kept-swap");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("C")).unwrap();
    let outside = dir.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "x").unwrap();
    let (c, target) = (entry.join("C"), outside.clone());
    let done = std::cell::Cell::new(false);
    let swept = vanishing(
        move |point, _| {
            if matches!(point, Point::Examined(n) if *n == "C") && !done.replace(true) {
                remove(&c);
                symlink(&target, &c).unwrap();
            }
        },
        || empty_keeping(&entry, me(), &["C"]),
    );
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("entry/C:"), "{swept:?}");
    assert!(outside.join("kept").exists(), "the link was followed");
}

/// Catches an entry of a kept folder that vanishes between the listing and its examination
/// read as an error: it is removed already, and the folder is emptied without one.
#[test]
fn an_entry_of_a_kept_folder_gone_since_the_listing_is_no_error() {
    let dir = scratch("kept-gone-entry");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("C")).unwrap();
    fs::write(entry.join("C/only"), "x").unwrap();
    let only = entry.join("C/only");
    let swept = vanishing(after_listing(2, move || remove(&only)), || {
        empty_keeping(&entry, me(), &["C"])
    });
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
}

/// Catches a flag clear that errors on an entry gone since the listing, and one that enters
/// another filesystem: the first is passed over, and with every entry reported on another
/// device none is changed or entered.
#[test]
fn clear_flags_passes_over_a_vanished_entry_and_never_crosses_a_device() {
    let dir = scratch("clear-edges");
    let home = dir.join("home");
    fs::create_dir_all(home.join("a")).unwrap();
    fs::write(home.join("b"), "x").unwrap();
    chmod(&home.join("b"), 0o400);
    let gone = home.join("b");
    let swept = vanishing(after_listing(1, move || remove(&gone)), || {
        clear_flags(&home, me())
    });
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    let opened = open_base(&home).unwrap().unwrap();
    let dev = rustix::fs::fstat(&opened).unwrap().st_dev;
    let mut other_device = Swept::default();
    clear_below(&opened, &home, me(), dev + 1, 0, &mut other_device).unwrap();
    assert_eq!(other_device, Swept::default());
}

/// Runs `body` in a child of this test binary whose soft limit of open descriptors is 120
/// (a Mac's default is 256, and the daemon's soft limit can stay there): the parent runs
/// the test again as the child (`name` is the test's own name) and asserts it passed.
/// A walk that holds a descriptor per level to 256 fails in the child with `EMFILE`.
fn with_few_descriptors(name: &str, body: impl FnOnce()) {
    if std::env::var_os("KBF_FEW_DESCRIPTORS").is_some() {
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let lowered = rustix::process::Rlimit {
            current: Some(120),
            maximum: limit.maximum,
        };
        rustix::process::setrlimit(rustix::process::Resource::Nofile, lowered).unwrap();
        body();
        return;
    }
    let path = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{path}::{name}"),
            "--test-threads=1",
            "--nocapture",
        ])
        .env("KBF_FEW_DESCRIPTORS", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "with 120 descriptors: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "the child ran the test: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Catches a walk that holds a descriptor per level to the full depth limit (a lease's
/// deep tree would then fail the flag clear with `EMFILE` on a Mac whose soft limit is
/// 256, on every re-clean): with only 120 descriptors, a tree 1000 deep is cleared of
/// flags and removed, each with no error.
#[test]
fn a_tree_a_thousand_deep_needs_few_descriptors() {
    with_few_descriptors("a_tree_a_thousand_deep_needs_few_descriptors", || {
        let dir = scratch("few-descriptors");
        let home = dir.join("home");
        fs::create_dir(&home).unwrap();
        build_deep(&home, 1000);
        let cleared = clear_flags(&home, me());
        assert_eq!(cleared.errors, Vec::<String>::new(), "{cleared:?}");
        let emptied = empty(&home, me());
        assert_eq!(emptied.errors, Vec::<String>::new(), "{emptied:?}");
        assert_eq!(names(&home), Vec::<String>::new());
    });
}

/// Catches a subtree moved up although it is not the entry that was judged (a directory
/// swapped in at its name since the judgement would be moved, and removed, as if it were
/// the uid's): the entry is looked at again before the move, and a different one is an
/// error that leaves it where it is.
#[test]
fn a_deep_subtree_replaced_since_it_was_judged_is_not_moved() {
    let dir = scratch("park-replaced");
    let top = dir.join("top");
    fs::create_dir(&top).unwrap();
    // Levels named l0 to l69, one inside the next, built through descriptors.
    let mut fd = rustix::fs::openat(rustix::fs::CWD, &top, DIRECTORY, Mode::empty()).unwrap();
    for level in 0..70 {
        let name = format!("l{level}");
        rustix::fs::mkdirat(&fd, name.as_str(), Mode::from_raw_mode(0o700)).unwrap();
        fd = rustix::fs::openat(&fd, name.as_str(), DIRECTORY, Mode::empty()).unwrap();
    }
    let mut parent = top.clone();
    for level in 0..64 {
        parent.push(format!("l{level}"));
    }
    let victim = parent.join("l64");
    let done = std::cell::Cell::new(false);
    let moved = parent.join("moved");
    let swept = vanishing(
        {
            let victim = victim.clone();
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "l64") && !done.replace(true) {
                    fs::rename(&victim, &moved).unwrap();
                    fs::create_dir(&victim).unwrap();
                }
            }
        },
        || empty(&top, me()),
    );
    assert_eq!(swept.errors.len(), 1, "{swept:?}");
    assert!(swept.errors[0].contains("replaced"), "{swept:?}");
    assert!(victim.is_dir(), "the replacement was moved or removed");
    assert!(parent.join("moved").is_dir(), "what was judged was lost");
}

/// Catches a move that replaces an entry already at the park's name (a name made ahead in
/// the tree being emptied would be lost or would stop the move): the name is taken, so
/// the next is used, and the entry made ahead is still there.
#[test]
fn a_name_made_ahead_at_the_parks_is_skipped_not_replaced() {
    let dir = scratch("park-name");
    let top = dir.join("top");
    fs::create_dir(&top).unwrap();
    build_deep(&top, 100);
    let planted = top.join(format!(".kbf-deep-{}-0", std::process::id()));
    let done = std::cell::Cell::new(false);
    let swept = vanishing(
        {
            let planted = planted.clone();
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "d") && !done.replace(true) {
                    fs::create_dir(&planted).unwrap();
                    fs::write(planted.join("kept"), "x").unwrap();
                }
            }
        },
        || empty(&top, me()),
    );
    assert_eq!(swept.errors, Vec::<String>::new(), "{swept:?}");
    assert_eq!(
        names(&top),
        [planted.file_name().unwrap().to_string_lossy().into_owned()]
    );
    assert!(
        planted.join("kept").exists(),
        "the entry made ahead was replaced"
    );
}

/// Catches a mode change that follows a link swapped in at an entry's name after it was
/// judged, or lands on another file swapped in (the target, which the uid does not own,
/// would be made writable by the owner bit): in either case the target's mode is as it
/// was.
#[test]
fn an_entry_swapped_in_after_the_judgement_does_not_change_its_targets_mode() {
    for swap in ["link", "file"] {
        let dir = scratch(&format!("chmod-{swap}"));
        let home = dir.join("home");
        fs::create_dir(&home).unwrap();
        fs::write(home.join("b"), "x").unwrap();
        chmod(&home.join("b"), 0o400);
        let target = dir.join("target");
        fs::write(&target, "x").unwrap();
        chmod(&target, 0o400);
        let done = std::cell::Cell::new(false);
        let (b, other) = (home.join("b"), target.clone());
        let swept = vanishing(
            move |point, _| {
                if matches!(point, Point::Examined(n) if *n == "b") && !done.replace(true) {
                    // Moved away, not removed: a freed inode number can be handed to the
                    // file made next, which would then look like the one judged.
                    fs::rename(&b, b.with_file_name("b.judged")).unwrap();
                    if swap == "link" {
                        symlink(&other, &b).unwrap();
                    } else {
                        fs::write(&b, "y").unwrap();
                        chmod(&b, 0o400);
                    }
                }
            },
            || clear_flags(&home, me()),
        );
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode(&target),
            0o400,
            "{swap}: the target was changed ({swept:?})"
        );
        if swap == "file" {
            assert_eq!(
                mode(&home.join("b")),
                0o400,
                "the file swapped in was changed"
            );
        }
    }
}

/// Catches a clear that changes the entry or one of its kept folders themselves (the
/// system owns their flags and ACLs, and the owner cannot change them), one that does not
/// go below a kept folder, and one that skips the other entries of the entry.
#[test]
fn clear_flags_within_clears_below_the_kept_folders_and_not_the_folders() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("within");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("T/sub")).unwrap();
    fs::create_dir_all(entry.join("junk")).unwrap();
    fs::write(entry.join("T/f"), "x").unwrap();
    chmod(&entry.join("T/f"), 0o400);
    chmod(&entry.join("T/sub"), 0o500);
    chmod(&entry.join("junk"), 0o500);
    chmod(&entry.join("T"), 0o500);
    let swept = clear_flags_within(&entry, me(), &["T"]);
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let modes = [
        mode(&entry.join("T")),
        mode(&entry.join("T/f")),
        mode(&entry.join("T/sub")),
        mode(&entry.join("junk")),
    ];
    chmod(&entry.join("T"), 0o700);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert_eq!(modes, [0o500, 0o700, 0o700, 0o700]);
    assert_eq!(
        clear_flags_within(&dir.join("absent"), me(), &["T"]),
        Swept::default()
    );
}

/// A kept folder that cannot be opened is an error that names it; a kept name that is a
/// file is cleared like any other entry.
#[test]
fn clear_flags_within_names_a_kept_folder_it_cannot_enter() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("within-closed");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("T")).unwrap();
    fs::write(entry.join("C"), "x").unwrap();
    chmod(&entry.join("C"), 0o400);
    chmod(&entry.join("T"), 0o000);
    let swept = clear_flags_within(&entry, me(), &["T", "C"]);
    chmod(&entry.join("T"), 0o700);
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert!(
        swept.errors[0].ends_with("entry/T: Permission denied (os error 13)"),
        "{:?}",
        swept.errors
    );
    assert_eq!(
        fs::metadata(entry.join("C")).unwrap().permissions().mode() & 0o700,
        0o700
    );
}

/// Catches a skip of Apple's protected cache that also skips what is not a refusal of
/// the system (any other error, or anything below the top of a base), and a warning
/// repeated for the same path.
#[test]
fn only_the_systems_refusal_of_a_top_level_entry_is_skipped_and_said_once() {
    let denied = io::Error::from_raw_os_error(Errno::PERM.raw_os_error());
    let other = io::Error::from_raw_os_error(Errno::IO.raw_os_error());
    assert!(protected_top_entry(0, &denied));
    assert!(!protected_top_entry(1, &denied));
    assert!(!protected_top_entry(0, &other));
    let mut seen = Vec::new();
    assert!(first_time(&mut seen, Path::new("/Library/Caches/a")));
    assert!(!first_time(&mut seen, Path::new("/Library/Caches/a")));
    assert!(first_time(&mut seen, Path::new("/Library/Caches/b")));
    warn_once(Path::new("/Library/Caches/a"));
    warn_once(Path::new("/Library/Caches/a"));
}

/// Catches a skip of what the process can open (a leftover it could remove must still
/// fail), a refusal other than the system's counted as one, and a root process that skips
/// (root is refused for want of a grant, which says nothing of the pool user).
#[test]
fn a_refusal_is_skipped_only_for_an_entry_the_process_cannot_open() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("tolerate");
    fs::create_dir_all(dir.join("closed")).unwrap();
    fs::create_dir_all(dir.join("open")).unwrap();
    chmod(&dir.join("closed"), 0o000);
    let fd = open_base(&dir).unwrap().unwrap();
    let denied = || Err::<usize, _>(io::Error::from_raw_os_error(Errno::PERM.raw_os_error()));
    let skipped = std::cell::Cell::new(0);
    let got = tolerate(
        denied(),
        (&fd, OsStr::new("closed"), &dir.join("closed")),
        &skipped,
    );
    assert_eq!((got.unwrap(), skipped.get()), (0, 1));
    let got = tolerate(
        denied(),
        (&fd, OsStr::new("open"), &dir.join("open")),
        &skipped,
    );
    assert!(got.is_err());
    let other = Err::<usize, _>(io::Error::from_raw_os_error(Errno::IO.raw_os_error()));
    let got = tolerate(
        other,
        (&fd, OsStr::new("closed"), &dir.join("closed")),
        &skipped,
    );
    assert!(got.is_err());
    assert_eq!(skipped.get(), 1);
    assert!(cannot_open(&fd, OsStr::new("closed")));
    assert!(!cannot_open(&fd, OsStr::new("open")));
    assert!(!cannot_open(&fd, OsStr::new("absent")));
    assert!(path_is_protected(&dir.join("closed")));
    assert!(path_is_protected(&dir.join("closed").join("inside")));
    assert!(!path_is_protected(&dir.join("open")));
    assert!(!path_is_protected(&dir.join("absent")));
    assert!(!path_is_protected(Path::new("/")));
    chmod(&dir.join("closed"), 0o700);
}

/// The sweep of a `/var/folders` folder and of a home: what the pool user cannot open
/// stays, with the directories that hold it, and is no error; what it can open is removed
/// or fails with its path.
#[test]
fn the_empty_leaves_what_the_user_cannot_open_and_removes_the_rest() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("empty-protected");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("T/vault")).unwrap();
    fs::write(entry.join("T/f"), "x").unwrap();
    fs::create_dir_all(entry.join("Library/Preferences")).unwrap();
    fs::write(entry.join("Library/note"), "x").unwrap();
    chmod(&entry.join("T/vault"), 0o000);
    chmod(&entry.join("Library/Preferences"), 0o000);
    // The owner's unlock is refused (a store of the system), as it is for a vault.
    UNLOCKING.with(|u| u.set(false));
    let swept = empty_keeping(&entry, me(), &["T"]);
    UNLOCKING.with(|u| u.set(true));
    chmod(&entry.join("T/vault"), 0o700);
    chmod(&entry.join("Library/Preferences"), 0o700);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert!(!entry.join("T/f").exists());
    assert!(entry.join("T/vault").exists());
    assert!(entry.join("Library/Preferences").exists());
    assert!(!entry.join("Library/note").exists());
}

#[test]
fn the_empty_still_fails_naming_what_the_user_can_open_and_cannot_remove() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("empty-fails");
    let home = dir.join("home");
    fs::create_dir_all(home.join("Library/locked")).unwrap();
    fs::write(home.join("Library/locked/f"), "x").unwrap();
    chmod(&home.join("Library/locked"), 0o500);
    let swept = empty(&home, me());
    chmod(&home.join("Library/locked"), 0o700);
    assert_eq!(swept.errors.len(), 1, "{:?}", swept.errors);
    assert!(
        swept.errors[0].contains("empty-fails") && swept.errors[0].contains("Library"),
        "{:?}",
        swept.errors
    );
}

/// Catches a top-level refusal that fails the sweep, and one that is skipped deeper, or
/// when it is another error.
#[test]
fn only_a_top_level_refusal_of_the_examination_is_taken_as_an_entry_not_there() {
    let denied = || Err(io::Error::from_raw_os_error(Errno::PERM.raw_os_error()));
    let here = Path::new("/Library/Caches/com.apple.example");
    assert!(skip_protected_top(denied(), 0, here).unwrap().is_none());
    assert!(skip_protected_top(denied(), 1, here).is_err());
    let other = Err(io::Error::from_raw_os_error(Errno::IO.raw_os_error()));
    assert!(skip_protected_top(other, 0, here).is_err());
    assert!(skip_protected_top(Ok(None), 0, here).unwrap().is_none());
}

fn place(names: &[&str]) {
    pretend_placed(names);
}

/// Catches a lease that takes every permission from a folder it owns to keep its files
/// past the clean: the owner can chmod it back, so the sweep does, and empties it.
#[test]
fn a_directory_the_user_owns_with_no_permission_is_emptied() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("unlock-own");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("T/hidden/deeper")).unwrap();
    fs::write(entry.join("T/hidden/deeper/secret"), "x").unwrap();
    chmod(&entry.join("T/hidden/deeper"), 0o000);
    chmod(&entry.join("T/hidden"), 0o000);
    let swept = empty_keeping(&entry, me(), &["T"]);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert!(names(&entry.join("T")).is_empty());
    // Cleared the same way before the removal.
    fs::create_dir_all(entry.join("T/closed")).unwrap();
    chmod(&entry.join("T/closed"), 0o000);
    let swept = clear_flags_within(&entry, me(), &["T"]);
    assert_eq!(swept.errors, Vec::<String>::new());
    let mode = fs::metadata(entry.join("T/closed"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o700, 0o700);
}

/// The unlock is for the owner only: an entry another uid owns, or a process that is root,
/// is left as it is; and with it off, a closed owned directory is skipped as a store.
#[test]
fn the_unlock_is_for_the_owner_and_a_refused_one_is_skipped_with_the_warning() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("unlock-owner");
    fs::create_dir_all(dir.join("closed")).unwrap();
    chmod(&dir.join("closed"), 0o000);
    let fd = open_base(&dir).unwrap().unwrap();
    let stat = rustix::fs::statat(&fd, "closed", AtFlags::SYMLINK_NOFOLLOW).unwrap();
    unlock_own(&fd, OsStr::new("closed"), &stat, other());
    unlock_own(&fd, OsStr::new("closed"), &stat, 0);
    assert!(cannot_open(&fd, OsStr::new("closed")));
    unlock_own(&fd, OsStr::new("closed"), &stat, me());
    assert!(!cannot_open(&fd, OsStr::new("closed")));
    // A refused change (a store the system protects) leaves it closed: skipped.
    chmod(&dir.join("closed"), 0o000);
    UNLOCKING.with(|u| u.set(false));
    fs::create_dir_all(dir.join("home/Library/Preferences")).unwrap();
    chmod(&dir.join("home/Library/Preferences"), 0o000);
    let swept = empty(&dir.join("home"), me());
    UNLOCKING.with(|u| u.set(true));
    chmod(&dir.join("home/Library/Preferences"), 0o700);
    chmod(&dir.join("closed"), 0o700);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert!(dir.join("home/Library/Preferences").exists());
}

/// A directory the system placed (`sunlnk`, `com.apple.rootless`) below a kept folder
/// keeps its flags and mode, stays, and is emptied; what is in it goes.
#[test]
fn a_directory_the_system_placed_is_kept_and_emptied() {
    if root() {
        eprintln!("skipped: permission bits do not stop root");
        return;
    }
    let dir = scratch("placed");
    let entry = dir.join("entry");
    fs::create_dir_all(entry.join("T/com.apple.trustd/TemporaryItems")).unwrap();
    fs::write(entry.join("T/com.apple.trustd/TemporaryItems/f"), "x").unwrap();
    fs::write(entry.join("T/com.apple.trustd/g"), "x").unwrap();
    fs::create_dir_all(entry.join("T/plain/com.apple.trustd")).unwrap();
    chmod(&entry.join("T/com.apple.trustd"), 0o500);
    chmod(&entry.join("T/com.apple.trustd/TemporaryItems"), 0o500);
    place(&["com.apple.trustd"]);
    // The clear leaves the placed directory's mode, clears what is in it.
    let swept = clear_flags_within(&entry, me(), &["T"]);
    assert_eq!(swept.errors, Vec::<String>::new());
    let mode = |p: &str| fs::metadata(entry.join(p)).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode("T/com.apple.trustd"), 0o500);
    assert_eq!(mode("T/com.apple.trustd/TemporaryItems"), 0o700);
    // The empty removes everything but the placed directories themselves.
    chmod(&entry.join("T/com.apple.trustd"), 0o700);
    let swept = empty_keeping(&entry, me(), &["T"]);
    place(&[]);
    assert_eq!(swept.errors, Vec::<String>::new());
    assert!(entry.join("T/com.apple.trustd").is_dir());
    assert!(names(&entry.join("T/com.apple.trustd")).is_empty());
    // The one nested in a plain directory stays with it.
    assert!(entry.join("T/plain/com.apple.trustd").is_dir());
    assert!(!path_is_system_placed(&entry.join("T/absent")));
    place(&["com.apple.trustd"]);
    assert!(path_is_system_placed(&entry.join("T/com.apple.trustd")));
    assert!(!path_is_system_placed(&entry.join("T/plain")));
    assert!(!path_is_system_placed(Path::new("/")));
    place(&[]);
}

#[test]
fn a_directory_is_the_systems_when_it_has_sunlnk_or_the_rootless_attribute() {
    assert!(placed(0x0010_0000, false));
    assert!(placed(0x0010_0000 | 0x8000, false));
    assert!(placed(0, true));
    assert!(!placed(0x8000 | 0x2, false));
    assert!(!placed(0, false));
}
