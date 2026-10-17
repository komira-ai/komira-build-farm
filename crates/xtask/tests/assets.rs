//! `cargo xtask package`, `sums assemble` and `verify-assets` run as the artifacts
//! workflow runs them, with `xtask-fake` as `gh` and its file `attested` as the store
//! of attestations.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const REPO: &str = "komira-ai/komira-build-farm";
const WORKFLOW: &str = "komira-ai/komira-build-farm/.github/workflows/artifacts.yml";
const SBOM_PREDICATE: &str = "--predicate-type https://cyclonedx.org/bom";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("xtask-assets")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("clear scratch");
    }
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The exit code, stdout and stderr of `cargo xtask <args>`.
fn xtask(args: &[&str], tmp: &Path, gh_defect: Option<&str>) -> (Option<i32>, String, String) {
    let mut cmd = Command::new(XTASK);
    cmd.args(args).env("TMPDIR", tmp);
    match gh_defect {
        Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
        None => cmd.env_remove("XTASK_FAKE_DEFECT"),
    };
    let out: Output = cmd.output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// The flags a deployment verifies an asset with (docs/artifacts.md), signed by
/// `signer`, from the branch `source_ref`.
fn flags_from(signer: &str, source_ref: &str) -> String {
    format!(
        "--repo {REPO} --signer-workflow {signer} --source-ref {source_ref} \
         --deny-self-hosted-runners"
    )
}

/// [`flags_from`] `refs/heads/main`.
fn flags(signer: &str) -> String {
    flags_from(signer, "refs/heads/main")
}

/// The branch the development line is built and attested on (docs/artifacts.md).
const DEV_REF: &str = "refs/heads/feature/arch-v2";

/// What the release path leaves: `<root>/dist` packaged and summed as the workflow
/// does, `<root>/gh/gh` the fake gh, `<root>/tmp` the temporary directory.
struct Release {
    dist: PathBuf,
    gh: PathBuf,
    tmp: PathBuf,
}

impl Release {
    /// Packages kbf-daemon for every platform and kbf-server for Linux, then
    /// assembles SHA256SUMS, each through the CLI as the workflow runs it.
    fn build(name: &str) -> Release {
        let root = scratch(name);
        let bin = root.join("bin");
        std::fs::create_dir(&bin).expect("bin");
        let tmp = root.join("tmp");
        std::fs::create_dir(&tmp).expect("tmp");
        let dist = root.join("dist");
        let jobs = [
            ("darwin-arm64", &["kbf-daemon"][..]),
            ("linux-x86_64", &["kbf-daemon", "kbf-server"][..]),
            ("linux-arm64", &["kbf-daemon", "kbf-server"][..]),
        ];
        for (platform, names) in jobs {
            for name in names {
                let binary = bin.join(name);
                std::fs::write(&binary, format!("{name} for {platform}")).expect("binary");
                // Distinct bytes: an attestation covers a digest, not a name.
                let sbom = root.join(format!("{name}-{platform}.cdx.json"));
                std::fs::write(&sbom, format!("{{\"for\":\"{name} {platform}\"}}")).expect("sbom");
                let (code, out, err) = xtask(
                    &[
                        "package",
                        "--binary",
                        s(&binary),
                        "--sbom",
                        s(&sbom),
                        "--platform",
                        platform,
                        "--commit",
                        COMMIT,
                        "--out",
                        s(&dist),
                    ],
                    &tmp,
                    None,
                );
                assert_eq!(code, Some(0), "{err}");
                let stem = format!("{name}-{COMMIT}-{platform}");
                assert!(out.starts_with(&format!("packaged {stem}:\n")), "{out}");
            }
        }
        let summary = root.join("summary.md");
        let (code, out, err) = xtask(
            &["sums", "assemble", s(&dist), "--summary", s(&summary)],
            &tmp,
            None,
        );
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(out.lines().count(), 10, "{out}");
        let gh = root.join("gh").join("gh");
        std::fs::create_dir(root.join("gh")).expect("gh dir");
        std::fs::hard_link(FAKE, &gh).expect("link the fake as gh");
        Release { dist, gh, tmp }
    }

    /// The SHA256SUMS lines: (digest, name).
    fn sums(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(self.dist.join("SHA256SUMS"))
            .expect("SHA256SUMS")
            .lines()
            .map(|l| {
                let (sha, name) = l.split_once("  ").expect("line");
                (sha.to_owned(), name.to_owned())
            })
            .collect()
    }

    /// Records in the fake's store the attestations the attest job signs: provenance
    /// for every file, an SBOM attestation for every tarball, all by `artifacts.yml`.
    /// `skip` leaves out the one for that (name, predicate).
    fn attest(&self, skip: Option<(&str, bool)>) {
        self.attest_from(skip, "refs/heads/main");
    }

    /// [`Release::attest`] for a run on the branch `source_ref`.
    fn attest_from(&self, skip: Option<(&str, bool)>, source_ref: &str) {
        let mut store = String::new();
        for (sha, name) in self.sums() {
            let mut kinds = vec![false];
            if name.ends_with(".tar.gz") {
                kinds.push(true);
            }
            for sbom in kinds {
                if skip == Some((name.as_str(), sbom)) {
                    continue;
                }
                let predicate = if sbom {
                    format!(" {SBOM_PREDICATE}")
                } else {
                    String::new()
                };
                store.push_str(&format!(
                    "{sha} {}{predicate}\n",
                    flags_from(WORKFLOW, source_ref)
                ));
            }
        }
        std::fs::write(self.gh.with_file_name("attested"), store).expect("store");
    }

    fn verify(&self, workflow_ref: &str, defect: Option<&str>) -> (Option<i32>, String, String) {
        self.verify_args(workflow_ref, &[], defect)
    }

    /// [`Release::verify`] with `more` flags.
    fn verify_args(
        &self,
        workflow_ref: &str,
        more: &[&str],
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        let mut args = vec![
            "verify-assets",
            "attestations",
            s(&self.dist),
            "--repo",
            REPO,
            "--workflow-ref",
            workflow_ref,
            "--gh",
            s(&self.gh),
        ];
        args.extend_from_slice(more);
        xtask(&args, &self.tmp, defect)
    }

    fn gh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.gh.with_file_name("gh-args"))
            .expect("gh-args")
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn first_tarball() -> String {
    format!("kbf-daemon-{COMMIT}-darwin-arm64.tar.gz")
}

#[test]
fn the_release_path_packages_sums_and_verifies_every_asset() {
    // Control for the whole path the workflow runs: catches an asset the sums do not
    // cover, a sums check or attestation check that refuses a correct release, a
    // tarball not checked for its SBOM attestation, a verify call with a flag the
    // documented command lacks (the fake verifies only the exact flags), and the
    // flipped copy left in the temporary directory.
    let r = Release::build("release");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "sums: 10 files match; the flipped copy does not\n");
    r.attest(None);
    let (code, out, err) = r.verify(&format!("{WORKFLOW}@refs/heads/main"), None);
    assert_eq!(code, Some(0), "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 11, "{out}");
    assert_eq!(
        lines[0],
        format!("verified: kbf-daemon-{COMMIT}-darwin-arm64.cdx.json (provenance)")
    );
    assert_eq!(
        lines[1],
        format!("verified: {} (provenance, SBOM)", first_tarball())
    );
    assert_eq!(
        lines[10],
        "attestations: every asset verifies; another signer workflow and a flipped byte do not"
    );
    let calls = r.gh_calls();
    // 5 tarballs twice, 5 SBOMs once, then the two controls.
    assert_eq!(calls.len(), 17, "{calls:#?}");
    let first = r.dist.join(first_tarball());
    assert_eq!(
        calls[1],
        format!("attestation verify {} {}", s(&first), flags(WORKFLOW))
    );
    assert_eq!(
        calls[2],
        format!(
            "attestation verify {} {} {SBOM_PREDICATE}",
            s(&first),
            flags(WORKFLOW)
        )
    );
    let other = format!("{REPO}/.github/workflows/ci.yml");
    assert_eq!(
        calls[15],
        format!("attestation verify {} {}", s(&first), flags(&other))
    );
    assert!(calls[16].contains("/xtask-verify-"), "{}", calls[16]);
    assert!(calls[16].ends_with(&format!("{} {}", first_tarball(), flags(WORKFLOW))));
    assert_eq!(
        std::fs::read_dir(&r.tmp).expect("tmp").count(),
        0,
        "the flipped copy's directory was left"
    );
    // A workflow named without `@<ref>` is the same workflow.
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn a_push_to_the_development_branch_is_verified_against_that_branch() {
    // Catches: the `--source-ref` of every verification hard-wired to main (the verify
    // job of a push to the development branch would refuse its own attestations); the
    // flag ignored, or applied to the signer controls only; and, as the control that
    // matters for safety, attestations from the development branch accepted when no
    // ref is given or main is: a file built there must never pass a check for main (a
    // release verifies as main).
    let r = Release::build("dev-ref");
    r.attest_from(None, DEV_REF);
    // Control: no `--source-ref` is main, so these attestations do not verify.
    let (code, _, err) = r.verify(&format!("{WORKFLOW}@{DEV_REF}"), None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("attestation signed by"), "{err}");
    let (code, _, err) = r.verify_args(WORKFLOW, &["--source-ref", "refs/heads/main"], None);
    assert_eq!(code, Some(1), "{err}");
    // Asked for the branch they were signed on, every asset verifies, controls included.
    let (code, out, err) = r.verify_args(WORKFLOW, &["--source-ref", DEV_REF], None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out.lines().count(), 11, "{out}");
    let calls = r.gh_calls();
    let first = r.dist.join(first_tarball());
    assert!(
        calls.contains(&format!(
            "attestation verify {} {}",
            s(&first),
            flags_from(WORKFLOW, DEV_REF)
        )),
        "{calls:#?}"
    );
    // Both controls ask for the same branch: another signer, and the flipped copy.
    let other = format!("{REPO}/.github/workflows/ci.yml");
    assert!(
        calls.contains(&format!(
            "attestation verify {} {}",
            s(&first),
            flags_from(&other, DEV_REF)
        )),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("/xtask-verify-") && c.ends_with(&flags_from(WORKFLOW, DEV_REF))),
        "{calls:#?}"
    );
    // And main's attestations do not verify as the development branch's.
    r.attest(None);
    let (code, _, err) = r.verify_args(WORKFLOW, &["--source-ref", DEV_REF], None);
    assert_eq!(code, Some(1), "{err}");
}

#[test]
fn a_source_ref_that_is_not_a_branch_of_the_repository_is_refused() {
    // Catches: `--source-ref` taken as free text, so a verification could be pointed at
    // a tag, a pull request ref or an empty value and pass.
    let r = Release::build("bad-ref");
    r.attest(None);
    for bad in [
        "",
        "main",
        "refs/tags/v1",
        "refs/pull/1/merge",
        "refs/heads/",
    ] {
        let (code, _, err) = r.verify_args(WORKFLOW, &["--source-ref", bad], None);
        assert_eq!(code, Some(2), "{bad:?}: {err}");
        assert!(
            err.contains("--source-ref") && err.contains("refs/heads/"),
            "{bad:?}: {err}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(r.gh.with_file_name("gh-args")).unwrap_or_default(),
        "",
        "gh was run for a refused ref"
    );
}

#[test]
fn a_verifier_that_accepts_any_signer_is_caught_by_the_signer_control() {
    // Catches: the signer control skipped, or run with the signing workflow: a gh that
    // verifies everything must fail the run, not pass it.
    let r = Release::build("accepts-all");
    let (code, out, err) = r.verify(WORKFLOW, Some("accepts-all"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: {} verified as signed by {REPO}/.github/workflows/ci.yml\n",
            first_tarball()
        )
    );
}

#[test]
fn a_verifier_that_ignores_the_bytes_is_caught_by_the_flipped_byte_control() {
    // Catches: the byte-flip control skipped: a gh that checks the flags but not which
    // file an attestation covers passes every check above it, so only the flipped
    // copy can fail the run.
    let r = Release::build("ignores-bytes");
    r.attest(None);
    let (code, out, err) = r.verify(WORKFLOW, Some("ignores-bytes"));
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: control failed: a flipped byte in {} still verified\n",
            first_tarball()
        )
    );
}

