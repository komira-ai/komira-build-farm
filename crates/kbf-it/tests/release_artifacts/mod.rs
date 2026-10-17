//! What `.github/workflows/artifacts.yml` builds, signs, checks and attests, pinned.
//!
//! The deployment installs these files and nothing else, so the set is the contract: a
//! `kbf` that silently drops out of the workflow, or an attestation step that stops
//! being reached, leaves nothing a host can verify, and no pull request check fails
//! (attest and verify run only on pushes). [`problems`] reads the workflow and reports
//! every way it differs from [`ASSETS`] and the rules below; docs/artifacts.md is held
//! to the same set.
//!
//! - **The release set.** The `cargo xtask package` lines of the workflow name exactly
//!   [`ASSETS`] (binary and platform): `kbf` for the three platforms, `kbf-daemon`,
//!   `kbf-server` and `kbf-deploy` for Linux, `kbf-daemon` and `kbf-deploy` for macOS.
//!   Each one's SBOM attestation is [`crate::attested_assets`]' business.
//! - **Who signs, and when.** The workflow runs for pull requests and the merge queue
//!   (builds only) and for pushes to exactly [`PUSH_BRANCHES`]. The `attest` and `verify`
//!   jobs run under the one `if:` [`SIGNING_IF`], which no pull request satisfies, and
//!   `attest` signs build provenance for every file in `SHA256SUMS` in a step that
//!   cannot be skipped. (The scopes that sign are held to the same `if:` by the
//!   workflow lint, `workflows/mod.rs`.)
//! - **What the verify job checks.** It names the branch it is verifying: `--source-ref
//!   "$GITHUB_REF"`, so the development branch's attestations verify as that branch's and
//!   a release (which verifies as `refs/heads/main`) never accepts them.
//! - **The binaries identify themselves.** Every job that packages `kbf` first runs
//!   `cargo xtask build-commit check --json` on it, so `kbf --version --json` of the
//!   shipped bytes names the commit and a release build.
//! - **macOS.** Every binary the darwin job packages is signed by `cargo xtask
//!   sign-darwin` before it is packaged and checked by `cargo xtask darwin-asset` on
//!   the tarball after, each in a step that cannot be skipped.

use super::{attested_assets, workflows};

/// Every (binary, platform) the workflow packages, in the order docs/artifacts.md lists
/// them.
pub const ASSETS: &[(&str, &str)] = &[
    ("kbf", "darwin-arm64"),
    ("kbf-daemon", "darwin-arm64"),
    ("kbf-deploy", "darwin-arm64"),
    ("kbf", "linux-x86_64"),
    ("kbf-daemon", "linux-x86_64"),
    ("kbf-server", "linux-x86_64"),
    ("kbf-deploy", "linux-x86_64"),
    ("kbf", "linux-arm64"),
    ("kbf-daemon", "linux-arm64"),
    ("kbf-server", "linux-arm64"),
    ("kbf-deploy", "linux-arm64"),
];

/// The branches whose pushes run the workflow: the ones a host's verification may name
/// as `--source-ref` (`refs/heads/<branch>`).
pub const PUSH_BRANCHES: &[&str] = &["main", "feature/arch-v2"];

/// The one `if:` of the jobs that sign and verify: a push, to one of [`PUSH_BRANCHES`].
pub const SIGNING_IF: &str = "${{ github.event_name == 'push' && (github.ref == 'refs/heads/main' || github.ref == 'refs/heads/feature/arch-v2') }}";

/// The jobs that build on one platform, named by it, and the signing and verifying jobs.
const BUILD_JOBS: &[&str] = &["darwin-arm64", "linux-x86_64", "linux-arm64"];
const ATTEST_JOB: &str = "attest";
const VERIFY_JOB: &str = "verify";
const DARWIN_JOB: &str = "darwin-arm64";

/// The lines of a step's script, leading blanks dropped.
fn lines(step: &workflows::Step) -> impl Iterator<Item = &str> {
    step.run
        .as_deref()
        .unwrap_or_default()
        .lines()
        .map(str::trim_start)
}

