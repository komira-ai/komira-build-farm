//! Every tarball `.github/workflows/artifacts.yml` packages is attested as its own
//! subject, and the bundle of that attestation reaches the release.
//!
//! A host verifies a tarball offline with one file, `<tarball>.sigstore.json`, the
//! Sigstore bundle of a build provenance attestation whose only subject is that tarball
//! (docs/artifacts.md). Nothing about it runs on a pull request: the attest jobs sign
//! only on a push to `main`, so a tarball added to a build job
//! without its steps would pass every pull request check and turn the branch red, or
//! worse, release without a bundle. [`gaps`] reads the workflows and names the tarball:
//!
//! - **Its own subject.** The attest job of the tarball's platform group has an
//!   unconditional `actions/attest-build-provenance` step with an `id` and
//!   `subject-path` exactly `dist/<tarball>` (not a glob or a list, which would make
//!   one bundle for several tarballs). A step for a tarball no job of that group
//!   packages is named too.
//! - **The packages.** The `kbf-installer` deb that `cargo xtask pkg-asset` lists as an
//!   asset ([`attested_assets::packaged_debs`]), the `kbf` deb that `cargo xtask
//!   pkg-kbf-asset` lists ([`attested_assets::packaged_kbf_debs`]) and the Mac `.pkg` that
//!   `cargo xtask pkg-macos-asset` lists ([`attested_assets::packaged_macos_pkgs`]) are
//!   held to the same two rules, as `dist/kbf-installer-<commit>-<platform>.deb` (`.pkg`),
//!   `dist/kbf-<commit>-<platform>.deb` and their `.deb.sigstore.json` (`.pkg.sigstore.json`).
//!   So are the frontend and coordinator images that `cargo xtask pkg-image-asset` lists
//!   ([`attested_assets::packaged_images`]), as `dist/kbf-<service>-image-<commit>-<platform>.oci.tar`
//!   and its `.oci.tar.sigstore.json`.
//! - **Its bundle's name.** The step after it sets `BUNDLE` to that step's
//!   `bundle-path` output and copies it to `dist/<tarball>.sigstore.json` (the action
//!   leaves it in a temporary directory under a name that is the same for every
//!   attestation). That `cp` is the only `run:` step the attest job may have: it holds
//!   `id-token: write` and signs, and any other shell step is named.
//! - **Out of the job.** The job uploads `dist/*.sigstore.json` as the artifact
//!   `bundles-<group>`, in a step that cannot be skipped and fails on no file.
//! - **Into the verification and the release.** The group's verify job downloads
//!   that artifact into `dist`; `release.yml`'s verify job downloads `bundles-*` into
//!   `dist`, fetches the trusted root and verifies offline for the release's commit
//!   (`--commit "$COMMIT"`, `--trusted-root`); and `cargo xtask release locate` lists
//!   each `bundles-*` artifact as one a run must still hold
//!   ([`xtask::release::ARTIFACTS`]), and lists no other.

use std::collections::BTreeSet;

use super::{attested_assets, release_artifacts, workflows};

const PROVENANCE: &str = "actions/attest-build-provenance@";
const UPLOAD: &str = "actions/upload-artifact@";
const DOWNLOAD: &str = "actions/download-artifact@";

/// What the bundle step of the job copies from: the `bundle-path` output of the
/// provenance step `id`.
fn bundle_path(id: &str) -> String {
    format!("${{{{ steps.{id}.outputs.bundle-path }}}}")
}

/// The platform group that attests, verifies and uploads the bundles of `platform`:
/// both Linux architectures share one, each other platform has its own.
fn group(platform: &str) -> &str {
    if platform.starts_with("linux-") {
        "linux"
    } else {
        platform
    }
}

/// The value of the input `key` of `step`.
fn input<'a>(step: &'a workflows::Step, key: &str) -> Option<&'a str> {
    step.with
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Whether `step` is `uses: <prefix>...`.
fn uses(step: &workflows::Step, prefix: &str) -> bool {
    step.uses.as_deref().is_some_and(|u| u.starts_with(prefix))
}

/// The words (double quotes dropped) after `prefix` on the first line of a step of
/// `steps` that starts with it, with the index of its step.
fn command<'a>(steps: &'a [workflows::Step], prefix: &str) -> Option<(usize, Vec<&'a str>)> {
    steps.iter().enumerate().find_map(|(at, s)| {
        s.run
            .as_deref()?
            .lines()
            .map(str::trim_start)
            .find_map(|l| {
                l.strip_prefix(prefix).map(|rest| {
                    let words = rest.split_whitespace().map(|w| w.trim_matches('"'));
                    (at, words.collect())
                })
            })
    })
}

