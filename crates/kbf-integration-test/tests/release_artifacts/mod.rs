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
//!   [`ASSETS`] (binary and platform): `kbf`, `kbf-installer` and `kbf-host-install` for the
//!   three platforms, `kbf-daemon`, `kbf-server`, `kbf-deploy` and `kbf-updater` for Linux
//!   (the updater drives hosts from an operator's Linux box), `kbf-deploy` for macOS (a Mac
//!   runs `kbf daemon`; there is no darwin `kbf-daemon`). Each one's SBOM attestation is
//!   [`crate::attested_assets`]' business, and its provenance bundle
//!   [`crate::attested_bundles`]'.
//! - **Who signs, and when.** The workflow runs for pull requests and the merge queue
//!   (builds only) and, called by the pipeline (`workflow_call`), for pushes to exactly
//!   [`PUSH_BRANCHES`]. The `attest` and `verify`
//!   jobs run under the one `if:` [`SIGNING_IF`], which no pull request satisfies, and
//!   `attest` signs build provenance for every file in `SHA256SUMS` in a step that
//!   cannot be skipped. (The scopes that sign are held to the same `if:` by the
//!   workflow lint, `workflows/mod.rs`.)
//! - **What the verify job checks.** It names the branch it is verifying: `--source-ref
//!   "$GITHUB_REF"`, so an attestation verifies as the branch that signed it and
//!   a release (which verifies as `refs/heads/main`) never accepts them. It also names
//!   the commit, `--commit "$GITHUB_SHA"`, and the trusted root it fetched first
//!   (`--trusted-root`), so each tarball's bundle is verified offline for the commit it
//!   was built from, as a host does ([`crate::attested_bundles`] holds the bundles
//!   themselves to the workflow).
//! - **The binaries identify themselves.** Every job that packages one of [`IDENTIFIED`]
//!   (`kbf`, the runtime, and `kbf-installer`, the root tool that installs it) first runs
//!   `cargo xtask build-commit check --json` on it, so `<binary> --version --json` of the
//!   binary built names the commit and a release build, and then, in a later step that
//!   cannot be skipped, `cargo xtask build-commit check-asset` on the tarball it
//!   packaged (`dist/<binary>-$GITHUB_SHA-<platform>.tar.gz`), which opens that file and
//!   makes the same check on the binary inside: the bytes that ship, not only the bytes
//!   built.
//! - **The `kbf` says what it is.** Every build job runs `cargo xtask release
//!   version-facts` on the `kbf` it built, once, between that check and the package, and
//!   `dist` gets `kbf-<commit>-<platform>.version.json` (the rules are `facts`'). Such a job
//!   checks out with `fetch-depth: 0`: a shallow clone cannot count the commits, so `seq`
//!   would be 0.
//! - **The installer's package.** Each Linux job builds the `kbf-installer` deb and makes it
//!   an asset (`cargo xtask pkg-asset`, [`PACKAGES`]), and no other job does. In that job
//!   `pkg-trust` and `pkg-asset` both take the branch the package trusts and is attested
//!   for from the job's `PKG_SOURCE_REF` ([`PKG_SOURCE_REF_EXPR`]: the pushed branch on a
//!   push), never a literal ref (a package built on `main` that trusted another branch
//!   would have a host believe that branch's releases); `pkg-install-test` runs before
//!   `pkg-asset`, and `pkg-asset` checks the deb against the tarball `build-commit
//!   check-asset` already checked. The verify job then runs `pkg-asset --verify-only` on
//!   each signed deb with `--expect-ref "$GITHUB_REF"`, so the file that was attested is the
//!   one checked.
//! - **The `kbf` package.** The same jobs build a second package, `kbf` (`/usr/bin/kbf` and
//!   the units of the modes, depending on the installer's package), from the same
//!   `pkg-config` step, and make its deb an asset (`cargo xtask pkg-kbf-asset`); the verify
//!   job checks each signed one (`kbf_package`'s rules).
//! - **The images.** The same jobs build the frontend's OCI image from the `kbf` of their own
//!   tarball and the update coordinator's from the `kbf-coordinator` they built, each twice with
//!   the commit's time and no BuildKit attachments, compare the two files, and make the first an
//!   asset (`cargo xtask pkg-image-asset --service`, `images`' rules); the verify job checks
//!   each signed one.
//! - **The installer's Mac package.** The darwin job builds the `kbf-installer` `.pkg`
//!   (`cargo xtask pkg build`, with the pinned darwin `gh`, the trusted root and an
//!   `installer.conf` from `pkg-trust`), installs it as root on the runner
//!   (`pkg-macos-install-test`), and makes it an asset (`cargo xtask pkg-macos-asset`,
//!   [`MAC_PACKAGE`]), under the rules of the deb above: the branch is `PKG_SOURCE_REF`,
//!   never a literal ref; the tarball's identity and signature are checked first; the
//!   install test runs before the asset; its downloads are pinned. The verify job then
//!   runs `pkg-macos-asset --verify-only` on the signed `.pkg` with `--expect-ref
//!   "$GITHUB_REF"`.
//! - **macOS.** Every binary the darwin job packages is signed by `cargo xtask
//!   sign-darwin` before it is packaged and checked by `cargo xtask darwin-asset` on
//!   the tarball after, each in a step that cannot be skipped.

use super::{attested_assets, attested_bundles, workflows};

/// Every (binary, platform) the workflow packages, in the order docs/artifacts.md lists
/// them.
pub const ASSETS: &[(&str, &str)] = &[
    ("kbf", "darwin-arm64"),
    ("kbf-deploy", "darwin-arm64"),
    ("kbf-installer", "darwin-arm64"),
    ("kbf-host-install", "darwin-arm64"),
    ("kbf", "linux-x86_64"),
    ("kbf-daemon", "linux-x86_64"),
    ("kbf-server", "linux-x86_64"),
    ("kbf-deploy", "linux-x86_64"),
    ("kbf-installer", "linux-x86_64"),
    ("kbf-host-install", "linux-x86_64"),
    ("kbf-updater", "linux-x86_64"),
    ("kbf", "linux-arm64"),
    ("kbf-daemon", "linux-arm64"),
    ("kbf-server", "linux-arm64"),
    ("kbf-deploy", "linux-arm64"),
    ("kbf-installer", "linux-arm64"),
    ("kbf-host-install", "linux-arm64"),
    ("kbf-updater", "linux-arm64"),
];

/// The binaries of [`ASSETS`] that name the commit they were built from (`<binary>
/// --version` prints `<binary> <package version>+<12 hex>`, and `--version --json` says
/// `commit` and `profile`): the runtime and the root tool that installs it. A job that
/// packages one checks it before and after packaging, as [`problems`] says.
pub const IDENTIFIED: &[&str] = &["kbf", "kbf-installer"];

/// The branches whose pushes run the workflow (through the pipeline): the ones a host's verification may name
/// as `--source-ref` (`refs/heads/<branch>`).
pub const PUSH_BRANCHES: &[&str] = &["main"];

/// The one `if:` of the jobs that sign and verify: a push, to one of [`PUSH_BRANCHES`].
pub const SIGNING_IF: &str =
    "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// The platforms whose job builds the `kbf-installer` deb and makes it an asset: the
/// `cargo xtask pkg-asset` lines name exactly these.
pub const PACKAGES: &[&str] = &["linux-x86_64", "linux-arm64"];

/// The platform whose job builds the Mac `kbf-installer` `.pkg` and makes it an asset
/// (`cargo xtask pkg-macos-asset`), and no other job does.
pub const MAC_PACKAGE: &str = "darwin-arm64";

/// The job variable `pkg-trust` and `pkg-asset` take the branch from, and the one
/// expression it is set to: on a push the pushed branch (what the attest job signs for),
/// on a pull request, which signs nothing, the branch it is aimed at.
pub const PKG_SOURCE_REF: &str = "PKG_SOURCE_REF";
pub const PKG_SOURCE_REF_EXPR: &str = "${{ github.event_name == 'pull_request' && format('refs/heads/{0}', github.base_ref) || github.ref }}";

/// The jobs that build on one platform, named by it, and the signing and verifying jobs.
const BUILD_JOBS: &[&str] = &["darwin-arm64", "linux-x86_64", "linux-arm64"];
/// The signing and verifying jobs, one pair per platform group, so the Linux files are
/// attested without waiting for the macOS build.
pub const ATTEST_JOBS: &[&str] = &["attest-linux", "attest-darwin-arm64"];
const VERIFY_JOBS: &[&str] = &["verify-linux", "verify-darwin-arm64"];
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
    facts::rules(workflow, &mut out)?;
    packages(workflow, &mut out)?;
    kbf_package::rules(workflow, &mut out)?;
    images::rules(workflow, &mut out)?;
    mac_package(workflow, &mut out)?;
    darwin(workflow, &mut out)?;
    pull_request_gate(workflow, &mut out)?;
    Ok(out)
}

/// A pull request builds nothing unless it carries [`PR_LABEL`]: each build job's `if:`
/// names the label, so an unlabelled pull request's run is skipped without a runner.
const PR_LABEL: &str = "needs-artifacts";

fn pull_request_gate(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in BUILD_JOBS {
        let condition = workflows::job_condition(workflow, job)?.unwrap_or_default();
        if !condition.contains(&format!("labels.*.name, '{PR_LABEL}'")) {
            out.push(format!(
                "the build job `{job}` does not run on a pull request only with the label `{PR_LABEL}`: its `if:` is `{condition}`"
            ));
        }
    }
    Ok(())
}

