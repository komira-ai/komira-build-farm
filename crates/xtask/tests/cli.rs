//! `cargo xtask` run as CI runs it: the real binary, with `xtask-fake` as cargo and as
//! the built kbf-server, and throwaway git repositories for `budget-diff`.

use kbf_testkit::dir::TestDir;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const HEAD: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const BUILD: &str = "build --locked -p kbf-server --bin kbf-server";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask"), name)
}

fn xtask(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(XTASK);
    cmd.arg("--root").arg(root).args(args);
    cmd
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The exit code, stdout and stderr of a finished run.
fn outcome(out: Output) -> (Option<i32>, String, String) {
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// `build-commit stamps` with the fake as cargo, planting `defect` (or none).
fn stamps(
    name: &str,
    defect: Option<&str>,
    commit: &str,
) -> (Option<i32>, String, String, TestDir) {
    let root = scratch(name);
    let mut cmd = xtask(&root, &["build-commit", "stamps", commit]);
    cmd.env("CARGO", FAKE).env("XTASK_FAKE_HEAD", &HEAD[..12]);
    // The caller's own override must not reach the plain build.
    cmd.env("KBF_BUILD_COMMIT_OVERRIDE", "dddddddddddd");
    match defect {
        Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
        None => cmd.env_remove("XTASK_FAKE_DEFECT"),
    };
    let (code, out, err) = outcome(cmd.output().expect("run xtask"));
    (code, out, err, root)
}

#[test]
fn stamps_passes_a_build_that_honours_the_override() {
    // Control: catches a stamps verb that refuses a correct build, builds the wrong
    // package, or lets the caller's own override reach the plain build.
    let (code, out, err, root) = stamps("stamps-good", None, HEAD);
    assert_eq!(code, Some(0), "stdout:\n{out}\nstderr:\n{err}");
    assert_eq!(
        out,
        "ok: kbf-server 0.1.0+c0ffee012345\nok: kbf-server 0.1.0+a1a1a1a1a1a1\n\
         ok: kbf-server 0.1.0+b2b2b2b2b2b2\nok: a malformed override fails the build\n"
    );
    let args = std::fs::read_to_string(root.join("target/debug/cargo-args")).expect("log");
    assert_eq!(args, format!("{BUILD}\n").repeat(4));
}

#[test]
fn stamps_refuses_a_build_that_ignores_the_override() {
    // Catches: the stamped builds not checked against their stamp (a build script that
    // drops the override would pass).
    let (code, _, err, _) = stamps("stamps-ignores", Some("ignores-override"), HEAD);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask-fake: built\nxtask-fake: built\nxtask: --version is \
         \"kbf-server 0.1.0+c0ffee012345\"; expected it to end +a1a1a1a1a1a1\n"
    );
}

#[test]
fn stamps_refuses_a_build_that_accepts_a_malformed_override() {
    // Catches: the malformed-override build not run, or its success not refused.
    let (code, _, err, _) = stamps("stamps-accepts-bad", Some("accepts-bad"), HEAD);
    assert_eq!(code, Some(1));
    assert!(
        err.ends_with("xtask: a build with KBF_BUILD_COMMIT_OVERRIDE=not-a-commit succeeded\n"),
        "{err}"
    );
}

#[test]
fn stamps_refuses_a_failure_that_is_not_on_the_override() {
    // Catches: any failed build taken as the refusal (a broken build would pass).
    let (code, _, err, _) = stamps("stamps-fails-other", Some("fails-other"), HEAD);
    assert_eq!(code, Some(1));
    assert!(
        err.ends_with("xtask: the build failed, but not on the override:\nerror: linking failed\n"),
        "{err}"
    );
}

#[test]
fn stamps_stops_at_a_failed_build_or_version() {
    // Catches: a failed build or `--version` run passed over instead of failing.
    let (code, _, err, _) = stamps("stamps-build-fails", Some("build-fails"), HEAD);
    assert_eq!(code, Some(1));
    assert!(
        err.ends_with("xtask: cargo build of kbf-server failed: exit status: 101\n"),
        "{err}"
    );
    let (code, _, err, root) = stamps("stamps-version-fails", Some("version-fails"), HEAD);
    assert_eq!(code, Some(1));
    let bin = root.join("target/debug/kbf-server");
    assert!(
        err.ends_with(&format!(
            "xtask: {} --version: exit status: 1\n",
            bin.display()
        )),
        "{err}"
    );
}

#[test]
fn stamps_refuses_a_commit_it_cannot_tell_from_a_stamp() {
    // Catches: a commit equal to a stamp, which makes the stamped builds prove nothing,
    // and a malformed commit, each refused before anything is built.
    let (code, _, err, root) = stamps("stamps-stamp-commit", None, "a1a1a1a1a1a1ffff");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask: a1a1a1a1a1a1ffff begins with a test stamp; the stamped builds cannot be told from it\n"
    );
    assert!(!root.join("target").exists());
    let (code, _, err, _) = stamps("stamps-not-commit", None, "");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask: not a commit: \"\" (expected 12 to 40 lowercase hex digits)\n"
    );
}

