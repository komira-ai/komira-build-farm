//! Embeds the commit the server is built from as `KBF_BUILD_COMMIT`, so `--version`,
//! the start line and `GET /v1/nodes` tell two builds of one package version apart.
//!
//! The commit is `KBF_BUILD_COMMIT_OVERRIDE` when set (12 lowercase hex digits, for CI
//! tests that stamp two builds of one checkout; anything else fails the build), else
//! `git rev-parse --short=12 HEAD` in the checkout being built, or `unknown` where that
//! fails (a source archive, no `git`). Uncommitted changes are not marked. The rule is
//! in `src/build_commit.rs`. Cargo runs this again when the override changes, `HEAD`
//! moves or the branch it names gets a new commit.

use std::path::PathBuf;
use std::process::Command;

#[path = "src/build_commit.rs"]
mod build_commit;

/// `git` with `args` in this crate's directory: its trimmed stdout, if it succeeded.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    out.status.success().then(|| text.trim().to_owned())
}

/// A path inside the git directory, as git resolves it (worktrees included).
fn git_path(name: &str) -> Option<PathBuf> {
    git(&["rev-parse", "--git-path", name]).map(PathBuf::from)
}

fn main() {
    println!("cargo:rerun-if-env-changed={}", build_commit::OVERRIDE);
    let given = std::env::var_os(build_commit::OVERRIDE);
    let head = git(&["rev-parse", "--short=12", "HEAD"]);
    let commit = match build_commit::choose(given.as_deref(), head) {
        Ok(commit) => commit,
        Err(e) => panic!("{e}"),
    };
    println!("cargo:rustc-env=KBF_BUILD_COMMIT={commit}");
    // Watched: HEAD (a checkout), the ref it names (a commit), and packed-refs (where
    // that ref lives once packed).
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    let watched = [Some("HEAD"), branch.as_deref(), Some("packed-refs")];
    for path in watched.into_iter().flatten().filter_map(git_path) {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