/// The jobs of beta.yml that run on a Mac. A pull request never runs them (the macOS pool is
/// small and a pull request runs the fast gate only); a push to main
/// always does, so none may carry an `if:` that skips it.
pub const MACOS_JOBS: [&str; 2] = ["native-macos", "pkg-macos"];

/// What is wrong with beta.yml's macOS jobs: a condition on one (which could skip it on a
/// push), or a `pull_request` trigger on the workflow (which would queue them for a pull
/// request).
pub fn macos_problems(beta: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for job in MACOS_JOBS {
        if let Some(condition) = workflows::job_condition(beta, job)? {
            out.push(format!(
                "the job `{job}` has `if: {condition}`; it must run on every push, unconditionally"
            ));
        }
    }
    if workflows::events(beta)?.iter().any(|e| e == "pull_request") {
        out.push(
            "beta.yml runs on `pull_request`; the macOS jobs would queue for every pull request"
                .to_owned(),
        );
    }
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
    for event in ["pull_request", "merge_group", "workflow_call"] {
        if !events.iter().any(|e| e == event) {
            out.push(format!("the workflow does not run on `{event}`"));
        }
    }
    if events.iter().any(|e| e == "push") {
        out.push(format!(
            "the workflow has a `push` trigger: a push to {PUSH_BRANCHES:?} reaches it as a called workflow of the pipeline (`workflow_call`), whose filter `pipeline_workflow` pins"
        ));
    }
    for job in ATTEST_JOBS.iter().chain(VERIFY_JOBS) {
        let condition = workflows::job_condition(workflow, job)?;
        if condition.as_deref() != Some(SIGNING_IF) {
            out.push(format!(
                "the job `{job}` has `if: {}`, not exactly `{SIGNING_IF}`",
                condition.as_deref().unwrap_or("(none)")
            ));
        }
    }
    for attest_job in ATTEST_JOBS {
        let attest = workflows::job_steps(workflow, attest_job)?;
        // The step over the sums; the steps over one tarball each are
        // `attested_bundles`' business.
        let provenance = attest.iter().find(|s| {
            s.uses
                .as_deref()
                .is_some_and(|u| u.starts_with("actions/attest-build-provenance@"))
                && s.with.iter().any(|(k, _)| k == "subject-checksums")
        });
        match provenance {
            None => out.push(format!(
                "the job `{attest_job}` has no actions/attest-build-provenance step with `subject-checksums`"
            )),
            Some(step) => {
                let subjects = step
                    .with
                    .iter()
                    .find(|(k, _)| k == "subject-checksums")
                    .map(|(_, v)| v.as_str());
                if subjects != Some("dist/SHA256SUMS") {
                    out.push(format!(
                        "the provenance step of `{attest_job}` does not cover `dist/SHA256SUMS`, but {subjects:?}"
                    ));
                }
                if step.conditional {
                    out.push(format!(
                        "the provenance step of `{attest_job}` is under `if:` or `continue-on-error:`"
                    ));
                }
            }
        }
    }
    for verify_job in VERIFY_JOBS {
        let verify = workflows::job_steps(workflow, verify_job)?;
        let verifying = verify.iter().find_map(|s| {
            lines(s)
                .find_map(|l| args(l, "cargo xtask verify-assets attestations ").map(|a| (s, a)))
        });
        match verifying {
            None => out.push(format!(
                "the job `{verify_job}` does not run `cargo xtask verify-assets attestations`"
            )),
            Some((step, words)) => {
                let at = words.iter().position(|w| *w == "--source-ref");
                if at.and_then(|i| words.get(i + 1)) != Some(&"$GITHUB_REF") {
                    out.push(format!(
                        "the job `{verify_job}` verifies attestations without `--source-ref \"$GITHUB_REF\"`, so a push to a branch other than main would be checked as main (or any branch as any)"
                    ));
                }
                if step.conditional {
                    out.push(format!(
                        "the attestation check of `{verify_job}` is under `if:` or `continue-on-error:`"
                    ));
                }
                attested_bundles::offline_verification(
                    &format!("the job `{verify_job}`"),
                    &verify,
                    "$GITHUB_SHA",
                    out,
                );
            }
        }
    }
    Ok(())
}

fn identity(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    for job in BUILD_JOBS {
        let steps = workflows::job_steps(workflow, job)?;
        for name in IDENTIFIED {
            let packages = |l: &str| {
                args(l, "cargo xtask package ").is_some_and(|a| {
                    let at = a.iter().position(|w| *w == "--binary");
                    at.and_then(|i| a.get(i + 1))
                        .is_some_and(|b| base(b) == *name)
                })
            };
            let Some((package, _)) = first(&steps, 0, packages) else {
                continue;
            };
            let checks = |l: &str| {
                args(l, "cargo xtask build-commit check --json ")
                    .is_some_and(|a| a.len() == 2 && base(a[0]) == *name && a[1] == "$GITHUB_SHA")
            };
            match first(&steps, 0, checks) {
                Some((at, step)) if at < package && !step.conditional => {}
                Some((at, _)) if at < package => out.push(format!(
                    "`{job}` checks `{name} --version --json` under `if:` or `continue-on-error:`"
                )),
                _ => out.push(format!(
                    "`{job}` packages {name} without `cargo xtask build-commit check --json <{name}> \"$GITHUB_SHA\"` before"
                )),
            }
            shipped_tarball(job, name, &steps, package, out);
        }
    }
    Ok(())
}

/// The tarball `job` packaged (step `package`) is opened again, in a later step that
/// cannot be skipped: `cargo xtask build-commit check-asset` of this job's tarball of
/// `name` and `"$GITHUB_SHA"`. The check before packaging is of the binary built; this
/// one is of the bytes that ship. (The job is named by its platform, and packages into
/// `dist`.)
fn shipped_tarball(
    job: &str,
    name: &str,
    steps: &[workflows::Step],
    package: usize,
    out: &mut Vec<String>,
) {
    let tarball = format!("dist/{name}-$GITHUB_SHA-{job}.tar.gz");
    let checks_tarball = |l: &str| {
        args(l, "cargo xtask build-commit check-asset ")
            .is_some_and(|a| a.len() == 2 && a[0] == tarball && a[1] == "$GITHUB_SHA")
    };
    let checks: Vec<&workflows::Step> = steps
        .iter()
        .skip(package + 1)
        .filter(|s| lines(s).any(&checks_tarball))
        .collect();
    if checks.iter().any(|s| !s.conditional) {
        return;
    }
    out.push(if checks.is_empty() {
        format!(
            "`{job}` packages {name} without `cargo xtask build-commit check-asset \"{tarball}\" \"$GITHUB_SHA\"` in a step after"
        )
    } else {
        format!("`{job}` checks the packaged {name} tarball under `if:` or `continue-on-error:`")
    });
}

/// The value of `--flag` in the words of a command line.
fn flag<'a>(words: &[&'a str], flag: &str) -> Option<&'a str> {
    let at = words.iter().position(|w| *w == flag)?;
    words.get(at + 1).copied()
}

/// Every tool a job fetches (`cargo xtask fetch ... --sha256 <value>`) is pinned by a
/// workflow variable that exists and holds 64 lowercase hex digits: `--sha256 "$NAME"`.
/// (A `"$$NAME"`, which the shell reads as its process id followed by `NAME`, passes the
/// lint of the step and fails the job on the first run.)
fn fetch_pins(workflow: &str, job: &str, steps: &[workflows::Step], out: &mut Vec<String>) {
    for step in steps {
        for line in lines(step) {
            let Some(words) = args(line, "cargo xtask fetch ") else {
                continue;
            };
            let pin = flag(&words, "--sha256").unwrap_or_default();
            let name = pin.strip_prefix('$').unwrap_or_default();
            let named = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
            let defined = named
                && workflow.lines().any(|l| {
                    l.trim_start()
                        .strip_prefix(name)
                        .and_then(|rest| rest.strip_prefix(": "))
                        .is_some_and(|v| {
                            v.len() == 64
                                && v.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                        })
                });
            if !defined {
                out.push(format!(
                    "`{job}`: `{line}` pins its download with --sha256 {pin:?}, which is not `$NAME` of a workflow variable holding 64 lowercase hex digits"
                ));
            }
        }
    }
}