#[test]
fn stamps_stops_at_a_failed_stamped_build() {
    // Catches: a stamped build's failure passed over (the plain build passed, so a
    // verb that checked only the first build would report the stamps as fine).
    let (code, out, err, _) = stamps("stamps-stamp-fails", Some("stamp-fails"), HEAD);
    assert_eq!(code, Some(1));
    assert_eq!(out, "");
    assert_eq!(
        err,
        "xtask-fake: built\nerror: could not compile\n\
         xtask: cargo build of kbf-server failed: exit status: 101\n"
    );
}

#[test]
fn stamps_reports_a_malformed_override_build_that_does_not_start() {
    // Catches: the malformed-override build's spawn failure taken as the refusal it
    // looks for (a build that never ran must not count as one that refused).
    let root = scratch("stamps-vanish");
    let cargo = root.join("cargo");
    std::fs::hard_link(FAKE, &cargo).expect("link the fake cargo");
    let out = xtask(&root, &["build-commit", "stamps", HEAD])
        .env("CARGO", &cargo)
        .env("XTASK_FAKE_HEAD", &HEAD[..12])
        .env("XTASK_FAKE_DEFECT", "vanish")
        .output()
        .expect("run xtask");
    let (code, _, err) = outcome(out);
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with("xtask-fake: built\nxtask-fake: built\nxtask-fake: built\n"),
        "{err}"
    );
    assert!(
        err.contains(&format!("xtask: {}: ", cargo.display())),
        "{err}"
    );
}

#[test]
fn check_reports_a_binary_that_does_not_start() {
    // Catches: a binary that cannot run reported as naming no commit, or as passing.
    let root = scratch("check-missing");
    let missing = root.join("kbf-server");
    let out = xtask(&root, &["build-commit", "check"])
        .arg(&missing)
        .arg(HEAD)
        .output()
        .expect("run xtask");
    let (code, _, err) = outcome(out);
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", missing.display())),
        "{err}"
    );
}

#[test]
fn stamps_reports_a_cargo_that_does_not_start() {
    // Catches: a spawn failure reported as a passed or failed check.
    let root = scratch("stamps-no-cargo");
    let missing = root.join("no-cargo");
    let out = xtask(&root, &["build-commit", "stamps", HEAD])
        .env("CARGO", &missing)
        .output()
        .expect("run xtask");
    let (code, _, err) = outcome(out);
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", missing.display())),
        "{err}"
    );
}

/// A `kbf-server` (the fake, hard-linked) whose `--version` names `stamp`.
fn server(name: &str, stamp: &str) -> (PathBuf, TestDir) {
    let dir = scratch(name);
    let bin = dir.join("kbf-server");
    std::fs::hard_link(FAKE, &bin).expect("link");
    std::fs::write(dir.join("kbf-server.stamp"), stamp).expect("stamp");
    (bin, dir)
}

