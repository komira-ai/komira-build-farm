//! Applying layers to a root: ordering, whiteouts, links, ownership and mode, and the
//! ways a hostile tar tries to write outside the root.

mod support;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use kbf_image::{Compression, Ownership, Unpack, UnpackError, ensure_paths};
use support::registry::{Item, gzip, my_ids, tar_of};
use support::scratch;
use tar::EntryType;

fn mine() -> Ownership {
    let (uid, gid) = my_ids();
    Ownership::AllTo { uid, gid }
}

fn file<'a>(path: &'a str, data: &'a [u8]) -> Item<'a> {
    Item::File {
        path,
        data,
        mode: 0o644,
    }
}

fn dir(path: &str) -> Item<'_> {
    Item::Dir { path, mode: 0o755 }
}

/// Unpacks the tars (lowest first) into `<scratch>/<name>/rootfs`.
fn unpack(name: &str, layers: &[Vec<u8>]) -> (PathBuf, Result<kbf_image::Unpacked, UnpackError>) {
    unpack_with(name, layers, Unpack::new(mine()))
}

fn unpack_with(
    name: &str,
    layers: &[Vec<u8>],
    unpack: Unpack,
) -> (PathBuf, Result<kbf_image::Unpacked, UnpackError>) {
    let base = scratch(name);
    let sources: Vec<(PathBuf, Compression)> = layers
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let path = base.join(format!("{i}.tar"));
            std::fs::write(&path, bytes).expect("write layer");
            (path, Compression::None)
        })
        .collect();
    let root = base.join("rootfs");
    let result = unpack.run(&root, &sources);
    (root, result)
}

