//! The `kbf-coverage` binary end to end, on a small workspace built in the test's
//! temporary directory (inside the target directory).
//!
//! Catches: the binary passing a run the ratchet fails (a crate missing more, or a
//! baseline looser than measured) or the reverse, exiting 1 on a bad input instead of
//! 2, a measured baseline that the binary cannot read back as passing, a directory
//! under `crates/` without `Cargo.toml` taken for a crate, and a crate with no records
//! left out of the table.
//!
//! Each test builds its own workspace in a fresh directory named for the test and the
//! process, so no test reads a file an earlier run (or a concurrent one) left behind.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fixture workspace, removed when dropped.
struct Workspace(PathBuf);

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for Workspace {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

/// A workspace with crates `a`, `b` and `empty` (no records), plus a loose file and a
/// directory without `Cargo.toml` under `crates/`, which are not crates. It lives in
/// `<name>-<pid>`, emptied first: a directory left by a run that was killed, or by an
/// earlier process with the same pid, never leaks files into this one.
fn workspace(name: &str) -> Workspace {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    // Absent on a normal run; anything else surfaces as the create or write below
    // failing, or as the `measured` freshness check.
    let _ = std::fs::remove_dir_all(&root);
    for krate in ["a", "b", "empty"] {
        let dir = root.join("crates").join(krate);
        std::fs::create_dir_all(&dir).expect("create crate dir");
        std::fs::write(dir.join("Cargo.toml"), "").expect("write Cargo.toml");
    }
    std::fs::create_dir_all(root.join("crates/notacrate")).expect("create dir");
    std::fs::write(root.join("crates/notes.txt"), "").expect("write file");
    let r = root.display();
    let lcov = format!(
        "SF:{r}/crates/a/src/lib.rs\nLF:4\nLH:3\nBRF:2\nBRH:2\nend_of_record\n\
         SF:{r}/crates/b/src/main.rs\nLF:10\nLH:10\nend_of_record\n"
    );
    std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
    Workspace(root)
}

fn run(root: &Path, baseline: &str, extra: &[&str]) -> Output {
    std::fs::write(root.join("coverage-baseline"), baseline).expect("write baseline");
    Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(root)
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .args(extra)
        .output()
        .expect("run kbf-coverage")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `a` misses 1 of 4 lines and none of 2 branches; `b` misses none of 10 lines.
const MATCHING: &str = "a 1 0\nb 0 -\nempty - -\n";

#[test]
fn passes_when_coverage_matches_the_baseline() {
    let root = workspace("matches");
    let o = run(&root, MATCHING, &[]);
    assert_eq!(o.status.code(), Some(0), "{}{}", stdout(&o), stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("| a | 3/4 | 75.00 | 25.00 | 1 | 2/2 | 100.00 | 0.00 | 0 | 1 / 0 | ok |"),
        "{out}"
    );
    assert!(
        out.contains("| empty | 0/0 | - | - | - | 0/0 | - | - | - | - / - | ok |"),
        "{out}"
    );
    assert!(
        !out.contains("notacrate") && !out.contains("notes"),
        "{out}"
    );
    assert!(out.ends_with("coverage ratchet: PASS\n"), "{out}");
}

/// The mutant this guards: a test deleted from crate `a` lowers its coverage.
#[test]
fn fails_when_a_crate_drops_below_its_baseline() {
    let root = workspace("drops");
    let o = run(&root, "a 0 0\nb 0 -\nempty - -\n", &[]);
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    assert!(
        stdout(&o).contains("MORE MISSED THAN BASELINE (lines)"),
        "{}",
        stdout(&o)
    );
}

/// The mutant this guards (issue 35): a baseline with slack passing. `a` misses 1 line
/// but the file allows 2, and `b` has measured lines where the file says `-`; either
/// would let a later change leave that much new code uncovered.
#[test]
fn fails_when_the_baseline_is_looser_than_measured() {
    let root = workspace("loose");
    for (baseline, want) in [
        (
            "a 2 0\nb 0 -\nempty - -\n",
            "| a | 3/4 | 75.00 | 25.00 | 1 | 2/2 | 100.00 | 0.00 | 0 | 2 / 0 | BASELINE LOOSER THAN MEASURED (lines); copy coverage-baseline.measured |",
        ),
        (
            "a 1 0\nb - -\nempty - -\n",
            "| b | 10/10 | 100.00 | 0.00 | 0 | 0/0 | - | - | - | - / - | BASELINE LOOSER THAN MEASURED (lines); copy coverage-baseline.measured |",
        ),
    ] {
        let o = run(&root, baseline, &[]);
        assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
        let out = stdout(&o);
        assert!(out.contains(want), "{out}");
        assert!(
            out.ends_with("coverage ratchet: FAIL (1 crate(s) need attention; see status)\n"),
            "{out}"
        );
    }
}

#[test]
fn measured_baseline_reads_back_as_passing() {
    let root = workspace("writes");
    let measured = root.join("measured");
    assert!(!measured.exists(), "fresh workspace");
    // An empty baseline fails (every crate unrecorded) but still writes the file.
    let o = run(
        &root,
        "",
        &["--write-baseline", measured.to_str().expect("UTF-8")],
    );
    assert_eq!(o.status.code(), Some(1), "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("NOT IN BASELINE"), "{}", stdout(&o));
    let written = std::fs::read_to_string(&measured).expect("a failing run writes the file");
    let o = run(&root, &written, &[]);
    assert_eq!(o.status.code(), Some(0), "{written}\n{}", stdout(&o));
}

#[test]
fn bad_inputs_exit_2_with_a_reason() {
    let root = workspace("bad");
    let expect_2 = |o: Output, want: &str| {
        assert_eq!(o.status.code(), Some(2), "{want}: {}", stderr(&o));
        assert!(stderr(&o).contains(want), "{want}: {}", stderr(&o));
    };
    expect_2(
        run(&root, "a 75.00 -\n", &[]),
        "coverage-baseline: line 1: not a count",
    );
    expect_2(
        run(&root, MATCHING, &["--write-baseline", "/nonexistent-dir/x"]),
        "write /nonexistent-dir/x:",
    );
    std::fs::remove_file(root.join("coverage-baseline")).expect("remove baseline");
    let o = Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(&*root)
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .output()
        .expect("run kbf-coverage");
    expect_2(o, "coverage-baseline: No such file");

    // A root without a crates directory: the crates cannot be listed.
    let o = Command::new(env!("CARGO_BIN_EXE_kbf-coverage"))
        .arg("--lcov")
        .arg(root.join("lcov.info"))
        .arg("--root")
        .arg(root.join("crates/a"))
        .arg("--baseline")
        .arg(root.join("coverage-baseline"))
        .output()
        .expect("run kbf-coverage");
    expect_2(o, "list crates under");

    // A tracefile that does not parse, one naming a file outside every crate, and a
    // missing one.
    for (lcov, want) in [
        ("", "lcov.info: the tracefile holds no records"),
        (
            "SF:/elsewhere/x.rs\nend_of_record\n",
            "/elsewhere/x.rs is not under",
        ),
    ] {
        std::fs::write(root.join("lcov.info"), lcov).expect("write lcov");
        expect_2(run(&root, MATCHING, &[]), want);
    }
    std::fs::remove_file(root.join("lcov.info")).expect("remove lcov");
    expect_2(run(&root, MATCHING, &[]), "lcov.info: No such file");
}