#[test]
fn check_passes_the_commit_the_binary_names_and_fails_another() {
    // Catches: a check that does not compare the stamp with the commit given (the
    // control inside the check catches one that accepts anything).
    let (bin, _own) = server("check", &HEAD[..12]);
    let run = |commit: &str| {
        let out = xtask(Path::new("."), &["build-commit", "check"])
            .arg(&bin)
            .arg(commit)
            .output()
            .expect("run xtask");
        outcome(out)
    };
    assert_eq!(
        run(HEAD),
        (
            Some(0),
            "ok: kbf-server 0.1.0+c0ffee012345\n".into(),
            String::new()
        )
    );
    let other = "d0ffee0123456789c0ffee0123456789c0ffee01";
    assert_eq!(
        run(other),
        (
            Some(1),
            String::new(),
            "xtask: --version is \"kbf-server 0.1.0+c0ffee012345\"; expected it to end +d0ffee012345\n"
                .into()
        )
    );
}

/// A `kbf` (the fake, hard-linked) whose `--version` names `stamp` and whose
/// `--version --json` prints `json`.
fn kbf(name: &str, stamp: &str, json: &str) -> (PathBuf, TestDir) {
    let dir = scratch(name);
    let bin = dir.join("kbf");
    std::fs::hard_link(FAKE, &bin).expect("link");
    std::fs::write(dir.join("kbf.stamp"), stamp).expect("stamp");
    std::fs::write(dir.join("kbf.json"), json).expect("json");
    (bin, dir)
}

/// The identity `kbf --version --json` prints, as a release build of `commit` makes it.
fn identity(commit: &str, profile: &str) -> String {
    format!(
        "{{\"admin_protocol\":1,\"commit\":\"{commit}\",\"meta_log\":4,\"min_protocol\":1,\"profile\":\"{profile}\",\"protocol\":1,\"seq\":812,\"state_schema\":1,\"version\":\"0.1.0\"}}\n"
    )
}

#[test]
fn check_json_passes_a_release_build_of_the_commit_and_names_what_is_wrong() {
    // Catches: the artifact check that reads only the plain `--version` line, so a
    // binary whose `--version --json` names another commit, or a debug build, or that
    // prints no object, ships; and a `--json` that accepts the control commit.
    let stamp = &HEAD[..12];
    let run = |name: &str, json: &str| {
        let (bin, _own) = kbf(name, stamp, json);
        outcome(
            xtask(Path::new("."), &["build-commit", "check", "--json"])
                .arg(&bin)
                .arg(HEAD)
                .output()
                .expect("run xtask"),
        )
    };
    assert_eq!(
        run("json-ok", &identity(stamp, "release")),
        (
            Some(0),
            "ok: kbf 0.1.0+c0ffee012345; --version --json: commit c0ffee012345, profile release\n"
                .into(),
            String::new()
        )
    );
    for (name, json, want) in [
        (
            "json-debug",
            identity(stamp, "debug"),
            "xtask: --version --json: \"profile\" is \"debug\"; expected \"release\"\n",
        ),
        (
            "json-commit",
            identity("d0ffee012345", "release"),
            "xtask: --version --json: \"commit\" is \"d0ffee012345\"; expected \"c0ffee012345\"\n",
        ),
        (
            "json-text",
            "kbf 0.1.0+c0ffee012345\n".to_owned(),
            "xtask: --version --json: not a JSON object: kbf 0.1.0+c0ffee012345\n",
        ),
        (
            "json-field",
            "{\"profile\":\"release\"}\n".to_owned(),
            "xtask: --version --json: no string field \"commit\"\n",
        ),
    ] {
        assert_eq!(
            run(name, &json),
            (Some(1), String::new(), want.into()),
            "{name}"
        );
    }
}

