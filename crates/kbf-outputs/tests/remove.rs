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
    for i in 0..4000 {
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
        if i == 3999 {
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
}