#[test]
fn a_missing_attestation_fails_the_run_and_names_it() {
    // Catches: a tarball's SBOM attestation, or an SBOM's provenance, not checked or a
    // failed check passed over.
    let r = Release::build("missing");
    let tarball = format!("kbf-server-{COMMIT}-linux-arm64.tar.gz");
    let sbom = format!("kbf-server-{COMMIT}-linux-x86_64.cdx.json");
    for (name, is_sbom, what) in [
        (&tarball, true, "https://cyclonedx.org/bom"),
        (&tarball, false, "build provenance"),
        (&sbom, false, "build provenance"),
    ] {
        r.attest(Some((name.as_str(), is_sbom)));
        let (code, _, err) = r.verify(WORKFLOW, None);
        assert_eq!(code, Some(1), "{name} {what}");
        assert_eq!(
            err,
            format!(
                "xtask: {}: no {what} attestation signed by {WORKFLOW} verifies\n",
                s(&r.dist.join(name))
            )
        );
    }
}

#[test]
fn attestations_refuse_a_foreign_workflow_an_unknown_file_and_a_missing_gh() {
    // Catches: a --workflow-ref of another repository accepted (it would be the
    // signer every check trusts), a listed file of neither kind passed unchecked, and
    // a gh that cannot start reported as an attestation that did not verify.
    let r = Release::build("refusals");
    r.attest(None);
    let (code, _, err) = r.verify("someone/else/.github/workflows/a.yml@refs/heads/main", None);
    assert_eq!(code, Some(2));
    assert_eq!(
        err,
        format!(
            "xtask: --workflow-ref \"someone/else/.github/workflows/a.yml@refs/heads/main\" \
             is not a workflow of {REPO}\n"
        )
    );
    let missing = r.gh.with_file_name("no-gh");
    let (code, _, err) = xtask(
        &[
            "verify-assets",
            "attestations",
            s(&r.dist),
            "--repo",
            REPO,
            "--workflow-ref",
            WORKFLOW,
            "--gh",
            s(&missing),
        ],
        &r.tmp,
        None,
    );
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", s(&missing))),
        "{err}"
    );
    let notes = "notes.txt";
    std::fs::write(r.dist.join(notes), b"notes").expect("notes");
    let digest = hex::encode(sha2::Sha256::digest(b"notes"));
    let mut sums = std::fs::read_to_string(r.dist.join("SHA256SUMS")).expect("sums");
    sums.push_str(&format!("{digest}  {notes}\n"));
    std::fs::write(r.dist.join("SHA256SUMS"), sums).expect("sums");
    let (code, _, err) = r.verify(WORKFLOW, None);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask: SHA256SUMS lists notes.txt, neither a tarball nor an SBOM\n"
    );
}

#[test]
fn verify_sums_fails_the_run_on_a_changed_asset() {
    // Catches: the CLI's sums check passing a directory whose asset changed after it
    // was summed (the exit status, not only the message, must say so).
    let r = Release::build("changed");
    let tarball = r.dist.join(first_tarball());
    let mut bytes = std::fs::read(&tarball).expect("tarball");
    bytes[0] ^= 1;
    std::fs::write(&tarball, bytes).expect("tarball");
    let (code, out, err) = xtask(&["verify-assets", "sums", s(&r.dist)], &r.tmp, None);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {}: does not match its line\n", first_tarball())
    );
}