#[test]
fn check_json_reports_a_json_form_that_fails_or_is_not_an_object() {
    // Catches: a `--version --json` that exits non-zero passed over, or reported as the
    // plain form failing (the message names the form that failed); and valid JSON that
    // is not an object (an array, a bare string) read as an object with no fields.
    let (bin, _own) = kbf("json-fails", &HEAD[..12], &identity(&HEAD[..12], "release"));
    let out = xtask(Path::new("."), &["build-commit", "check", "--json"])
        .arg(&bin)
        .arg(HEAD)
        .env("XTASK_FAKE_DEFECT", "json-fails")
        .output()
        .expect("run xtask");
    assert_eq!(
        outcome(out),
        (
            Some(1),
            String::new(),
            format!(
                "xtask: {} --version --json: exit status: 1\n",
                bin.display()
            )
        )
    );
    for json in ["[1]\n", "\"release\"\n"] {
        let (bin, _own) = kbf("json-array", &HEAD[..12], json);
        let out = xtask(Path::new("."), &["build-commit", "check", "--json"])
            .arg(&bin)
            .arg(HEAD)
            .output()
            .expect("run xtask");
        assert_eq!(
            outcome(out),
            (
                Some(1),
                String::new(),
                format!(
                    "xtask: --version --json: not a JSON object: {}\n",
                    json.trim_end()
                )
            )
        );
    }
}

#[test]
fn check_without_json_does_not_run_the_json_form() {
    // Catches: the identity check made implicit for every binary, so the kbf-server and
    // kbf-daemon checks (which print no JSON) would fail: here a `kbf` whose JSON is
    // wrong still passes a plain check.
    let (bin, _own) = kbf("json-not-asked", &HEAD[..12], "garbage");
    let out = xtask(Path::new("."), &["build-commit", "check"])
        .arg(&bin)
        .arg(HEAD)
        .output()
        .expect("run xtask");
    assert_eq!(
        outcome(out),
        (
            Some(0),
            "ok: kbf 0.1.0+c0ffee012345\n".into(),
            String::new()
        )
    );
}