/// The `kbf-installer` deb: see the module docs.
fn packages(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let made = attested_assets::packaged_debs(workflow)?;
    for platform in PACKAGES {
        let n = made.iter().filter(|p| p == platform).count();
        if n != 1 {
            out.push(format!(
                "the kbf-installer package for {platform} is made an asset by {n} `cargo xtask pkg-asset` lines, not exactly one"
            ));
        }
    }
    for platform in &made {
        if !PACKAGES.contains(&platform.as_str()) {
            out.push(format!(
                "`cargo xtask pkg-asset --platform {platform}` makes a package that is not in `PACKAGES`: add it there and to docs/artifacts.md"
            ));
        }
    }
    for platform in PACKAGES {
        let steps = workflows::job_steps(workflow, platform)?;
        let tarball = format!("dist/kbf-installer-$GITHUB_SHA-{platform}.tar.gz");
        let asset = first(&steps, 0, |l| {
            args(l, "cargo xtask pkg-asset ").is_some_and(|a| !a.contains(&"--verify-only"))
        });
        let Some((asset_at, asset_step)) = asset else {
            continue;
        };
        let words = lines(asset_step)
            .find_map(|l| args(l, "cargo xtask pkg-asset "))
            .unwrap_or_default();
        let who = format!("the kbf-installer package of `{platform}`");
        if asset_step.conditional {
            out.push(format!(
                "{who} is made an asset under `if:` or `continue-on-error:`"
            ));
        }
        for (name, want) in [
            ("--commit", "$GITHUB_SHA"),
            ("--platform", platform),
            ("--binary-tarball", tarball.as_str()),
            ("--expect-ref", "$PKG_SOURCE_REF"),
            ("--out", "dist"),
        ] {
            if flag(&words, name) != Some(want) {
                out.push(format!(
                    "`cargo xtask pkg-asset` in `{platform}` has {name} {:?}, not `{want}`",
                    flag(&words, name).unwrap_or("(none)")
                ));
            }
        }
        if flag(&words, "--gh").is_none() {
            out.push(format!(
                "`cargo xtask pkg-asset` in `{platform}` is not given the pinned gh (`--gh`) to compare the package's with"
            ));
        }
        // The tarball is checked for the commit first, the package is installed and run
        // before it is made an asset, and its trust directory names the branch.
        let ordered =
            |found: Option<(usize, &workflows::Step)>, what: &str, out: &mut Vec<String>| {
                match found {
                    Some((at, step)) if at < asset_at && !step.conditional => {}
                    Some((at, _)) if at < asset_at => out.push(format!(
                        "{who}: {what} is under `if:` or `continue-on-error:`"
                    )),
                    _ => out.push(format!(
                        "{who} is made an asset without {what} in a step before"
                    )),
                }
            };
        // (A tarball never checked at all is `identity`'s to name; here, one checked too late.)
        let checked = first(&steps, 0, |l| {
            args(l, "cargo xtask build-commit check-asset ")
                .is_some_and(|a| a.len() == 2 && a[0] == tarball && a[1] == "$GITHUB_SHA")
        });
        if checked.is_some_and(|(at, _)| at > asset_at) {
            out.push(format!(
                "{who} is made an asset before `cargo xtask build-commit check-asset` of the tarball whose binary it holds"
            ));
        }
        ordered(
            first(&steps, 0, |l| {
                args(l, "cargo xtask pkg-install-test ")
                    .is_some_and(|a| flag(&a, "--format") == Some("deb"))
            }),
            "`cargo xtask pkg-install-test --format deb`",
            out,
        );
        ordered(
            first(&steps, 0, |l| {
                args(l, "cargo xtask pkg-trust ")
                    .is_some_and(|a| flag(&a, "--source-ref") == Some("$PKG_SOURCE_REF"))
            }),
            "`cargo xtask pkg-trust --source-ref \"$PKG_SOURCE_REF\"`",
            out,
        );
        for step in &steps {
            for line in lines(step) {
                let pkg = line.starts_with("cargo xtask pkg-trust ")
                    || line.starts_with("cargo xtask pkg-asset ");
                if pkg && line.contains("refs/heads/") {
                    out.push(format!(
                        "`{platform}`: `{line}` names a literal branch: the package trusts and is attested for `$PKG_SOURCE_REF` and no other"
                    ));
                }
            }
        }
        fetch_pins(workflow, platform, &steps, out);
        let env = workflows::job_env(workflow, platform)?;
        let got = env
            .iter()
            .find(|(k, _)| k == PKG_SOURCE_REF)
            .map(|(_, v)| v.as_str());
        if got != Some(PKG_SOURCE_REF_EXPR) {
            out.push(format!(
                "the job `{platform}` has {PKG_SOURCE_REF}: {}, not exactly `{PKG_SOURCE_REF_EXPR}`",
                got.unwrap_or("(none)")
            ));
        }
    }
    // The verify job checks the file that was signed.
    let verify = workflows::job_steps(workflow, "verify-linux")?;
    let verified_at = first(&verify, 0, |l| {
        args(l, "cargo xtask verify-assets attestations ").is_some()
    })
    .map_or(usize::MAX, |(at, _)| at);
    for platform in PACKAGES {
        let deb = format!("dist/kbf-installer-$GITHUB_SHA-{platform}.deb");
        let tarball = format!("dist/kbf-installer-$GITHUB_SHA-{platform}.tar.gz");
        let found = first(&verify, 0, |l| {
            args(l, "cargo xtask pkg-asset ").is_some_and(|a| {
                a.contains(&"--verify-only")
                    && flag(&a, "--package") == Some(deb.as_str())
                    && flag(&a, "--binary-tarball") == Some(tarball.as_str())
                    && flag(&a, "--platform") == Some(platform)
                    && flag(&a, "--commit") == Some("$GITHUB_SHA")
                    && flag(&a, "--expect-ref") == Some("$GITHUB_REF")
            })
        });
        match found {
            Some((at, step)) if at > verified_at && !step.conditional => {}
            _ => out.push(format!(
                "`verify-linux` does not run `cargo xtask pkg-asset --verify-only` on `{deb}` (with `--expect-ref \"$GITHUB_REF\"`) in an unconditional step after the attestation check, so the package that was signed is not the one checked"
            )),
        }
    }
    Ok(())
}

