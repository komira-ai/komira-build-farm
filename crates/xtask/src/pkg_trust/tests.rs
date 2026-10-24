use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::*;

const ROOT: &str =
    "{\"mediaType\":\"application/vnd.dev.sigstore.trustedroot+json;version=0.1\"}\n";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-pkg-trust-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A `gh` and a trusted root in `dir`.
fn inputs(dir: &Path, gh: &[u8], root: &str) -> (PathBuf, PathBuf) {
    let (g, r) = (dir.join("gh-in"), dir.join("root-in"));
    std::fs::write(&g, gh).expect("gh");
    std::fs::write(&r, root).expect("root");
    (g, r)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

fn refused(r: Result<Vec<String>, Error>) -> Vec<String> {
    match r {
        Err(Error::Refused(p)) => p,
        other => panic!("not refused: {other:?}"),
    }
}

#[test]
fn the_trust_directory_is_the_three_files_with_the_branch_in_the_conf() {
    // Catches: a conf that names a ref other than the one given (the case that matters:
    // a package built on a feature branch that trusts main, or a package built on main
    // that trusts a feature branch, for which every release verifies as the wrong
    // branch's), a conf with a key or comment beyond SOURCE_REFS, `gh` that is not
    // executable (a host's installer could not run it), the data files executable, a
    // file other than the three, and bytes not copied.
    for source_ref in ["refs/heads/main", "refs/heads/team/topic-2"] {
        let dir = scratch("good");
        let (gh, root) = inputs(&dir, b"\x7fELF gh", ROOT);
        let out = dir.join("trust");
        let printed = write(&gh, &root, source_ref, &out).expect("write");
        assert_eq!(printed.len(), 4, "{printed:?}");
        assert_eq!(
            printed[3],
            format!("installer.conf: SOURCE_REFS={source_ref}")
        );
        let mut names: Vec<String> = std::fs::read_dir(&out)
            .expect("ls")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["gh", "installer.conf", "trusted_root.jsonl"]);
        assert_eq!(
            std::fs::read_to_string(out.join("installer.conf")).expect("conf"),
            format!("SOURCE_REFS={source_ref}\n")
        );
        assert_eq!(std::fs::read(out.join("gh")).expect("gh"), b"\x7fELF gh");
        assert_eq!(
            std::fs::read_to_string(out.join("trusted_root.jsonl")).expect("root"),
            ROOT
        );
        for (name, want) in TRUST_FILES {
            assert_eq!(mode(&out.join(name)), want, "{name}");
        }
    }
}

#[test]
fn a_ref_the_installer_would_refuse_is_refused_before_anything_is_written() {
    // Catches: a ref that is not a branch (a tag, a bare name, an empty branch), one with
    // a space, `..` or a doubled slash (the installer passes it to gh as an argument),
    // and, the injection, a newline that would add a second key (RUN_AS_UID=0) to the
    // conf; and a partial write after the refusal.
    let bad = [
        "main",
        "refs/tags/v1",
        "refs/heads/",
        "refs/heads/a b",
        "refs/heads/a..b",
        "refs/heads//a",
        "refs/heads/a/",
        "",
        "refs/heads/main\nRUN_AS_UID=0",
        "refs/heads/main RUN_AS_UID=0",
    ];
    for source_ref in bad {
        let dir = scratch("bad-ref");
        let (gh, root) = inputs(&dir, b"gh", ROOT);
        let out = dir.join("trust");
        let problems = refused(write(&gh, &root, source_ref, &out));
        assert_eq!(problems.len(), 1, "{source_ref:?}: {problems:?}");
        assert!(
            problems[0].contains("is not a branch the installer accepts"),
            "{problems:?}"
        );
        assert!(!out.exists(), "{source_ref:?}: wrote the directory");
    }
}

#[test]
fn an_empty_gh_and_a_trusted_root_that_is_not_json_lines_are_refused() {
    // Catches: a package built with an empty `gh` (it would install and never verify), an
    // empty, binary, non-JSON or non-object trusted root (an installer reading it
    // refuses every release), and the problems reported together.
    let dir = scratch("bad-inputs");
    let (gh, root) = inputs(&dir, b"", "");
    let problems = refused(write(&gh, &root, "refs/heads/main", &dir.join("t1")));
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].ends_with("gh-in is empty"), "{problems:?}");
    assert!(
        problems[1].ends_with("root-in: holds no JSON object"),
        "{problems:?}"
    );
    for (text, want) in [
        ("not json\n", "line 1: not JSON"),
        ("[1,2]\n", "line 1: not a JSON object"),
        ("{}\n{\n", "line 2: not JSON"),
        ("\n\n", "holds no JSON object"),
    ] {
        let (gh, root) = inputs(&dir, b"gh", text);
        let out = dir.join("t2");
        let problems = refused(write(&gh, &root, "refs/heads/main", &out));
        assert!(
            problems.iter().any(|p| p.contains(want)),
            "{text:?}: {problems:?}"
        );
        assert!(!out.exists());
    }
    let (gh, _) = inputs(&dir, b"gh", ROOT);
    std::fs::write(dir.join("root-in"), [0xff, 0xfe]).expect("binary");
    let problems = refused(write(
        &gh,
        &dir.join("root-in"),
        "refs/heads/main",
        &dir.join("t3"),
    ));
    assert!(problems[0].ends_with("is not UTF-8"), "{problems:?}");
}

