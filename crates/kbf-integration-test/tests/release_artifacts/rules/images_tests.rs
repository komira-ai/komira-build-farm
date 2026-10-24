//! The rules about the frontend image, read on the workflow of `rules` broken one way at a time.

use super::*;

/// Asserts that the lint names a Linux job that does not make its frontend image an asset, one
/// that makes it twice, and an image for a platform the set does not list.
#[test]
fn the_frontend_image_is_made_an_asset_exactly_where_the_set_says() {
    for platform in PACKAGES {
        let step = image_steps(platform);
        let asset = step
            .split_once("      - name: Image asset\n")
            .expect("asset")
            .1;
        let asset = format!("      - name: Image asset\n{asset}");
        let none = good().replacen(&asset, "", 1);
        assert_ne!(none, good(), "{platform}: in the fixture");
        let wanted = format!(
            "the frontend image for {platform} is made an asset by 0 `cargo xtask pkg-image-asset` lines, not exactly one"
        );
        assert!(found(&none).contains(&wanted), "{:?}", found(&none));
        let twice = good().replacen(&asset, &format!("{asset}{asset}"), 1);
        let wanted = wanted.replace("by 0", "by 2");
        assert!(found(&twice).contains(&wanted), "{:?}", found(&twice));
    }
    let asset = image_steps("linux-arm64");
    let riscv = asset.replace("--platform linux-arm64", "--platform linux-riscv");
    let got = found(&good().replacen(&asset, &riscv, 1));
    assert!(
        got.iter().any(|l| l.starts_with(
            "`cargo xtask pkg-image-asset --platform linux-riscv` makes an image that is not in `PACKAGES`"
        )),
        "{got:?}"
    );
}

/// Asserts that the lint names an image made an asset with another commit, tarball, image or
/// output directory than the job's own or in a step that can be skipped, and one made without
/// the context from the kbf tarball, either build, the comparison of the builds, or reproducible
/// build flags, or before the tarball's identity check.
#[test]
fn the_frontend_image_is_built_twice_from_the_attested_tarball_and_compared() {
    let steps = image_steps("linux-x86_64");
    let line = |from: &str, to: &str, wanted: &str| {
        let w = good().replacen(&steps, &steps.replace(from, to), 1);
        assert_ne!(w, good(), "{from}");
        let got = found(&w);
        assert!(got.iter().any(|l| l.contains(wanted)), "{from}: {got:?}");
    };
    line(
        "--commit \"$GITHUB_SHA\"",
        "--commit abc",
        "has --commit \"abc\", not `$GITHUB_SHA`",
    );
    line(
        "--kbf-tarball \"dist/kbf-$GITHUB_SHA-",
        "--kbf-tarball \"dist/kbf-installer-$GITHUB_SHA-",
        "has --kbf-tarball \"dist/kbf-installer-$GITHUB_SHA-linux-x86_64.tar.gz\", not `dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz`",
    );
    line(
        "--image target/image-out/1.oci.tar",
        "--image target/image-out/2.oci.tar",
        "has --image \"target/image-out/2.oci.tar\", not `target/image-out/1.oci.tar`",
    );
    line(
        "--out dist",
        "--out elsewhere",
        "has --out \"elsewhere\", not `dist`",
    );
    line(
        "      - name: Image asset\n        run:",
        "      - name: Image asset\n        if: always()\n        run:",
        "the frontend image of `linux-x86_64` is made an asset under `if:` or `continue-on-error:`",
    );
    let missing = |name: &str, what: &str| {
        let start = steps
            .find(&format!("      - name: {name}\n"))
            .expect("step");
        let end = start
            + 1
            + steps[start + 1..]
                .find("      - name:")
                .unwrap_or(steps.len() - start - 1);
        let w = good().replacen(&steps, &format!("{}{}", &steps[..start], &steps[end..]), 1);
        let wanted = format!("is made an asset without {what} in a step before");
        let got = found(&w);
        assert!(got.iter().any(|l| l.contains(&wanted)), "{name}: {got:?}");
    };
    missing(
        "Context",
        "the context taken out of this job's kbf tarball (`tar -xzf` into target/image)",
    );
    missing(
        "Epoch",
        "the commit's time in the environment (`cargo xtask source-date-epoch`)",
    );
    missing("Build 1", "a `docker buildx build` into 1.oci.tar");
    missing("Build 2", "a `docker buildx build` into 2.oci.tar");
    missing("Same", "`cmp` of the two builds");
    for flag in [
        "--provenance=false",
        "--sbom=false",
        "--build-arg SOURCE_DATE_EPOCH",
    ] {
        let w = good().replacen(&steps, &steps.replacen(flag, "", 1), 1);
        let got = found(&w);
        assert!(
            got.iter()
                .any(|l| l.contains("lacks `") && l.contains("not reproducible")),
            "{flag}: {got:?}"
        );
    }
    let skippable = steps.replace(
        "      - name: Same\n",
        "      - name: Same\n        if: always()\n",
    );
    let got = found(&good().replacen(&steps, &skippable, 1));
    assert!(
        got.iter()
            .any(|l| l.contains("`cmp` of the two builds is under `if:`")),
        "{got:?}"
    );
    let check = check_asset_step("kbf", "linux-x86_64");
    let without = good().replacen(&check, "", 1);
    let late = without.replacen(&steps, &format!("{steps}{check}"), 1);
    assert!(
        found(&late).iter().any(|l| l.contains(
            "is made an asset before `cargo xtask build-commit check-asset` of the kbf tarball"
        )),
        "{:?}",
        found(&late)
    );
}