/// The darwin job's `.pkg`: see the module docs.
fn mac_package(workflow: &str, out: &mut Vec<String>) -> Result<(), String> {
    let made = attested_assets::packaged_macos_pkgs(workflow)?;
    if made.len() != 1 {
        out.push(format!(
            "the kbf-installer package for {MAC_PACKAGE} is made an asset by {} `cargo xtask pkg-macos-asset` lines, not exactly one",
            made.len()
        ));
    }
    let steps = workflows::job_steps(workflow, DARWIN_JOB)?;
    let tarball = format!("dist/kbf-installer-$GITHUB_SHA-{MAC_PACKAGE}.tar.gz");
    let asset = first(&steps, 0, |l| {
        args(l, "cargo xtask pkg-macos-asset ").is_some_and(|a| !a.contains(&"--verify-only"))
    });
    let who = format!("the kbf-installer package of `{DARWIN_JOB}`");
    let Some((asset_at, asset_step)) = asset else {
        out.push(format!(
            "`{DARWIN_JOB}` does not run `cargo xtask pkg-macos-asset`, so {who} is not an asset"
        ));
        return Ok(());
    };
    let words = lines(asset_step)
        .find_map(|l| args(l, "cargo xtask pkg-macos-asset "))
        .unwrap_or_default();
    if asset_step.conditional {
        out.push(format!(
            "{who} is made an asset under `if:` or `continue-on-error:`"
        ));
    }
    for (name, want) in [
        ("--commit", "$GITHUB_SHA"),
        ("--binary-tarball", tarball.as_str()),
        ("--expect-ref", "$PKG_SOURCE_REF"),
        ("--out", "dist"),
    ] {
        if flag(&words, name) != Some(want) {
            out.push(format!(
                "`cargo xtask pkg-macos-asset` in `{DARWIN_JOB}` has {name} {:?}, not `{want}`",
                flag(&words, name).unwrap_or("(none)")
            ));
        }
    }
    if flag(&words, "--gh").is_none() {
        out.push(format!(
            "`cargo xtask pkg-macos-asset` in `{DARWIN_JOB}` is not given the pinned gh (`--gh`) to compare the package's with"
        ));
    }
    // What runs before it, each in a step that cannot be skipped: the tarball whose binary the
    // package holds is signed and names the commit, the package is installed and checked, and its
    // trust directory names the branch.
    let ordered =
        |found: Option<(usize, &workflows::Step)>, what: &str, out: &mut Vec<String>| match found {
            Some((at, step)) if at < asset_at && !step.conditional => {}
            Some((at, _)) if at < asset_at => out.push(format!(
                "{who}: {what} is under `if:` or `continue-on-error:`"
            )),
            _ => out.push(format!(
                "{who} is made an asset without {what} in a step before"
            )),
        };
    // (A tarball never checked at all is `identity`'s and `darwin`'s to name; here, one checked
    // too late, after the package made of its binary is already an asset.)
    for (what, found) in [
        (
            "`cargo xtask build-commit check-asset`",
            first(&steps, 0, |l| {
                args(l, "cargo xtask build-commit check-asset ")
                    .is_some_and(|a| a.len() == 2 && a[0] == tarball && a[1] == "$GITHUB_SHA")
            }),
        ),
        (
            "`cargo xtask darwin-asset`",
            first(&steps, 0, |l| {
                args(l, "cargo xtask darwin-asset ")
                    .is_some_and(|a| a.len() == 2 && a[0] == tarball && a[1] == "kbf-installer")
            }),
        ),
    ] {
        if found.is_some_and(|(at, _)| at > asset_at) {
            out.push(format!(
                "{who} is made an asset before {what} of the tarball whose binary it holds"
            ));
        }
    }
    ordered(
        first(&steps, 0, |l| {
            args(l, "cargo xtask pkg-macos-install-test ").is_some()
        }),
        "`cargo xtask pkg-macos-install-test`",
        out,
    );
    ordered(
        first(&steps, 0, |l| {
            args(l, "cargo xtask pkg-trust ")
                .is_some_and(|a| flag(&a, "--source-ref") == Some("$PKG_SOURCE_REF"))
        }),
        "`cargo xtask pkg-trust --source-ref \"$PKG_SOURCE_REF\"`",
        out,
    );
    for step in &steps {
        for line in lines(step) {
            let pkg = line.starts_with("cargo xtask pkg-trust ")
                || line.starts_with("cargo xtask pkg-macos-asset ");
            if pkg && line.contains("refs/heads/") {
                out.push(format!(
                    "`{DARWIN_JOB}`: `{line}` names a literal branch: the package trusts and is attested for `$PKG_SOURCE_REF` and no other"
                ));
            }
        }
    }
    fetch_pins(workflow, DARWIN_JOB, &steps, out);
    let env = workflows::job_env(workflow, DARWIN_JOB)?;
    let got = env
        .iter()
        .find(|(k, _)| k == PKG_SOURCE_REF)
        .map(|(_, v)| v.as_str());
    if got != Some(PKG_SOURCE_REF_EXPR) {
        out.push(format!(
            "the job `{DARWIN_JOB}` has {PKG_SOURCE_REF}: {}, not exactly `{PKG_SOURCE_REF_EXPR}`",
            got.unwrap_or("(none)")
        ));
    }
    // The verify job checks the file that was signed.
    let verify = workflows::job_steps(workflow, "verify-darwin-arm64")?;
    let verified_at = first(&verify, 0, |l| {
        args(l, "cargo xtask verify-assets attestations ").is_some()
    })
    .map_or(usize::MAX, |(at, _)| at);
    let pkg = format!("dist/kbf-installer-$GITHUB_SHA-{MAC_PACKAGE}.pkg");
    let found = first(&verify, 0, |l| {
        args(l, "cargo xtask pkg-macos-asset ").is_some_and(|a| {
            a.contains(&"--verify-only")
                && flag(&a, "--package") == Some(pkg.as_str())
                && flag(&a, "--binary-tarball") == Some(tarball.as_str())
                && flag(&a, "--commit") == Some("$GITHUB_SHA")
                && flag(&a, "--expect-ref") == Some("$GITHUB_REF")
        })
    });
    match found {
        Some((at, step)) if at > verified_at && !step.conditional => {}
        _ => out.push(format!(
            "`verify-darwin-arm64` does not run `cargo xtask pkg-macos-asset --verify-only` on `{pkg}` (with `--expect-ref \"$GITHUB_REF\"`) in an unconditional step after the attestation check, so the package that was signed is not the one checked"
        )),
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
fn every_component_a_platform_needs_is_in_its_release_set() {
    // Catches: `ReleasePlatform::components()` naming a tarball the artifacts workflow
    // does not build for that platform (the Mac's was `kbf-daemon`, which only Linux
    // builds), so the release store never calls the platform complete, or calls it
    // complete on a file no host takes. `the_workflow_builds_signs_checks_and_attests_what_the_release_set_says`
    // holds the workflow to `ASSETS`; this holds the store's idea of complete to it.
    for platform in kbf_types::ReleasePlatform::ALL {
        for need in platform.components() {
            assert!(
                ASSETS.contains(&(*need, platform.as_str())),
                "the release store needs {need} for {platform}, which the release set does not build: {ASSETS:?}"
            );
        }
    }
}

#[test]
fn the_macos_jobs_run_on_every_push_and_on_no_pull_request() {
    // Catches: a condition added to native-macos or pkg-macos in beta.yml (a push that
    // skips the only real-Mac test), or a `pull_request` trigger on beta.yml, which
    // would queue the small macOS pool for every pull request.
    let path = ".github/workflows/beta.yml";
    let text = std::fs::read_to_string(super::repo_root().join(path)).expect(path);
    match macos_problems(&text) {
        Ok(found) => assert!(found.is_empty(), "{path}:\n{}", found.join("\n")),
        Err(e) => panic!("{path}: {e}"),
    }
}

#[test]
fn the_docs_describe_the_release_set() {
    // Catches: docs/artifacts.md describing another set of files than the workflow
    // builds (the doc is what an operator verifies against), or no longer saying that
    // pushes to the development branch are attested, what `--version --json` is, and
    // how a tarball is verified offline with its bundle.
    let doc = std::fs::read_to_string(super::repo_root().join("docs/artifacts.md"))
        .expect("docs/artifacts.md");
    let mut missing = Vec::new();
    for (name, platform) in ASSETS {
        let file = format!("`{name}-<commit>-{platform}.tar.gz`");
        if !doc.contains(&file) {
            missing.push(format!("docs/artifacts.md does not list {file}"));
        }
    }
    for platform in PACKAGES {
        for file in [
            format!("`kbf-installer-<commit>-{platform}.deb`"),
            format!("`kbf-<commit>-{platform}.deb`"),
            format!("`kbf-frontend-image-<commit>-{platform}.oci.tar`"),
            format!("`kbf-coordinator-image-<commit>-{platform}.oci.tar`"),
        ] {
            if !doc.contains(&file) {
                missing.push(format!("docs/artifacts.md does not list {file}"));
            }
        }
    }
    let file = format!("`kbf-installer-<commit>-{MAC_PACKAGE}.pkg`");
    if !doc.contains(&file) {
        missing.push(format!("docs/artifacts.md does not list {file}"));
    }
    for branch in PUSH_BRANCHES {
        let named = format!("`{branch}`");
        if !doc.contains(&named) {
            missing.push(format!("docs/artifacts.md does not name {named}"));
        }
    }
    for phrase in [
        "kbf --version --json",
        "kbf-installer --version --json",
        "`--source-ref \"$GITHUB_REF\"`",
        "`<tarball>.sigstore.json`",
        "--bundle <tarball>.sigstore.json",
        "--source-digest <commit>",
        "sudo dpkg -i kbf-installer-<commit>-linux-x86_64.deb",
        "sudo installer -pkg kbf-installer-<commit>-darwin-arm64.pkg -target /",
        "`kbf-<commit>-<platform>.version.json`",
        "**no SBOM attestation**",
        "**the branch the package is attested on**",
    ] {
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

    /// The step that re-opens `platform`'s packaged `name` tarball and checks its identity.
    fn check_asset_step(name: &str, platform: &str) -> String {
        format!(
            "      - name: The packaged {name} names the commit\n        run: cargo xtask build-commit check-asset \"dist/{name}-$GITHUB_SHA-{platform}.tar.gz\" \"$GITHUB_SHA\"\n"
        )
    }

    /// The steps that make the installer deb of `platform` an asset, as the Linux jobs
    /// have them.
    fn package_steps(platform: &str) -> String {
        format!(
            "      - name: Trust\n        run: cargo xtask pkg-trust --gh \"$RUNNER_TEMP/tools/gh\" --trusted-root \"$RUNNER_TEMP/pkg-root.jsonl\" --source-ref \"$PKG_SOURCE_REF\" --out target/pkg-trust\n      - name: Install\n        run: cargo xtask pkg-install-test --package p.deb --format deb --listing target/pkg/listing.txt --image-dir packaging/linux/test/ubuntu\n      - name: Asset\n        run: cargo xtask pkg-asset --package p.deb --platform {platform} --commit \"$GITHUB_SHA\" --binary-tarball \"dist/kbf-installer-$GITHUB_SHA-{platform}.tar.gz\" --expect-ref \"$PKG_SOURCE_REF\" --gh \"$RUNNER_TEMP/tools/gh\" --out dist\n"
        )
    }

    /// The verify job's check of the signed deb of `platform`.
    fn verify_package_step(platform: &str) -> String {
        format!(
            "      - name: Package {platform}\n        run: cargo xtask pkg-asset --verify-only --package \"dist/kbf-installer-$GITHUB_SHA-{platform}.deb\" --platform {platform} --commit \"$GITHUB_SHA\" --binary-tarball \"dist/kbf-installer-$GITHUB_SHA-{platform}.tar.gz\" --expect-ref \"$GITHUB_REF\"\n"
        )
    }

    /// The steps that write the configuration of both Linux packages from the job's `kbf` and
    /// build the `kbf` package, as the Linux jobs have them.
    fn kbf_build_steps() -> String {
        "      - name: Config\n        run: cargo xtask pkg-config --binary \"target/$TARGET/release/kbf-installer\" --kbf \"target/$TARGET/release/kbf\" --units u --trust t --sysusers s --tmpfiles f --platform p --version 0.0.0+ci --out target/pkg\n      - name: Build kbf\n        run: cargo xtask pkg-build --nfpm n --config target/pkg/kbf/nfpm.yaml --out target/pkg/kbf-out\n".to_owned()
    }

    /// The step that makes the `kbf` deb of `platform` an asset.
    fn kbf_asset_step(platform: &str) -> String {
        format!(
            "      - name: Kbf asset\n        run: cargo xtask pkg-kbf-asset --package target/pkg/kbf-out --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\" --out dist\n"
        )
    }

    /// The verify job's check of the signed `kbf` deb of `platform`.
    fn verify_kbf_package_step(platform: &str) -> String {
        format!(
            "      - name: Kbf package {platform}\n        run: cargo xtask pkg-kbf-asset --verify-only --package \"dist/kbf-$GITHUB_SHA-{platform}.deb\" --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\"\n"
        )
    }

    /// The steps that build the frontend image of `platform` from the job's kbf tarball, twice,
    /// compare the builds and make the first an asset.
    fn image_steps(platform: &str) -> String {
        let build = |n: u32| {
            format!(
                "      - name: Build {n}\n        run: docker buildx build --builder b --no-cache --platform p --provenance=false --sbom=false --build-arg SOURCE_DATE_EPOCH --file f --output \"type=oci,dest=target/image-out/{n}.oci.tar,rewrite-timestamp=true\" target/image\n"
            )
        };
        format!(
            "      - name: Epoch\n        run: cargo xtask source-date-epoch --commit \"$GITHUB_SHA\" --output \"$GITHUB_ENV\"\n      - name: Context\n        run: tar -xzf \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\" -C target/image kbf\n{}{}      - name: Same\n        run: cmp target/image-out/1.oci.tar target/image-out/2.oci.tar\n      - name: Image asset\n        run: cargo xtask pkg-image-asset --service frontend --image target/image-out/1.oci.tar --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\" --out dist\n",
            build(1),
            build(2)
        )
    }

    /// The verify job's check of the signed frontend image of `platform`.
    fn verify_image_step(platform: &str) -> String {
        format!(
            "      - name: Image {platform}\n        run: cargo xtask pkg-image-asset --verify-only --service frontend --image \"dist/kbf-frontend-image-$GITHUB_SHA-{platform}.oci.tar\" --platform {platform} --commit \"$GITHUB_SHA\" --kbf-tarball \"dist/kbf-$GITHUB_SHA-{platform}.tar.gz\"\n"
        )
    }

    /// The steps that check the job's `kbf-coordinator`, build its image twice from it, compare
    /// the builds and make the first an asset (the commit's time is `image_steps`').
    fn coordinator_steps(platform: &str) -> String {
        let build = |n: u32| {
            format!(
                "      - name: Coordinator build {n}\n        run: docker buildx build --builder b --no-cache --platform p --provenance=false --sbom=false --build-arg SOURCE_DATE_EPOCH --file f --output \"type=oci,dest=target/coordinator-image-out/{n}.oci.tar,rewrite-timestamp=true\" target/coordinator-image\n"
            )
        };
        format!(
            "      - name: Coordinator check\n        run: cargo xtask build-commit check \"target/$TARGET/release/kbf-coordinator\" \"$GITHUB_SHA\"\n      - name: Coordinator context\n        run: cp \"target/$TARGET/release/kbf-coordinator\" target/coordinator-image/kbf-coordinator\n{}{}      - name: Coordinator same\n        run: cmp target/coordinator-image-out/1.oci.tar target/coordinator-image-out/2.oci.tar\n      - name: Coordinator asset\n        run: cargo xtask pkg-image-asset --service coordinator --image target/coordinator-image-out/1.oci.tar --platform {platform} --commit \"$GITHUB_SHA\" --binary \"target/$TARGET/release/kbf-coordinator\" --out dist\n",
            build(1),
            build(2)
        )
    }

    /// The verify job's check of the signed coordinator image of `platform` (no file of the build).
    fn verify_coordinator_step(platform: &str) -> String {
        format!(
            "      - name: Coordinator image {platform}\n        run: cargo xtask pkg-image-asset --verify-only --service coordinator --image \"dist/kbf-coordinator-image-$GITHUB_SHA-{platform}.oci.tar\" --platform {platform} --commit \"$GITHUB_SHA\"\n"
        )
    }

    /// The steps that make the Mac `.pkg` an asset, as the darwin job has them (after the
    /// tarball's identity and signature checks, which `good()` writes before).
    fn mac_package_steps() -> String {
        "      - name: Fetch gh\n        run: cargo xtask fetch --url u --sha256 \"$GH_SHA256_AARCH64_DARWIN\" --out gh-darwin\n      - name: Trust\n        run: cargo xtask pkg-trust --gh gh-darwin --trusted-root root.jsonl --source-ref $PKG_SOURCE_REF --out target/pkg-trust\n      - name: Build\n        run: cargo xtask pkg build --binary b --launchd-plist p --out target/pkg/out\n      - name: Install\n        run: cargo xtask pkg-macos-install-test --package target/pkg/out\n      - name: Asset\n        run: cargo xtask pkg-macos-asset --package target/pkg/out --commit $GITHUB_SHA --binary-tarball dist/kbf-installer-$GITHUB_SHA-darwin-arm64.tar.gz --expect-ref $PKG_SOURCE_REF --gh gh-darwin --out dist\n".to_owned()
    }

    /// The verify job's check of the signed `.pkg`.
    fn verify_mac_package_step() -> String {
        "      - name: Package darwin-arm64\n        run: cargo xtask pkg-macos-asset --verify-only --package dist/kbf-installer-$GITHUB_SHA-darwin-arm64.pkg --commit $GITHUB_SHA --binary-tarball dist/kbf-installer-$GITHUB_SHA-darwin-arm64.tar.gz --expect-ref $GITHUB_REF\n".to_owned()
    }

    /// The step that checks `name --version --json` of the binary built.
    fn identity_step(name: &str) -> String {
        format!(
            "      - run: cargo xtask build-commit check --json \"target/$TARGET/release/{name}\" \"$GITHUB_SHA\"\n"
        )
    }

    /// The step that records the facts of the `kbf` built, after its identity is checked.
    fn facts_step(platform: &str) -> String {
        format!(
            "      - name: Facts\n        run: cargo xtask release version-facts --binary \"target/$TARGET/release/kbf\" --commit \"$GITHUB_SHA\" --platform {platform} --out dist\n"
        )
    }

    /// The checkout step of a build job: the whole history, so the build can count its `seq`.
    const CHECKOUT: &str =
        "      - uses: actions/checkout@abc\n        with:\n          fetch-depth: 0\n";

    /// A workflow of the shape the rules read, meeting every one.
    fn good() -> String {
        let mut w = String::from(
            "on:\n  pull_request:\n  merge_group:\n  workflow_call:\npermissions: {}\nenv:\n  GH_SHA256_AARCH64_DARWIN: abababababababababababababababababababababababababababababababab\njobs:\n",
        );
        for job in BUILD_JOBS {
            w.push_str(&format!(
                "  {job}:\n    if: ${{{{ github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, '{PR_LABEL}') }}}}\n    runs-on: ubuntu-24.04\n"
            ));
            if PACKAGES.contains(job) || *job == MAC_PACKAGE {
                w.push_str(&format!(
                    "    env:\n      {PKG_SOURCE_REF}: {PKG_SOURCE_REF_EXPR}\n"
                ));
            }
            w.push_str("    steps:\n");
            w.push_str(CHECKOUT);
            let platform = *job;
            let mine: Vec<&str> = ASSETS
                .iter()
                .filter(|(_, p)| *p == platform)
                .map(|(n, _)| *n)
                .collect();
            let identified: Vec<&str> = IDENTIFIED
                .iter()
                .copied()
                .filter(|n| mine.contains(n))
                .collect();
            if platform != "darwin-arm64" {
                for name in &identified {
                    w.push_str(&identity_step(name));
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
                // Darwin signs first, so the check names the signed bytes.
                for name in &identified {
                    w.push_str(&identity_step(name));
                }
            }
            w.push_str(&facts_step(platform));
            for name in &mine {
                w.push_str(&format!(
                    "      - name: Package {name}\n        run: cargo xtask package --binary \"target/$TARGET/release/{name}\" --sbom s.json --platform {platform} --commit \"$GITHUB_SHA\" --out dist\n"
                ));
                if platform == "darwin-arm64" {
                    w.push_str(&format!(
                        "      - name: Check {name}\n        run: cargo xtask darwin-asset \"dist/{name}-$GITHUB_SHA-darwin-arm64.tar.gz\" {name}\n"
                    ));
                }
                if identified.contains(name) {
                    w.push_str(&check_asset_step(name, platform));
                }
            }
            if PACKAGES.contains(job) {
                w.push_str(&kbf_build_steps());
                w.push_str(&package_steps(platform));
                w.push_str(&kbf_asset_step(platform));
                w.push_str(&image_steps(platform));
                w.push_str(&coordinator_steps(platform));
            }
            if *job == MAC_PACKAGE {
                w.push_str(&mac_package_steps());
            }
        }
        for platform in ["linux", "darwin-arm64"] {
            w.push_str(&format!(
                "  attest-{platform}:\n    if: {SIGNING_IF}\n    runs-on: ubuntu-24.04\n    steps:\n      - uses: actions/attest-build-provenance@0123 # v4\n        with:\n          subject-checksums: dist/SHA256SUMS\n"
            ));
        }
        for platform in ["linux", "darwin-arm64"] {
            w.push_str(&format!(
                "  verify-{platform}:\n    if: {SIGNING_IF}\n    runs-on: ubuntu-24.04\n    steps:\n      - run: gh attestation trusted-root > \"$RUNNER_TEMP/trusted_root.jsonl\"\n      - run: cargo xtask verify-assets attestations dist --repo \"$GITHUB_REPOSITORY\" --workflow-ref \"$GITHUB_REPOSITORY/.github/workflows/artifacts.yml\" --source-ref \"$GITHUB_REF\" --commit \"$GITHUB_SHA\" --trusted-root \"$RUNNER_TEMP/trusted_root.jsonl\"\n"
            ));
            if platform == "linux" {
                for p in PACKAGES {
                    w.push_str(&verify_package_step(p));
                    w.push_str(&verify_kbf_package_step(p));
                    w.push_str(&verify_image_step(p));
                    w.push_str(&verify_coordinator_step(p));
                }
            } else {
                w.push_str(&verify_mac_package_step());
            }
        }
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

    /// `good()` with the first `from` of the job `job` (and not of the jobs before it)
    /// replaced by `to`.
    fn in_job(job: &str, from: &str, to: &str) -> String {
        let w = good();
        let start = w.find(&format!("\n  {job}:\n")).expect("the job");
        let at = start + w[start..].find(from).expect("the text is in the job");
        format!("{}{to}{}", &w[..at], &w[at + from.len()..])
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
    fn a_macos_job_with_a_condition_or_a_pull_request_trigger_is_refused() {
        // Catches: an `if:` on either macOS job, and a `pull_request` trigger, each
        // accepted; and a clean push-only workflow refused.
        let beta = "name: beta\non:\n  push:\n    branches: [main]\njobs:\n  native-macos:\n    runs-on: macos-latest\n  pkg-macos:\n    runs-on: macos-latest\n";
        assert_eq!(
            macos_problems(beta).expect("readable"),
            Vec::<String>::new()
        );
        for job in MACOS_JOBS {
            let bad = beta.replacen(
                &format!("  {job}:\n"),
                &format!("  {job}:\n    if: ${{{{ github.ref == 'refs/heads/main' }}}}\n"),
                1,
            );
            assert_ne!(bad, beta, "{job}");
            let got = macos_problems(&bad).expect("readable");
            assert!(
                got.iter().any(|p| p.contains(&format!("`{job}` has `if:"))),
                "{job}: {got:?}"
            );
        }
        let bad = beta.replacen("  push:", "  pull_request:\n  push:", 1);
        let got = macos_problems(&bad).expect("readable");
        assert!(got.iter().any(|p| p.contains("`pull_request`")), "{got:?}");
    }

    #[test]
    fn a_build_job_runs_on_a_pull_request_only_with_the_label() {
        // Catches: a build job left without the label condition, so every pull request
        // queues a runner for it (the load this gate removes).
        let w = good();
        let line = format!(
            "    if: ${{{{ github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, '{PR_LABEL}') }}}}\n"
        );
        assert!(w.contains(&line), "the fixture's condition");
        let parts: Vec<&str> = w.split(&line).collect();
        assert_eq!(parts.len(), 4, "three build jobs");
        for (k, job) in BUILD_JOBS.iter().enumerate() {
            let mut planted = String::new();
            for (i, part) in parts.iter().enumerate() {
                planted.push_str(part);
                if i < 3 && i != k {
                    planted.push_str(&line);
                }
            }
            names(&planted, &[&format!("the build job `{job}` does not run")]);
        }
    }

    #[test]
    fn the_signing_jobs_run_only_for_a_push_to_a_listed_branch() {
        // Catches: attest or verify opened to every event or every push (a pull request
        // would sign), re-widened to the retired development branch `feature/arch-v2`, or left
        // without an `if:`; and a push trigger of other branches than main.
        let line = format!("    if: {SIGNING_IF}\n");
        for (k, job) in ATTEST_JOBS.iter().chain(VERIFY_JOBS).enumerate() {
            for to in [
                "if: ${{ github.event_name == 'push' }}",
                "if: ${{ github.ref == 'refs/heads/main' }}",
                "if: ${{ github.event_name == 'push' && (github.ref == 'refs/heads/main' || github.ref == 'refs/heads/feature/arch-v2') }}",
                "if: ${{ github.event_name != 'push' }}",
            ] {
                // The k-th `if:` of the signing jobs, replaced.
                let w = good();
                let parts: Vec<&str> = w.split(&line).collect();
                assert_eq!(
                    parts.len(),
                    5,
                    "the attest and verify jobs of both platform groups"
                );
                let mut planted = String::new();
                for (i, part) in parts.iter().enumerate() {
                    planted.push_str(part);
                    if i < 4 {
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
        names(
            &broken(
                "  workflow_call:\n",
                "  workflow_call:\n  push:\n    branches: [main]\n",
            ),
            &["the workflow has a `push` trigger"],
        );
        names(
            &broken("  workflow_call:\n", ""),
            &["does not run on `workflow_call`"],
        );
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
            &["provenance step of `attest-linux` is under `if:`"],
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
            &["attestation check of `verify-linux` is under `if:`"],
        );
        names(
            &broken("verify-assets attestations", "verify-assets sums"),
            &["does not run `cargo xtask verify-assets attestations`"],
        );
    }

    #[test]
    fn the_verify_job_checks_the_bundles_offline_for_the_commit_it_verifies() {
        // Catches: the bundles verified for no commit, or for another variable than the
        // job's own `$GITHUB_SHA` (an attestation of any commit of the branch would
        // pass), with no trusted root or one other than the file fetched (gh would
        // reach for it itself: not the check a host makes), or with no root fetched.
        let fetch =
            "      - run: gh attestation trusted-root > \"$RUNNER_TEMP/trusted_root.jsonl\"\n";
        let cases = [
            // (The package lines of the build jobs have a `--commit "$GITHUB_SHA"` too,
            // so each plant names what follows it.)
            (
                " --commit \"$GITHUB_SHA\" --trusted-root",
                " --trusted-root",
                "verifies attestations without `--commit \"$GITHUB_SHA\"`",
            ),
            (
                "--commit \"$GITHUB_SHA\" --trusted-root",
                "--commit \"$COMMIT\" --trusted-root",
                "verifies attestations without `--commit \"$GITHUB_SHA\"`",
            ),
            (
                " --trusted-root \"$RUNNER_TEMP/trusted_root.jsonl\"",
                "",
                "does not verify offline",
            ),
            (
                "--trusted-root \"$RUNNER_TEMP/trusted_root.jsonl\"",
                "--trusted-root elsewhere.jsonl",
                "does not verify offline",
            ),
            (fetch, "", "does not fetch the trusted root"),
        ];
        for (from, to, wanted) in cases {
            names(
                &broken(from, to),
                &[&format!("the job `verify-linux` {wanted}")],
            );
        }
    }

    #[test]
    fn every_job_that_packages_kbf_checks_its_identity_first() {
        // Catches: `kbf --version --json` of the shipped bytes unchecked (a debug build
        // or another commit could ship), the plain `--version` check standing in for
        // it, the check of another binary or another commit, the check after packaging
        // (the tarball already made) or one that cannot fail the job. Each of the
        // identified binaries is held to it (`kbf-installer` is the second).
        for name in IDENTIFIED {
            let step = identity_step(name);
            let the_check = step
                .trim_start_matches("      - run: ")
                .trim_end()
                .to_owned();
            // The other identified binary's check standing in for this one's.
            let peer = IDENTIFIED.iter().find(|n| *n != name).expect("a peer");
            for to in [
                "cargo xtask build-commit check".to_owned(),
                identity_step("kbf-daemon").replace("      - run: ", ""),
                identity_step("kbf-deploy").replace("\"$GITHUB_SHA\"", "abc"),
                identity_step(peer),
            ] {
                // The step's text is a script line: one `run:` step of the fixture.
                let to = to.trim_start_matches("      - run: ").trim_end();
                // Linux x86_64's check is the second of the file; darwin's is the first.
                let w = good().replacen(&the_check, to, 2);
                assert_ne!(w, good(), "{name}: the check is in the planted workflow");
                names(
                    &w,
                    &[&format!(
                        "packages {name} without `cargo xtask build-commit check --json"
                    )],
                );
            }
            let conditional = broken(
                step.trim_end(),
                &step
                    .trim_end()
                    .replace("      - run:", "      - if: always()\n        run:"),
            );
            names(
                &conditional,
                &[&format!("checks `{name} --version --json` under `if:`")],
            );
        }
    }

    /// `w` without the `k`-th (from 0) occurrence of `step`.
    fn without_nth(w: &str, step: &str, k: usize) -> String {
        let at = w
            .match_indices(step)
            .map(|(i, _)| i)
            .nth(k)
            .unwrap_or_else(|| panic!("occurrence {k} of {step:?} is not in the workflow"));
        format!("{}{}", &w[..at], &w[at + step.len()..])
    }

    #[test]
    fn one_binarys_check_does_not_stand_in_for_the_other() {
        // Catches: an identity rule that is satisfied by any `build-commit check --json`
        // (or `check-asset`) in the job, so the installer ships unchecked as long as
        // `kbf` is checked: each binary's step removed alone is named for that binary,
        // in every job, and for it only.
        for name in IDENTIFIED {
            for (k, job) in BUILD_JOBS.iter().enumerate() {
                let w = good();
                let check = format!(
                    "`{job}` packages {name} without `cargo xtask build-commit check --json <{name}> \"$GITHUB_SHA\"` before"
                );
                assert_eq!(
                    found(&without_nth(&w, &identity_step(name), k)),
                    vec![check],
                    "{job}: {name}'s check removed"
                );
                let asset = format!(
                    "`{job}` packages {name} without `cargo xtask build-commit check-asset \"dist/{name}-$GITHUB_SHA-{job}.tar.gz\" \"$GITHUB_SHA\"` in a step after"
                );
                assert_eq!(
                    found(&without_nth(&w, &check_asset_step(name, job), 0)),
                    vec![asset],
                    "{job}: {name}'s check-asset removed"
                );
            }
        }
    }

    #[test]
    fn every_job_that_packages_kbf_re_opens_the_tarball_after() {
        // Catches: the identity checked only on the binary before it is packaged, so the
        // tarball that ships is never opened again; the re-check missing from one job
        // while the others have it, naming another platform's tarball or another
        // binary's, checking another commit, commented out, followed by `|| true` (or
        // `; true`), run before the tarball exists, or skippable by `if:`. For each of the
        // identified binaries, `kbf-installer` included.
        for name in IDENTIFIED {
            re_opens_the_tarball(name);
        }
    }

    fn re_opens_the_tarball(name: &str) {
        let package = format!("      - name: Package {name}\n");
        // The other identified binary: its re-check standing in for this one's.
        let peer = IDENTIFIED.iter().find(|n| **n != name).expect("a peer");
        for (k, job) in BUILD_JOBS.iter().enumerate() {
            let step = check_asset_step(name, job);
            let missing = format!(
                "`{job}` packages {name} without `cargo xtask build-commit check-asset \"dist/{name}-$GITHUB_SHA-{job}.tar.gz\" \"$GITHUB_SHA\"` in a step after"
            );
            let skippable = format!(
                "`{job}` checks the packaged {name} tarball under `if:` or `continue-on-error:`"
            );
            let other = BUILD_JOBS[(k + 1) % BUILD_JOBS.len()];
            let without = good().replacen(&step, "", 1);
            assert_ne!(without, good(), "{job}: the step is in the fixture");
            // The step moved to just before this job's `Package <name>`.
            let at = without
                .match_indices(&package)
                .nth(k)
                .map(|(i, _)| i)
                .expect("this job's package step");
            let moved = format!("{}{step}{}", &without[..at], &without[at..]);
            let tarball = format!("dist/{name}-");
            let variants = [
                ("removed", without.clone(), &missing),
                (
                    "another platform's tarball",
                    good().replacen(&step, &step.replace(job, other), 1),
                    &missing,
                ),
                (
                    "another binary's tarball",
                    good().replacen(&step, &step.replace(&tarball, "dist/kbf-daemon-"), 1),
                    &missing,
                ),
                (
                    "the other identified binary's tarball",
                    good().replacen(&step, &step.replace(&tarball, &format!("dist/{peer}-")), 1),
                    &missing,
                ),
                (
                    "another commit",
                    good().replacen(&step, &step.replace("\" \"$GITHUB_SHA\"\n", "\" abc\n"), 1),
                    &missing,
                ),
                (
                    "a comment",
                    good().replacen(&step, &step.replace("run: cargo", "run: echo cargo"), 1),
                    &missing,
                ),
                // A failure of the check swallowed by the shell: the step cannot fail.
                (
                    "a trailing || true",
                    good().replacen(
                        &step,
                        &step.replace("\"$GITHUB_SHA\"\n", "\"$GITHUB_SHA\" || true\n"),
                        1,
                    ),
                    &missing,
                ),
                (
                    "a trailing ; true",
                    good().replacen(
                        &step,
                        &step.replace("\"$GITHUB_SHA\"\n", "\"$GITHUB_SHA\"; true\n"),
                        1,
                    ),
                    &missing,
                ),
                ("before the package step", moved, &missing),
                (
                    "under if:",
                    good().replacen(
                        &step,
                        &step.replace("\n        run:", "\n        if: always()\n        run:"),
                        1,
                    ),
                    &skippable,
                ),
                (
                    "continue-on-error",
                    good().replacen(
                        &step,
                        &step.replace(
                            "\n        run:",
                            "\n        continue-on-error: true\n        run:",
                        ),
                        1,
                    ),
                    &skippable,
                ),
            ];
            for (what, planted, want) in variants {
                assert_eq!(found(&planted), vec![want.clone()], "{job}: {what}");
            }
        }
    }

    #[test]
    fn every_darwin_binary_is_signed_before_and_checked_after_packaging() {
        // Catches: a darwin binary (kbf-deploy and kbf-installer were the new ones)
        // packaged unsigned, a tarball never run through `darwin-asset`, the check of
        // another binary's tarball standing in, and either step skippable.
        for new in ["kbf-deploy", "kbf-installer"] {
            names(
                &broken(
                    &format!("cargo xtask sign-darwin \"target/$TARGET/release/{new}\""),
                    "cargo xtask sign-darwin \"target/$TARGET/release/kbf\"",
                ),
                &[&format!(
                    "packages {new} without a `cargo xtask sign-darwin`"
                )],
            );
        }
        // The darwin `kbf-daemon` tarball is gone from the release set: packaging it
        // again (a revert of the rename, half done) is named, not waved through.
        names(
            &broken(
                "      - name: Package kbf-deploy\n",
                "      - name: Package kbf-daemon\n        run: cargo xtask package --binary \"target/$TARGET/release/kbf-daemon\" --sbom s.json --platform darwin-arm64 --commit \"$GITHUB_SHA\" --out dist\n      - name: Package kbf-deploy\n",
            ),
            &["kbf-daemon for darwin-arm64 is packaged but not in the pinned release set"],
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
                "cargo xtask darwin-asset \"dist/kbf-deploy-$GITHUB_SHA-darwin-arm64.tar.gz\" kbf-deploy",
                "cargo xtask darwin-asset \"dist/kbf-$GITHUB_SHA-darwin-arm64.tar.gz\" kbf-deploy",
            ),
            &["packages kbf-deploy without a `cargo xtask darwin-asset`"],
        );
        for new in ["kbf-deploy", "kbf-installer"] {
            for step in [format!("Sign {new}"), format!("Check {new}")] {
                names(
                    &broken(
                        &format!("      - name: {step}\n"),
                        &format!("      - name: {step}\n        continue-on-error: true\n"),
                    ),
                    &[&format!("packages {new} without")],
                );
            }
            // A check that comes before the packaging does not check the tarball.
            let w = good();
            let check = format!(
                "      - name: Check {new}\n        run: cargo xtask darwin-asset \"dist/{new}-$GITHUB_SHA-darwin-arm64.tar.gz\" {new}\n"
            );
            let package = format!("      - name: Package {new}\n");
            let moved =
                w.replacen(&check, "", 1)
                    .replacen(&package, &format!("{check}{package}"), 1);
            names(
                &moved,
                &[&format!(
                    "packages {new} without a `cargo xtask darwin-asset`"
                )],
            );
        }
    }

    #[test]
    fn the_packages_are_made_assets_exactly_where_the_set_says() {
        // Catches: the deb of one architecture dropped from its job (the other keeps the
        // lint satisfied if it only counted lines), a second package line (two debs under
        // one name), and a package for a platform the set and docs/artifacts.md lack.
        for platform in PACKAGES {
            let line =
                format!("cargo xtask pkg-asset --package p.deb --platform {platform} --commit");
            let w = good().replacen(&line, &format!("echo {platform} --commit"), 1);
            assert_ne!(w, good(), "{platform}: in the fixture");
            names(
                &w,
                &[&format!(
                    "the kbf-installer package for {platform} is made an asset by 0 `cargo xtask pkg-asset` lines"
                )],
            );
        }
        let w = good().replacen(
            "--platform linux-arm64 --commit \"$GITHUB_SHA\" --binary-tarball",
            "--platform linux-riscv --commit \"$GITHUB_SHA\" --binary-tarball",
            1,
        );
        names(
            &w,
            &[
                "makes a package that is not in `PACKAGES`",
                "the kbf-installer package for linux-arm64 is made an asset by 0",
            ],
        );
    }

    #[test]
    fn the_package_trusts_the_branch_it_is_attested_for_and_no_literal_one() {
        // Catches: a literal `refs/heads/main` on `pkg-trust` or `pkg-asset` (a package
        // built on the development branch that trusts main, or the reverse: a host would
        // believe the wrong branch's releases), either flag naming another variable, and
        // the job's PKG_SOURCE_REF changed from the one expression (to `github.ref`
        // alone, which a pull request's `refs/pull/N/merge` is not a branch, or to a
        // literal).
        for to in [
            "--source-ref refs/heads/main",
            "--source-ref \"$GITHUB_REF\"",
            "--source-ref \"$PKG_SOURCE_REF_OTHER\"",
        ] {
            let w = good().replacen("--source-ref \"$PKG_SOURCE_REF\"", to, 1);
            assert_ne!(w, good(), "{to}");
            names(
                &w,
                &[
                    "without `cargo xtask pkg-trust --source-ref \"$PKG_SOURCE_REF\"` in a step before",
                ],
            );
        }
        let w = good().replacen(
            "--expect-ref \"$PKG_SOURCE_REF\"",
            "--expect-ref refs/heads/main",
            1,
        );
        names(
            &w,
            &[
                "has --expect-ref \"refs/heads/main\", not `$PKG_SOURCE_REF`",
                "names a literal branch",
            ],
        );
        for to in [
            "${{ github.ref }}",
            "refs/heads/main",
            "${{ github.event_name == 'pull_request' && 'refs/heads/main' || github.ref }}",
        ] {
            let w = in_job("linux-x86_64", PKG_SOURCE_REF_EXPR, to);
            assert_ne!(w, good(), "{to}");
            names(
                &w,
                &[&format!("the job `linux-x86_64` has {PKG_SOURCE_REF}: ")],
            );
        }
        let none = in_job(
            "linux-x86_64",
            &format!("    env:\n      {PKG_SOURCE_REF}: {PKG_SOURCE_REF_EXPR}\n"),
            "",
        );
        names(
            &none,
            &["the job `linux-x86_64` has PKG_SOURCE_REF: (none)"],
        );
    }

    #[test]
    fn the_package_is_installed_and_the_tarball_checked_before_it_is_an_asset() {
        // Catches: the install test dropped, skippable or run after `pkg-asset`; the
        // `check-asset` of the tarball whose binary the deb must hold missing or after;
        // `pkg-asset` skippable, or comparing with another commit, platform, tarball or
        // output directory; and no pinned gh given to compare the package's with.
        let install = "      - name: Install\n        run: cargo xtask pkg-install-test";
        let skippable = good().replacen(install, "      - name: Install\n        if: always()\n        run: cargo xtask pkg-install-test", 1);
        names(
            &skippable,
            &["`cargo xtask pkg-install-test --format deb` is under `if:`"],
        );
        let gone = good().replacen("cargo xtask pkg-install-test", "echo", 1);
        names(
            &gone,
            &[
                "is made an asset without `cargo xtask pkg-install-test --format deb` in a step before",
            ],
        );
        let asset_step = "      - name: Asset\n        run: cargo xtask pkg-asset";
        let run = "        run: cargo xtask pkg-install-test --package p.deb --format deb --listing target/pkg/listing.txt --image-dir packaging/linux/test/ubuntu\n";
        let asset_end = "--gh \"$RUNNER_TEMP/tools/gh\" --out dist\n";
        let moved = good()
            .replacen(&format!("{install} --package p.deb --format deb --listing target/pkg/listing.txt --image-dir packaging/linux/test/ubuntu\n"), "", 1)
            .replacen(asset_end, &format!("{asset_end}      - name: Install\n{run}"), 1);
        assert_ne!(moved, good());
        names(
            &moved,
            &[
                "is made an asset without `cargo xtask pkg-install-test --format deb` in a step before",
            ],
        );
        let conditional = good().replacen(
            asset_step,
            "      - name: Asset\n        if: always()\n        run: cargo xtask pkg-asset",
            1,
        );
        names(&conditional, &["is made an asset under `if:`"]);
        for (from, to, want) in [
            (
                "--commit \"$GITHUB_SHA\" --binary-tarball",
                "--commit abc --binary-tarball",
                "--commit \"abc\", not `$GITHUB_SHA`",
            ),
            (
                "--binary-tarball \"dist/kbf-installer-$GITHUB_SHA-linux-x86_64.tar.gz\" --expect-ref",
                "--binary-tarball \"dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz\" --expect-ref",
                "has --binary-tarball",
            ),
            (
                "--gh \"$RUNNER_TEMP/tools/gh\" --out dist",
                "--out dist",
                "is not given the pinned gh",
            ),
            (
                "--gh \"$RUNNER_TEMP/tools/gh\" --out dist",
                "--gh \"$RUNNER_TEMP/tools/gh\" --out out",
                "has --out \"out\", not `dist`",
            ),
        ] {
            let w = good().replacen(from, to, 1);
            assert_ne!(w, good(), "{from}");
            names(&w, &[want]);
        }
        // The tarball checked after the package is made an asset (the check-asset step
        // moved to the end of the job), not before.
        let check = check_asset_step("kbf-installer", "linux-x86_64");
        let late =
            good()
                .replacen(&check, "", 1)
                .replacen(asset_end, &format!("{asset_end}{check}"), 1);
        assert_ne!(late, good());
        names(
            &late,
            &[
                "is made an asset before `cargo xtask build-commit check-asset` of the tarball whose binary it holds",
            ],
        );
    }

    #[test]
    fn the_mac_package_is_installed_checked_and_attested_for_the_branch_where_the_set_says() {
        // Catches, each in the darwin job: the `.pkg` not made an asset, or made twice or
        // in another job (the signed set then lacks it, or the attest job's step for it
        // fails); no install test, a skippable one; a package whose trust directory or
        // attestation names a literal branch or the wrong variable (a main package that
        // trusts a feature branch); the job without `PKG_SOURCE_REF`; the pinned gh of
        // the trust directory undefined; and the verify job not checking the signed file.
        let install = "      - name: Install\n        run: cargo xtask pkg-macos-install-test --package target/pkg/out\n";
        let verify = verify_mac_package_step();
        // The whole asset step, the last of the darwin job's package steps.
        let steps = mac_package_steps();
        let asset_line = steps[steps.find("      - name: Asset").expect("asset step")..].to_owned();
        let cases: Vec<(&str, String, &str)> = vec![
            (
                "no asset line",
                broken(&asset_line, ""),
                "is made an asset by 0 `cargo xtask pkg-macos-asset` lines, not exactly one",
            ),
            (
                "made twice",
                broken(&asset_line, &format!("{asset_line}{asset_line}")),
                "by 2 `cargo xtask pkg-macos-asset` lines, not exactly one",
            ),
            (
                "no install test",
                broken(install, ""),
                "is made an asset without `cargo xtask pkg-macos-install-test` in a step before",
            ),
            (
                "install test skippable",
                broken(
                    install,
                    &install.replace("run:", "if: always()\n        run:"),
                ),
                "`cargo xtask pkg-macos-install-test` is under `if:`",
            ),
            (
                "install test after the asset",
                broken(install, "").replacen(&asset_line, &format!("{asset_line}{install}"), 1),
                "is made an asset without `cargo xtask pkg-macos-install-test` in a step before",
            ),
            (
                "a literal ref in the trust directory",
                broken(
                    "--source-ref $PKG_SOURCE_REF --out target/pkg-trust\n      - name: Build",
                    "--source-ref refs/heads/main --out target/pkg-trust\n      - name: Build",
                ),
                "names a literal branch",
            ),
            (
                "the asset attested for the pushed ref",
                broken(
                    "--expect-ref $PKG_SOURCE_REF --gh gh-darwin --out dist",
                    "--expect-ref $GITHUB_REF --gh gh-darwin --out dist",
                ),
                "has --expect-ref \"$GITHUB_REF\", not `$PKG_SOURCE_REF`",
            ),
            (
                "another tarball",
                broken(
                    "--binary-tarball dist/kbf-installer-$GITHUB_SHA-darwin-arm64.tar.gz --expect-ref $PKG_SOURCE_REF",
                    "--binary-tarball dist/kbf-$GITHUB_SHA-darwin-arm64.tar.gz --expect-ref $PKG_SOURCE_REF",
                ),
                "has --binary-tarball",
            ),
            (
                "no pinned gh to compare",
                broken(" --gh gh-darwin --out dist", " --out dist"),
                "is not given the pinned gh",
            ),
            (
                "no PKG_SOURCE_REF",
                broken(
                    &format!(
                        "    env:\n      {PKG_SOURCE_REF}: {PKG_SOURCE_REF_EXPR}\n    steps:\n{CHECKOUT}      - name: Sign kbf\n"
                    ),
                    &format!("    steps:\n{CHECKOUT}      - name: Sign kbf\n"),
                ),
                "the job `darwin-arm64` has PKG_SOURCE_REF: (none)",
            ),
            (
                "the pinned gh undefined",
                broken(
                    "  GH_SHA256_AARCH64_DARWIN: abababababababababababababababababababababababababababababababab\n",
                    "",
                ),
                "pins its download with --sha256",
            ),
            (
                "the verify job not checking the package",
                broken(&verify, ""),
                "`verify-darwin-arm64` does not run `cargo xtask pkg-macos-asset --verify-only`",
            ),
            (
                "the verify job checking for the wrong ref",
                broken(
                    &verify,
                    &verify.replace("--expect-ref $GITHUB_REF", "--expect-ref refs/heads/main"),
                ),
                "`verify-darwin-arm64` does not run `cargo xtask pkg-macos-asset --verify-only`",
            ),
        ];
        for (_, w, wanted) in cases {
            names(&w, &[wanted]);
        }
    }

    #[test]
    fn a_fetched_tool_is_pinned_by_a_workflow_variable_that_exists() {
        // Catches: `--sha256 "$$NAME"` (the shell's process id, then NAME), a pin that is
        // a variable nobody defines, a literal digest, a lower-case name, a digest of the
        // wrong length, and no pin at all, in the Linux jobs that fetch gh and nfpm for
        // the package.
        let hex = "ab".repeat(32);
        let pins = |w: &str| {
            let mut out = Vec::new();
            for job in PACKAGES.iter().chain(&[MAC_PACKAGE]) {
                let steps = workflows::job_steps(w, job).expect("job");
                fetch_pins(w, job, &steps, &mut out);
            }
            out
        };
        let base = good().replacen("env:\n", &format!("env:\n  GH_SHA: {hex}\n"), 1);
        let fetch = "      - name: Fetch\n        run: cargo xtask fetch --url u --sha256 \"$GH_SHA\" --out o\n";
        let with =
            |w: &str, line: &str| w.replacen("    steps:\n", &format!("    steps:\n{line}"), 3);
        let good_pins = pins(&with(&base, fetch));
        assert!(good_pins.is_empty(), "{good_pins:?}");
        for bad in [
            fetch.replace("\"$GH_SHA\"", "\"$$GH_SHA\""),
            fetch.replace("$GH_SHA", "$OTHER_SHA"),
            fetch.replace("$GH_SHA", &hex),
            fetch.replace(" --sha256 \"$GH_SHA\"", ""),
            fetch.replace("$GH_SHA", "$gh_sha"),
        ] {
            let got = pins(&with(&base, &bad));
            assert!(
                got.iter()
                    .any(|p| p.contains("pins its download with --sha256")),
                "{bad}: {got:?}"
            );
        }
        let short = base.replace(&hex, &hex[..63]);
        assert!(
            !pins(&with(&short, fetch)).is_empty(),
            "a 63-digit pin is not a pin"
        );
    }

    #[test]
    fn the_verify_job_checks_the_signed_package_for_the_branch_it_verifies() {
        // Catches: the package checked only before signing (verify-linux not checking the
        // file it downloads), the check against a literal branch or another variable, one
        // architecture left out, the check before the attestation check, and skippable.
        for platform in PACKAGES {
            let w = good().replacen(&verify_package_step(platform), "", 1);
            assert_ne!(w, good(), "{platform}");
            names(
                &w,
                &[&format!(
                    "`verify-linux` does not run `cargo xtask pkg-asset --verify-only` on `dist/kbf-installer-$GITHUB_SHA-{platform}.deb`"
                )],
            );
        }
        let step = verify_package_step("linux-arm64");
        for to in [
            step.replace(
                "--expect-ref \"$GITHUB_REF\"",
                "--expect-ref refs/heads/main",
            ),
            step.replace(
                "--expect-ref \"$GITHUB_REF\"",
                "--expect-ref \"$PKG_SOURCE_REF\"",
            ),
            step.replace("      - name", "      - if: always()\n        name"),
        ] {
            let w = good().replacen(&step, &to, 1);
            assert_ne!(w, good());
            names(
                &w,
                &[
                    "`verify-linux` does not run `cargo xtask pkg-asset --verify-only` on `dist/kbf-installer-$GITHUB_SHA-linux-arm64.deb`",
                ],
            );
        }
        let early = good().replacen(&step, "", 1);
        let (head, tail) = early.split_once("  verify-linux:\n").expect("verify-linux");
        let tail = tail.replacen("    steps:\n", &format!("    steps:\n{step}"), 1);
        names(
            &format!("{head}  verify-linux:\n{tail}"),
            &[
                "`verify-linux` does not run `cargo xtask pkg-asset --verify-only` on `dist/kbf-installer-$GITHUB_SHA-linux-arm64.deb`",
            ],
        );
    }

    mod images_tests;
    mod kbf_package_tests;
}

mod facts;
mod images;
#[cfg(test)]
mod installer;
mod kbf_package;
