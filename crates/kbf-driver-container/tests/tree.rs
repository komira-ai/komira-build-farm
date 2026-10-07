//! Writing input roots and reading outputs: the rules that keep a client's tree from
//! reaching outside the lease's directory.

mod support;

use kbf_driver_container::cas::digest_of;
use kbf_driver_container::tree::{
    Exceeded, OutputLimits, TreeError, collect, materialize, output_paths,
    refuse_hidden_working_directory, refuse_outputs_in_inputs,
};
use kbf_driver_container::{CasError, MemoryCas};
use kbf_proto::reapi::{
    ActionResult, Command, Directory, DirectoryNode, FileNode, OutputSymlink, SymlinkNode, Tree,
};
use prost::Message;
use support::exists;

fn file(name: &str, digest: Option<kbf_proto::reapi::Digest>) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest,
        ..FileNode::default()
    }
}

async fn write(cas: &MemoryCas, directory: &Directory, name: &str) -> Result<(), TreeError> {
    let root = cas.insert(directory.encode_to_vec());
    let dir = support::scratch(&format!("tree-{name}"));
    materialize(cas, &root, &dir).await
}

/// Catches an entry name that would put a file outside the lease's directory (`..`,
/// a slash, an absolute path) or nowhere (empty, `.`, NUL).
#[tokio::test]
async fn names_that_are_not_one_component_are_refused() {
    let cas = MemoryCas::new();
    let blob = Some(cas.insert(b"x".to_vec()));
    for (i, name) in ["..", ".", "", "a/b", "/etc", "nul\0"]
        .into_iter()
        .enumerate()
    {
        let tree = Directory {
            files: vec![file(name, blob.clone())],
            ..Directory::default()
        };
        let outcome = write(&cas, &tree, &format!("name-{i}")).await;
        assert!(
            matches!(outcome, Err(TreeError::Invalid(_))),
            "{name:?}: {outcome:?}"
        );
    }
    let escaping_dir = Directory {
        directories: vec![DirectoryNode {
            name: "..".to_owned(),
            digest: Some(cas.insert(Directory::default().encode_to_vec())),
        }],
        ..Directory::default()
    };
    let outcome = write(&cas, &escaping_dir, "name-dir").await;
    assert!(matches!(outcome, Err(TreeError::Invalid(_))), "{outcome:?}");
}

/// Catches a name used twice in one directory (a symlink and a file named alike would
/// let the second write follow the first).
#[tokio::test]
async fn a_name_used_twice_is_refused() {
    let cas = MemoryCas::new();
    let blob = Some(cas.insert(b"x".to_vec()));
    let tree = Directory {
        files: vec![file("a", blob)],
        symlinks: vec![SymlinkNode {
            name: "a".to_owned(),
            target: "/etc/passwd".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    };
    let outcome = write(&cas, &tree, "twice").await;
    assert!(
        matches!(outcome, Err(TreeError::Invalid(ref why)) if why.contains("twice")),
        "{outcome:?}"
    );
}

/// Catches entries without digests, blobs that do not decode, and blobs the CAS lacks
/// or returns altered, each being written as something.
#[tokio::test]
async fn broken_trees_are_refused() {
    let cas = MemoryCas::new();
    let no_file_digest = Directory {
        files: vec![file("f", None)],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &no_file_digest, "no-file-digest").await,
        Err(TreeError::Invalid(_))
    ));
    let no_dir_digest = Directory {
        directories: vec![DirectoryNode {
            name: "d".to_owned(),
            digest: None,
        }],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &no_dir_digest, "no-dir-digest").await,
        Err(TreeError::Invalid(_))
    ));

    let garbage = cas.insert(vec![0xff, 0xff, 0xff]);
    let dir = support::scratch("tree-garbage");
    let outcome = materialize(&cas, &garbage, &dir).await;
    assert!(
        matches!(outcome, Err(TreeError::Invalid(ref why)) if why.contains("does not decode")),
        "{outcome:?}"
    );

    let absent = Directory {
        files: vec![file("f", Some(digest_of(b"never stored")))],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &absent, "absent").await,
        Err(TreeError::Cas(CasError::Missing(_)))
    ));

    let blob = cas.insert(b"good".to_vec());
    cas.corrupt(&blob, b"evil".to_vec());
    let corrupt = Directory {
        files: vec![file("f", Some(blob))],
        ..Directory::default()
    };
    assert!(matches!(
        write(&cas, &corrupt, "corrupt").await,
        Err(TreeError::Cas(CasError::Corrupt(..)))
    ));
}

