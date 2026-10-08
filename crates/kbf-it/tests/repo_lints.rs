//! Repository lints over the files git knows about (tracked, plus untracked files that
//! are not ignored, so a planted file is caught before it is committed).
//!
//! Catches:
//! - a workflow that can run on a non-hosted runner, uses `pull_request_target`, uses
//!   an action not pinned by commit SHA or outside the allowed set (`actions/*` and
//!   `tailscale/github-action`), or lacks top-level `permissions: {}`, and an
//!   `action.yml` in any directory with an unpinned step; a local `uses: ./...` that
//!   names no linted action or workflow; a workflow or action file that is not valid
//!   UTF-8 or holds a NUL byte (rules in `workflows/mod.rs`, which parses each file as
//!   YAML; its module docs say what it cannot cover);
//! - a text file carrying a non-documentation IPv4 address or an absolute home path,
//!   and a file holding a NUL byte whose extension is not on the binary allow list
//!   (rules in `kbf_it::hygiene`);
//! - the scans passing vacuously because git listed nothing or the workflows moved;
//! - a drift in the rules that pick which files each lint reads (module `selection`).

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

/// A workflow file: `.github/workflows/<...>.yml` or `.yaml`, the extension in any case.
fn is_workflow(f: &str) -> bool {
    let lower = f.to_ascii_lowercase();
    f.starts_with(".github/workflows/") && (lower.ends_with(".yml") || lower.ends_with(".yaml"))
}

/// An `action.yml` or `action.yaml` in any directory, any case.
fn is_action_file(f: &str) -> bool {
    let name = f.rsplit('/').next().unwrap_or(f);
    name.eq_ignore_ascii_case("action.yml") || name.eq_ignore_ascii_case("action.yaml")
}