/// The words of `line` after `prefix`, with the double quotes of each dropped; `None`
/// when `line` does not start with `prefix` (which ends in a space).
fn args<'a>(line: &'a str, prefix: &str) -> Option<Vec<&'a str>> {
    line.strip_prefix(prefix).map(|rest| {
        rest.split_whitespace()
            .map(|w| w.trim_matches('"'))
            .collect()
    })
}

/// The file name of the binary a path names.
fn base(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The position and the step of the first step of `steps` from `from` on with a line for
/// which `found` is true.
fn first(
    steps: &[workflows::Step],
    from: usize,
    found: impl Fn(&str) -> bool,
) -> Option<(usize, &workflows::Step)> {
    steps
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, s)| lines(s).any(&found))
}

/// What differs between the workflow and the pinned release set and rules: one line per
/// difference, empty when there is none. An error when the workflow cannot be read.
pub fn problems(workflow: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    release_set(workflow, &mut out)?;
    signing(workflow, &mut out)?;
    identity(workflow, &mut out)?;
    darwin(workflow, &mut out)?;
    Ok(out)
}

fn release_set(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let packaged = attested_assets::packaged(workflow)?;
    for (name, platform) in ASSETS {
        if !packaged.contains(&((*name).to_owned(), (*platform).to_owned())) {
            out.push(format!(
                "{name} for {platform} is in the release set, but no `cargo xtask package` line packages it"
            ));
        }
    }
    for (name, platform) in &packaged {
        if !ASSETS.contains(&(name.as_str(), platform.as_str())) {
            out.push(format!(
                "{name} for {platform} is packaged but not in the pinned release set: add it to `ASSETS` and to docs/artifacts.md"
            ));
        }
    }
    Ok(())
}

fn signing(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let events = workflows::events(workflow)?;
    for event in ["pull_request", "merge_group", "push"] {
        if !events.iter().any(|e| e == event) {
            out.push(format!("the workflow does not run on `{event}`"));
        }
    }
    let branches = workflows::push_branches(workflow)?;
    if branches != PUSH_BRANCHES {
        out.push(format!(
            "`push` runs the workflow for {branches:?}, not exactly {PUSH_BRANCHES:?}"
        ));
    }
    for job in [ATTEST_JOB, VERIFY_JOB] {
        let condition = workflows::job_condition(workflow, job)?;
        if condition.as_deref() != Some(SIGNING_IF) {
            out.push(format!(
                "the job `{job}` has `if: {}`, not exactly `{SIGNING_IF}`",
                condition.as_deref().unwrap_or("(none)")
            ));
        }
    }
    let attest = workflows::job_steps(workflow, ATTEST_JOB)?;
    let provenance = attest.iter().find(|s| {
        s.uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/attest-build-provenance@"))
    });
    match provenance {
        None => out.push(format!(
            "the job `{ATTEST_JOB}` has no actions/attest-build-provenance step"
        )),
        Some(step) => {
            let subjects = step
                .with
                .iter()
                .find(|(k, _)| k == "subject-checksums")
                .map(|(_, v)| v.as_str());
            if subjects != Some("dist/SHA256SUMS") {
                out.push(format!(
                    "the provenance step of `{ATTEST_JOB}` does not cover `dist/SHA256SUMS`, but {subjects:?}"
                ));
            }
            if step.conditional {
                out.push(format!(
                    "the provenance step of `{ATTEST_JOB}` is under `if:` or `continue-on-error:`"
                ));
            }
        }
    }
    let verify = workflows::job_steps(workflow, VERIFY_JOB)?;
    let verifying = verify.iter().find_map(|s| {
        lines(s).find_map(|l| args(l, "cargo xtask verify-assets attestations ").map(|a| (s, a)))
    });
    match verifying {
        None => out.push(format!(
            "the job `{VERIFY_JOB}` does not run `cargo xtask verify-assets attestations`"
        )),
        Some((step, words)) => {
            let at = words.iter().position(|w| *w == "--source-ref");
            if at.and_then(|i| words.get(i + 1)) != Some(&"$GITHUB_REF") {
                out.push(format!(
                    "the job `{VERIFY_JOB}` verifies attestations without `--source-ref \"$GITHUB_REF\"`, so a push to a branch other than main would be checked as main (or any branch as any)"
                ));
            }
            if step.conditional {
                out.push(format!(
                    "the attestation check of `{VERIFY_JOB}` is under `if:` or `continue-on-error:`"
                ));
            }
        }
    }
    Ok(())
}