/// Catches a whole tree written wrong: a file's bytes, its executable bit (set, or
/// set where it was not asked for), a symlink written as anything but a link to its
/// target, and a subdirectory flattened or left empty.
#[tokio::test]
async fn a_whole_tree_is_written_as_given() {
    use std::os::unix::fs::PermissionsExt;
    let cas = MemoryCas::new();
    let leaf = Directory {
        files: vec![file("leaf.txt", Some(cas.insert(b"leaf".to_vec())))],
        ..Directory::default()
    };
    let tree = Directory {
        files: vec![
            file("plain.txt", Some(cas.insert(b"plain".to_vec()))),
            FileNode {
                is_executable: true,
                ..file("tool.sh", Some(cas.insert(b"#!/bin/sh\n".to_vec())))
            },
        ],
        directories: vec![DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(cas.insert(leaf.encode_to_vec())),
        }],
        symlinks: vec![SymlinkNode {
            name: "link".to_owned(),
            target: "sub/leaf.txt".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    };
    let root = cas.insert(tree.encode_to_vec());
    let dir = support::scratch("tree-whole");
    materialize(&cas, &root, &dir).await.expect("written");
    let mode = |name: &str| {
        std::fs::metadata(dir.join(name))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(
        std::fs::read(dir.join("plain.txt")).expect("read"),
        b"plain"
    );
    assert_eq!((mode("plain.txt"), mode("tool.sh")), (0o644, 0o755));
    assert_eq!(
        std::fs::read(dir.join("sub/leaf.txt")).expect("read"),
        b"leaf"
    );
    assert_eq!(
        std::fs::read_link(dir.join("link")).expect("a symlink"),
        std::path::Path::new("sub/leaf.txt")
    );
}

/// Catches a write through a path that already exists: files are created exclusively,
/// so a second entry can never overwrite or follow the first.
#[tokio::test]
async fn files_are_created_exclusively() {
    let cas = MemoryCas::new();
    let blob = Some(cas.insert(b"x".to_vec()));
    let tree = Directory {
        files: vec![file("f", blob)],
        ..Directory::default()
    };
    let root = cas.insert(tree.encode_to_vec());
    let dir = support::scratch("tree-exclusive");
    std::fs::write(dir.join("f"), b"already here").expect("plant");
    let outcome = materialize(&cas, &root, &dir).await;
    assert!(matches!(outcome, Err(TreeError::Io { .. })), "{outcome:?}");
    assert!(outcome.expect_err("io").to_string().contains("exists"));
    assert!(exists(&dir.join("f")));
}

/// Catches older clients' outputs being ignored (REAPI before 2.1 lists files and
/// directories separately), and an empty or escaping output path being accepted.
#[test]
#[allow(deprecated)]
fn output_paths_old_and_new() {
    let new = Command {
        output_paths: vec!["a".to_owned(), "b/c".to_owned()],
        output_files: vec!["ignored".to_owned()],
        ..Command::default()
    };
    assert_eq!(output_paths(&new).expect("paths"), ["a", "b/c"]);
    let old = Command {
        output_files: vec!["f".to_owned()],
        output_directories: vec!["d".to_owned()],
        ..Command::default()
    };
    assert_eq!(output_paths(&old).expect("paths"), ["f", "d"]);
    for bad in ["", "../up", "/abs", "a/./b"] {
        let command = Command {
            output_paths: vec![bad.to_owned()],
            ..Command::default()
        };
        assert!(
            matches!(output_paths(&command), Err(TreeError::Invalid(_))),
            "{bad:?}"
        );
    }
}

/// Catches an output that is already an input being run: the driver reads outputs from
/// the overlay's upper layer, which lacks the unchanged inputs, so the result would be
/// cached incomplete. Also catches the opposite defect, a path that only passes
/// through an input (a parent that is an input directory, or a component that is an
/// input file or symlink the driver's upper directory hides) being refused, a symlink
/// in the input root being followed out of it, and an unreadable input directory read
/// as "no overlap".
#[tokio::test]
async fn outputs_that_are_inputs_are_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = support::scratch("tree-overlap");
    let outside = support::scratch("tree-overlap-outside");
    std::fs::create_dir_all(root.join("pkg/sub")).expect("mkdir");
    std::fs::create_dir_all(outside.join("x")).expect("mkdir outside");
    std::fs::write(root.join("pkg/in.txt"), b"in").expect("write");
    std::fs::write(root.join("file"), b"f").expect("write");
    symlink(&outside, root.join("pkg/escape")).expect("symlink");
    let check = |wd: &'static str, output: &str| {
        let root = root.clone();
        let outputs = vec![output.to_owned()];
        async move { refuse_outputs_in_inputs(&root, wd, &outputs).await }
    };
    for (wd, output) in [
        ("", "pkg"),
        ("", "pkg/in.txt"),
        ("pkg", "in.txt"),
        ("pkg", "sub"),
        ("pkg", "escape"),
    ] {
        let outcome = check(wd, output).await;
        assert!(
            matches!(outcome, Err(TreeError::Invalid(ref why)) if why.contains(output)),
            "{wd:?} {output:?}: {outcome:?}"
        );
    }
    for (wd, output) in [
        ("", "out"),
        ("", "pkg/new"),
        ("pkg", "sub/new/deeper"),
        ("absent", "pkg"),
        ("", "file/x"),
        ("pkg", "escape/x"),
    ] {
        let outcome = check(wd, output).await;
        assert!(outcome.is_ok(), "{wd:?} {output:?}: {outcome:?}");
    }

    std::fs::set_permissions(root.join("pkg"), std::fs::Permissions::from_mode(0o000))
        .expect("chmod");
    let outcome = check("", "pkg/in.txt").await;
    std::fs::set_permissions(root.join("pkg"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod back");
    assert!(matches!(outcome, Err(TreeError::Io { .. })), "{outcome:?}");
}

/// Catches a working directory that is an input symlink or file, or lies below one,
/// being run: the driver makes the working directory in the upper layer, which hides
/// the input of that name, so the action would start in an empty directory instead of
/// its inputs. A working directory that is a directory, or absent, is fine.
#[tokio::test]
async fn a_working_directory_that_is_not_an_input_directory_is_refused() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = support::scratch("tree-workdir");
    std::fs::create_dir_all(root.join("pkg/sub")).expect("mkdir");
    std::fs::write(root.join("file"), b"f").expect("write");
    symlink("pkg", root.join("link")).expect("symlink");
    symlink("pkg/sub", root.join("pkg/to-sub")).expect("symlink");
    for wd in ["link", "file", "file/x", "pkg/to-sub", "link/sub"] {
        let outcome = refuse_hidden_working_directory(&root, wd).await;
        assert!(
            matches!(outcome, Err(TreeError::Invalid(ref why)) if why.contains(wd)),
            "{wd:?}: {outcome:?}"
        );
    }
    for wd in ["", "pkg", "pkg/sub", "absent", "pkg/absent/deeper"] {
        let outcome = refuse_hidden_working_directory(&root, wd).await;
        assert!(outcome.is_ok(), "{wd:?}: {outcome:?}");
    }

    std::fs::set_permissions(root.join("pkg"), std::fs::Permissions::from_mode(0o000))
        .expect("chmod");
    let outcome = refuse_hidden_working_directory(&root, "pkg/sub").await;
    std::fs::set_permissions(root.join("pkg"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod back");
    assert!(matches!(outcome, Err(TreeError::Io { .. })), "{outcome:?}");
}

/// The bytes of the host file every collect test below plants out of the action's reach.
const SECRET: &[u8] = b"host secret, never an output";

/// A directory standing for a host path the action must never read: holds `key.pem`.
fn host_dir(name: &str) -> std::path::PathBuf {
    let dir = support::scratch(name);
    std::fs::write(dir.join("key.pem"), SECRET).expect("plant the host file");
    dir
}

/// Collects `outputs` under `working_directory` in `upper` into a fresh CAS, and checks
/// that the host file's bytes never reached it.
async fn collect_from(
    upper: &std::path::Path,
    working_directory: &str,
    outputs: &[&str],
) -> (Result<(), TreeError>, ActionResult, MemoryCas) {
    collect_within(upper, working_directory, outputs, OutputLimits::DEFAULT).await
}

/// [`collect_from`] within `limits`.
async fn collect_within(
    upper: &std::path::Path,
    working_directory: &str,
    outputs: &[&str],
    limits: OutputLimits,
) -> (Result<(), TreeError>, ActionResult, MemoryCas) {
    let cas = MemoryCas::new();
    let outputs: Vec<String> = outputs.iter().map(|&o| o.to_owned()).collect();
    let mut result = ActionResult::default();
    let outcome = collect(
        &cas,
        upper,
        working_directory,
        &outputs,
        limits,
        &mut result,
    )
    .await;
    assert_eq!(
        cas.blob(&digest_of(SECRET)),
        None,
        "the host file was uploaded: {result:?}"
    );
    (outcome, result, cas)
}

/// Catches output collection following a parent directory that the action replaced
/// with a symlink to a host path (`rm -rf d; ln -s /host/dir d`, output `d/key.pem`),
/// directly below the upper directory or deeper, absolute or relative: the daemon
/// would upload the host file as the action's output. Such an output is left out.
#[tokio::test]
async fn a_parent_symlinked_out_of_the_upper_directory_is_not_followed() {
    use std::os::unix::fs::symlink;

    let host = host_dir("collect-parent-host");
    let upper = support::scratch("collect-parent");
    symlink(&host, upper.join("d")).expect("symlink");
    std::fs::create_dir(upper.join("e")).expect("mkdir");
    symlink(&host, upper.join("e/f")).expect("symlink");
    symlink("../collect-parent-host", upper.join("rel")).expect("symlink");
    let (outcome, result, _) =
        collect_from(&upper, "", &["d/key.pem", "e/f/key.pem", "rel/key.pem"]).await;
    outcome.expect("collected");
    assert_eq!(result, ActionResult::default());
}

/// Catches output collection following a working directory that the action replaced
/// with a symlink to a host path: every output would be read from the host. The
/// working directory's components are walked like the outputs', so the outputs are
/// left out, whether the symlink is the working directory or a directory above it.
#[tokio::test]
async fn a_symlinked_working_directory_is_not_followed() {
    use std::os::unix::fs::symlink;

    let host = host_dir("collect-wd-host");
    let upper = support::scratch("collect-wd");
    symlink(&host, upper.join("wd")).expect("symlink");
    std::fs::create_dir(upper.join("pkg")).expect("mkdir");
    symlink(host.parent().expect("a parent"), upper.join("pkg/up")).expect("symlink");
    for (wd, output) in [("wd", "key.pem"), ("pkg/up/collect-wd-host", "key.pem")] {
        let (outcome, result, _) = collect_from(&upper, wd, &[output]).await;
        outcome.expect("collected");
        assert_eq!(result, ActionResult::default(), "{wd:?} {output:?}");
    }
}

/// Catches `..` (or `.`, or an empty component) in a working directory or an output
/// path reaching collect and walking out of the upper directory: refused as the
/// client's error, whichever path carries it. `output_paths` and prepare refuse them
/// first; collect does not rely on that.
#[tokio::test]
async fn a_dot_dot_component_is_refused() {
    let base = host_dir("collect-dotdot");
    let upper = base.join("upper");
    std::fs::create_dir_all(upper.join("a/b")).expect("mkdir");
    for (wd, output) in [
        ("", "../key.pem"),
        ("", "a/../../key.pem"),
        ("..", "key.pem"),
        ("a/..", "../key.pem"),
        ("", "./a/b"),
        ("", "a//b"),
        ("", ""),
    ] {
        let (outcome, result, _) = collect_from(&upper, wd, &[output]).await;
        assert!(
            matches!(outcome, Err(TreeError::Invalid(_))),
            "{wd:?} {output:?}: {outcome:?}"
        );
        assert_eq!(result, ActionResult::default(), "{wd:?} {output:?}");
    }
}

/// Catches a symlink being dereferenced where it is the output, or inside an output
/// directory: a declared output that is a symlink to a host directory or file must be
/// recorded as an `OutputSymlink` with its target as written, and a symlink in an
/// output directory as a `SymlinkNode`, never as the host's contents. The files,
/// subdirectory and FIFO beside them are read as found (the FIFO left out), so the
/// test also catches collecting nothing at all, a lost executable bit, and a FIFO
/// read or failing the action.
#[tokio::test]
async fn a_declared_output_that_is_a_symlink_is_recorded_not_followed() {
    use rustix::fs::{CWD, FileType, Mode, mknodat};
    use std::os::unix::fs::{PermissionsExt, symlink};

    let host = host_dir("collect-link-host");
    let upper = support::scratch("collect-link");
    let host_file = host.join("key.pem");
    symlink(&host, upper.join("link-dir")).expect("symlink");
    symlink(&host_file, upper.join("link-file")).expect("symlink");
    std::fs::write(upper.join("tool.sh"), b"#!/bin/sh\n").expect("write");
    std::fs::set_permissions(
        upper.join("tool.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    std::fs::create_dir_all(upper.join("out/sub")).expect("mkdir");
    std::fs::write(upper.join("out/plain.txt"), b"plain").expect("write");
    std::fs::write(upper.join("out/sub/n.txt"), b"nested").expect("write");
    symlink(&host, upper.join("out/inner")).expect("symlink");
    mknodat(CWD, upper.join("out/fifo"), FileType::Fifo, Mode::RUSR, 0).expect("mkfifo");
    let (outcome, result, cas) =
        collect_from(&upper, "", &["link-dir", "link-file", "tool.sh", "out"]).await;
    outcome.expect("collected");
    let [tool] = result.output_files.as_slice() else {
        panic!("one output file: {result:?}");
    };
    assert_eq!(
        (tool.path.as_str(), tool.digest.clone(), tool.is_executable),
        ("tool.sh", Some(digest_of(b"#!/bin/sh\n")), true)
    );

    let shown = |p: &std::path::Path| p.to_string_lossy().into_owned();
    assert_eq!(
        result.output_symlinks,
        [
            OutputSymlink {
                path: "link-dir".to_owned(),
                target: shown(&host),
                ..OutputSymlink::default()
            },
            OutputSymlink {
                path: "link-file".to_owned(),
                target: shown(&host_file),
                ..OutputSymlink::default()
            },
        ]
    );
    let [out] = result.output_directories.as_slice() else {
        panic!("one output directory: {result:?}");
    };
    let tree = Tree::decode(
        cas.blob(out.tree_digest.as_ref().expect("a tree digest"))
            .expect("the tree is stored")
            .as_slice(),
    )
    .expect("a Tree");
    let root = tree.root.expect("a root");
    assert_eq!(
        root.symlinks,
        [SymlinkNode {
            name: "inner".to_owned(),
            target: shown(&host),
            ..SymlinkNode::default()
        }]
    );
    assert_eq!(root.files, [file("plain.txt", Some(digest_of(b"plain")))]);
    let sub = Directory {
        files: vec![file("n.txt", Some(digest_of(b"nested")))],
        ..Directory::default()
    };
    assert_eq!(
        root.directories,
        [DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(digest_of(&sub.encode_to_vec())),
        }]
    );
    assert_eq!(tree.children, [sub]);
}

/// Catches what is neither file, directory nor symlink (here a FIFO) being read or
/// failing the action, and an output that cannot be examined (its directory lacks
/// search permission) being taken as absent: it fails, naming the output.
#[tokio::test]
async fn odd_entries_are_left_out_and_unexaminable_ones_fail() {
    use rustix::fs::{CWD, FileType, Mode, mknodat};
    use std::os::unix::fs::PermissionsExt;

    let upper = support::scratch("collect-odd");
    mknodat(CWD, upper.join("fifo"), FileType::Fifo, Mode::RUSR, 0).expect("mkfifo");
    let (outcome, result, _) = collect_from(&upper, "", &["fifo"]).await;
    outcome.expect("collected");
    assert_eq!(result, ActionResult::default());

    std::fs::create_dir(upper.join("ro")).expect("mkdir");
    std::fs::write(upper.join("ro/x"), b"x").expect("write");
    std::fs::set_permissions(upper.join("ro"), std::fs::Permissions::from_mode(0o400))
        .expect("chmod");
    let (outcome, _, _) = collect_from(&upper, "", &["ro/x"]).await;
    std::fs::set_permissions(upper.join("ro"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod back");
    assert!(
        matches!(outcome, Err(TreeError::Io { ref path, .. }) if path.ends_with("ro/x")),
        "{outcome:?}"
    );
}

/// Runs `collect` of the output `out` in `upper` within `limits` on a thread with a
/// 256 KiB stack, an eighth of a tokio worker's.
fn collect_on_a_small_stack(
    upper: &std::path::Path,
    limits: OutputLimits,
) -> (Result<(), TreeError>, ActionResult, MemoryCas) {
    let upper = upper.to_owned();
    std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(collect_within(&upper, "", &["out"], limits))
        })
        .expect("spawn")
        .join()
        .expect("the walk finished")
}

/// Catches a recursive walk of an output directory. The action decides how deep its
/// outputs are, and a walk that recursed once per level ran a 3000-level tree off the
/// thread's stack: SIGABRT, the whole daemon down with every lease on it. The walk
/// must take 3000 levels on a 256 KiB stack, where a recursive one overflows (seen
/// red: the test binary aborts), and record every level, each Directory's digest
/// naming the next.
#[test]
fn a_tree_of_any_depth_is_walked_without_recursion() {
    const LEVELS: usize = 3000;
    let upper = support::scratch("collect-deep");
    std::fs::create_dir(upper.join("out")).expect("mkdir");
    support::deep(&upper.join("out"), LEVELS, b"bottom");
    let limits = OutputLimits {
        max_depth: LEVELS,
        ..OutputLimits::DEFAULT
    };
    let (outcome, result, cas) = collect_on_a_small_stack(&upper, limits);
    outcome.expect("collected");
    let [out] = result.output_directories.as_slice() else {
        panic!("one output directory: {result:?}");
    };
    let tree = Tree::decode(
        cas.blob(out.tree_digest.as_ref().expect("a tree digest"))
            .expect("the tree is stored")
            .as_slice(),
    )
    .expect("a Tree");
    assert_eq!(tree.children.len(), LEVELS);
    let mut above = tree.root.expect("a root");
    for child in tree.children {
        assert_eq!(
            above.directories,
            [DirectoryNode {
                name: "d".to_owned(),
                digest: Some(digest_of(&child.encode_to_vec())),
            }]
        );
        above = child;
    }
    assert_eq!(above.files, [file("f", Some(digest_of(b"bottom")))]);
    support::force_remove(&upper);
}

/// Catches an output limit not enforced, off by one, or counted per output instead
/// of per action: outputs that reach each limit exactly are collected, and one past
/// it fails as `TreeError::Limit`, naming the limit, its flag and where it was passed.
/// Seen red with the depth check removed.
#[tokio::test]
async fn outputs_past_a_limit_fail_the_collection() {
    let upper = support::scratch("collect-limits");
    // `top`, and `out` holding a/b/c (levels 1 to 3) and a/b/c/x: 6 entries, two files
    // of 5 bytes.
    std::fs::create_dir_all(upper.join("out/a/b/c")).expect("mkdir");
    std::fs::write(upper.join("out/a/b/c/x"), b"12345").expect("write");
    std::fs::write(upper.join("top"), b"67890").expect("write");
    let exact = OutputLimits {
        max_depth: 3,
        max_entries: 6,
        max_bytes: 10,
    };
    let (outcome, result, _) = collect_within(&upper, "", &["top", "out"], exact).await;
    outcome.expect("collected at the limits");
    assert_eq!(
        (result.output_files.len(), result.output_directories.len()),
        (1, 1)
    );

    let depth = |max_depth| OutputLimits { max_depth, ..exact };
    let entries = |max_entries| OutputLimits {
        max_entries,
        ..exact
    };
    let bytes = |max_bytes| OutputLimits { max_bytes, ..exact };
    for (limits, what, at, flag, limit) in [
        (
            depth(2),
            Exceeded::Depth,
            "out/a/b/c",
            "--output-max-depth",
            2,
        ),
        (
            entries(5),
            Exceeded::Entries,
            "out/a/b/c",
            "--output-max-entries",
            5,
        ),
        (
            entries(0),
            Exceeded::Entries,
            "top",
            "--output-max-entries",
            0,
        ),
        (
            bytes(9),
            Exceeded::Bytes,
            "out/a/b/c/x",
            "--output-max-bytes",
            9,
        ),
        (bytes(4), Exceeded::Bytes, "top", "--output-max-bytes", 4),
    ] {
        let (outcome, _, _) = collect_within(&upper, "", &["top", "out"], limits).await;
        let Err(
            why @ TreeError::Limit {
                path,
                what: seen,
                limit: seen_limit,
            },
        ) = &outcome
        else {
            panic!("{what:?} past {limits:?}: {outcome:?}");
        };
        assert_eq!(
            (*seen, path.strip_prefix(&upper).ok(), *seen_limit),
            (what, Some(std::path::Path::new(at)), limit),
        );
        let why = why.to_string();
        assert!(
            why.contains(flag) && why.contains(&format!("({limit})")),
            "{why}"
        );
    }
}