#[test]
fn a_usage_error_exits_2() {
    // Catches: a missing argument run as a check with an empty value.
    let out = Command::new(XTASK)
        .args(["build-commit", "check"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn the_darwin_verbs_refuse_a_wrong_call() {
    // Catches: a mistyped signing call taken as a check with an empty value: the
    // minimum macOS left out, a tarball without the binary's name, or nothing at all,
    // each exit 2 with the usage line.
    for args in [
        &["sign-darwin", "x"][..],
        &["sign-darwin", "--minos", "14.0"][..],
        &["darwin-asset", "x.tar.gz"][..],
        &["darwin-asset"][..],
    ] {
        let (code, out, err) = outcome(Command::new(XTASK).args(args).output().expect("run"));
        assert_eq!(code, Some(2), "{args:?}: {out}{err}");
        assert!(err.contains("Usage:"), "{args:?}: {err}");
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_darwin_verbs_fail_off_macos() {
    // Catches: a signing step that passes, signing nothing, on a host without codesign.
    for (args, verb) in [
        (&["sign-darwin", "x", "--minos", "14.0"][..], "sign-darwin"),
        (
            &["darwin-asset", "x.tar.gz", "kbf-daemon"][..],
            "darwin-asset",
        ),
    ] {
        let (code, out, err) = outcome(Command::new(XTASK).args(args).output().expect("run"));
        assert_eq!((code, out.as_str()), (Some(1), ""), "{args:?}");
        assert_eq!(
            err,
            format!("xtask: {verb} runs on macOS only: it needs codesign, vtool and csrutil\n")
        );
    }
}

/// A git repository whose one commit holds `budget` as the budget file.
fn repo(name: &str, budget: &str) -> TestDir {
    let root = scratch(name);
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&*root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    };
    git(&["init", "-q"]);
    std::fs::create_dir_all(root.join("tools")).expect("tools");
    std::fs::write(root.join("tools/interpreter_budget.tsv"), budget).expect("budget");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    root
}

const BASE: &str = "[files]\na.sh\tto port\n[workflow]\n[src-sh-c]\n";

fn budget_diff(root: &Path, base: &str) -> (Option<i32>, String, String) {
    outcome(
        xtask(root, &["budget-diff", "--base", base])
            .output()
            .expect("run xtask"),
    )
}

#[test]
fn budget_diff_refuses_a_new_row_and_passes_a_removal() {
    // Catches: a budget-diff that reads the base from the working tree (it would
    // compare the file with itself), or that refuses the list shrinking.
    let root = repo("budget-new-row", BASE);
    let file = root.join("tools/interpreter_budget.tsv");
    assert_eq!(
        budget_diff(&root, "HEAD"),
        (
            Some(0),
            "ok: tools/interpreter_budget.tsv adds no row to HEAD's\n".into(),
            String::new()
        )
    );
    std::fs::write(
        &file,
        BASE.replace("[src-sh-c]\n", "[src-sh-c]\nk.rs\tnew keeper\n"),
    )
    .expect("plant");
    assert_eq!(
        budget_diff(&root, "HEAD"),
        (
            Some(1),
            String::new(),
            "xtask: tools/interpreter_budget.tsv:5: a new [src-sh-c] row for `k.rs`; the \
             budget only shrinks: port it to Rust instead\n"
                .into()
        )
    );
    std::fs::write(&file, "[files]\n[workflow]\n[src-sh-c]\n").expect("shrink");
    assert_eq!(budget_diff(&root, "HEAD").0, Some(0));
}

/// `git <args>` in `root`, with the identity the commits need.
fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
}

/// A repository whose base commit holds `a.sh` (listed in the budget) and `b.sh`, and
/// whose next commit moves `a.sh` to `moved/a.sh` (the file's bytes unchanged) and
/// writes `budget` as the budget file.
fn repo_with_rename(name: &str, budget: &str) -> TestDir {
    let root = repo(name, "[files]\na.sh\tto port\n[workflow]\n[src-sh-c]\n");
    let body = "one\ntwo\nthree\nfour\nfive\n";
    std::fs::write(root.join("a.sh"), body).expect("a.sh");
    git(&root, &["add", "a.sh"]);
    git(&root, &["commit", "-q", "-m", "add a.sh"]);
    std::fs::create_dir_all(root.join("moved")).expect("moved");
    git(&root, &["mv", "a.sh", "moved/a.sh"]);
    std::fs::write(root.join("tools/interpreter_budget.tsv"), budget).expect("budget");
    git(&root, &["add", "tools"]);
    git(&root, &["commit", "-q", "-m", "move a.sh"]);
    root
}

#[test]
fn budget_diff_follows_a_git_rename_and_refuses_growth_with_it() {
    // Catches (kbf#1401): a pure `git mv` of a listed file refused as a new row; a rename
    // that excuses a second row added with it, or any new path.
    let moved = "[files]\nmoved/a.sh\tto port\n[workflow]\n[src-sh-c]\n";
    let root = repo_with_rename("budget-rename", moved);
    let (code, out, err) = budget_diff(&root, "HEAD~1");
    assert_eq!(
        (code, err.as_str()),
        (Some(0), ""),
        "a pure rename adds no row"
    );
    assert_eq!(
        out,
        "ok: tools/interpreter_budget.tsv adds no row to HEAD~1's\n"
    );
    let grown = "[files]\nmoved/a.sh\tto port\nnew.sh\tnew\n[workflow]\n[src-sh-c]\n";
    let root = repo_with_rename("budget-rename-grows", grown);
    let (code, _, err) = budget_diff(&root, "HEAD~1");
    assert_eq!(code, Some(1));
    assert!(err.contains("a new [files] row for `new.sh`"), "{err}");
    assert!(!err.contains("moved/a.sh"), "{err}");
    let unrelated = "[files]\nother/z.sh\tx\n[workflow]\n[src-sh-c]\n";
    let root = repo_with_rename("budget-rename-other", unrelated);
    let (code, _, err) = budget_diff(&root, "HEAD~1");
    assert_eq!(code, Some(1));
    assert!(err.contains("a new [files] row for `other/z.sh`"), "{err}");
    let gone = "[files]\n[workflow]\n[src-sh-c]\n";
    let root = repo_with_rename("budget-rename-gone", gone);
    assert_eq!(budget_diff(&root, "HEAD~1").0, Some(0));
}

#[test]
fn budget_diff_refuses_what_it_cannot_compare() {
    // Catches: an unknown base, a missing budget file, a malformed one, or git not
    // starting, each passed as "nothing added".
    let root = repo("budget-errors", BASE);
    let (code, _, err) = budget_diff(&root, "no-such-rev");
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with("xtask: git show no-such-rev:tools/interpreter_budget.tsv: "),
        "{err}"
    );
    let bad = "[files]\na.sh\n[workflow]\n[src-sh-c]\n";
    std::fs::write(root.join("tools/interpreter_budget.tsv"), bad).expect("bad");
    let (code, _, err) = budget_diff(&root, "HEAD");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask: tools/interpreter_budget.tsv: line 2: a row needs two tab-separated fields \
         (<key>, <reason>), found 1\n"
    );
    std::fs::remove_file(root.join("tools/interpreter_budget.tsv")).expect("remove");
    let (code, _, err) = budget_diff(&root, "HEAD");
    assert_eq!(code, Some(1));
    assert!(err.starts_with("xtask: "), "{err}");
    assert!(err.contains("interpreter_budget.tsv: "), "{err}");
    let out = xtask(&root, &["budget-diff", "--base", "HEAD"])
        .env("PATH", "")
        .output()
        .expect("run xtask");
    let (code, _, err) = outcome(out);
    assert_eq!(code, Some(1));
    assert!(err.starts_with("xtask: git: "), "{err}");
}

#[test]
fn budget_diff_defaults_to_this_repository() {
    // Catches: a default root other than this workspace (CI passes no --root). The
    // working tree's budget against its own HEAD adds nothing unless a row is being
    // added right now, which this check exists to refuse.
    let out = Command::new(XTASK)
        .args(["budget-diff", "--base", "HEAD"])
        .output()
        .expect("run xtask");
    let (code, out, err) = outcome(out);
    assert_eq!(code, Some(0), "stderr:\n{err}");
    assert_eq!(
        out,
        "ok: tools/interpreter_budget.tsv adds no row to HEAD's\n"
    );
}

/// The commit `HEAD` names in `root`.
fn head_of(root: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git");
    text(&out.stdout).trim().to_owned()
}

#[test]
fn source_date_epoch_appends_the_committer_time_of_the_commit_and_refuses_anything_else() {
    // Catches: an image stamped with the build's clock (the line holds another time than the
    // commit's), a line written over the file's earlier ones, a commit that is not in the
    // repository accepted, and a name that is not a commit passed to git as an option.
    let root = repo("source-date-epoch", "[files]\n[workflow]\n[src-sh-c]\n");
    git(&root, &["commit", "-q", "--allow-empty", "-m", "second"]);
    let env = root.join("github-env");
    std::fs::write(&env, "EARLIER=1\n").expect("env");
    let first = head_of(&root);
    let commit_time = |rev: &str| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&*root)
            .args(["show", "-s", "--format=%ct", rev])
            .output()
            .expect("git");
        text(&out.stdout).trim().to_owned()
    };
    let (code, out, err) = outcome(
        xtask(
            &root,
            &[
                "source-date-epoch",
                "--commit",
                &first,
                "--output",
                env.to_str().expect("utf8"),
            ],
        )
        .output()
        .expect("run"),
    );
    assert_eq!(code, Some(0), "{err}");
    let want = format!("SOURCE_DATE_EPOCH={}", commit_time(&first));
    assert_eq!(out, format!("{want}\n"));
    assert_eq!(
        std::fs::read_to_string(&env).expect("env"),
        format!("EARLIER=1\n{want}\n")
    );
    for bad in [
        "--output=/dev/null",
        "abc",
        "0123456789abcdef0123456789abcdef01234567",
    ] {
        let (code, _, err) = outcome(
            xtask(
                &root,
                &[
                    "source-date-epoch",
                    "--commit",
                    bad,
                    "--output",
                    env.to_str().expect("utf8"),
                ],
            )
            .output()
            .expect("run"),
        );
        assert_ne!(code, Some(0), "{bad}");
        assert!(!err.is_empty(), "{bad}");
    }
    assert_eq!(
        std::fs::read_to_string(&env).expect("env"),
        format!("EARLIER=1\n{want}\n"),
        "a refused commit writes nothing"
    );
}