fn identity(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in BUILD_JOBS {
        let steps = workflows::job_steps(workflow, job)?;
        let packages_kbf = |l: &str| {
            args(l, "cargo xtask package ").is_some_and(|a| {
                let at = a.iter().position(|w| *w == "--binary");
                at.and_then(|i| a.get(i + 1))
                    .is_some_and(|b| base(b) == "kbf")
            })
        };
        let Some((package, _)) = first(&steps, 0, packages_kbf) else {
            continue;
        };
        let checks_kbf = |l: &str| {
            args(l, "cargo xtask build-commit check --json ")
                .is_some_and(|a| a.len() == 2 && base(a[0]) == "kbf" && a[1] == "$GITHUB_SHA")
        };
        match first(&steps, 0, checks_kbf) {
            Some((at, step)) if at < package && !step.conditional => {}
            Some((at, _)) if at < package => out.push(format!(
                "`{job}` checks `kbf --version --json` under `if:` or `continue-on-error:`"
            )),
            _ => out.push(format!(
                "`{job}` packages kbf without `cargo xtask build-commit check --json <kbf> \"$GITHUB_SHA\"` before"
            )),
        }
    }
    Ok(())
}

fn darwin(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(workflow, DARWIN_JOB)?;
    let mut packaged = Vec::new();
    for step in &steps {
        for line in lines(step) {
            let Some(a) = args(line, "cargo xtask package ") else {
                continue;
            };
            let binary = a
                .iter()
                .position(|w| *w == "--binary")
                .and_then(|i| a.get(i + 1))
                .map(|b| base(b).to_owned());
            packaged.extend(binary);
        }
    }
    for name in &packaged {
        let package = first(&steps, 0, |l| {
            args(l, "cargo xtask package ").is_some_and(|a| {
                a.iter().any(|w| base(w) == name.as_str()) && a.contains(&"--binary")
            })
        })
        .map_or(0, |(i, _)| i);
        let signed = first(&steps, 0, |l| {
            args(l, "cargo xtask sign-darwin ")
                .is_some_and(|a| a.first().is_some_and(|b| base(b) == name.as_str()))
        });
        match signed {
            Some((at, step)) if at < package && !step.conditional => {}
            _ => out.push(format!(
                "`{DARWIN_JOB}` packages {name} without a `cargo xtask sign-darwin` of it in an unconditional step before"
            )),
        }
        let checked = first(&steps, package + 1, |l| {
            args(l, "cargo xtask darwin-asset ").is_some_and(|a| {
                a.len() == 2 && a[1] == name && a[0].starts_with(&format!("dist/{name}-"))
            })
        });
        match checked {
            Some((_, step)) if !step.conditional => {}
            _ => out.push(format!(
                "`{DARWIN_JOB}` packages {name} without a `cargo xtask darwin-asset` of its tarball in an unconditional step after"
            )),
        }
    }
    Ok(())
}

#[test]
fn the_workflow_builds_signs_checks_and_attests_what_the_release_set_says() {
    // Catches: `kbf` (or any binary of the set) dropped from a build job, or a binary
    // added with no row in the set and in docs/artifacts.md; the attest or verify job
    // opened to a pull request, or to a branch no host verifies; the provenance step
    // skippable or no longer covering SHA256SUMS; a push to the development branch
    // verified as main; `kbf --version --json` of the shipped bytes unchecked; a darwin
    // binary packaged unsigned or unchecked.
    let path = super::ARTIFACTS_WORKFLOW;
    let text = std::fs::read_to_string(super::repo_root().join(path)).expect(path);
    match problems(&text) {
        Ok(found) => assert!(found.is_empty(), "{path}:\n{}", found.join("\n")),
        Err(e) => panic!("{path}: {e}"),
    }
}

