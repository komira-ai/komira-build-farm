//! Every asset `.github/workflows/artifacts.yml` packages gets an SBOM attestation.
//!
//! The attest job names each tarball and its SBOM by hand, one `actions/attest-sbom`
//! step per tarball, and runs only on a push to `main`; the verify job after it refuses
//! a tarball with no SBOM attestation (`cargo xtask verify-assets attestations`). So a
//! binary added to a build job's `Package` steps without its attest step passes every
//! pull request check and turns `main` red. [`unattested`] reads both lists from the
//! workflow and names the difference, on the pull request.
//!
//! It reads the `cargo xtask package` lines of every `run:` step (the binary's file
//! name and `--platform`), and the `subject-path` and `sbom-path` of every
//! `actions/attest-sbom` step of the attest job. Build provenance needs no list: its
//! one step covers every file in `SHA256SUMS`.

use super::workflows;

/// What `cargo xtask package` names an asset, `<binary>-<commit>-<platform>`, with the
/// commit as the attest job spells it.
fn stem(name: &str, platform: &str) -> String {
    format!("{name}-${{{{ github.sha }}}}-{platform}")
}

/// The (binary name, platform) of every `cargo xtask package` line in the workflow's
/// `run:` steps, in file order. An error names a package line it cannot read.
pub fn packaged(workflow: &str) -> Result<Vec<(String, String)>, String> {
    let mut found = Vec::new();
    for step in workflows::run_steps(workflow)? {
        for line in step.script.lines().map(str::trim_start) {
            let Some(args) = line.strip_prefix("cargo xtask package ") else {
                continue;
            };
            let words: Vec<&str> = args.split_whitespace().collect();
            let flag = |f: &str| {
                let at = words.iter().position(|w| *w == f)?;
                words.get(at + 1).map(|v| v.trim_matches('"'))
            };
            let (Some(binary), Some(platform)) = (flag("--binary"), flag("--platform")) else {
                return Err(format!(
                    "{}/{}: a package line without --binary and --platform: {line}",
                    step.job, step.step
                ));
            };
            let name = binary.rsplit_once('/').map_or(binary, |(_, n)| n);
            found.push((name.to_owned(), platform.to_owned()));
        }
    }
    Ok(found)
}