#[test]
fn pkg_image_asset_refuses_a_file_that_is_not_an_image_both_when_adding_and_when_verifying() {
    // Catches: the verb not wired to the check (a non-image accepted and listed), `--out`
    // writing before the refusal, and `--verify-only` accepting a file under the wrong name.
    let root = scratch("image-asset-cli");
    let image = root.join("image.oci.tar");
    std::fs::write(&image, b"not a tar").expect("image");
    let tarball = root.join("kbf.tar.gz");
    std::fs::write(&tarball, b"x").expect("tarball");
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let path = |p: &Path| p.to_str().expect("utf8").to_owned();
    let out = root.join("dist");
    for extra in [
        vec!["--out".to_owned(), path(&out)],
        vec!["--verify-only".to_owned()],
    ] {
        let mut args = vec![
            "pkg-image-asset".to_owned(),
            "--image".to_owned(),
            path(&image),
            "--service".to_owned(),
            "frontend".to_owned(),
            "--platform".to_owned(),
            "linux-x86_64".to_owned(),
            "--commit".to_owned(),
            commit.to_owned(),
            "--kbf-tarball".to_owned(),
            path(&tarball),
        ];
        args.extend(extra);
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, err) = outcome(xtask(&root, &words).output().expect("run"));
        assert_eq!(code, Some(1), "{err}");
        assert!(err.contains("holds no kbf"), "{err}");
    }
    assert!(!out.join("SHA256SUMS.linux-x86_64").exists());
}