#[test]
fn the_docs_describe_the_release_set() {
    // Catches: docs/artifacts.md describing another set of files than the workflow
    // builds (the doc is what an operator verifies against), or no longer saying that
    // pushes to the development branch are attested and what `--version --json` is.
    let doc = std::fs::read_to_string(super::repo_root().join("docs/artifacts.md"))
        .expect("docs/artifacts.md");
    let mut missing = Vec::new();
    for (name, platform) in ASSETS {
        let file = format!("`{name}-<commit>-{platform}.tar.gz`");
        if !doc.contains(&file) {
            missing.push(format!("docs/artifacts.md does not list {file}"));
        }
    }
    for branch in PUSH_BRANCHES {
        let named = format!("`{branch}`");
        if !doc.contains(&named) {
            missing.push(format!("docs/artifacts.md does not name {named}"));
        }
    }
    for phrase in ["kbf --version --json", "`--source-ref \"$GITHUB_REF\"`"] {
        if !doc.contains(phrase) {
            missing.push(format!("docs/artifacts.md does not mention {phrase}"));
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

/// The rules [`problems`] reads the workflow by, on a workflow built to meet them and
/// then broken one way at a time.
mod rules {
    use super::*;

    /// A workflow of the shape the rules read, meeting every one.
    fn good() -> String {
        let mut w = String::from(
            "on:\n  pull_request:\n  merge_group:\n  push:\n    branches: [main, feature/arch-v2]\npermissions: {}\njobs:\n",
        );
        for job in BUILD_JOBS {
            w.push_str(&format!(
                "  {job}:\n    runs-on: ubuntu-24.04\n    steps:\n"
            ));
            let platform = *job;
            let mine: Vec<&str> = ASSETS
                .iter()
                .filter(|(_, p)| *p == platform)
                .map(|(n, _)| *n)
                .collect();
            if mine.contains(&"kbf") {
                if platform == "darwin-arm64" {
                    // Darwin signs first, so the check names the signed bytes.
                } else {
                    w.push_str("      - run: cargo xtask build-commit check --json \"target/$TARGET/release/kbf\" \"$GITHUB_SHA\"\n");
                }
            }
            for name in &mine {
                if platform == "darwin-arm64" {
                    w.push_str(&format!(
                        "      - name: Sign {name}\n        run: cargo xtask sign-darwin \"target/$TARGET/release/{name}\" --minos 14.0\n"
                    ));
                }
            }
            if platform == "darwin-arm64" {
                w.push_str("      - run: cargo xtask build-commit check --json \"target/$TARGET/release/kbf\" \"$GITHUB_SHA\"\n");
            }
            for name in &mine {
                w.push_str(&format!(
                    "      - name: Package {name}\n        run: cargo xtask package --binary \"target/$TARGET/release/{name}\" --sbom s.json --platform {platform} --commit \"$GITHUB_SHA\" --out dist\n"
                ));
                if platform == "darwin-arm64" {
                    w.push_str(&format!(
                        "      - name: Check {name}\n        run: cargo xtask darwin-asset \"dist/{name}-$GITHUB_SHA-darwin-arm64.tar.gz\" {name}\n"
                    ));
                }
            }
        }
        w.push_str(&format!(
            "  attest:\n    if: {SIGNING_IF}\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/attest-build-provenance@0123 # v4\n        with:\n          subject-checksums: dist/SHA256SUMS\n"
        ));
        w.push_str(&format!(
            "  verify:\n    if: {SIGNING_IF}\n    runs-on: ubuntu-24.04\n    steps:\n      - run: cargo xtask verify-assets attestations dist --repo \"$GITHUB_REPOSITORY\" --workflow-ref \"$GITHUB_WORKFLOW_REF\" --source-ref \"$GITHUB_REF\"\n"
        ));
        w
    }

    /// `good()` with the first `from` replaced by `to` (the first is the darwin job's,
    /// which comes first); the replacement must change it.
    fn broken(from: &str, to: &str) -> String {
        let w = good();
        assert!(w.contains(from), "{from:?} is not in the planted workflow");
        w.replacen(from, to, 1)
    }

    fn found(w: &str) -> Vec<String> {
        problems(w).expect("readable")
    }

    /// Asserts that `w` has a problem containing each of `wanted`.
    fn names(w: &str, wanted: &[&str]) {
        let got = found(w);
        for what in wanted {
            assert!(
                got.iter().any(|p| p.contains(what)),
                "wanted a problem containing {what:?}, got {got:#?}"
            );
        }
    }

    const KBF_ARM: &str = "release/kbf\" --sbom s.json --platform linux-arm64";
    const SHA_CHECK: &str = "cargo xtask build-commit check --json";

    #[test]
    fn a_workflow_meeting_every_rule_passes() {
        // Catches: a rule misreading a correct workflow (quoted arguments, a path with
        // a directory part, steps in order), so every real change is refused.
        assert_eq!(found(&good()), Vec::<String>::new());
    }

    #[test]
    fn a_binary_missing_from_the_workflow_or_the_set_is_named() {
        // Catches: kbf dropped from a job (the point of this module), and a binary
        // added to a job with no row in the set.
        names(
            &broken(
                KBF_ARM,
                "release/kbf-x\" --sbom s.json --platform linux-arm64",
            ),
            &[
                "kbf for linux-arm64 is in the release set",
                "kbf-x for linux-arm64 is packaged but not in the pinned release set",
            ],
        );
    }

    #[test]
    fn the_signing_jobs_run_only_for_a_push_to_a_listed_branch() {
        // Catches: attest or verify opened to every event or every push (a pull request
        // would sign), narrowed to main (the development branch unattested), or left
        // without an `if:`; and a push trigger of other branches than the pinned two.
        let line = format!("    if: {SIGNING_IF}\n");
        for (k, job) in ["attest", "verify"].into_iter().enumerate() {
            for to in [
                "if: ${{ github.event_name == 'push' }}",
                "if: ${{ github.ref == 'refs/heads/main' }}",
                "if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}",
                "if: ${{ github.event_name != 'push' }}",
            ] {
                // The k-th `if:` of the signing jobs, replaced.
                let w = good();
                let parts: Vec<&str> = w.split(&line).collect();
                assert_eq!(parts.len(), 3, "attest and verify");
                let mut planted = String::new();
                for (i, part) in parts.iter().enumerate() {
                    planted.push_str(part);
                    if i < 2 {
                        planted.push_str(&if i == k {
                            format!("    {to}\n")
                        } else {
                            line.clone()
                        });
                    }
                }
                names(&planted, &[&format!("the job `{job}` has `if: ")]);
            }
        }
        let none = broken(&line, "");
        names(&none, &["has `if: (none)`"]);
        for branches in [
            "[main]",
            "[main, feature/arch-v2, other]",
            "[feature/arch-v2]",
        ] {
            names(
                &broken("[main, feature/arch-v2]", branches),
                &["`push` runs the workflow for"],
            );
        }
        names(
            &broken("  merge_group:\n", ""),
            &["does not run on `merge_group`"],
        );
    }

    #[test]
    fn the_provenance_step_must_cover_the_sums_and_cannot_be_skipped() {
        // Catches: the build provenance step removed, pointed at one file instead of
        // SHA256SUMS (so most assets are unattested), or made skippable.
        names(
            &broken("actions/attest-build-provenance@", "actions/cache@"),
            &["no actions/attest-build-provenance step"],
        );
        names(
            &broken(
                "subject-checksums: dist/SHA256SUMS",
                "subject-checksums: dist/other",
            ),
            &["does not cover `dist/SHA256SUMS`"],
        );
        names(
            &broken(
                "      - uses: actions/attest-build-provenance@0123 # v4\n",
                "      - if: always()\n        uses: actions/attest-build-provenance@0123 # v4\n",
            ),
            &["provenance step of `attest` is under `if:`"],
        );
    }

    #[test]
    fn the_verify_job_names_the_branch_it_verifies() {
        // Catches: the verify job back at its default (main), which refuses a push to
        // the development branch's own attestations; or pinned to a literal ref, which
        // would verify every branch as that one; or the check skippable.
        let flag = " --source-ref \"$GITHUB_REF\"";
        for to in ["", " --source-ref refs/heads/main", " --source-ref"] {
            names(
                &broken(flag, to),
                &["verifies attestations without `--source-ref \"$GITHUB_REF\"`"],
            );
        }
        names(
            &broken(
                "      - run: cargo xtask verify-assets attestations",
                "      - if: always()\n        run: cargo xtask verify-assets attestations",
            ),
            &["attestation check of `verify` is under `if:`"],
        );
        names(
            &broken("verify-assets attestations", "verify-assets sums"),
            &["does not run `cargo xtask verify-assets attestations`"],
        );
    }

    #[test]
    fn every_job_that_packages_kbf_checks_its_identity_first() {
        // Catches: `kbf --version --json` of the shipped bytes unchecked (a debug build
        // or another commit could ship), the plain `--version` check standing in for
        // it, the check of another binary or another commit, the check after packaging
        // (the tarball already made) or one that cannot fail the job.
        for to in [
            "cargo xtask build-commit check",
            "cargo xtask build-commit check --json \"target/$TARGET/release/kbf-daemon\" \"$GITHUB_SHA\" #",
            "cargo xtask build-commit check --json \"target/$TARGET/release/kbf\" abc #",
        ] {
            // Linux x86_64's check is the second of the file; darwin's is the first.
            let w = good().replacen(
                &format!("{SHA_CHECK} \"target/$TARGET/release/kbf\" \"$GITHUB_SHA\""),
                to,
                2,
            );
            names(
                &w,
                &["packages kbf without `cargo xtask build-commit check --json"],
            );
        }
        let conditional = broken(
            "      - run: cargo xtask build-commit check --json",
            "      - if: always()\n        run: cargo xtask build-commit check --json",
        );
        names(&conditional, &["checks `kbf --version --json` under `if:`"]);
    }

    #[test]
    fn every_darwin_binary_is_signed_before_and_checked_after_packaging() {
        // Catches: a darwin binary (kbf-deploy was the new one) packaged unsigned, a
        // tarball never run through `darwin-asset`, the check of another binary's
        // tarball standing in, and either step skippable.
        names(
            &broken(
                "cargo xtask sign-darwin \"target/$TARGET/release/kbf-deploy\"",
                "cargo xtask sign-darwin \"target/$TARGET/release/kbf-daemon\"",
            ),
            &["packages kbf-deploy without a `cargo xtask sign-darwin`"],
        );
        // The check as a comment, as an echo, or as the shell script it replaced.
        for planted in [
            "echo cargo xtask darwin-asset",
            "bash tools/ci/check-darwin-asset.sh x y",
            "|\n          # cargo xtask darwin-asset \"dist/kbf-$GITHUB_SHA-darwin-arm64.tar.gz\" kbf\n          true",
        ] {
            names(
                &broken(
                    "cargo xtask darwin-asset \"dist/kbf-$GITHUB_SHA-darwin-arm64.tar.gz\" kbf",
                    planted,
                ),
                &["packages kbf without a `cargo xtask darwin-asset`"],
            );
        }
        names(
            &broken(
                "cargo xtask darwin-asset \"dist/kbf-daemon-$GITHUB_SHA-darwin-arm64.tar.gz\" kbf-daemon",
                "cargo xtask darwin-asset \"dist/kbf-$GITHUB_SHA-darwin-arm64.tar.gz\" kbf-daemon",
            ),
            &["packages kbf-daemon without a `cargo xtask darwin-asset`"],
        );
        for step in ["Sign kbf-deploy", "Check kbf-deploy"] {
            names(
                &broken(
                    &format!("      - name: {step}\n"),
                    &format!("      - name: {step}\n        continue-on-error: true\n"),
                ),
                &["packages kbf-deploy without"],
            );
        }
        // A check that comes before the packaging does not check the tarball.
        let w = good();
        let check = "      - name: Check kbf-deploy\n        run: cargo xtask darwin-asset \"dist/kbf-deploy-$GITHUB_SHA-darwin-arm64.tar.gz\" kbf-deploy\n";
        let package = "      - name: Package kbf-deploy\n";
        let moved = w
            .replacen(check, "", 1)
            .replacen(package, &format!("{check}{package}"), 1);
        names(
            &moved,
            &["packages kbf-deploy without a `cargo xtask darwin-asset`"],
        );
    }
}