/// What differs between the tarballs the workflow `artifacts` packages and the
/// bundles its jobs and `release` (release.yml) carry to the release, given the
/// artifacts `listed` that `release locate` requires: one line per difference, empty
/// when there is none. An error when the workflow packages nothing (the lint would
/// pass vacuously) or cannot be read.
pub fn gaps(artifacts: &str, release: &str, listed: &[&str]) -> Result<Vec<String>, String> {
    let packaged = attested_assets::packaged(artifacts)?;
    if packaged.is_empty() {
        return Err("no `cargo xtask package` line in any `run:` step".to_owned());
    }
    // The packages: (binary, platform, extension), the installer's deb and the `kbf` deb of a
    // Linux job and the installer's `.pkg` of the macOS one.
    let mut packages: Vec<(&str, String, &str)> = Vec::new();
    for platform in attested_assets::packaged_debs(artifacts)? {
        packages.push(("kbf-installer", platform, "deb"));
    }
    for platform in attested_assets::packaged_kbf_debs(artifacts)? {
        packages.push(("kbf", platform, "deb"));
    }
    for platform in attested_assets::packaged_macos_pkgs(artifacts)? {
        packages.push(("kbf-installer", platform, "pkg"));
    }
    // The images are held to the same rules, as `dist/kbf-<service>-image-<commit>-<platform>.oci.tar`.
    for (service, platform) in attested_assets::packaged_images(artifacts)? {
        let name = match service.as_str() {
            "frontend" => "kbf-frontend-image",
            "coordinator" => "kbf-coordinator-image",
            other => {
                return Err(format!(
                    "`pkg-image-asset --service {other}`: not an image of the release set"
                ));
            }
        };
        packages.push((name, platform, "oci.tar"));
    }
    let groups: BTreeSet<&str> = packaged
        .iter()
        .map(|(_, p)| group(p))
        .chain(packages.iter().map(|(_, p, _)| group(p)))
        .collect();
    let mut out = Vec::new();
    for g in &groups {
        let attest_job = format!("attest-{g}");
        if !release_artifacts::ATTEST_JOBS.contains(&attest_job.as_str()) {
            out.push(format!(
                "a tarball is packaged for the group `{g}`, which has no attest job `{attest_job}`"
            ));
            continue;
        }
        let mine: Vec<(&str, &str)> = packaged
            .iter()
            .filter(|(_, p)| group(p) == *g)
            .map(|(n, p)| (n.as_str(), p.as_str()))
            .collect();
        let my_packages: Vec<(&str, &str, &str)> = packages
            .iter()
            .filter(|(_, p, _)| group(p) == *g)
            .map(|(name, p, ext)| (*name, p.as_str(), *ext))
            .collect();
        let steps = workflows::job_steps(artifacts, &attest_job)?;
        attested_alone(&attest_job, &steps, &mine, &my_packages, &mut out);
        uploaded(&attest_job, g, &steps, &mut out);
        let verify_job = format!("verify-{g}");
        let verify = workflows::job_steps(artifacts, &verify_job)?;
        let artifact = format!("bundles-{g}");
        let downloads = verify.iter().any(|s| {
            uses(s, DOWNLOAD)
                && input(s, "name") == Some(&artifact)
                && input(s, "path") == Some("dist")
        });
        if !downloads {
            out.push(format!(
                "`{verify_job}` does not download `{artifact}` into `dist`, so it cannot verify the bundles"
            ));
        }
        if !listed.contains(&artifact.as_str()) {
            out.push(format!(
                "`xtask::release::ARTIFACTS` does not list `{artifact}`: `release locate` would take a run that lost its bundles"
            ));
        }
    }
    for artifact in listed.iter().filter(|a| a.starts_with("bundles-")) {
        let named = artifact.strip_prefix("bundles-").unwrap_or_default();
        if !groups.contains(named) {
            out.push(format!(
                "`xtask::release::ARTIFACTS` lists `{artifact}`, which no attest job uploads: `release locate` would refuse every run"
            ));
        }
    }
    released(release, &mut out)?;
    Ok(out)
}

