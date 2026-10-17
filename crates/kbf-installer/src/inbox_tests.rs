use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;

use super::*;
use crate::fixtures::scratch_dir;

fn entry_with(name: &str, mode: OpenMode) -> (PathBuf, Entry) {
    let dir = scratch_dir(name);
    let entry = dir.join("entry");
    fs::create_dir(&entry).expect("entry");
    let opened = Entry::open(&entry).expect("opens").with_mode(mode);
    (dir, opened)
}

fn refusal(r: Result<File, Refusal>) -> &'static str {
    r.expect_err("refused").code()
}

/// Both ways of opening beneath the entry are exercised on every kernel that has
/// `openat2` (the production choice is `Beneath` there, `NoFollow` elsewhere).
fn modes() -> [OpenMode; 2] {
    [OpenMode::Beneath, OpenMode::NoFollow]
}

/// Catches a regular file not read back whole: its content comes out by the descriptor.
#[test]
fn a_regular_file_is_opened_and_read() {
    for mode in modes() {
        let (dir, entry) = entry_with("inbox-regular", mode);
        fs::write(dir.join("entry/a.txt"), b"hello").expect("seed");
        let mut f = entry.open_file("a.txt", 100).expect("opens");
        assert_eq!(read_capped(&mut f, 100, "a.txt").expect("reads"), b"hello");
        assert_eq!(entry.name(), dir.join("entry").display().to_string());
    }
}

/// Catches each kind of file the stage must not read being read: a symbolic link (to a
/// file outside the entry, which must stay unread), a FIFO (whose open must not hang), a
/// directory, a hard link to a file that is elsewhere too, an empty file, one over its cap,
/// and a name that is not there. Each has its own code, for both ways of opening.
///
/// Mutants seen red: `O_NOFOLLOW` (and the `openat2` resolve flags) removed, which
/// opens the link's target (the secret is read and the refusal is gone); the link count
/// not checked; the type not checked (the FIFO is read as a file).
#[test]
fn a_file_the_stage_must_not_read_is_refused_by_its_kind() {
    for mode in modes() {
        let (dir, entry) = entry_with("inbox-kinds", mode);
        let e = dir.join("entry");
        fs::write(dir.join("secret"), b"not yours").expect("seed");
        symlink(dir.join("secret"), e.join("link")).expect("link");
        symlink("../secret", e.join("relative-link")).expect("link");
        let fifo = e.join("fifo");
        let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("no NUL");
        // SAFETY: mkfifo reads the NUL-terminated path and keeps nothing. (Not rustix's
        // `mknodat`, which macOS does not have.)
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0, "a fifo");
        fs::create_dir(e.join("subdir")).expect("dir");
        fs::write(dir.join("other-name"), b"two names").expect("seed");
        fs::hard_link(dir.join("other-name"), e.join("hard")).expect("hard link");
        fs::write(e.join("empty"), b"").expect("seed");
        fs::write(e.join("big"), vec![1u8; 11]).expect("seed");

        assert_eq!(
            refusal(entry.open_file("link", 100)),
            "inbox-symlink",
            "{mode:?}"
        );
        assert_eq!(
            refusal(entry.open_file("relative-link", 100)),
            "inbox-symlink"
        );
        assert_eq!(refusal(entry.open_file("fifo", 100)), "inbox-not-regular");
        assert_eq!(refusal(entry.open_file("subdir", 100)), "inbox-not-regular");
        assert_eq!(refusal(entry.open_file("hard", 100)), "inbox-hard-linked");
        assert_eq!(refusal(entry.open_file("empty", 100)), "inbox-empty");
        assert_eq!(refusal(entry.open_file("big", 10)), "inbox-oversize");
        assert_eq!(refusal(entry.open_file("nothing", 100)), "inbox-missing");
        // A name with a path is no name of the entry: no way out of it.
        for bad in ["../secret", "a/b", "/etc/passwd", "..", ".", "", "a\0b"] {
            assert_eq!(
                refusal(entry.open_file(bad, 100)),
                "inbox-bad-name",
                "{bad:?} {mode:?}"
            );
        }
    }
}

/// Catches the kernel's own refusal of a way out of the entry being lost: even past the
/// name check, `openat2` with `RESOLVE_BENEATH` refuses `../secret` and a link to it
/// (the second line behind the offer's file-name check and `inbox-bad-name`). Where the
/// kernel has no `openat2` the same calls fail for want of it.
#[test]
fn the_kernel_refuses_a_way_out_of_the_entry() {
    let (dir, entry) = entry_with("inbox-beneath", OpenMode::Beneath);
    fs::write(dir.join("secret"), b"not yours").expect("seed");
    symlink("../secret", dir.join("entry/link")).expect("link");
    assert!(open_beneath(&entry.dir, "../secret").is_err());
    assert!(open_beneath(&entry.dir, "link").is_err());
}