#[test]
fn pkg_image_asset_wants_the_reference_of_its_service_and_no_other() {
    // Catches: a coordinator image checked against nothing when adding (the build's file
    // forgotten), a frontend image checked against a bare binary and not the attested
    // tarball, and `--verify-only` of a coordinator image given a file the verify job lacks.
    let root = scratch("image-asset-flags");
    let image = root.join("image.oci.tar");
    std::fs::write(&image, b"not a tar").expect("image");
    let file = root.join("file");
    std::fs::write(&file, b"x").expect("file");
    let path = |p: &Path| p.to_str().expect("utf8").to_owned();
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let out = path(&root.join("dist"));
    let (image, file) = (path(&image), path(&file));
    let base = |service: &'static str| {
        vec![
            "pkg-image-asset",
            "--image",
            image.as_str(),
            "--service",
            service,
            "--platform",
            "linux-x86_64",
            "--commit",
            commit,
        ]
    };
    let run = |extra: &[&str], service: &'static str| {
        let mut args = base(service);
        args.extend_from_slice(extra);
        outcome(xtask(&root, &args).output().expect("run"))
    };
    let (code, _, err) = run(&["--binary", &file, "--out", &out], "frontend");
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("the frontend image is checked against --kbf-tarball"),
        "{err}"
    );
    let (code, _, err) = run(&["--out", &out], "coordinator");
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("the coordinator image is checked against --binary"),
        "{err}"
    );
    let (code, _, err) = run(&["--kbf-tarball", &file, "--out", &out], "coordinator");
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("the coordinator image is checked against --binary"),
        "{err}"
    );
    let (code, _, err) = run(&["--binary", &file, "--verify-only"], "coordinator");
    assert_eq!(code, Some(2), "clap refuses the pair: {err}");
    // A coordinator image with its binary reaches the check, which refuses a non-image.
    let (code, _, err) = run(&["--binary", &file, "--out", &out], "coordinator");
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("not an OCI image tar"), "{err}");
    assert!(!root.join("dist").join("SHA256SUMS.linux-x86_64").exists());
}
