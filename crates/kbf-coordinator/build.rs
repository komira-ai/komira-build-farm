//! Embeds the commit the coordinator is built from as `KBF_BUILD_COMMIT`, so `--version`
//! tells two builds of one package version apart and the image that carries the binary can
//! be checked against the commit it was built for (`cargo xtask image-test`).
//!
//! The commit is `git rev-parse --short=12 HEAD` in the checkout being built, or `unknown`
//! where that fails (a source archive, no `git`). Uncommitted changes are not marked. There
//! is no override: a release is built from its commit, and the artifacts workflow checks that
//! the binary it puts in an image names that commit (`cargo xtask build-commit check`). Cargo
//! runs this again when `HEAD` moves or the branch it names gets a new commit.

use std::path::PathBuf;
use std::process::Command;

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
    let commit = git(&["rev-parse", "--short=12", "HEAD"])
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=KBF_BUILD_COMMIT={commit}");
    // Watched: HEAD (a checkout), the ref it names (a commit), and packed-refs (where that
    // ref lives once packed).
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    let watched = [Some("HEAD"), branch.as_deref(), Some("packed-refs")];
    for path in watched.into_iter().flatten().filter_map(git_path) {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