/// Catches an entry directory that is a link or not a directory being opened, and a path
/// with no parent: the entry is the inbox's own child and not a link, and everything
/// else is refused.
#[test]
fn an_entry_that_is_a_link_or_not_a_directory_is_refused() {
    let dir = scratch_dir("inbox-entry");
    fs::create_dir(dir.join("real")).expect("dir");
    symlink(dir.join("real"), dir.join("link")).expect("link");
    fs::write(dir.join("file"), b"x").expect("seed");
    let open = |p: &Path| Entry::open(p).err().map(|e| e.code());
    assert_eq!(open(&dir.join("link")), Some("inbox-not-directory"));
    assert_eq!(open(&dir.join("file")), Some("inbox-not-directory"));
    assert_eq!(open(&dir.join("none")), Some("inbox-missing"));
    assert_eq!(open(&dir.join("none/deeper")), Some("inbox-missing"));
    assert_eq!(open(Path::new("/")), Some("inbox-missing"));
    // A file in the place of the parent.
    assert_eq!(open(&dir.join("file/entry")), Some("inbox-not-directory"));
    assert_eq!(open(&dir.join("real")), None);
}

/// Catches the listing of an entry wrong: its names without `.` and `..`, and an entry
/// holding more than the most a stage takes refused before any is read.
#[test]
fn the_names_of_an_entry_are_listed_and_bounded() {
    let (dir, entry) = entry_with("inbox-names", OpenMode::Beneath);
    let e = dir.join("entry");
    assert!(entry.names().expect("lists").is_empty());
    fs::write(e.join("b"), b"1").expect("seed");
    fs::write(e.join("a"), b"1").expect("seed");
    let mut names = entry.names().expect("lists");
    names.sort();
    assert_eq!(names, ["a", "b"]);
    for n in 0..MAX_FILES {
        fs::write(e.join(format!("f{n}")), b"1").expect("seed");
    }
    let err = entry.names().expect_err("too many");
    assert_eq!(err.code(), "inbox-too-many");
}

