//! `remove_tree` against trees an action could leave: locked, deep, and full of
//! symlinks to things that must survive.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use kbf_outputs::remove_tree;

fn scratch(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test binary");
    let dir = exe
        .parent()
        .expect("deps directory")
        .join("kbf-outputs-remove")
        .join(name);
    if dir.exists() {
        remove_tree(&dir).expect("clear old scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn chmod(path: &std::path::Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Catches a removal that follows a symlink (deleting the host's files), gives up on a
/// directory the action made unreadable or unwritable, or leaves anything behind.
#[test]
fn a_locked_tree_goes_and_what_its_links_name_stays() {
    let base = scratch("locked");
    let host = base.join("host");
    std::fs::create_dir_all(host.join("inner")).expect("mkdir");
    std::fs::write(host.join("keep"), b"keep").expect("write");
    let lease = base.join("lease");
    std::fs::create_dir_all(lease.join("a/b/c")).expect("mkdir");
    std::fs::write(lease.join("a/b/c/f"), b"x").expect("write");
    std::fs::write(lease.join("a/locked-file"), b"x").expect("write");
    chmod(&lease.join("a/locked-file"), 0o000);
    symlink(&host, lease.join("a/b/to-host")).expect("symlink");
    symlink(host.join("keep"), lease.join("to-file")).expect("symlink");
    chmod(&lease.join("a/b/c"), 0o000);
    chmod(&lease.join("a/b"), 0o500);
    chmod(&lease.join("a"), 0o100);

    remove_tree(&lease).expect("remove");
    assert!(
        std::fs::symlink_metadata(&lease).is_err(),
        "the lease directory is gone"
    );
    assert_eq!(
        std::fs::read(host.join("keep")).expect("host file"),
        b"keep"
    );
    assert!(
        host.join("inner").is_dir(),
        "the host directory is untouched"
    );
}

/// Catches a removal that is recursive or holds a descriptor per level, or works by
/// path: a tree thousands of levels deep (deeper than any path) is removed.
#[test]
fn a_tree_deeper_than_a_path_goes() {
    let base = scratch("deep");
    let mut here = rustix::fs::openat(
        rustix::fs::CWD,
        &base,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )
    .expect("open");
    for i in 0..2100 {
        rustix::fs::mkdirat(&here, "d", rustix::fs::Mode::RWXU).expect("mkdir");
        let file = rustix::fs::openat(
            &here,
            "f",
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE,
            rustix::fs::Mode::RUSR,
        )
        .expect("create");
        drop(file);
        let next = rustix::fs::openat(
            &here,
            "d",
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )
        .expect("open");
        if i == 2099 {
            // The deepest level locked (the top one is locked below).
            rustix::fs::chmodat(
                &here,
                "d",
                rustix::fs::Mode::RUSR,
                rustix::fs::AtFlags::empty(),
            )
            .expect("chmod");
        }
        here = next;
    }
    drop(here);
    chmod(&base.join("d"), 0o100);
    remove_tree(&base.join("d")).expect("remove");
    assert!(!base.join("d").exists());
}

/// Catches a removal that follows a symlink given as the path itself, fails on an
/// absent path, or accepts a path with no parent.
#[test]
fn the_path_itself_may_be_a_link_a_file_or_absent() {
    let base = scratch("path-itself");
    let target = base.join("target");
    std::fs::create_dir(&target).expect("mkdir");
    std::fs::write(target.join("keep"), b"keep").expect("write");
    symlink(&target, base.join("link")).expect("symlink");
    remove_tree(&base.join("link")).expect("unlink the link");
    assert!(std::fs::symlink_metadata(base.join("link")).is_err());
    assert!(target.join("keep").exists(), "the link's target stays");
    std::fs::write(base.join("file"), b"x").expect("write");
    remove_tree(&base.join("file")).expect("unlink the file");
    assert!(!base.join("file").exists());
    remove_tree(&base.join("absent")).expect("nothing to remove");
    // A bare name is relative to the working directory.
    remove_tree(std::path::Path::new("kbf-outputs-absent")).expect("nothing to remove");
    assert!(remove_tree(std::path::Path::new("/")).is_err());
    // A parent that is not there is the caller's mistake, not "nothing to remove".
    assert!(remove_tree(&base.join("absent/child")).is_err());
}

/// Catches an entry that cannot be examined (its directory may be read but not
/// searched) skipped as if absent: the removal fails and says so.
#[test]
fn an_entry_that_cannot_be_examined_fails_the_removal() {
    let base = scratch("blind");
    std::fs::create_dir(base.join("blind")).expect("mkdir");
    std::fs::create_dir(base.join("blind/child")).expect("mkdir");
    chmod(&base.join("blind"), 0o400);
    assert!(remove_tree(&base.join("blind/child")).is_err());
    chmod(&base.join("blind"), 0o755);
    remove_tree(&base.join("blind")).expect("remove");
}

/// Sets BSD file flags with `chflags`, as an action would (`-h`: on a link itself).
#[cfg(target_os = "macos")]
fn chflags(args: &[&str], path: &std::path::Path) {
    let status = std::process::Command::new("/usr/bin/chflags")
        .args(args)
        .arg(path)
        .status()
        .expect("chflags");
    assert!(status.success(), "chflags {args:?} {}", path.display());
}

/// Catches a removal that fails on what an action marked immutable or append-only
/// (`chflags uchg`, `uappnd`: no privilege needed on macOS), whether a file, a link
/// or a directory, the lease directory itself included; and one that clears the
/// flags of what a link names instead of the link's own.
#[cfg(target_os = "macos")]
#[test]
fn what_the_action_made_immutable_goes_too() {
    use std::os::macos::fs::MetadataExt as _;
    let base = scratch("flags");
    let host = base.join("host");
    std::fs::write(&host, b"keep").expect("write");
    chflags(&["uchg"], &host);
    let lease = base.join("lease");
    std::fs::create_dir_all(lease.join("out/sub")).expect("mkdir");
    std::fs::write(lease.join("out/x"), b"x").expect("write");
    std::fs::write(lease.join("out/sub/y"), b"y").expect("write");
    std::fs::write(lease.join("out/log"), b"l").expect("write");
    symlink(&host, lease.join("out/to-host")).expect("symlink");
    chflags(&["uchg"], &lease.join("out/x"));
    chflags(&["uappnd"], &lease.join("out/log"));
    chflags(&["-h", "uchg"], &lease.join("out/to-host"));
    chflags(&["uappnd"], &lease.join("out/sub"));
    chflags(&["uchg"], &lease.join("out"));
    chflags(&["uchg"], &lease);
    assert!(
        std::fs::remove_file(lease.join("out/x")).is_err(),
        "the flags hold before the removal"
    );

    remove_tree(&lease).expect("remove");
    assert!(
        std::fs::symlink_metadata(&lease).is_err(),
        "the lease directory is gone"
    );
    let flags = std::fs::symlink_metadata(&host).expect("host").st_flags();
    assert_ne!(flags & 0x2, 0, "the link's target keeps its uchg flag");
    chflags(&["nouchg"], &host);
}

/// Changes an ACL with `chmod` (`+a` adds an entry, `-N` removes the ACL), as an
/// action would.
#[cfg(target_os = "macos")]
fn acl(args: &[&str], path: &std::path::Path) {
    let status = std::process::Command::new("/bin/chmod")
        .args(args)
        .arg(path)
        .status()
        .expect("chmod +a");
    assert!(status.success(), "chmod {args:?} {}", path.display());
}

/// Catches a removal that fails on what an action locked with an ACL, which denies
/// even the owner and which no permission bit undoes (`everyone deny delete` on a
/// file, `delete_child` on a directory, `writesecurity` on top); and one that strips
/// the ACL of what a link names instead of the link's own.
#[cfg(target_os = "macos")]
#[test]
fn what_the_action_locked_with_an_acl_goes_too() {
    let base = scratch("acl");
    std::fs::create_dir(base.join("hostdir")).expect("mkdir");
    let host = base.join("hostdir/host");
    std::fs::write(&host, b"keep").expect("write");
    acl(&["+a", "everyone deny delete"], &host);
    acl(&["+a", "everyone deny delete_child"], &base.join("hostdir"));
    let lease = base.join("lease");
    std::fs::create_dir_all(lease.join("out/sub")).expect("mkdir");
    std::fs::write(lease.join("out/x"), b"x").expect("write");
    std::fs::write(lease.join("out/sub/y"), b"y").expect("write");
    symlink(&host, lease.join("out/to-host")).expect("symlink");
    acl(&["+a", "everyone deny delete"], &lease.join("out/x"));
    acl(
        &["+a", "everyone deny delete,writesecurity"],
        &lease.join("out/sub/y"),
    );
    acl(&["+a", "everyone deny delete_child"], &lease.join("out"));
    acl(
        &["+a", "everyone deny delete,delete_child"],
        &lease.join("out/sub"),
    );
    acl(&["+a", "everyone deny delete"], &lease);
    assert!(
        std::fs::remove_file(lease.join("out/x")).is_err(),
        "the ACLs hold before the removal"
    );

    remove_tree(&lease).expect("remove");
    assert!(
        std::fs::symlink_metadata(&lease).is_err(),
        "the lease directory is gone"
    );
    assert!(
        std::fs::remove_file(&host).is_err(),
        "the link's target keeps its ACL"
    );
    acl(&["-N"], &host);
    acl(&["-N"], &base.join("hostdir"));
}