/// Asserts that the lint names a verify job that does not check the signed frontend image of
/// each platform with its own tarball and the commit, in a step that cannot be skipped and
/// comes after the attestation check.
#[test]
fn the_verify_job_checks_the_signed_frontend_image_with_its_tarball_and_commit() {
    for platform in PACKAGES {
        let w = good().replacen(&verify_image_step(platform), "", 1);
        assert_ne!(w, good(), "{platform}");
        let wanted = format!(
            "`verify-linux` does not run `cargo xtask pkg-image-asset --verify-only` on `dist/kbf-frontend-image-$GITHUB_SHA-{platform}.oci.tar`"
        );
        assert!(found(&w).iter().any(|l| l.contains(&wanted)), "{platform}");
    }
    let wanted = "`verify-linux` does not run `cargo xtask pkg-image-asset --verify-only` on `dist/kbf-frontend-image-$GITHUB_SHA-linux-arm64.oci.tar`";
    let step = verify_image_step("linux-arm64");
    let tarball = "--kbf-tarball \"dist/kbf-$GITHUB_SHA-linux-arm64.tar.gz\"";
    let other = "--kbf-tarball \"dist/kbf-$GITHUB_SHA-linux-x86_64.tar.gz\"";
    for to in [
        step.replace(tarball, other),
        step.replace("--commit \"$GITHUB_SHA\"", "--commit abc"),
        step.replace("--platform linux-arm64", "--platform linux-x86_64"),
        step.replace("      - name", "      - if: always()\n        name"),
    ] {
        let w = good().replacen(&step, &to, 1);
        assert_ne!(w, good());
        assert!(found(&w).iter().any(|l| l.contains(wanted)), "{to}");
    }
    let early = good().replacen(&step, "", 1);
    let (head, tail) = early.split_once("  verify-linux:\n").expect("verify-linux");
    let tail = tail.replacen("    steps:\n", &format!("    steps:\n{step}"), 1);
    let w = format!("{head}  verify-linux:\n{tail}");
    assert!(found(&w).iter().any(|l| l.contains(wanted)));
}

/// Asserts that the lint names a Linux job that does not make its coordinator image an asset,
/// one that makes it twice, and an image of a service the set does not list.
#[test]
fn the_coordinator_image_is_made_an_asset_exactly_where_the_set_says() {
    for platform in PACKAGES {
        let step = coordinator_steps(platform);
        let asset = step
            .split_once("      - name: Coordinator asset\n")
            .expect("asset")
            .1;
        let asset = format!("      - name: Coordinator asset\n{asset}");
        let none = good().replacen(&asset, "", 1);
        assert_ne!(none, good(), "{platform}: in the fixture");
        let wanted = format!(
            "the coordinator image for {platform} is made an asset by 0 `cargo xtask pkg-image-asset` lines, not exactly one"
        );
        assert!(found(&none).contains(&wanted), "{:?}", found(&none));
        let twice = good().replacen(&asset, &format!("{asset}{asset}"), 1);
        let wanted = wanted.replace("by 0", "by 2");
        assert!(found(&twice).contains(&wanted), "{:?}", found(&twice));
    }
    let steps = coordinator_steps("linux-x86_64");
    let other = steps.replace("--service coordinator", "--service router");
    let got = found(&good().replacen(&steps, &other, 1));
    assert!(
        got.iter().any(|l| l.starts_with(
            "`cargo xtask pkg-image-asset --service router` makes an image that is not in the release set"
        )),
        "{got:?}"
    );
}