/// Catches the copy of a file not being made whole, root's and 0644, or being allowed to
/// replace a file; and a source that grew past its cap since its size was read being
/// copied in full.
#[test]
fn a_copy_is_whole_new_and_capped() {
    let dir = scratch_dir("inbox-copy");
    fs::write(dir.join("src"), b"0123456789").expect("seed");
    let mut src = File::open(dir.join("src")).expect("open");
    copy_capped(&mut src, &dir.join("copy"), 10, "src").expect("copies");
    assert_eq!(fs::read(dir.join("copy")).expect("there"), b"0123456789");
    assert_eq!(
        fs::metadata(dir.join("copy"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o7777,
        0o644 & !current_umask()
    );

    let mut src = File::open(dir.join("src")).expect("open");
    let err = copy_capped(&mut src, &dir.join("copy"), 10, "src").expect_err("exists");
    assert_eq!(err.code(), "work-dir");

    let mut src = File::open(dir.join("src")).expect("open");
    let err = copy_capped(&mut src, &dir.join("small"), 9, "src").expect_err("over");
    assert_eq!(err.code(), "inbox-oversize");

    let mut src = File::open(dir.join("src")).expect("open");
    let err = read_capped(&mut src, 9, "src").expect_err("over");
    assert_eq!(err.code(), "inbox-oversize");
}

/// A read error of the source is reported against the inbox file, not the copy: a
/// directory opened as a file cannot be read.
#[test]
fn a_source_that_cannot_be_read_is_reported_as_the_inbox_file() {
    let dir = scratch_dir("inbox-unreadable");
    let mut as_file = File::open(&dir).expect("a directory opens read-only");
    let err = copy_capped(&mut as_file, &dir.join("copy"), 10, "src").expect_err("EISDIR");
    assert_eq!(err.code(), "inbox-unreadable");
    let err = read_capped(&mut as_file, 10, "src").expect_err("EISDIR");
    assert_eq!(err.code(), "inbox-unreadable");
}

/// Catches the choice of how to open going the wrong way round: `openat2` when the
/// kernel has it, `O_NOFOLLOW` when it does not.
#[test]
fn openat2_is_used_where_the_kernel_has_it() {
    assert_eq!(OpenMode::choose(true), OpenMode::Beneath);
    assert_eq!(OpenMode::choose(false), OpenMode::NoFollow);
    let (_, entry) = entry_with("inbox-choose", OpenMode::NoFollow);
    let chosen = OpenMode::for_entry(&entry.dir);
    assert_eq!(
        chosen == OpenMode::Beneath,
        cfg!(target_os = "linux") && openat2_works()
    );
}

fn openat2_works() -> bool {
    let (_, entry) = entry_with("inbox-openat2", OpenMode::NoFollow);
    open_beneath(&entry.dir, ".").is_ok()
}

/// Catches the verdict on an open file's attributes wrong: only a regular file with one
/// link, a size and a cap passes.
#[test]
fn the_verdict_on_an_open_file_is_by_type_links_and_size() {
    use rustix::fs::FileType::{Directory, Fifo, RegularFile, Socket};
    assert!(verdict(RegularFile, 1, 5, 5).is_none());
    assert_eq!(
        verdict(Fifo, 1, 5, 5).expect("refused").code(),
        "inbox-not-regular"
    );
    assert_eq!(
        verdict(Directory, 1, 5, 5).expect("refused").code(),
        "inbox-not-regular"
    );
    assert_eq!(
        verdict(Socket, 1, 5, 5).expect("refused").code(),
        "inbox-not-regular"
    );
    assert_eq!(
        verdict(RegularFile, 2, 5, 5).expect("refused").code(),
        "inbox-hard-linked"
    );
    assert_eq!(
        verdict(RegularFile, 1, 0, 5).expect("refused").code(),
        "inbox-empty"
    );
    assert_eq!(
        verdict(RegularFile, 1, 6, 5).expect("refused").code(),
        "inbox-oversize"
    );
}

// `mode_t` is a u16 on macOS and a u32 on Linux, where the conversion is a no-op.
#[allow(clippy::useless_conversion)]
fn current_umask() -> u32 {
    let mask = rustix::process::umask(rustix::fs::Mode::empty());
    rustix::process::umask(mask);
    mask.as_raw_mode().into()
}

/// Catches an `open` error that no rule names (permission denied here) being read as one
/// of the rules (a link, a missing file) or dropped: it is reported as unreadable, with
/// the system's own error.
#[test]
fn an_open_error_no_rule_names_is_reported_as_unreadable() {
    let fault = fault_of(Errno::ACCESS, InboxFault::NotRegular);
    assert!(
        matches!(&fault, InboxFault::Unreadable(e) if e.kind() == io::ErrorKind::PermissionDenied),
        "{fault:?}"
    );
}

/// Catches the helper removing what a link points at instead of the link: the entry holds a
/// link to a file outside it, and removing the link by name leaves the target. Also a name
/// that is not a single component is refused, and a name that is gone is not an error.
///
/// Mutant seen red: the removal done through a path (`fs::remove_file(dir/name)`) with the
/// name `../secret`: the BadName check dropped, the outside file goes.
#[test]
fn a_removal_takes_the_name_in_the_entry_and_nothing_else() {
    let (dir, entry) = entry_with("inbox-remove", OpenMode::NoFollow);
    let e = dir.join("entry");
    fs::write(dir.join("secret"), b"not yours").expect("seed");
    fs::write(e.join("order"), b"{}").expect("seed");
    symlink(dir.join("secret"), e.join("link")).expect("link");
    entry.remove_file("order").expect("removed");
    entry.remove_file("link").expect("the link is removed");
    entry.remove_file("order").expect("gone already");
    assert!(!e.join("order").exists());
    assert!(e.join("link").symlink_metadata().is_err());
    assert_eq!(fs::read(dir.join("secret")).expect("kept"), b"not yours");
    for bad in ["../secret", "a/b", "", ".", ".."] {
        assert_eq!(
            entry.remove_file(bad).expect_err("refused").code(),
            "inbox-bad-name",
            "{bad:?}"
        );
    }
    assert!(dir.join("secret").exists());
    // A directory is not a file to remove (unlinkat without REMOVEDIR): refused, kept.
    fs::create_dir(e.join("sub")).expect("dir");
    assert!(entry.remove_file("sub").is_err());
    assert!(e.join("sub").is_dir());
}

/// Catches a sub-directory followed out of the entry: `orders` replaced by a link to a
/// directory elsewhere is refused, a real one opens and lists, and a name that is not one
/// component is refused.
///
/// Mutant seen red: the `NOFOLLOW` flag dropped from `sub` (the linked directory opens).
#[test]
fn a_sub_directory_is_opened_beneath_the_entry_and_a_link_is_refused() {
    let (dir, entry) = entry_with("inbox-sub", OpenMode::NoFollow);
    let e = dir.join("entry");
    fs::create_dir(e.join("real")).expect("dir");
    fs::write(e.join("real/a"), b"x").expect("file");
    fs::create_dir(dir.join("elsewhere")).expect("dir");
    symlink(dir.join("elsewhere"), e.join("linked")).expect("link");
    assert_eq!(
        entry.sub("real").expect("opens").names().expect("lists"),
        ["a"]
    );
    assert!(entry.sub("linked").is_err());
    assert!(entry.sub("missing").is_err());
    for bad in ["", ".", "..", "a/b"] {
        assert_eq!(
            entry.sub(bad).err().expect("refused").code(),
            "inbox-bad-name"
        );
    }
}