/// The provenance and bundle-naming steps of every tarball of `mine` (name and platform)
/// and every package of `packages` (binary, platform, extension) in the attest job `job`, the
/// steps that attest anything else, and any `run:` step that is not the naming of a bundle.
fn attested_alone(
    job: &str,
    steps: &[workflows::Step],
    mine: &[(&str, &str)],
    packages: &[(&str, &str, &str)],
    out: &mut Vec<String>,
) {
    let mut expected = Vec::new();
    // (what it is, the file it attests, the file its bundle is copied to)
    let mut targets: Vec<(String, String, String)> = Vec::new();
    for (name, platform) in mine {
        targets.push((
            format!("{name} for {platform}"),
            format!("dist/{}.tar.gz", attested_assets::stem(name, platform)),
            format!("dist/{name}-${{GITHUB_SHA}}-{platform}.tar.gz.sigstore.json"),
        ));
    }
    for (name, platform, ext) in packages {
        targets.push((
            format!("{name} package ({ext}) for {platform}"),
            format!("dist/{}.{ext}", attested_assets::stem(name, platform)),
            format!("dist/{name}-${{GITHUB_SHA}}-{platform}.{ext}.sigstore.json"),
        ));
    }
    for (what, subject, bundle) in targets {
        expected.push(subject.clone());
        let found = steps
            .iter()
            .enumerate()
            .find(|(_, s)| uses(s, PROVENANCE) && input(s, "subject-path") == Some(&subject));
        let Some((at, step)) = found else {
            out.push(format!(
                "{what} is packaged, but `{job}` has no actions/attest-build-provenance step with subject-path {subject}: it is not attested as its own subject, so it has no bundle of its own"
            ));
            continue;
        };
        if step.conditional {
            out.push(format!(
                "the provenance step for {what} in `{job}` is under `if:` or `continue-on-error:`"
            ));
        }
        let Some(id) = step.id.as_deref() else {
            out.push(format!(
                "the provenance step for {what} in `{job}` has no `id:`, so its `bundle-path` cannot be named"
            ));
            continue;
        };
        let want_run = format!("cp \"$BUNDLE\" \"{bundle}\"");
        let want_env = bundle_path(id);
        let named = steps.get(at + 1).is_some_and(|s| {
            s.run.as_deref().map(str::trim) == Some(want_run.as_str())
                && s.env.iter().any(|(k, v)| k == "BUNDLE" && *v == want_env)
                && !s.conditional
        });
        if !named {
            out.push(format!(
                "the step after the provenance step for {what} in `{job}` does not name its bundle: it is to be an unconditional step with `env: BUNDLE: {want_env}` and `run: {want_run}`"
            ));
        }
    }
    for step in steps.iter().filter(|s| uses(s, PROVENANCE)) {
        let Some(subject) = input(step, "subject-path") else {
            // The one over `dist/SHA256SUMS`: release_artifacts holds it.
            continue;
        };
        if !expected.iter().any(|e| e == subject) {
            out.push(format!(
                "`{job}` attests {subject} with a provenance step of its own, a tarball no job of its platform group packages"
            ));
        }
    }
    // The job holds `id-token: write` and signs, and nothing else runs in it: the only
    // `run:` steps are the ones right after a provenance step (judged above to be the
    // `cp` that names its bundle, or named there). Any other is a shell in the signing job.
    let after_provenance: BTreeSet<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| uses(s, PROVENANCE) && input(s, "subject-path").is_some())
        .map(|(at, _)| at + 1)
        .collect();
    for (at, step) in steps.iter().enumerate() {
        let Some(run) = step.run.as_deref() else {
            continue;
        };
        if !after_provenance.contains(&at) {
            let first: String = run
                .trim_start()
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect();
            out.push(format!(
                "`{job}` has a `run:` step (`{first}`) that is not the naming of a bundle right after a provenance step: the job holds `id-token: write` and signs, so nothing else runs in it"
            ));
        }
    }
}