#[test]
fn a_leftover_trust_directory_is_replaced_whole() {
    // Catches: a restored build cache (or an earlier run) that already holds the trust
    // directory failing the step, or surviving into the package: the old conf, an old
    // `gh`, and any stray file or subdirectory must be gone after the run.
    let dir = scratch("leftover");
    let (gh, root) = inputs(&dir, b"new-gh", ROOT);
    let out = dir.join("trust");
    write(&gh, &root, "refs/heads/main", &out).expect("first");
    std::fs::write(out.join("stale"), b"old").expect("stale file");
    std::fs::create_dir_all(out.join("sub")).expect("stale dir");
    std::fs::write(out.join("sub").join("x"), b"old").expect("stale nested");
    write(&gh, &root, "refs/heads/feature/x", &out).expect("second");
    let mut names: Vec<_> = std::fs::read_dir(&out)
        .expect("ls")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf8"))
        .collect();
    names.sort();
    assert_eq!(names, ["gh", "installer.conf", "trusted_root.jsonl"]);
    assert_eq!(
        std::fs::read_to_string(out.join("installer.conf")).expect("conf"),
        "SOURCE_REFS=refs/heads/feature/x\n"
    );
    assert_eq!(mode(&out.join("gh")), 0o755);
}

#[test]
fn an_out_path_that_is_not_a_directory_is_refused_and_left_alone() {
    // Catches: pkg-trust deleting a file or following a symlink at `--out`.
    let dir = scratch("notdir");
    let (gh, root) = inputs(&dir, b"gh", ROOT);
    let file = dir.join("file");
    std::fs::write(&file, b"keep").expect("file");
    let p = refused(write(&gh, &root, "refs/heads/main", &file));
    assert!(p[0].contains("not a directory"), "{p:?}");
    assert_eq!(std::fs::read(&file).expect("kept"), b"keep");
    let target = dir.join("target");
    std::fs::create_dir_all(&target).expect("target");
    std::fs::write(target.join("keep"), b"keep").expect("keep");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    refused(write(&gh, &root, "refs/heads/main", &link));
    assert!(target.join("keep").exists());
}

#[test]
fn an_out_path_that_cannot_be_inspected_is_refused_with_the_os_error() {
    // Catches: a stat failure other than "not there" (here a parent that is a file) being
    // treated as an absent directory and then failing later in create_dir_all, or being
    // swallowed.
    let dir = scratch("unstatable");
    let (gh, root) = inputs(&dir, b"gh", ROOT);
    let parent = dir.join("file");
    std::fs::write(&parent, b"keep").expect("file");
    let out = parent.join("trust");
    let p = refused(write(&gh, &root, "refs/heads/main", &out));
    assert!(p[0].starts_with(&format!("{}: ", out.display())), "{p:?}");
    assert_eq!(std::fs::read(&parent).expect("kept"), b"keep");
}

#[test]
fn conf_ref_reads_back_only_what_conf_writes() {
    // Catches: pkg-asset accepting a conf with more than the one line it generates (a
    // second key, a comment, CRLF, no final newline, an empty value), which the
    // installer might read differently from the build.
    for r in [
        "refs/heads/main",
        "refs/heads/team/topic-2",
        "refs/heads/a.b_c-d",
    ] {
        assert_eq!(conf_ref(&conf(r)), Some(r));
    }
    for text in [
        "SOURCE_REFS=refs/heads/main",
        "SOURCE_REFS=refs/heads/main\r\n",
        "SOURCE_REFS=refs/heads/main\nRUN_AS_UID=0\n",
        "# note\nSOURCE_REFS=refs/heads/main\n",
        "SOURCE_REFS=\n",
        "SOURCE_REFS=refs/heads/a refs/heads/b\n",
        "SOURCE_REFS = refs/heads/main\n",
        "",
    ] {
        assert_eq!(conf_ref(text), None, "{text:?}");
    }
}
