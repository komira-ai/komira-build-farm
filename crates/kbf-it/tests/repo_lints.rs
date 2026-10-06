//! Repository lints over the files git knows about (tracked, plus untracked files that
//! are not ignored, so a planted file is caught before it is committed).
//!
//! Catches:
//! - a workflow that can run on a non-hosted runner, uses `pull_request_target`, uses
//!   an action not pinned by commit SHA, or lacks top-level `permissions: {}`
//!   (rules in `kbf_it::workflows`);
//! - a text file carrying a non-documentation IPv4 address or an absolute home path
//!   (rules in `kbf_it::hygiene`);
//! - the scans passing vacuously because git listed nothing or the workflows moved.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// Paths, relative to the root, of tracked and untracked-but-not-ignored files.
fn listed_files(root: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .expect("run git ls-files (the lints need git)");
    assert!(
        out.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut files: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8(p.to_vec()).expect("UTF-8 path"))
        .collect();
    files.sort();
    files.dedup();
    files
}

/// The file's text, or `None` for a binary file (one holding a NUL byte) or a
/// tracked file deleted in the working tree.
fn read_text(path: &Path) -> Option<String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => panic!("read {}: {e}", path.display()),
    };
    (!bytes.contains(&0)).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn workflows_are_hosted_pinned_and_least_privilege() {
    let root = repo_root();
    let workflows: Vec<String> = listed_files(&root)
        .into_iter()
        .filter(|f| {
            f.starts_with(".github/workflows/") && (f.ends_with(".yml") || f.ends_with(".yaml"))
        })
        .collect();
    assert!(
        workflows.iter().any(|f| f == ".github/workflows/ci.yml"),
        "ci.yml not found; the workflow lint would check nothing: {workflows:?}"
    );
    let mut problems = Vec::new();
    for f in &workflows {
        let Some(text) = read_text(&root.join(f)) else {
            continue;
        };
        for p in kbf_it::workflows::scan(&text) {
            problems.push(format!("{f}: {p}"));
        }
    }
    assert!(
        problems.is_empty(),
        "workflow lint:\n{}",
        problems.join("\n")
    );
}

#[test]
fn text_files_carry_no_addresses_or_home_paths() {
    let root = repo_root();
    let files = listed_files(&root);
    let mut scanned = 0;
    let mut problems = Vec::new();
    for f in &files {
        let Some(text) = read_text(&root.join(f)) else {
            continue;
        };
        scanned += 1;
        for p in kbf_it::hygiene::scan(&text) {
            problems.push(format!("{f}: {p}"));
        }
    }
    assert!(
        scanned >= 20 && files.iter().any(|f| f == "Cargo.toml"),
        "only {scanned} text files scanned; git listed too little"
    );
    assert!(
        problems.is_empty(),
        "public hygiene:\n{}",
        problems.join("\n")
    );
}