/// The step of the attest job `job` (group `g`) that uploads the bundles.
fn uploaded(job: &str, g: &str, steps: &[workflows::Step], out: &mut Vec<String>) {
    let artifact = format!("bundles-{g}");
    let upload = steps
        .iter()
        .enumerate()
        .find(|(_, s)| uses(s, UPLOAD) && input(s, "name") == Some(&artifact));
    let Some((at, step)) = upload else {
        out.push(format!(
            "`{job}` has no actions/upload-artifact step for `{artifact}`: the bundles never leave the job"
        ));
        return;
    };
    if input(step, "path") != Some("dist/*.sigstore.json") {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` has path {:?}, not `dist/*.sigstore.json`",
            input(step, "path").unwrap_or_default()
        ));
    }
    if input(step, "if-no-files-found") != Some("error") {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` does not fail on no file (`if-no-files-found: error`)"
        ));
    }
    if step.conditional {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` is under `if:` or `continue-on-error:`"
        ));
    }
    let last_provenance = steps
        .iter()
        .rposition(|s| uses(s, PROVENANCE) && input(s, "subject-path").is_some());
    if last_provenance.is_some_and(|last| at <= last + 1) {
        out.push(format!(
            "the upload of `{artifact}` in `{job}` comes before the last bundle is named"
        ));
    }
}

/// What release.yml's verify job must do with the bundles.
fn released(release: &str, out: &mut Vec<String>) -> Result<(), String> {
    let steps = workflows::job_steps(release, "verify")?;
    let downloads = steps.iter().any(|s| {
        uses(s, DOWNLOAD)
            && input(s, "pattern") == Some("bundles-*")
            && input(s, "path") == Some("dist")
            && input(s, "merge-multiple") == Some("true")
    });
    if !downloads {
        out.push(
            "`release.yml`'s `verify` job does not download `bundles-*` into `dist` (merge-multiple), so a release would carry no bundle".to_owned(),
        );
    }
    if command(&steps, "cargo xtask verify-assets attestations ").is_none() {
        out.push(
            "`release.yml`'s `verify` job does not run `cargo xtask verify-assets attestations`"
                .to_owned(),
        );
    } else {
        offline_verification("`release.yml`'s `verify` job", &steps, "$COMMIT", out);
    }
    Ok(())
}

/// What the steps of a job must do to verify the bundles offline for the commit held by
/// the shell variable `commit_var`: fetch the trusted root into a file
/// (`gh attestation trusted-root > <file>`), then run `cargo xtask verify-assets
/// attestations` with `--commit "<commit_var>"` and `--trusted-root <file>`. `who` names
/// the job in the problems. A job that does not run the verification at all is for the
/// caller to report.
pub fn offline_verification(
    who: &str,
    steps: &[workflows::Step],
    commit_var: &str,
    out: &mut Vec<String>,
) {
    let Some((verify_at, words)) = command(steps, "cargo xtask verify-assets attestations ") else {
        return;
    };
    let value = |flag: &str| {
        let at = words.iter().position(|w| *w == flag)?;
        words.get(at + 1).copied()
    };
    if value("--commit") != Some(commit_var) {
        out.push(format!(
            "{who} verifies attestations without `--commit \"{commit_var}\"`, so the bundles are not held to the commit they were built from"
        ));
    }
    let root = command(steps, "gh attestation trusted-root > ")
        .and_then(|(at, w)| Some((at, *w.first()?)));
    let Some((root_at, root_path)) = root else {
        out.push(format!(
            "{who} does not fetch the trusted root (`gh attestation trusted-root > <file>`)"
        ));
        return;
    };
    if value("--trusted-root") != Some(root_path) || verify_at <= root_at {
        out.push(format!(
            "{who} does not verify offline: `verify-assets attestations` is to follow the trusted-root step and take `--trusted-root {root_path}`"
        ));
    }
}

#[test]
fn every_packaged_tarball_is_attested_alone_and_its_bundle_reaches_the_release() {
    // Catches: a tarball added to a build job (or a platform) with no provenance step of
    // its own, so it has no bundle and a host cannot verify it offline, which no pull
    // request run notices (attest and verify run only on a push) and a release would
    // ship without; a bundle step, upload, download or `release locate` entry dropped on
    // the way from the attest job to the release.
    let root = super::repo_root();
    let path = super::ARTIFACTS_WORKFLOW;
    let artifacts = std::fs::read_to_string(root.join(path)).expect(path);
    let release_path = ".github/workflows/release.yml";
    let release = std::fs::read_to_string(root.join(release_path)).expect(release_path);
    match gaps(&artifacts, &release, &xtask::release::ARTIFACTS) {
        Ok(wrong) => assert!(wrong.is_empty(), "{path}:\n{}", wrong.join("\n")),
        Err(e) => panic!("{path}: {e}"),
    }
}

#[cfg(test)]
mod rules;