/// The (subject-path, sbom-path) of every `actions/attest-sbom` step of `job`. An error
/// for such a step that lacks either path, or that is under `if:` or
/// `continue-on-error:` (it could be skipped while the job passes).
fn sbom_attested(workflow: &str, job: &str) -> Result<Vec<(String, String)>, String> {
    let mut found = Vec::new();
    for step in workflows::job_steps(workflow, job)? {
        let attests = step
            .uses
            .as_deref()
            .is_some_and(|u| u.starts_with("actions/attest-sbom@"));
        if !attests {
            continue;
        }
        let input = |k: &str| {
            step.with
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        let (Some(subject), Some(sbom)) = (input("subject-path"), input("sbom-path")) else {
            return Err(format!(
                "an attest-sbom step of `{job}` lacks subject-path or sbom-path"
            ));
        };
        if step.conditional {
            return Err(format!(
                "the attest-sbom step for {subject} is under `if:` or `continue-on-error:`"
            ));
        }
        found.push((subject, sbom));
    }
    Ok(found)
}

/// What differs between the assets the workflow packages and the SBOM attestations its
/// job `attest_job` signs: one line per packaged tarball with no step attesting it with
/// its own SBOM, and one per step naming a pair no job packages. An error when the
/// workflow packages nothing (the lint would pass vacuously) or a step cannot be read.
pub fn unattested(workflow: &str, attest_job: &str) -> Result<Vec<String>, String> {
    let packaged = packaged(workflow)?;
    if packaged.is_empty() {
        return Err("no `cargo xtask package` line in any `run:` step".to_owned());
    }
    let attested = sbom_attested(workflow, attest_job)?;
    let mut wrong = Vec::new();
    let mut expected = Vec::new();
    for (name, platform) in &packaged {
        let stem = stem(name, platform);
        let pair = (
            format!("dist/{stem}.tar.gz"),
            format!("dist/{stem}.cdx.json"),
        );
        if !attested.contains(&pair) {
            wrong.push(format!(
                "{name} for {platform} is packaged, but `{attest_job}` has no attest-sbom step with subject-path {} and sbom-path {}",
                pair.0, pair.1
            ));
        }
        expected.push(pair);
    }
    for (subject, sbom) in &attested {
        if !expected.contains(&(subject.clone(), sbom.clone())) {
            wrong.push(format!(
                "`{attest_job}` attests {subject} with {sbom}, a pair no job packages"
            ));
        }
    }
    Ok(wrong)
}

#[test]
fn every_packaged_asset_has_an_sbom_attestation() {
    // Catches: a binary added to a build job's Package steps (or a new platform) with no
    // attest-sbom step for it, which no pull request run notices (attest and verify run
    // only on main) and which fails the verify job on main; an attest step left behind
    // for an asset no longer built.
    let path = super::ARTIFACTS_WORKFLOW;
    let text = std::fs::read_to_string(super::repo_root().join(path)).expect(path);
    match unattested(&text, "attest") {
        Ok(wrong) => assert!(wrong.is_empty(), "{path}:\n{}", wrong.join("\n")),
        Err(e) => panic!("{path}: {e}"),
    }
}

/// The rules [`unattested`] reads the workflow by.
mod rules {
    use super::*;

    const PIN: &str = "c604332985a26aa8cf1bdc465b92731239ec6b9e # v4.1.0";

    /// A build job packaging each (binary, platform), then an attest job with `attest`
    /// as its steps.
    fn workflow(packages: &[(&str, &str)], attest: &str) -> String {
        let mut w = String::from(
            "on: push\npermissions: {}\njobs:\n  build:\n    runs-on: ubuntu-24.04\n    steps:\n",
        );
        for (name, platform) in packages {
            w.push_str(&format!(
                "      - name: Package {name}\n        run: cargo xtask package --binary \"target/$TARGET/release/{name}\" --sbom crates/{name}/{name}_bin.cdx.json --platform {platform} --commit \"$GITHUB_SHA\" --out dist\n"
            ));
        }
        w.push_str("  attest:\n    runs-on: ubuntu-24.04\n    steps:\n");
        w.push_str(attest);
        w
    }

    /// An attest-sbom step for the tarball of `subject` and the SBOM of `sbom`, each a
    /// (binary, platform).
    fn step(subject: (&str, &str), sbom: (&str, &str), extra: &str) -> String {
        format!(
            "      - uses: actions/attest-sbom@{PIN}\n{extra}        with:\n          subject-path: dist/{}.tar.gz\n          sbom-path: dist/{}.cdx.json\n",
            stem(subject.0, subject.1),
            stem(sbom.0, sbom.1)
        )
    }

    const BUILT: [(&str, &str); 2] = [("kbf-a", "linux-x86_64"), ("kbf-b", "linux-arm64")];

    fn both() -> String {
        format!(
            "{}{}",
            step(BUILT[0], BUILT[0], ""),
            step(BUILT[1], BUILT[1], "")
        )
    }

    #[test]
    fn a_workflow_attesting_every_packaged_asset_passes() {
        // Catches: the lint misreading the package line (quoted path, directory part)
        // or the step's inputs, so a correct workflow is refused.
        assert_eq!(unattested(&workflow(&BUILT, &both()), "attest"), Ok(vec![]));
    }

    #[test]
    fn a_packaged_asset_without_its_attest_step_is_named() {
        // Catches: the defect the lint exists for, a second binary packaged with no
        // attest-sbom step (as kbf-deploy would be without its own).
        let wrong = unattested(&workflow(&BUILT, &step(BUILT[0], BUILT[0], "")), "attest")
            .expect("readable");
        assert_eq!(wrong.len(), 1, "{wrong:?}");
        assert!(
            wrong[0].starts_with("kbf-b for linux-arm64 is packaged"),
            "{wrong:?}"
        );
    }

    #[test]
    fn an_attest_step_pairing_the_wrong_sbom_is_named() {
        // Catches: a step copied for a new asset with only one of its two paths edited,
        // which signs one tarball's attestation over another's SBOM.
        let attest = format!(
            "{}{}",
            step(BUILT[0], BUILT[0], ""),
            step(BUILT[1], BUILT[0], "")
        );
        let wrong = unattested(&workflow(&BUILT, &attest), "attest").expect("readable");
        assert_eq!(wrong.len(), 2, "{wrong:?}");
        assert!(wrong[0].starts_with("kbf-b for linux-arm64"), "{wrong:?}");
        assert!(wrong[1].ends_with("a pair no job packages"), "{wrong:?}");
    }

    #[test]
    fn an_attest_step_for_an_asset_no_job_packages_is_named() {
        // Catches: a binary dropped from the build with its attest step left behind,
        // which fails the attest job on main (the file does not exist).
        let wrong = unattested(&workflow(&BUILT[..1], &both()), "attest").expect("readable");
        assert_eq!(wrong.len(), 1, "{wrong:?}");
        assert!(wrong[0].contains("kbf-b-"), "{wrong:?}");
    }

    #[test]
    fn a_skippable_attest_step_is_refused() {
        // Catches: an attest step under `if:` or `continue-on-error:` counted, though
        // it may not run.
        for extra in ["        if: false\n", "        continue-on-error: true\n"] {
            let attest = format!(
                "{}{}",
                step(BUILT[0], BUILT[0], ""),
                step(BUILT[1], BUILT[1], extra)
            );
            let got = unattested(&workflow(&BUILT, &attest), "attest");
            assert!(
                got.as_ref().is_err_and(|e| e.contains("is under `if:`")),
                "{extra}: {got:?}"
            );
        }
    }

    #[test]
    fn a_workflow_packaging_nothing_is_refused() {
        // Catches: the lint passing vacuously when the package lines move or change
        // their spelling, so it reads none.
        let got = unattested(&workflow(&[], &both()), "attest");
        assert!(got.is_err(), "{got:?}");
        let commented = workflow(&BUILT, &both()).replace("cargo xtask package", "# cargo");
        assert!(unattested(&commented, "attest").is_err());
    }

    #[test]
    fn a_package_line_without_its_flags_is_refused() {
        // Catches: a package line whose binary or platform the lint cannot read skipped
        // silently, so its asset is never compared.
        let w = workflow(&BUILT, &both()).replace("--platform linux-arm64", "linux-arm64");
        let got = unattested(&w, "attest");
        assert!(
            got.as_ref()
                .is_err_and(|e| e.contains("without --binary and --platform")),
            "{got:?}"
        );
    }
}