/// Asserts that the lint names a coordinator image made an asset with another commit, binary,
/// image or output directory than the job's own or in a step that can be skipped, and one made
/// without the context from the built binary, either build, the comparison of the builds, or
/// before the binary's identity check.
#[test]
fn the_coordinator_image_is_built_twice_from_the_checked_binary_and_compared() {
    let steps = coordinator_steps("linux-x86_64");
    let line = |from: &str, to: &str, wanted: &str| {
        let w = good().replacen(&steps, &steps.replace(from, to), 1);
        assert_ne!(w, good(), "{from}");
        let got = found(&w);
        assert!(got.iter().any(|l| l.contains(wanted)), "{from}: {got:?}");
    };
    line(
        "--commit \"$GITHUB_SHA\"",
        "--commit abc",
        "has --commit \"abc\", not `$GITHUB_SHA`",
    );
    line(
        "--binary \"target/$TARGET/release/kbf-coordinator\"",
        "--binary \"target/$TARGET/release/kbf-updater\"",
        "has --binary \"target/$TARGET/release/kbf-updater\", not `target/$TARGET/release/kbf-coordinator`",
    );
    line(
        "--image target/coordinator-image-out/1.oci.tar",
        "--image target/coordinator-image-out/2.oci.tar",
        "has --image \"target/coordinator-image-out/2.oci.tar\", not `target/coordinator-image-out/1.oci.tar`",
    );
    line(
        "      - name: Coordinator asset\n        run:",
        "      - name: Coordinator asset\n        if: always()\n        run:",
        "the coordinator image of `linux-x86_64` is made an asset under `if:` or `continue-on-error:`",
    );
    let missing = |name: &str, what: &str| {
        let start = steps
            .find(&format!("      - name: {name}\n"))
            .expect("step");
        let end = start
            + 1
            + steps[start + 1..]
                .find("      - name:")
                .unwrap_or(steps.len() - start - 1);
        let w = good().replacen(&steps, &format!("{}{}", &steps[..start], &steps[end..]), 1);
        let wanted = format!("is made an asset without {what} in a step before");
        let got = found(&w);
        assert!(got.iter().any(|l| l.contains(&wanted)), "{name}: {got:?}");
    };
    missing(
        "Coordinator context",
        "the context filled with this job's kbf-coordinator (`cp` into target/coordinator-image)",
    );
    missing(
        "Coordinator build 1",
        "a `docker buildx build` into 1.oci.tar",
    );
    missing(
        "Coordinator build 2",
        "a `docker buildx build` into 2.oci.tar",
    );
    missing("Coordinator same", "`cmp` of the two builds");
    // Without the identity check at all, and with it after the asset.
    let check = "      - name: Coordinator check\n        run: cargo xtask build-commit check \"target/$TARGET/release/kbf-coordinator\" \"$GITHUB_SHA\"\n";
    let without = good().replacen(check, "", 1);
    assert_ne!(without, good());
    assert!(
        found(&without).iter().any(|l| l.contains(
            "is made an asset without `cargo xtask build-commit check` of the kbf-coordinator before it"
        )),
        "{:?}",
        found(&without)
    );
    let moved = steps.replacen(check, "", 1);
    let late = good().replacen(&steps, &format!("{moved}{check}"), 1);
    assert!(
        found(&late).iter().any(|l| l.contains(
            "is made an asset before `cargo xtask build-commit check` of the kbf-coordinator"
        )),
        "{:?}",
        found(&late)
    );
}

/// Asserts that the lint names a verify job that does not check the signed coordinator image of
/// each platform with the commit, in a step that cannot be skipped and comes after the
/// attestation check, or that names a file of the build the verify job does not have.
#[test]
fn the_verify_job_checks_the_signed_coordinator_image_with_the_commit_alone() {
    for platform in PACKAGES {
        let w = good().replacen(&verify_coordinator_step(platform), "", 1);
        assert_ne!(w, good(), "{platform}");
        let wanted = format!(
            "`verify-linux` does not run `cargo xtask pkg-image-asset --verify-only` on `dist/kbf-coordinator-image-$GITHUB_SHA-{platform}.oci.tar`"
        );
        assert!(found(&w).iter().any(|l| l.contains(&wanted)), "{platform}");
    }
    let wanted = "`verify-linux` does not run `cargo xtask pkg-image-asset --verify-only` on `dist/kbf-coordinator-image-$GITHUB_SHA-linux-arm64.oci.tar`";
    let step = verify_coordinator_step("linux-arm64");
    for to in [
        step.replace("--commit \"$GITHUB_SHA\"", "--commit abc"),
        step.replace("--platform linux-arm64", "--platform linux-x86_64"),
        step.replace("--service coordinator", "--service frontend"),
        step.replace(
            "--commit",
            "--binary target/release/kbf-coordinator --commit",
        ),
        step.replace("      - name", "      - if: always()\n        name"),
    ] {
        let w = good().replacen(&step, &to, 1);
        assert_ne!(w, good());
        assert!(found(&w).iter().any(|l| l.contains(wanted)), "{to}");
    }
}