fn tar(items: &[Item<'_>]) -> Vec<u8> {
    tar_of(items, (u64::from(my_ids().0), u64::from(my_ids().1)))
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn exists(root: &Path, rel: &str) -> bool {
    std::fs::symlink_metadata(root.join(rel)).is_ok()
}

fn names(root: &Path, rel: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root.join(rel))
        .expect("list")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Catches layers applied out of order or not at all: a later layer's file replaces an
/// earlier one's, directories merge, parents the tar never listed are made, `./` and
/// `/` prefixes land inside the root, and the counts are the layers'.
#[test]
fn layers_apply_in_order() {
    let lower = tar(&[
        dir("etc"),
        file("etc/a", b"lower a"),
        file("etc/b", b"lower b"),
        file("usr/bin/tool", b"tool"),
    ]);
    let upper = tar(&[file("./etc/a", b"upper a"), file("/abs/new", b"new")]);
    let (root, got) = unpack("unpack-order", &[lower, upper]);
    let counts = got.expect("unpacked");
    assert_eq!(read(&root, "etc/a"), "upper a");
    assert_eq!(read(&root, "etc/b"), "lower b");
    assert_eq!(read(&root, "usr/bin/tool"), "tool");
    assert_eq!(read(&root, "abs/new"), "new");
    assert_eq!(
        (counts.entries, counts.skipped, counts.bytes),
        (6, 0, 7 + 7 + 4 + 7 + 3)
    );
}

/// Catches ownership and mode applied in the wrong order (the "chown after chmod"
/// mutant: `chown` clears setuid and setgid, so a mode set first is lost), and
/// a directory's mode not holding its own contents back: the setuid, setgid and sticky
/// bits survive, a read-only directory still receives its files, and times are the tar's.
#[test]
fn modes_survive_the_owner_and_directories_do_not_block_their_contents() {
    let layer = tar(&[
        Item::File {
            path: "bin/su",
            data: b"x",
            mode: 0o4755,
        },
        Item::File {
            path: "bin/wall",
            data: b"x",
            mode: 0o2755,
        },
        Item::Dir {
            path: "tmp",
            mode: 0o1777,
        },
        Item::Dir {
            path: "ro",
            mode: 0o555,
        },
        file("ro/inside", b"in"),
        Item::Dir {
            path: "ro/deeper",
            mode: 0o500,
        },
        file("ro/deeper/more", b"more"),
    ]);
    let (root, got) = unpack("unpack-modes", &[layer]);
    got.expect("unpacked");
    let mode = |rel: &str| {
        std::fs::symlink_metadata(root.join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("bin/su"), 0o4755);
    assert_eq!(mode("bin/wall"), 0o2755);
    assert_eq!(mode("tmp"), 0o1777);
    assert_eq!(mode("ro"), 0o555);
    assert_eq!(mode("ro/deeper"), 0o500);
    let mtime = |rel: &str| std::fs::symlink_metadata(root.join(rel)).unwrap().mtime();
    assert_eq!(mtime("bin/su"), 1_000_000);
    assert_eq!(mtime("ro"), 1_000_000);
    let (uid, gid) = my_ids();
    let meta = std::fs::symlink_metadata(root.join("bin/su")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (uid, gid));
    // Left writable for the test's own clean up.
    for d in ["ro/deeper", "ro"] {
        std::fs::set_permissions(root.join(d), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Catches the header's owner not being what is applied under `Ownership::Header`, and
/// an owner outside the container's ids being applied or ignored (the id is an error
/// naming it; 65535 is the last id that maps).
#[test]
fn the_headers_owner_is_applied_and_ids_beyond_the_namespace_are_refused() {
    let (uid, gid) = my_ids();
    let (root, got) = unpack_with(
        "unpack-header-owner",
        &[tar(&[file("f", b"x")])],
        Unpack::new(Ownership::Header),
    );
    got.expect("unpacked");
    let meta = std::fs::metadata(root.join("f")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (uid, gid));

    for (ids, what) in [
        ((70_000, u64::from(gid)), "uid"),
        ((u64::from(uid), 70_000), "gid"),
        ((1 << 40, u64::from(gid)), "big"),
    ] {
        let layer = tar_of(&[file("f", b"x")], ids);
        for ownership in [Ownership::Header, mine()] {
            let (_, got) = unpack_with(
                "unpack-id-range",
                std::slice::from_ref(&layer),
                Unpack::new(ownership),
            );
            match got {
                Err(UnpackError::IdRange { id, max, .. }) => {
                    assert_eq!(max, 65_535);
                    assert!(id == 70_000 || id == 1 << 40, "{what}: {id}");
                }
                other => panic!("{what}: {other:?}"),
            }
        }
    }
}

/// Catches a whiteout that misses its file, takes a directory's subtree only in part,
/// touches what its own layer wrote, or is itself written (the "skipped whiteout"
/// mutant): the lower layer's file is gone, a whiteout of a directory removes it whole,
/// a whiteout beside a same-layer file leaves that file, a whiteout of nothing is
/// fine, and no `.wh.` file is ever in the root.
#[test]
fn a_whiteout_deletes_what_lower_layers_made_and_is_not_written() {
    let lower = tar(&[
        file("a", b"a"),
        file("b", b"b"),
        file("tree/deep/f", b"f"),
        file("keep", b"keep"),
    ]);
    let upper = tar(&[
        file(".wh.a", b""),
        file(".wh.tree", b""),
        file(".wh.nothing-here", b""),
        file("b", b"rewritten"),
        file(".wh.b", b""),
        file(".wh..wh.plnk", b""),
    ]);
    let (root, got) = unpack("unpack-whiteout", &[lower, upper]);
    got.expect("unpacked");
    assert_eq!(names(&root, "."), ["b", "keep"]);
    assert_eq!(read(&root, "b"), "rewritten");
}

/// Catches an opaque marker that leaves lower layers' files, removes its own layer's
/// (whichever order they come in the tar), or removes siblings of its directory:
/// afterwards the directory holds only this layer's entries, parents never listed in
/// the tar included.
#[test]
fn an_opaque_directory_hides_lower_layers_but_not_its_own() {
    let lower = tar(&[
        file("d/old1", b"1"),
        file("d/sub/old2", b"2"),
        file("sibling", b"s"),
        file("e/old3", b"3"),
        file("g/old4", b"4"),
    ]);
    let upper = tar(&[
        file("d/new-before", b"n"),
        file("d/.wh..wh..opq", b""),
        file("d/new-after", b"n"),
        file("d/sub2/inner", b"i"),
        file("e/.wh..wh..opq", b""),
        // A directory the lower layers do not have: nothing to hide.
        file("fresh/.wh..wh..opq", b""),
        file("fresh/x", b"x"),
    ]);
    let (root, got) = unpack("unpack-opaque", &[lower, upper]);
    got.expect("unpacked");
    assert_eq!(names(&root, "d"), ["new-after", "new-before", "sub2"]);
    assert_eq!(names(&root, "e"), Vec::<String>::new());
    assert_eq!(names(&root, "g"), ["old4"]);
    assert_eq!(names(&root, "fresh"), ["x"]);
    assert!(exists(&root, "sibling"));
}

/// Catches a layer's replacement of one kind of entry by another failing: file by
/// directory, directory by file, symlink by directory, file by symlink; and a directory
/// entry over a directory keeping what is in it.
#[test]
fn an_entry_replaces_what_is_there_whatever_its_kind() {
    let lower = tar(&[
        file("was-file", b"f"),
        file("was-dir/inside", b"i"),
        Item::Symlink {
            path: "was-link",
            target: "was-file",
        },
        file("kept/inside", b"i"),
        file("now-link", b"x"),
    ]);
    let upper = tar(&[
        dir("was-file"),
        file("was-dir", b"now a file"),
        dir("was-link"),
        Item::Dir {
            path: "kept",
            mode: 0o700,
        },
        Item::Symlink {
            path: "now-link",
            target: "elsewhere",
        },
    ]);
    let (root, got) = unpack("unpack-replace", &[lower, upper]);
    got.expect("unpacked");
    assert!(root.join("was-file").is_dir());
    assert_eq!(read(&root, "was-dir"), "now a file");
    assert!(root.join("was-link").is_dir());
    assert_eq!(read(&root, "kept/inside"), "i");
    assert_eq!(
        std::fs::read_link(root.join("now-link")).unwrap(),
        Path::new("elsewhere")
    );
}

/// Catches hard links that are copies or dangle: within a layer and into a lower layer,
/// both names are one inode; a link to nothing is an error naming the target.
#[test]
fn hard_links_share_an_inode_inside_the_root() {
    let lower = tar(&[file("one", b"data")]);
    let upper = tar(&[
        Item::Hardlink {
            path: "two",
            target: "one",
        },
        file("three", b"3"),
        Item::Hardlink {
            path: "dir/four",
            target: "/three",
        },
    ]);
    let (root, got) = unpack("unpack-hardlink", &[lower, upper]);
    got.expect("unpacked");
    let inode = |rel: &str| std::fs::metadata(root.join(rel)).unwrap().ino();
    assert_eq!(inode("one"), inode("two"));
    assert_eq!(inode("three"), inode("dir/four"));
    assert_eq!(read(&root, "two"), "data");

    let (_, got) = unpack(
        "unpack-hardlink-missing",
        &[tar(&[Item::Hardlink {
            path: "x",
            target: "missing",
        }])],
    );
    let why = got.expect_err("dangling").to_string();
    assert!(why.contains("missing"), "{why}");
}

/// Catches a path that gets out of the root: `..` anywhere in a name is refused; an
/// absolute symlink is followed from the root, not the host's `/`; a relative one climbs
/// no higher than the root; both are followed for the entries below them, which land
/// inside; a symlink loop is an error; a whiteout named `...` is refused. A sentinel
/// directory beside the root stays empty.
#[test]
fn nothing_is_written_outside_the_root() {
    for (name, items) in [
        ("dotdot", vec![file("../escape", b"x")]),
        ("dotdot-deep", vec![file("a/../../escape", b"x")]),
        (
            "dotdot-link-target",
            vec![Item::Hardlink {
                path: "l",
                target: "../escape",
            }],
        ),
        ("whiteout-dotdot", vec![file(".wh...", b"")]),
        ("whiteout-dot", vec![file(".wh..", b"")]),
    ] {
        let (root, got) = unpack(&format!("unpack-escape-{name}"), &[tar(&items)]);
        assert!(
            matches!(got, Err(UnpackError::Unsafe(_))),
            "{name}: {got:?}"
        );
        assert!(!exists(root.parent().unwrap(), "escape"), "{name}");
    }

    let layer = tar(&[
        Item::Symlink {
            path: "abs",
            target: "/inside",
        },
        file("abs/one", b"1"),
        Item::Symlink {
            path: "up",
            target: "../../../../..",
        },
        file("up/two", b"2"),
        Item::Symlink {
            path: "rel",
            target: "sub/../inside",
        },
        file("rel/three", b"3"),
        Item::Symlink {
            path: "bin",
            target: "usr/bin",
        },
        file("bin/tool", b"t"),
        Item::Symlink {
            path: "lib64",
            target: "/usr/lib",
        },
        file("lib64/libc", b"c"),
    ]);
    let (root, got) = unpack("unpack-symlinks", &[layer]);
    got.expect("unpacked");
    assert_eq!(read(&root, "inside/one"), "1");
    assert_eq!(read(&root, "two"), "2");
    assert_eq!(read(&root, "inside/three"), "3");
    assert_eq!(read(&root, "usr/bin/tool"), "t");
    assert_eq!(read(&root, "usr/lib/libc"), "c");
    assert_eq!(names(root.parent().unwrap(), "."), ["0.tar", "rootfs"]);

    let loops = tar(&[
        Item::Symlink {
            path: "a",
            target: "b",
        },
        Item::Symlink {
            path: "b",
            target: "a",
        },
        file("a/f", b"x"),
    ]);
    let (_, got) = unpack("unpack-loop", &[loops]);
    assert!(matches!(got, Err(UnpackError::SymlinkLoop(_))), "{got:?}");
}

/// Catches device nodes and fifos being made (they need privileges an unprivileged
/// user lacks, and never belong in an image root), and an entry type nothing here
/// handles being taken for fine.
#[test]
fn special_files_are_skipped_and_unknown_types_are_errors() {
    let layer = tar(&[
        Item::Special {
            path: "dev-null",
            kind: EntryType::Char,
        },
        Item::Special {
            path: "disk",
            kind: EntryType::Block,
        },
        Item::Special {
            path: "pipe",
            kind: EntryType::Fifo,
        },
        Item::Special {
            path: "global",
            kind: EntryType::XGlobalHeader,
        },
        file("real", b"r"),
    ]);
    let (root, got) = unpack("unpack-special", &[layer]);
    let counts = got.expect("unpacked");
    assert_eq!(names(&root, "."), ["real"]);
    assert_eq!((counts.entries, counts.skipped), (1, 4));

    let (_, got) = unpack(
        "unpack-sparse",
        &[tar(&[Item::Special {
            path: "sparse",
            kind: EntryType::GNUSparse,
        }])],
    );
    assert!(matches!(got, Err(UnpackError::Archive { .. })), "{got:?}");
}

/// Catches the byte bound not holding: files beyond it are an error, files within it
/// are not.
#[test]
fn the_bytes_of_files_are_bounded() {
    let layers = [tar(&[file("a", &[0; 600])]), tar(&[file("b", &[0; 600])])];
    let (_, got) = unpack_with("unpack-bound", &layers, Unpack::new(mine()).max_bytes(1000));
    assert!(matches!(got, Err(UnpackError::TooBig(1000))), "{got:?}");
    let (_, got) = unpack_with(
        "unpack-bound-ok",
        &layers,
        Unpack::new(mine()).max_bytes(1200),
    );
    assert_eq!(got.expect("within the bound").bytes, 1200);
}

/// Catches a damaged layer being read as an empty one: gzip that is cut short or is not
/// gzip, a tar that is not a tar, a layer file that is gone, and a root that exists.
#[test]
fn damaged_inputs_are_errors() {
    let good = tar(&[file("f", &[7; 4000])]);
    let zipped = gzip(&good);
    let base = scratch("unpack-damaged");
    let write = |name: &str, bytes: &[u8]| {
        let p = base.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    };
    let run = |root: &str, layer: PathBuf, compression| {
        Unpack::new(mine()).run(&base.join(format!("root-{root}")), &[(layer, compression)])
    };
    let ok = run("ok", write("ok.gz", &zipped), Compression::Gzip).expect("good gzip");
    assert_eq!(ok.bytes, 4000);
    for (name, bytes, compression) in [
        (
            "cut.gz",
            zipped[..zipped.len() / 2].to_vec(),
            Compression::Gzip,
        ),
        ("notgz.gz", good.clone(), Compression::Gzip),
        ("junk.tar", vec![0xAB; 2000], Compression::None),
        ("cut.tar", good[..700].to_vec(), Compression::None),
    ] {
        let got = run(name, write(name, &bytes), compression);
        assert!(
            matches!(got, Err(UnpackError::Archive { .. })),
            "{name}: {got:?}"
        );
    }
    let got = run("gone", base.join("absent.tar"), Compression::None);
    assert!(matches!(got, Err(UnpackError::Io { .. })), "{got:?}");
    let got = run("ok", write("again.tar", &good), Compression::None);
    assert!(matches!(got, Err(UnpackError::Io { .. })), "{got:?}");
}

/// Catches the root's own entry (`./`) being dropped or another type there being made:
/// the root takes the tar's mode, and a non-directory named `./` is ignored.
#[test]
fn the_root_entry_sets_the_roots_mode() {
    let layer = tar(&[
        Item::Dir {
            path: "./",
            mode: 0o750,
        },
        file("f", b"x"),
    ]);
    let (root, got) = unpack("unpack-root", &[layer]);
    got.expect("unpacked");
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
        0o750
    );
    let (root, got) = unpack("unpack-root-file", &[tar(&[file("./", b"x"), dir("d")])]);
    got.expect("unpacked");
    assert_eq!(names(&root, "."), ["d"]);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Catches mount points made wrongly: directories and files made with parents, what is
/// there left alone (content and kind), a symlink on the way followed inside the root
/// (`etc/resolv.conf` pointing at `/run/x` makes `run/x`), and a name that climbs refused.
#[test]
fn mount_points_are_made_inside_the_root_and_left_alone_if_present() {
    let (root, got) = unpack(
        "unpack-ensure",
        &[tar(&[
            file("etc/hosts", b"mine"),
            dir("tmp"),
            Item::Symlink {
                path: "etc/resolv.conf",
                target: "/run/resolv",
            },
            Item::Symlink {
                path: "var",
                target: "/data",
            },
        ])],
    );
    got.expect("unpacked");
    ensure_paths(
        &root,
        &["tmp", "kbf/root", "var/tmp"],
        &["etc/hosts", "etc/hostname", "etc/resolv.conf", "sys/fs/x"],
    )
    .expect("ensured");
    assert_eq!(read(&root, "etc/hosts"), "mine");
    assert!(root.join("kbf/root").is_dir());
    assert!(
        root.join("data/tmp").is_dir(),
        "var -> /data was followed inside the root"
    );
    assert!(exists(&root, "etc/hostname"));
    assert!(root.join("run/resolv").is_file());
    assert!(root.join("sys/fs/x").is_file());
    let mode = |rel: &str| {
        std::fs::metadata(root.join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("kbf"), 0o755 & !0o022);
    let why = ensure_paths(&root, &["../outside"], &[]).expect_err("climbs");
    assert!(matches!(why, UnpackError::Unsafe(_)), "{why:?}");
    let why = ensure_paths(&root, &[], &["etc/hosts/under-a-file"]).expect_err("under a file");
    assert!(matches!(why, UnpackError::Io { .. }), "{why:?}");
    let why = ensure_paths(&root, &["etc/hosts/under-a-file"], &[]).expect_err("dir under a file");
    assert!(matches!(why, UnpackError::Io { .. }), "{why:?}");
    let loop_root = scratch("unpack-ensure-loop");
    std::os::unix::fs::symlink("l", loop_root.join("l")).unwrap();
    let why = ensure_paths(&loop_root, &["l/x"], &[]).expect_err("loop");
    assert!(matches!(why, UnpackError::SymlinkLoop(_)), "{why:?}");
}

/// Catches a hard link to a symlink following it (it links the symlink itself, as `ln`
/// without `-L` does), and a link, symlink or entry that names nothing or has a type
/// nothing here handles being taken for fine.
#[test]
fn links_to_links_and_entries_that_cannot_be_applied() {
    let (root, got) = unpack(
        "unpack-link-to-link",
        &[tar(&[
            file("target", b"t"),
            Item::Symlink {
                path: "sym",
                target: "target",
            },
            Item::Hardlink {
                path: "hard",
                target: "sym",
            },
        ])],
    );
    got.expect("unpacked");
    assert_eq!(
        std::fs::read_link(root.join("hard")).unwrap(),
        Path::new("target")
    );

    for (name, item) in [
        (
            "empty symlink",
            Item::Symlink {
                path: "s",
                target: "",
            },
        ),
        (
            "empty hard link",
            Item::Hardlink {
                path: "h",
                target: "",
            },
        ),
        (
            "unknown type",
            Item::Special {
                path: "z",
                kind: EntryType::new(b'Z'),
            },
        ),
    ] {
        let (_, got) = unpack(
            &format!("unpack-bad-{}", name.replace(' ', "-")),
            &[tar(&[item])],
        );
        assert!(
            matches!(got, Err(UnpackError::Archive { .. })),
            "{name}: {got:?}"
        );
    }
}

/// Catches an opaque marker in a place that is not a directory being taken for an empty
/// one, and a file the layer cannot remove or give its owner being taken for done (the
/// unprivileged test user may not re-own files, nor delete from a read-only directory).
#[test]
fn what_cannot_be_done_is_an_error() {
    let (_, got) = unpack(
        "unpack-opaque-file",
        &[
            tar(&[file("d", b"a file")]),
            tar(&[file("d/.wh..wh..opq", b"")]),
        ],
    );
    assert!(matches!(got, Err(UnpackError::Io { .. })), "{got:?}");

    if rustix::process::geteuid().is_root() {
        return;
    }
    let (root, got) = unpack(
        "unpack-read-only-parent",
        &[
            tar(&[
                Item::Dir {
                    path: "ro",
                    mode: 0o555,
                },
                file("ro/f", b"old"),
            ]),
            tar(&[file("ro/f", b"new")]),
        ],
    );
    assert!(matches!(got, Err(UnpackError::Io { .. })), "{got:?}");
    std::fs::set_permissions(root.join("ro"), std::fs::Permissions::from_mode(0o755)).ok();

    let (_, got) = unpack_with(
        "unpack-not-mine",
        &[tar(&[file("f", b"x")])],
        Unpack::new(Ownership::AllTo { uid: 0, gid: 0 }),
    );
    assert!(matches!(got, Err(UnpackError::Io { .. })), "{got:?}");
}

/// `tar` with the `len` bytes at `offset` of its first header overwritten with `z` (not
/// octal digits) and the header's checksum made right again, so the tar is still a tar
/// and only the field is bad.
fn with_bad_field(mut tar: Vec<u8>, offset: usize, len: usize) -> Vec<u8> {
    tar[offset..offset + len].fill(b'z');
    tar[148..156].fill(b' ');
    let sum: u32 = tar[..512].iter().map(|b| u32::from(*b)).sum();
    let text = format!("{sum:06o}\0 ");
    tar[148..156].copy_from_slice(text.as_bytes());
    tar
}

/// Catches a header whose mode or modification time is not a number being applied as
/// zero (the tar's own words for it are the error), for a file and for a directory.
#[test]
fn a_header_field_that_is_not_a_number_is_an_error() {
    let file_tar = || tar(&[file("f", b"x")]);
    let dir_tar = || tar(&[dir("d")]);
    for (name, bad) in [
        ("file mtime", with_bad_field(file_tar(), 136, 11)),
        ("file mode", with_bad_field(file_tar(), 100, 7)),
        ("dir mtime", with_bad_field(dir_tar(), 136, 11)),
        ("dir mode", with_bad_field(dir_tar(), 100, 7)),
    ] {
        let (_, got) = unpack(
            &format!("unpack-bad-field-{}", name.replace(' ', "-")),
            &[bad],
        );
        assert!(
            matches!(got, Err(UnpackError::Archive { .. })),
            "{name}: {got:?}"
        );
    }
}