/// The directory of an action file, as a local `uses: ./<dir>` names it (`""` for the
/// root).
fn action_dir(f: &str) -> &str {
    f.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Which rules the workflow lint applies to a file.
#[derive(Debug, Clone, Copy)]
enum Linted {
    Workflow,
    Action,
}

/// The workflow lint's problems with the file `f`, each prefixed with `f`. A file the
/// lint cannot read is a problem, never skipped.
fn lint_workflow_file(
    f: &str,
    bytes: &[u8],
    linted: Linted,
    repo: &workflows::Repo,
) -> Vec<String> {
    let found = match (workflows::decode(bytes), linted) {
        (Ok(text), Linted::Action) => workflows::scan_action(text, repo),
        (Ok(text), Linted::Workflow) => workflows::scan(text, repo),
        (Err(e), _) => vec![e],
    };
    found.into_iter().map(|p| format!("{f}: {p}")).collect()
}

/// What the hygiene scan made of one file.
#[derive(Debug, PartialEq, Eq)]
enum TextFile {
    /// Skipped: its extension is on the binary allow list.
    Binary,
    /// Not read, a problem; the message (prefixed with the path) says why.
    Refused(String),
    /// Decoded and scanned; holds the findings, each prefixed with the path.
    Scanned(Vec<String>),
}

fn check_text_file(f: &str, bytes: &[u8]) -> TextFile {
    match kbf_it::hygiene::decode(f, bytes) {
        Ok(Some(text)) => TextFile::Scanned(
            kbf_it::hygiene::scan(&text)
                .iter()
                .map(|p| format!("{f}: {p}"))
                .collect(),
        ),
        Ok(None) => TextFile::Binary,
        Err(e) => TextFile::Refused(format!("{f}: {e}")),
    }
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
            .map(|f| action_dir(f).to_owned())
            .collect(),
        workflows: workflows.iter().filter(present).cloned().collect(),
    };
    let mut problems = Vec::new();
    let files = workflows.iter().map(|f| (f, Linted::Workflow));
    for (f, linted) in files.chain(actions.iter().map(|f| (f, Linted::Action))) {
        if let Some(bytes) = read_bytes(&root.join(f)) {
            problems.extend(lint_workflow_file(f, &bytes, linted, &repo));
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
        let Some(bytes) = read_bytes(&root.join(f)) else {
            continue;
        };
        match check_text_file(f, &bytes) {
            TextFile::Binary => {}
            TextFile::Refused(e) => problems.push(e),
            TextFile::Scanned(found) => {
                scanned += 1;
                problems.extend(found);
            }
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

/// The workflow paths `docs/artifacts.md` gives as `--signer-workflow`, relative to the
/// repository root.
fn documented_signer_workflows(doc: &str) -> Vec<&str> {
    const FLAG: &str = "--signer-workflow komira-ai/komira-build-farm/";
    doc.match_indices(FLAG)
        .map(|(at, _)| {
            doc[at + FLAG.len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
        })
        .collect()
}

/// What a workflow must hold to be the signer a node's verification names: an attest
/// step, under a job whose `if:` limits it to a push to `main`.
fn signs_provenance_on_main(workflow: &str) -> bool {
    workflow.contains("uses: actions/attest-build-provenance@")
        && workflow
            .contains("if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}")
}

#[test]
fn the_documented_signer_workflow_signs_provenance_on_main() {
    // Catches: the artifacts workflow renamed, moved or stripped of its attest step
    // while docs/artifacts.md still names it, so every node's verification command
    // names a workflow that signs nothing (and every deployment fails to verify).
    let root = repo_root();
    let doc = std::fs::read_to_string(root.join("docs/artifacts.md")).expect("docs/artifacts.md");
    let signers = documented_signer_workflows(&doc);
    assert!(
        !signers.is_empty(),
        "docs/artifacts.md names no --signer-workflow"
    );
    for path in signers {
        assert!(is_workflow(path), "`{path}` is not a workflow file");
        let text = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|e| panic!("the documented signer workflow `{path}`: {e}"));
        assert!(
            signs_provenance_on_main(&text),
            "`{path}` has no main-only attest step"
        );
    }
}

/// The rules that pick which files each lint reads. The repository tests above cannot
/// pin them: a selection that misses a file leaves those tests green.
mod selection {
    use super::*;

    #[test]
    fn signer_workflows_are_read_from_the_verify_command() {
        // Catches: a signer path read with the line's trailing `\` or the next flag, so
        // the repository test opens the wrong file; another repository's workflow
        // taken as ours; a flag at the very end of the text read as a path.
        let doc = "gh attestation verify x \\\n  --signer-workflow komira-ai/komira-build-farm/.github/workflows/a.yml \\\n  --source-ref refs/heads/main\nalso --signer-workflow komira-ai/komira-build-farm/.github/workflows/b.yml";
        assert_eq!(
            documented_signer_workflows(doc),
            vec![".github/workflows/a.yml", ".github/workflows/b.yml"]
        );
        assert!(
            documented_signer_workflows("--signer-workflow other/repo/.github/workflows/a.yml")
                .is_empty()
        );
        assert_eq!(
            documented_signer_workflows("--signer-workflow komira-ai/komira-build-farm/"),
            vec![""]
        );
    }

    #[test]
    fn a_signer_workflow_needs_both_the_attest_step_and_the_main_only_job() {
        // Catches: a workflow accepted as the signer with its attest step gone, or with
        // the attest job no longer limited to a push to main.
        let step = "      - uses: actions/attest-build-provenance@0123 # v4\n";
        let cond =
            "    if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}\n";
        assert!(signs_provenance_on_main(&format!("{cond}{step}")));
        assert!(!signs_provenance_on_main(step));
        assert!(!signs_provenance_on_main(cond));
    }

    #[test]
    fn action_files_are_selected_by_name_in_any_case() {
        // Catches: an action file matched case-sensitively, so `Action.YAML` (which
        // GitHub runs) escaped the action lint; or a match on a name prefix or suffix.
        for f in [
            "action.yml",
            "action.yaml",
            ".github/actions/t/Action.YAML",
            "tools/x/ACTION.yml",
        ] {
            assert!(is_action_file(f), "{f}");
        }
        for f in [
            "myaction.yml",
            "action.yml.bak",
            "action.json",
            "action.yml/README.md",
            ".github/workflows/ci.yml",
        ] {
            assert!(!is_action_file(f), "{f}");
        }
    }

    #[test]
    fn an_action_file_names_its_directory() {
        // Catches: a wrong directory recorded for an action, so a valid `uses: ./<dir>`
        // is refused, or the root action is keyed by its file name.
        assert_eq!(action_dir("Action.yml"), "");
        assert_eq!(
            action_dir(".github/actions/t/action.yaml"),
            ".github/actions/t"
        );
    }

    #[test]
    fn workflow_files_are_selected_by_extension_in_any_case() {
        // Catches: a workflow extension matched case-sensitively, so `x.YML` escaped
        // the workflow lint; or a file outside `.github/workflows/` linted as one.
        for f in [
            ".github/workflows/ci.yml",
            ".github/workflows/x.YML",
            ".github/workflows/y.Yaml",
        ] {
            assert!(is_workflow(f), "{f}");
        }
        for f in [
            ".github/workflows/README.md",
            ".github/workflows/ci.yml.orig",
            ".github/ci.yml",
            "docs/.github/workflows/ci.yml",
        ] {
            assert!(!is_workflow(f), "{f}");
        }
    }

    #[test]
    fn an_unreadable_workflow_or_action_is_refused_not_skipped() {
        // Catches: a workflow or action file holding a NUL byte dropped from the lint
        // without a problem, so its steps are never checked.
        let repo = workflows::Repo::default();
        let bytes = b"on: push\0\njobs: {}\n";
        for (f, linted) in [
            (".github/workflows/x.YML", Linted::Workflow),
            ("t/Action.yml", Linted::Action),
        ] {
            assert_eq!(
                lint_workflow_file(f, bytes, linted, &repo),
                vec![format!(
                    "{f}: line 0: the file holds a NUL byte, so it is not read"
                )],
                "{f}"
            );
        }
    }

    #[test]
    fn an_action_file_is_linted_as_an_action() {
        // Catches: an action file read by the workflow rules (or the reverse), which
        // refuses every valid action.
        let repo = workflows::Repo::default();
        let action = b"name: a\nruns: {using: node24, main: index.js}\n";
        assert_eq!(
            lint_workflow_file("action.yml", action, Linted::Action, &repo),
            Vec::<String>::new()
        );
        assert!(!lint_workflow_file("action.yml", action, Linted::Workflow, &repo).is_empty());
    }

    #[test]
    fn a_utf16_text_file_is_decoded_and_scanned() {
        // Catches: a UTF-16 file (its ASCII holds NUL bytes) skipped or refused rather
        // than decoded, or scanned as raw bytes; either hides an address from the scan.
        // The address is assembled at run time so this file does not trip the scan.
        let addr = format!("{}.1.2.3", 10);
        let mut bytes = vec![0xFF, 0xFE];
        for u in format!("host {addr}\n").encode_utf16() {
            bytes.extend(u.to_le_bytes());
        }
        assert_eq!(
            check_text_file("notes.txt", &bytes),
            TextFile::Scanned(vec![format!(
                "notes.txt: line 1: private IPv4 address {addr}"
            )])
        );
    }

    #[test]
    fn an_allow_listed_binary_file_is_skipped() {
        // Catches: an allow-listed binary refused, or scanned as text.
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        for f in ["logo.png", "img/Logo.PNG"] {
            assert_eq!(check_text_file(f, png), TextFile::Binary, "{f}");
        }
    }

    #[test]
    fn a_nul_text_file_off_the_allow_list_is_refused() {
        // Catches: a text file with a planted NUL skipped as binary, so the rest of its
        // text escapes the scan; or an allow-listed directory name or inner extension
        // taken as binary.
        let bytes = b"host text\0\n";
        for f in ["notes.txt", "Makefile", "dir.png/Makefile", "a.png.txt"] {
            let got = check_text_file(f, bytes);
            assert!(
                matches!(&got, TextFile::Refused(e)
                    if e.starts_with(&format!("{f}: ")) && e.contains("NUL byte")),
                "{f}: {got:?}"
            );
        }
    }
}
