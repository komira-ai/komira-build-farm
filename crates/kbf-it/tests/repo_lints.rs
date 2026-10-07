//! Repository lints over the files git knows about (tracked, plus untracked files that
//! are not ignored, so a planted file is caught before it is committed).
//!
//! Catches:
//! - a workflow that can run on a non-hosted runner, uses `pull_request_target`, uses
//!   an action not pinned by commit SHA, or lacks top-level `permissions: {}`, and an
//!   `action.yml` in any directory with an unpinned step; a local `uses: ./...` that
//!   names no linted action or workflow; a workflow or action file that is not valid
//!   UTF-8 or holds a NUL byte (rules in `workflows/mod.rs`, which parses each file as
//!   YAML; its module docs say what it cannot cover);
//! - a text file carrying a non-documentation IPv4 address or an absolute home path,
//!   and a file holding a NUL byte whose extension is not on the binary allow list
//!   (rules in `kbf_it::hygiene`);
//! - the scans passing vacuously because git listed nothing or the workflows moved.

mod workflows;

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

/// The file's bytes, or `None` for a tracked file deleted in the working tree.
fn read_bytes(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("read {}: {e}", path.display()),
    }
}

fn is_workflow(f: &str) -> bool {
    let lower = f.to_ascii_lowercase();
    f.starts_with(".github/workflows/") && (lower.ends_with(".yml") || lower.ends_with(".yaml"))
}

/// An `action.yml` or `action.yaml` in any directory, any case.
fn is_action_file(f: &str) -> bool {
    let name = f.rsplit('/').next().unwrap_or(f);
    name.eq_ignore_ascii_case("action.yml") || name.eq_ignore_ascii_case("action.yaml")
}

#[test]
fn workflows_are_hosted_pinned_and_least_privilege() {
    let root = repo_root();
    let listed = listed_files(&root);
    let present = |f: &&String| root.join(f.as_str()).exists();
    let workflows: Vec<String> = listed.iter().filter(|f| is_workflow(f)).cloned().collect();
    let actions: Vec<String> = listed
        .iter()
        .filter(|f| is_action_file(f))
        .cloned()
        .collect();
    assert!(
        workflows.iter().any(|f| f == ".github/workflows/ci.yml"),
        "ci.yml not found; the workflow lint would check nothing: {workflows:?}"
    );
    // A local `uses: ./...` may name only a file this test lints below.
    let repo = workflows::Repo {
        action_dirs: actions
            .iter()
            .filter(present)
            .map(|f| f.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned())
            .collect(),
        workflows: workflows.iter().filter(present).cloned().collect(),
    };
    let mut problems = Vec::new();
    let files = workflows.iter().map(|f| (f, false));
    for (f, is_action) in files.chain(actions.iter().map(|f| (f, true))) {
        let Some(bytes) = read_bytes(&root.join(f)) else {
            continue;
        };
        let found = match workflows::decode(&bytes) {
            Ok(text) if is_action => workflows::scan_action(text, &repo),
            Ok(text) => workflows::scan(text, &repo),
            Err(e) => vec![e],
        };
        problems.extend(found.into_iter().map(|p| format!("{f}: {p}")));
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
        let Some(bytes) = read_bytes(&root.join(f)) else {
            continue;
        };
        let text = match kbf_it::hygiene::decode(f, &bytes) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(e) => {
                problems.push(format!("{f}: {e}"));
                continue;
            }
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
