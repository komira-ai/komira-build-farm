//! `cargo xtask release` run as the release workflow runs it, with `xtask-fake` as `gh`:
//! its `gh-api.json` is GitHub's API as each test writes it, and its `releases/`
//! directory the releases `gh release` made.

use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kbf_testkit::dir::TestDir;
use sha2::Digest;

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const REPO: &str = "komira-ai/kbf";
/// The kind of attestation of a bundle a host verifies a tarball with.
const PROVENANCE: &str = "https://slsa.dev/provenance/v1";
const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";
const OTHER: &str = "badbad0123456789badbad0123456789badbad01";
const RUN: u64 = 4242;
const TAG: &str = "v0.1.0";
const ARTIFACTS: [&str; 6] = [
    "assets-darwin-arm64",
    "assets-linux-x86_64",
    "assets-linux-arm64",
    "sha256sums",
    "bundles-darwin-arm64",
    "bundles-linux",
];

/// An empty directory of its own for one test, holding the fake as `gh/gh` and an
/// empty `tmp/`.
fn scratch(name: &str) -> TestDir {
    let dir = TestDir::under(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("xtask-release"),
        name,
    );
    std::fs::create_dir_all(dir.join("gh")).expect("scratch");
    std::fs::create_dir_all(dir.join("tmp")).expect("tmp");
    std::fs::hard_link(FAKE, dir.join("gh/gh")).expect("link the fake as gh");
    dir
}

fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

/// The exit code, stdout and stderr of `cargo xtask <args>` with `<root>/gh/gh` as
/// `gh`, planting `defect` in it (or none).
fn xtask(root: &Path, args: &[&str], defect: Option<&str>) -> (Option<i32>, String, String) {
    let gh = root.join("gh/gh");
    let mut cmd = Command::new(XTASK);
    cmd.args(args)
        .arg("--gh")
        .arg(&gh)
        .env("TMPDIR", root.join("tmp"));
    match defect {
        Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
        None => cmd.env_remove("XTASK_FAKE_DEFECT"),
    };
    let out = cmd.output().expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// `cargo xtask release check`, which takes no `--gh`.
fn check(dir: &Path, commit: &str) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .args(["release", "check", s(dir), "--commit", commit])
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// `cargo xtask release check --without-version-facts`, as the previous release is read.
fn check_without_facts(dir: &Path, commit: &str) -> (Option<i32>, String, String) {
    let out = Command::new(XTASK)
        .args(["release", "check", s(dir), "--commit", commit])
        .arg("--without-version-facts")
        .output()
        .expect("run xtask");
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code(), text(&out.stdout), text(&out.stderr))
}

/// Writes GitHub's answers, `(path, JSON)`, to the fake's `gh-api.json`.
fn answers(root: &Path, answers: &[(String, serde_json::Value)]) {
    let map: serde_json::Map<String, serde_json::Value> = answers.iter().cloned().collect();
    std::fs::write(
        root.join("gh/gh-api.json"),
        serde_json::Value::Object(map).to_string(),
    )
    .expect("gh-api.json");
}

/// What the fake gh was asked, one line per command.
fn gh_args(root: &Path) -> String {
    std::fs::read_to_string(root.join("gh/gh-args")).unwrap_or_default()
}

fn compare(commit: &str, status: &str) -> (String, serde_json::Value) {
    (
        format!("repos/{REPO}/compare/main...{commit}"),
        serde_json::json!({ "status": status }),
    )
}

/// The pipeline run `RUN` for `COMMIT`: a push on main that succeeded, with `changed` set
/// on top.
fn run(changed: serde_json::Value) -> serde_json::Value {
    let mut r = serde_json::json!({
        "id": RUN, "head_sha": COMMIT, "head_branch": "main", "event": "push",
        "conclusion": "success",
    });
    for (k, v) in changed.as_object().expect("an object") {
        r[k] = v.clone();
    }
    r
}

/// GitHub's answer for the run `RUN` itself.
fn the_run(r: serde_json::Value) -> (String, serde_json::Value) {
    (format!("repos/{REPO}/actions/runs/{RUN}"), r)
}

fn artifacts(list: &[(&str, bool)]) -> (String, serde_json::Value) {
    let list: Vec<serde_json::Value> = list
        .iter()
        .map(|(n, expired)| serde_json::json!({ "name": n, "expired": expired }))
        .collect();
    (
        format!("repos/{REPO}/actions/runs/{RUN}/artifacts?per_page=100"),
        serde_json::json!({ "artifacts": list }),
    )
}

fn all_artifacts() -> (String, serde_json::Value) {
    artifacts(&ARTIFACTS.map(|a| (a, false)))
}

fn locate(root: &Path, commit: &str, defect: Option<&str>) -> (Option<i32>, String, String) {
    let output = root.join("github-output");
    xtask(
        root,
        &[
            "release",
            "locate",
            "--repo",
            REPO,
            "--commit",
            commit,
            "--run-id",
            &RUN.to_string(),
            "--output",
            s(&output),
        ],
        defect,
    )
}

#[test]
fn locate_accepts_the_successful_main_push_run_of_the_commit() {
    // Control: catches a locate that refuses a commit on main (`main` ahead of it, or
    // at it), or does not hand the run on to the next steps. An expired artifact that is
    // not one a release needs does not matter.
    for status in ["behind", "identical"] {
        let root = scratch(&format!("locate-{status}"));
        let mut kept = ARTIFACTS.map(|a| (a, false)).to_vec();
        kept.push(("beta-b0-1", true));
        answers(
            &root,
            &[
                compare(COMMIT, status),
                the_run(run(serde_json::json!({}))),
                artifacts(&kept),
            ],
        );
        let (code, out, err) = locate(&root, COMMIT, None);
        assert_eq!(code, Some(0), "{err}");
        assert_eq!(
            out,
            format!("{COMMIT} is on main; run {RUN} built it\nrun-id={RUN}\n")
        );
        let output = std::fs::read_to_string(root.join("github-output")).expect("output");
        assert_eq!(output, format!("run-id={RUN}\n"));
    }
}

#[test]
fn locate_without_output_only_prints_the_run() {
    // Catches: a locate that needs `--output` to print the run it found.
    let root = scratch("locate-no-output");
    answers(
        &root,
        &[
            compare(COMMIT, "behind"),
            the_run(run(serde_json::json!({}))),
            all_artifacts(),
        ],
    );
    let (code, out, err) = xtask(
        &root,
        &[
            "release",
            "locate",
            "--repo",
            REPO,
            "--commit",
            COMMIT,
            "--run-id",
            &RUN.to_string(),
        ],
        None,
    );
    assert_eq!(code, Some(0), "{err}");
    assert!(out.ends_with(&format!("run-id={RUN}\n")), "{out}");
}

#[test]
fn locate_refuses_a_commit_that_is_not_on_main() {
    // Catches: a tag on a branch commit (`main` behind it or diverged from it) or on a
    // commit GitHub does not know released, and a check made after looking at the run.
    for (name, status) in [
        ("ahead", Some("ahead")),
        ("diverged", Some("diverged")),
        ("unknown", None),
    ] {
        let root = scratch(&format!("locate-off-main-{name}"));
        if let Some(status) = status {
            answers(
                &root,
                &[compare(COMMIT, status), the_run(run(serde_json::json!({})))],
            );
        }
        let (code, out, err) = locate(&root, COMMIT, None);
        assert_eq!(code, Some(1), "{name}: {out}");
        assert_eq!(
            err,
            format!(
                "xtask: {COMMIT} is not on main: a release is cut only from a commit on main\n"
            ),
            "{name}"
        );
        assert!(
            !gh_args(&root).contains("runs"),
            "{name}: looked at the run"
        );
        assert!(
            !root.join("github-output").exists(),
            "{name}: wrote an output"
        );
    }
}

#[test]
fn locate_refuses_a_run_that_did_not_build_the_commit_on_a_main_push() {
    // Catches: a run of another commit, another branch, another event, a run that did not
    // succeed (failed, cancelled, still going), or none at all taken as the one that built
    // the commit.
    let decoys = [
        serde_json::json!({ "head_sha": OTHER }),
        serde_json::json!({ "head_branch": "feature" }),
        serde_json::json!({ "event": "pull_request" }),
        serde_json::json!({ "event": "workflow_dispatch" }),
        serde_json::json!({ "conclusion": "cancelled" }),
        serde_json::json!({ "conclusion": "failure" }),
        serde_json::json!({ "conclusion": null }),
    ];
    let mut cases: Vec<(String, Option<serde_json::Value>)> = vec![("404".to_owned(), None)];
    for (i, d) in decoys.into_iter().enumerate() {
        cases.push((format!("decoy-{i}"), Some(run(d))));
    }
    for (name, the) in cases {
        let root = scratch(&format!("locate-no-run-{name}"));
        let mut given = vec![compare(COMMIT, "behind")];
        given.extend(the.map(the_run));
        given.push(all_artifacts());
        answers(&root, &given);
        let (code, _, err) = locate(&root, COMMIT, None);
        assert_eq!(code, Some(1), "{name}");
        assert_eq!(
            err,
            format!("xtask: run {RUN} is not a successful push run on main for {COMMIT}\n"),
            "{name}"
        );
        assert!(
            !root.join("github-output").exists(),
            "{name}: wrote an output"
        );
    }
}

#[test]
fn locate_refuses_a_run_whose_artifacts_are_gone() {
    // Catches: a run whose artifacts expired (after 90 days) or are missing taken, so
    // the download fails later with a less clear message, or a partial set released.
    let root = scratch("locate-expired");
    answers(
        &root,
        &[
            compare(COMMIT, "behind"),
            the_run(run(serde_json::json!({}))),
            artifacts(&[
                ("assets-darwin-arm64", true),
                ("assets-linux-x86_64", false),
                ("assets-linux-arm64", false),
                ("bundles-darwin-arm64", false),
            ]),
        ],
    );
    let (code, _, err) = locate(&root, COMMIT, None);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "xtask: run {RUN} holds no unexpired artifact assets-darwin-arm64\n\
             run {RUN} holds no unexpired artifact sha256sums\n\
             run {RUN} holds no unexpired artifact bundles-linux\n"
        )
    );
    // No artifacts listing at all: every one is missing.
    let root = scratch("locate-no-artifacts");
    answers(
        &root,
        &[
            compare(COMMIT, "behind"),
            the_run(run(serde_json::json!({}))),
        ],
    );
    let (code, _, err) = locate(&root, COMMIT, None);
    assert_eq!(code, Some(1));
    assert_eq!(err.lines().count(), 6, "{err}");
}

#[test]
fn locate_refuses_a_run_whose_bundles_are_gone() {
    // Catches: a run that kept the files but lost the attestation bundles (the attest
    // job's artifact expired, or the job never ran) released anyway: a release without
    // its bundles is a release no host can verify offline.
    let root = scratch("locate-no-bundles");
    let kept: Vec<(&str, bool)> = ARTIFACTS
        .iter()
        .filter(|a| !a.starts_with("bundles-"))
        .map(|a| (*a, false))
        .collect();
    answers(
        &root,
        &[
            compare(COMMIT, "behind"),
            the_run(run(serde_json::json!({}))),
            artifacts(&kept),
        ],
    );
    let (code, _, err) = locate(&root, COMMIT, None);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "xtask: run {RUN} holds no unexpired artifact bundles-darwin-arm64\n\
             run {RUN} holds no unexpired artifact bundles-linux\n"
        )
    );
}

#[test]
fn locate_refuses_what_is_not_a_full_commit() {
    // Catches: a short SHA (it can name another commit later) or a branch name
    // accepted, which GitHub's compare would resolve as a ref.
    let root = scratch("locate-short");
    for bad in [&COMMIT[..12], "main", &"g".repeat(40)] {
        let (code, _, err) = locate(&root, bad, None);
        assert_eq!(code, Some(2), "{bad}");
        assert!(err.contains("is not a full commit SHA"), "{bad}: {err}");
    }
    assert_eq!(gh_args(&root), "", "asked GitHub about a malformed commit");
}

#[test]
fn an_api_failure_fails_the_run_rather_than_reading_as_an_answer() {
    // Catches: a 5xx or a body that is not JSON read as "not on main" or "no run",
    // which would hide an outage behind a refusal that blames the commit.
    for (defect, want) in [
        ("api-fails", "gh: Server Error (HTTP 502)"),
        ("api-garbage", "not JSON"),
    ] {
        let root = scratch(&format!("api-{defect}"));
        let (code, _, err) = locate(&root, COMMIT, Some(defect));
        assert_eq!(code, Some(1), "{defect}");
        assert!(
            err.starts_with(&format!(
                "xtask: gh api repos/{REPO}/compare/main...{COMMIT}: "
            )),
            "{err}"
        );
        assert!(err.contains(want), "{defect}: {err}");
    }
}

#[test]
fn a_gh_that_cannot_be_started_fails_the_run() {
    // Catches: a gh that cannot be started read as a 404 (so as "not on main").
    let root = scratch("no-gh");
    let absent = root.join("absent");
    let out = Command::new(XTASK)
        .args([
            "release",
            "locate",
            "--repo",
            REPO,
            "--commit",
            COMMIT,
            "--run-id",
            "1",
            "--gh",
            s(&absent),
        ])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.starts_with(&format!("xtask: {}: ", absent.display())),
        "{err}"
    );
}

/// `release main-control` with `on` and `off` answered by the statuses given.
fn main_control(name: &str, on: &str, off: &str) -> (Option<i32>, String, String) {
    let root = scratch(name);
    answers(&root, &[compare(COMMIT, on), compare(OTHER, off)]);
    xtask(
        &root,
        &[
            "release",
            "main-control",
            "--repo",
            REPO,
            "--on-main",
            COMMIT,
            "--off-main",
            OTHER,
        ],
        None,
    )
}

#[test]
fn main_control_passes_when_the_check_tells_on_from_off() {
    // Control: catches a main-control that fails a correct check.
    let (code, out, err) = main_control("control-good", "behind", "ahead");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!("on-main check: {COMMIT} is accepted and {OTHER} is refused\n")
    );
}

#[test]
fn main_control_fails_a_check_that_refuses_main_or_accepts_a_branch() {
    // Catches: the check inverted, or reading `ahead` as on main (each a planted
    // answer the real check must judge as the control expects).
    let (code, _, err) = main_control("control-refuses-main", "ahead", "ahead");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!("xtask: control failed: {COMMIT}, on main, was refused\n")
    );
    let (code, _, err) = main_control("control-accepts-branch", "identical", "behind");
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!("xtask: control failed: {OTHER}, not on main, was accepted\n")
    );
    let root = scratch("control-short");
    let (code, _, err) = xtask(
        &root,
        &[
            "release",
            "main-control",
            "--repo",
            REPO,
            "--on-main",
            COMMIT,
            "--off-main",
            "abc",
        ],
        None,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--off-main \"abc\""), "{err}");
}

/// The SHA-256 of `text`, as `SHA256SUMS` writes it.
fn sha_of(text: &str) -> String {
    hex::encode(sha2::Sha256::digest(text))
}

/// A bundle file: one JSON line of the shape of a Sigstore bundle, whose DSSE envelope
/// holds `payload` as it is (or none when it is `null`). The signature is not real:
/// `release check` reads the shape and the subject, `gh` judges the signature.
fn bundle_with_payload(payload: serde_json::Value) -> String {
    let mut envelope = serde_json::json!({
        "payloadType": "application/vnd.in-toto+json",
        "signatures": [],
    });
    if !payload.is_null() {
        envelope["payload"] = payload;
    }
    let bundle = serde_json::json!({
        "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
        "verificationMaterial": {},
        "dsseEnvelope": envelope,
    });
    format!("{bundle}\n")
}

/// A bundle file whose payload is the in-toto statement of an attestation of
/// `predicate_type` over the files with the SHA-256 digests `subjects`.
fn bundle_with(subjects: &[&str], predicate_type: &str) -> String {
    let subject: Vec<serde_json::Value> = subjects
        .iter()
        .map(|sha| serde_json::json!({ "name": "asset", "digest": { "sha256": sha } }))
        .collect();
    let statement = serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": subject,
        "predicateType": predicate_type,
        "predicate": {},
    });
    bundle_with_payload(serde_json::Value::String(
        STANDARD.encode(statement.to_string()),
    ))
}

/// The bundle file of the asset whose SHA-256 is `sha256`, as the attest job leaves it:
/// one build provenance attestation with that one subject.
fn bundle_text(sha256: &str) -> String {
    bundle_with(&[sha256], PROVENANCE)
}

/// The assets of `commit` as the artifacts run leaves them, in `<root>/dist`: a tarball
/// and an SBOM per platform, `SHA256SUMS` over them, and the provenance bundle of each
/// tarball (which `SHA256SUMS` does not list: the attestation makes it after the sums).
fn dist(root: &Path, commit: &str) -> PathBuf {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).expect("dist");
    let mut sums = String::new();
    for platform in ["darwin-arm64", "linux-arm64", "linux-x86_64"] {
        for ext in ["cdx.json", "tar.gz"] {
            let name = format!("kbf-daemon-{commit}-{platform}.{ext}");
            let bytes = format!("{name} bytes");
            std::fs::write(dist.join(&name), &bytes).expect("asset");
            let sha = sha_of(&bytes);
            sums.push_str(&format!("{sha}  {name}\n"));
            if ext == "tar.gz" {
                std::fs::write(
                    dist.join(format!("{name}.sigstore.json")),
                    bundle_text(&sha),
                )
                .expect("bundle");
            }
        }
    }
    std::fs::write(dist.join("SHA256SUMS"), sums).expect("SHA256SUMS");
    dist
}

#[test]
fn check_passes_the_assets_of_the_commit() {
    // Control: catches a check that refuses a correct directory, one that counts a
    // bundle as an unlisted file (the sums are made before the attestation, so they
    // cannot list it), and one that does not say the bundles were checked.
    let root = scratch("check-good");
    let dist = dist(&root, COMMIT);
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "sums: 6 files match; the flipped copy does not\n\
             names: every listed file is an asset of {COMMIT}\n\
             bundles: each of the 3 tarballs has one provenance bundle\n"
        )
    );
}

/// The tarball of the platform `platform` in [`dist`].
fn tarball(commit: &str, platform: &str) -> String {
    format!("kbf-daemon-{commit}-{platform}.tar.gz")
}

/// The SHA-256 [`dist`] lists for the tarball of the platform `platform`.
fn tarball_sha(commit: &str, platform: &str) -> String {
    sha_of(&format!("{} bytes", tarball(commit, platform)))
}

#[test]
fn check_refuses_a_tarball_with_no_bundle() {
    // Catches: a release whose bundle is missing (the attest job's artifact lacks one
    // tarball's) passing, which leaves a host nothing to verify that tarball with; and
    // the message not naming the tarball and the file expected.
    let root = scratch("check-no-bundle");
    let dist = dist(&root, COMMIT);
    let t = tarball(COMMIT, "linux-arm64");
    std::fs::remove_file(dist.join(format!("{t}.sigstore.json"))).expect("remove");
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {t}: no provenance bundle {t}.sigstore.json\n")
    );
}

#[test]
fn check_refuses_a_bundle_no_listed_tarball_owns() {
    // Catches: a stray bundle released with the set: one for a tarball that is not
    // listed (another commit's, or a renamed one), one for an SBOM, one with the
    // suffix alone, and a second copy under another name. "Exactly one per tarball" is
    // both halves: none missing, none left over.
    let root = scratch("check-extra-bundle");
    let dist = dist(&root, COMMIT);
    let sbom = format!("kbf-daemon-{COMMIT}-linux-arm64.cdx.json");
    let strays = [
        format!("kbf-daemon-{OTHER}-linux-arm64.tar.gz.sigstore.json"),
        format!("{sbom}.sigstore.json"),
        ".sigstore.json".to_owned(),
        format!(
            "{}.sigstore.json.bak.sigstore.json",
            tarball(COMMIT, "linux-arm64")
        ),
    ];
    for stray in &strays {
        std::fs::write(dist.join(stray), bundle_text(&sha_of(stray))).expect("stray");
        let (code, out, err) = check(&dist, COMMIT);
        assert_eq!(code, Some(1), "{stray}: {out}");
        assert_eq!(
            err,
            format!("xtask: {stray}: not the bundle of a listed tarball\n"),
            "{stray}"
        );
        std::fs::remove_file(dist.join(stray)).expect("remove");
    }
    let (code, _, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn check_refuses_a_bundle_that_is_not_one_sigstore_bundle() {
    // Catches: an empty or truncated file, a log, two bundles in one file, a JSON value
    // of another kind, and a bundle over the 256 KiB the order record allows, each
    // accepted as "the bundle" because only its name was looked at.
    let root = scratch("check-bad-bundle");
    let dist = dist(&root, COMMIT);
    let t = tarball(COMMIT, "darwin-arm64");
    let path = dist.join(format!("{t}.sigstore.json"));
    let good = bundle_text(&sha_of(&format!("{t} bytes")));
    let mut big = good
        .trim_end()
        .strip_suffix('}')
        .expect("an object")
        .to_owned();
    big.push_str(&format!(",\"padding\":\"{}\"}}\n", "x".repeat(262_144)));
    let cases: [(&str, String, &str); 7] = [
        ("empty", String::new(), "holds 0 lines"),
        ("log", "gh: attestation failed\n".to_owned(), "not JSON"),
        ("twice", format!("{good}{good}"), "holds 2 lines"),
        ("array", "[]\n".to_owned(), "not a JSON object"),
        (
            "no media type",
            good.replace("mediaType", "media"),
            "no `mediaType` of a Sigstore bundle",
        ),
        (
            "no envelope",
            good.replace("dsseEnvelope", "envelope"),
            "no `dsseEnvelope` object",
        ),
        ("too big", big, "over the 262144 bytes"),
    ];
    for (what, text, want) in cases {
        std::fs::write(&path, text).expect("bundle");
        let (code, out, err) = check(&dist, COMMIT);
        assert_eq!(code, Some(1), "{what}: {out}");
        assert!(
            err.starts_with(&format!("xtask: {t}.sigstore.json: ")) && err.contains(want),
            "{what}: {err}"
        );
    }
    std::fs::write(&path, good).expect("bundle");
    let (code, _, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn check_refuses_a_bundle_whose_subject_is_not_its_tarball() {
    // Catches: a bundle taken on its name and shape. Two tarballs' bundles swapped (each
    // a whole, valid bundle, of the other file: the defect the contract's digest
    // comparison exists for), one with two subjects (the tarball's and another's: not
    // its own, so it would verify the file beside it too), one with none, an SBOM
    // attestation under the tarball's bundle name, and an undecodable payload. Each is
    // refused with the bundle named, before GitHub is asked for anything.
    let root = scratch("check-subject");
    let dist = dist(&root, COMMIT);
    let (a, b) = ("darwin-arm64", "linux-arm64");
    let (path_a, path_b) = (
        dist.join(format!("{}.sigstore.json", tarball(COMMIT, a))),
        dist.join(format!("{}.sigstore.json", tarball(COMMIT, b))),
    );
    let (sha_a, sha_b) = (tarball_sha(COMMIT, a), tarball_sha(COMMIT, b));
    let (text_a, text_b) = (bundle_text(&sha_a), bundle_text(&sha_b));
    let name = format!("{}.sigstore.json", tarball(COMMIT, a));

    std::fs::write(&path_a, &text_b).expect("bundle");
    std::fs::write(&path_b, &text_a).expect("bundle");
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: {name}: has the subject digest sha256:{sha_b}, not the tarball's \
             sha256:{sha_a}\n{}.sigstore.json: has the subject digest sha256:{sha_a}, not \
             the tarball's sha256:{sha_b}\n",
            tarball(COMMIT, b)
        )
    );
    std::fs::write(&path_b, &text_b).expect("bundle");

    let sbom = "https://cyclonedx.org/bom";
    let cases = [
        (
            "two subjects",
            bundle_with(&[&sha_a, &sha_b], PROVENANCE),
            "names 2 subjects, not exactly one".to_owned(),
        ),
        (
            "no subject",
            bundle_with(&[], PROVENANCE),
            "names 0 subjects, not exactly one".to_owned(),
        ),
        (
            "an SBOM attestation",
            bundle_with(&[&sha_a], sbom),
            format!("has a payload of predicate type {sbom:?}, not build provenance"),
        ),
        (
            "not base64",
            bundle_with_payload(serde_json::json!("not base64!")),
            "that is not base64".to_owned(),
        ),
        (
            "no payload",
            bundle_with_payload(serde_json::Value::Null),
            "has no `dsseEnvelope.payload`".to_owned(),
        ),
    ];
    for (what, text, want) in cases {
        std::fs::write(&path_a, text).expect("bundle");
        let (code, out, err) = check(&dist, COMMIT);
        assert_eq!(code, Some(1), "{what}: {out}");
        assert!(
            err.starts_with(&format!("xtask: {name}: ")) && err.contains(&want),
            "{what}: {err}"
        );
        assert_eq!(err.lines().count(), 1, "{what}: {err}");
    }
    std::fs::write(&path_a, &text_a).expect("bundle");
    let (code, _, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn a_bundle_is_not_a_sums_file_entry() {
    // Catches: the sums verification counting the bundles as files SHA256SUMS fails to
    // list (it cannot list them: they are made after it), which would fail every real
    // release; and its exemption swallowing a file that merely ends `.json`.
    let root = scratch("check-sums-and-bundles");
    let dist = dist(&root, COMMIT);
    let out = Command::new(XTASK)
        .args(["verify-assets", "sums", s(&dist)])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(0));
    std::fs::write(dist.join("notes.json"), b"{}").expect("notes");
    let out = Command::new(XTASK)
        .args(["verify-assets", "sums", s(&dist)])
        .output()
        .expect("run xtask");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "xtask: not listed in SHA256SUMS: notes.json\n"
    );
}

#[test]
fn check_refuses_assets_of_another_commit_or_of_no_binary() {
    // Catches: the artifacts of another run (another commit) released under this tag,
    // and a listed file that is not `<binary>-<commit>-<platform>` with a known
    // platform and extension.
    let root = scratch("check-other");
    let other_dist = dist(&root, OTHER);
    let (code, _, err) = check(&other_dist, COMMIT);
    assert_eq!(code, Some(1));
    assert_eq!(err.lines().count(), 6, "{err}");
    assert!(
        err.contains(&format!(
            "kbf-daemon-{OTHER}-linux-arm64.tar.gz: not an asset of {COMMIT}"
        )),
        "{err}"
    );
    for bad in [
        format!("-{COMMIT}-linux-arm64.tar.gz"),
        format!("kbf-daemon-{COMMIT}-windows-x86_64.tar.gz"),
        format!("kbf-daemon-{COMMIT}-linux-arm64.zip"),
        "notes.txt".to_owned(),
    ] {
        let root = scratch("check-bad-name");
        let commit_dist = dist(&root, COMMIT);
        let sums = std::fs::read_to_string(commit_dist.join("SHA256SUMS")).expect("sums");
        std::fs::write(commit_dist.join(&bad), "x").expect("file");
        let line = format!("{}  {bad}\n", hex::encode(sha2::Sha256::digest(b"x")));
        std::fs::write(commit_dist.join("SHA256SUMS"), format!("{line}{sums}")).expect("sums");
        let (code, _, err) = check(&commit_dist, COMMIT);
        assert_eq!(code, Some(1), "{bad}");
        assert_eq!(err, format!("xtask: {bad}: not an asset of {COMMIT}\n"));
    }
}

/// [`dist`] with one more listed file, `name`, holding `x`.
fn dist_with(root: &Path, name: &str) -> PathBuf {
    let d = dist(root, COMMIT);
    let sums = std::fs::read_to_string(d.join("SHA256SUMS")).expect("sums");
    std::fs::write(d.join(name), "x").expect("file");
    let line = format!("{}  {name}\n", hex::encode(sha2::Sha256::digest(b"x")));
    std::fs::write(d.join("SHA256SUMS"), format!("{sums}{line}")).expect("sums");
    d
}

/// [`dist`] with the `kbf` tarball of `platform`, its bundle and, when `facts`, the
/// version facts beside it.
fn dist_with_kbf(root: &Path, platform: &str, facts: bool) -> PathBuf {
    let d = dist(root, COMMIT);
    let mut sums = std::fs::read_to_string(d.join("SHA256SUMS")).expect("sums");
    let tarball = format!("kbf-{COMMIT}-{platform}.tar.gz");
    let mut names = vec![tarball.clone()];
    if facts {
        names.push(format!("kbf-{COMMIT}-{platform}.version.json"));
    }
    for name in names {
        let bytes = format!("{name} bytes");
        std::fs::write(d.join(&name), &bytes).expect("file");
        let sha = sha_of(&bytes);
        sums.push_str(&format!("{sha}  {name}\n"));
        if name == tarball {
            let bundle = d.join(format!("{name}.sigstore.json"));
            std::fs::write(bundle, bundle_text(&sha)).expect("bundle");
        }
    }
    std::fs::write(d.join("SHA256SUMS"), sums).expect("sums");
    d
}

#[test]
fn check_takes_the_version_facts_of_kbf_for_the_commit() {
    // Catches: the facts file the build jobs now list refused as no asset of the commit,
    // which would stop every release. Control for the next tests: the name that passes is
    // exactly `kbf-<commit>-<platform>.version.json`, beside its tarball.
    let root = scratch("check-facts");
    let d = dist_with_kbf(&root, "linux-arm64", true);
    let (code, out, err) = check(&d, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("sums: 8 files match"), "{out}");
    assert!(out.contains("is an asset of"), "{out}");
    assert!(out.contains("each of the 4 tarballs"), "{out}");
}

#[test]
fn check_waives_the_version_facts_of_a_release_made_before_them_and_only_that() {
    // Catches: the first release after the facts were recorded failing at its check of the
    // previous release (a `kbf` tarball and no facts: nothing is published), and a waiver
    // that is a general pass: without the flag the same directory is refused, with it
    // facts without their tarball are still refused, and so is a changed file. Catches:
    // the flag ignored; the flag skipping the whole pairing; the flag skipping the sums.
    let root = scratch("check-waived");
    let d = dist_with_kbf(&root, "linux-arm64", false);
    let facts = format!("kbf-{COMMIT}-linux-arm64.version.json");
    let tarball = format!("kbf-{COMMIT}-linux-arm64.tar.gz");
    let (code, _, err) = check(&d, COMMIT);
    assert_eq!(code, Some(1), "{err}");
    let want =
        format!("xtask: {tarball}: its version facts {facts} are not listed in SHA256SUMS\n");
    assert_eq!(err, want);
    let (code, out, err) = check_without_facts(&d, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains("sums: 7 files match"), "{out}");
    assert!(out.contains("each of the 4 tarballs"), "{out}");
    // Facts with no tarball are refused whether or not the flag is given.
    let orphan_root = scratch("check-orphan");
    let orphan = dist_with(&orphan_root, &facts);
    let want = format!(
        "xtask: {facts}: the tarball it states the facts of, {tarball}, is not listed in \
         SHA256SUMS\n"
    );
    for run in [check, check_without_facts] {
        let (code, _, err) = run(&orphan, COMMIT);
        assert_eq!(code, Some(1), "{err}");
        assert_eq!(err, want);
    }
    // The sums are still read.
    std::fs::write(d.join(&tarball), "other").expect("change");
    let (code, _, err) = check_without_facts(&d, COMMIT);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("does not match its line"), "{err}");
}

#[test]
fn check_refuses_version_facts_that_are_not_kbfs_for_the_commit() {
    // Catches: any file ending `.version.json` taken as an asset (another binary's, an
    // unknown platform's, a renamed copy), when the set holds one name per platform.
    // Another commit's facts are refused earlier, by the sums (no tarball of it is listed);
    // `is_facts_of` has a test of its own for that name. Catches: the exact name replaced by
    // the suffix.
    for bad in [
        format!("kbf-daemon-{COMMIT}-linux-arm64.version.json"),
        format!("kbf-{COMMIT}-windows-x86_64.version.json"),
        format!("kbf-{COMMIT}-linux-arm64.version.json.bak"),
    ] {
        let root = scratch("check-bad-facts");
        let d = dist_with(&root, &bad);
        let (code, _, err) = check(&d, COMMIT);
        assert_eq!(code, Some(1), "{bad}");
        assert_eq!(err, format!("xtask: {bad}: not an asset of {COMMIT}\n"));
    }
}

#[test]
fn check_refuses_a_changed_file() {
    // Catches: a check that reads names only (the `sha256sum --check` half dropped).
    let root = scratch("check-changed");
    let dist = dist(&root, COMMIT);
    std::fs::write(
        dist.join(format!("kbf-daemon-{COMMIT}-linux-arm64.tar.gz")),
        "other",
    )
    .expect("change");
    let (code, _, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(1));
    assert!(err.contains("does not match its line"), "{err}");
    let (code, _, err) = check(&dist, "abc");
    assert_eq!(code, Some(2), "{err}");
}

fn publish(root: &Path, defect: Option<&str>) -> (Option<i32>, String, String) {
    // The set hash `release verify` prints for the files in `dist`: sha256 of its SHA256SUMS.
    let sums = std::fs::read_to_string(root.join("dist/SHA256SUMS")).unwrap_or_default();
    publish_set(root, &sha_of(&sums), defect)
}

/// What `release notes` would have written, as the tests' notes file.
const NOTES: &str = "# v0.1.0\n\n## Before you click\n\n### Not reversible\n\n- meta_log raised from 3 \
                     (v0.0.9) to 4\n\n## Commits since v0.0.9\n\n- abc1234 a change\n";

/// Writes [`NOTES`] to `<root>/notes.md` and returns the path.
fn notes_file(root: &Path) -> PathBuf {
    let path = root.join("notes.md");
    std::fs::write(&path, NOTES).expect("notes");
    path
}

/// Adds to the fake's API the release tags `tags` (one page) and the compare of the
/// highest, `TAG`, with `commit`.
fn tags_standing(root: &Path, tags: &[&str], top: &str, status: &str) {
    let path = root.join("gh/gh-api.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".into());
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&text).expect("gh-api.json");
    let refs: Vec<serde_json::Value> = tags
        .iter()
        .map(|t| serde_json::json!({ "ref": format!("refs/tags/{t}") }))
        .collect();
    map.insert(
        format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100&page=1"),
        serde_json::Value::Array(refs),
    );
    map.insert(
        format!("repos/{REPO}/compare/{top}...{COMMIT}"),
        serde_json::json!({ "status": status }),
    );
    std::fs::write(path, serde_json::Value::Object(map).to_string()).expect("gh-api.json");
}

fn publish_set(root: &Path, set_hash: &str, defect: Option<&str>) -> (Option<i32>, String, String) {
    publish_tag(root, TAG, set_hash, defect)
}

fn publish_tag(
    root: &Path,
    tag: &str,
    set_hash: &str,
    defect: Option<&str>,
) -> (Option<i32>, String, String) {
    publish_with(root, tag, set_hash, &notes_file(root), defect)
}

fn publish_notes(
    root: &Path,
    set_hash: &str,
    notes: &Path,
    defect: Option<&str>,
) -> (Option<i32>, String, String) {
    publish_with(root, TAG, set_hash, notes, defect)
}

fn publish_with(
    root: &Path,
    tag: &str,
    set_hash: &str,
    notes: &Path,
    defect: Option<&str>,
) -> (Option<i32>, String, String) {
    // Rule S reads the release tags: `TAG` is the highest and names `COMMIT`, unless the
    // test has said otherwise.
    if !std::fs::read_to_string(root.join("gh/gh-api.json"))
        .is_ok_and(|t| t.contains("matching-refs"))
    {
        tags_standing(root, &[TAG], TAG, "identical");
    }
    let dist = root.join("dist");
    let run = RUN.to_string();
    xtask(
        root,
        &[
            "release",
            "publish",
            s(&dist),
            "--repo",
            REPO,
            "--tag",
            tag,
            "--commit",
            COMMIT,
            "--run-id",
            &run,
            "--set-hash",
            set_hash,
            "--notes-file",
            s(notes),
        ],
        defect,
    )
}

/// The release `TAG` as the fake holds it: file name and bytes, sorted.
fn released(root: &Path) -> Vec<(String, Vec<u8>)> {
    let dir = root.join("gh/releases").join(TAG);
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .map(|e| {
                    let e = e.expect("entry");
                    let bytes = std::fs::read(e.path()).expect("asset");
                    (e.file_name().to_string_lossy().into_owned(), bytes)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// The files of `<root>/dist`: name and bytes, sorted.
fn verified(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = std::fs::read_dir(root.join("dist"))
        .expect("dist")
        .map(|e| {
            let e = e.expect("entry");
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).expect("file"),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn publish_creates_the_release_with_every_verified_file() {
    // Catches: a release created without the tag check (`--verify-tag`), without some
    // file (SHA256SUMS included), or with notes that do not say how to verify.
    let root = scratch("publish-new");
    dist(&root, COMMIT);
    let (code, out, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.ends_with(&format!("created release {TAG} with 10 files\n")),
        "{out}"
    );
    assert_eq!(released(&root), verified(&root));
    let args = gh_args(&root);
    let create = args
        .lines()
        .find(|l| l.starts_with("release create"))
        .expect("a create");
    assert!(
        create.starts_with(&format!(
            "release create {TAG} --repo {REPO} --verify-tag --title {TAG} --notes "
        )),
        "{create}"
    );
    assert!(args.contains(&format!("/actions/runs/{RUN})")), "{args}");
    // The notes span lines of the log; the verify command is one of them.
    assert!(
        args.contains(&format!(
            "--signer-workflow {REPO}/.github/workflows/artifacts.yml --source-ref refs/heads/main --deny-self-hosted-runners"
        )),
        "{args}"
    );
    // And the offline command, for a tarball with the bundle released beside it: the
    // flags a host's installer verifies with, for this release's commit.
    assert!(
        args.contains(&format!(
            "gh attestation verify <asset> --bundle <asset>.sigstore.json \
             --custom-trusted-root trusted_root.jsonl --repo {REPO} \
             --signer-workflow {REPO}/.github/workflows/artifacts.yml \
             --cert-oidc-issuer https://token.actions.githubusercontent.com \
             --source-ref refs/heads/main --source-digest {COMMIT} \
             --predicate-type https://slsa.dev/provenance/v1 --deny-self-hosted-runners"
        )),
        "{args}"
    );
}

#[test]
fn publish_makes_the_notes_file_the_start_of_the_release_body() {
    // Catches: a release whose body is not the generated notes (the reviewer approved
    // notes that nobody published), or whose notes are cut, reflowed or followed by
    // nothing: the body is the file verbatim, then how to verify the files.
    let root = scratch("publish-body");
    dist(&root, COMMIT);
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    let args = gh_args(&root);
    let create = args.split("\nrelease create").last().expect("a create");
    let body = create.split_once(" --notes ").expect("--notes").1;
    assert!(
        body.starts_with(&format!(
            "{}\n## The files\n\nThe files run {RUN} of the pipeline built and attested",
            NOTES.trim_end()
        )),
        "{body}"
    );
    assert!(
        body.contains("Verify an asset before use"),
        "the verification instructions are gone: {body}"
    );
}

#[test]
fn publish_requires_the_notes_file() {
    // Catches: `--notes-file` made optional (a release with a fixed body again) and a
    // missing, empty or unreadable file read as no notes. None may reach GitHub.
    let root = scratch("publish-no-notes");
    dist(&root, COMMIT);
    let sums = std::fs::read_to_string(root.join("dist/SHA256SUMS")).unwrap();
    let dist_dir = root.join("dist");
    let run = RUN.to_string();
    let (code, _, err) = xtask(
        &root,
        &[
            "release",
            "publish",
            s(&dist_dir),
            "--repo",
            REPO,
            "--tag",
            TAG,
            "--commit",
            COMMIT,
            "--run-id",
            &run,
            "--set-hash",
            &sha_of(&sums),
        ],
        None,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("--notes-file"), "{err}");
    let gone = root.join("gone.md");
    let (code, _, err) = publish_notes(&root, &sha_of(&sums), &gone, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("gone.md"), "{err}");
    for blank in ["", " \n\n"] {
        let empty = root.join("empty.md");
        std::fs::write(&empty, blank).unwrap();
        let (code, _, err) = publish_notes(&root, &sha_of(&sums), &empty, None);
        assert_eq!(code, Some(1), "{err}");
        assert!(err.contains("is empty"), "{err}");
    }
    assert_eq!(
        gh_args(&root),
        "",
        "GitHub was asked about a release with no notes"
    );
    assert_eq!(released(&root), vec![], "a release was made");
}

#[test]
fn publish_refuses_a_body_github_would_cut() {
    // Catches: a body over GitHub's 125 000 characters, which `gh` would fail on after
    // the checks, or which would be cut.
    let root = scratch("publish-long-notes");
    dist(&root, COMMIT);
    let sums = std::fs::read_to_string(root.join("dist/SHA256SUMS")).unwrap();
    let long = root.join("long.md");
    std::fs::write(&long, "x".repeat(125_000)).unwrap();
    let (code, _, err) = publish_notes(&root, &sha_of(&sums), &long, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("GitHub takes 125000"), "{err}");
    assert_eq!(gh_args(&root), "", "GitHub was asked");
    std::fs::write(&long, "x".repeat(100_000)).unwrap();
    let (code, _, err) = publish_notes(&root, &sha_of(&sums), &long, None);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn publish_releases_every_bundle_beside_its_tarball() {
    // Catches: a release made of the listed files alone (the bundles left in the
    // directory, not uploaded), so a host that downloads the tarball finds nothing to
    // verify it with offline.
    let root = scratch("publish-bundles");
    dist(&root, COMMIT);
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    let names: Vec<String> = released(&root).into_iter().map(|(n, _)| n).collect();
    for platform in ["darwin-arm64", "linux-arm64", "linux-x86_64"] {
        let t = tarball(COMMIT, platform);
        assert!(names.contains(&t), "{t} not released");
        assert!(
            names.contains(&format!("{t}.sigstore.json")),
            "the bundle of {t} not released: {names:?}"
        );
    }
}

#[test]
fn publish_refuses_a_directory_with_a_missing_bundle_before_asking_github() {
    // Catches: a publish that checks the sums and names but not the bundles, creating a
    // release whose tarball cannot be verified offline (a release asset is never
    // replaced, so the gap would stay).
    let root = scratch("publish-no-bundle");
    let dist = dist(&root, COMMIT);
    let t = tarball(COMMIT, "darwin-arm64");
    std::fs::remove_file(dist.join(format!("{t}.sigstore.json"))).expect("remove");
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!("xtask: {t}: no provenance bundle {t}.sigstore.json\n")
    );
    assert_eq!(gh_args(&root), "");
}

#[test]
fn publish_refuses_a_bundle_of_another_tarball_before_asking_github() {
    // Catches: a publish that checks a bundle's name and shape but not whose it is, so
    // a bundle copied under the wrong name is released (an asset is never replaced, so
    // the tarball would stay without a bundle a host can use).
    let root = scratch("publish-swapped-bundle");
    let dist = dist(&root, COMMIT);
    let t = tarball(COMMIT, "darwin-arm64");
    std::fs::write(
        dist.join(format!("{t}.sigstore.json")),
        bundle_text(&tarball_sha(COMMIT, "linux-arm64")),
    )
    .expect("bundle");
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        format!(
            "xtask: {t}.sigstore.json: has the subject digest sha256:{}, not the tarball's \
             sha256:{}\n",
            tarball_sha(COMMIT, "linux-arm64"),
            tarball_sha(COMMIT, "darwin-arm64")
        )
    );
    assert_eq!(gh_args(&root), "");
}

#[test]
fn a_rerun_changes_nothing() {
    // Catches: a re-run that creates a second release, uploads an asset again, or
    // replaces one (`--clobber`).
    let root = scratch("publish-rerun");
    dist(&root, COMMIT);
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    let before = released(&root);
    let (code, out, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.ends_with(&format!(
            "release {TAG}: 10 files already held byte for byte, 0 uploaded\n"
        )),
        "{out}"
    );
    assert_eq!(released(&root), before);
    let args = gh_args(&root);
    assert_eq!(args.matches("release create").count(), 1, "{args}");
    assert!(!args.contains("release upload"), "{args}");
    assert!(!args.contains("--clobber"), "{args}");
}

#[test]
fn a_rerun_uploads_only_what_the_release_lacks() {
    // Catches: a partial release (a run stopped mid-upload, or created by hand with
    // no assets) left partial, or completed by uploading what it already holds.
    for (name, keep) in [("partial", 3), ("empty", 0)] {
        let root = scratch(&format!("publish-{name}"));
        dist(&root, COMMIT);
        let held = root.join("gh/releases").join(TAG);
        std::fs::create_dir_all(&held).expect("release");
        let files = verified(&root);
        let assets: Vec<serde_json::Value> = files[..keep]
            .iter()
            .map(|(n, bytes)| {
                std::fs::write(held.join(n), bytes).expect("asset");
                serde_json::json!({ "name": n })
            })
            .collect();
        answers(
            &root,
            &[(
                format!("repos/{REPO}/releases/tags/{TAG}"),
                serde_json::json!({ "assets": assets }),
            )],
        );
        let (code, out, err) = publish(&root, None);
        assert_eq!(code, Some(0), "{name}: {err}");
        assert!(
            out.ends_with(&format!(
                "release {TAG}: {keep} files already held byte for byte, {} uploaded\n",
                10 - keep
            )),
            "{name}: {out}"
        );
        assert_eq!(released(&root), files, "{name}");
        let args = gh_args(&root);
        assert!(!args.contains("release create"), "{name}: {args}");
        assert_eq!(
            args.contains("release download"),
            keep > 0,
            "{name}: {args}"
        );
    }
}

#[test]
fn publish_refuses_a_held_asset_that_is_not_the_verified_file() {
    // Catches: an asset with other bytes under a verified file's name kept (or
    // replaced, silently), and an asset no verified file names kept beside them; in
    // either case nothing may be uploaded.
    for (name, bytes, want) in [
        (
            format!("kbf-daemon-{COMMIT}-linux-arm64.tar.gz"),
            b"other bytes".as_slice(),
            "with other bytes than the verified file",
        ),
        (
            "kbf-extra.tar.gz".to_owned(),
            b"x".as_slice(),
            "not an asset of",
        ),
    ] {
        let root = scratch("publish-differs");
        dist(&root, COMMIT);
        let held = root.join("gh/releases").join(TAG);
        std::fs::create_dir_all(&held).expect("release");
        std::fs::write(held.join(&name), bytes).expect("asset");
        answers(
            &root,
            &[(
                format!("repos/{REPO}/releases/tags/{TAG}"),
                serde_json::json!({ "assets": [{ "name": name }] }),
            )],
        );
        let (code, _, err) = publish(&root, None);
        assert_eq!(code, Some(1), "{name}");
        assert!(
            err.starts_with(&format!("xtask: release {TAG} holds {name}")),
            "{err}"
        );
        assert!(err.contains(want), "{err}");
        assert_eq!(released(&root), vec![(name.clone(), bytes.to_vec())]);
        assert!(!gh_args(&root).contains("release upload"));
    }
}

#[test]
fn publish_refuses_files_that_fail_the_check_before_asking_github() {
    // Catches: a publish that skips the check of the files it is handed.
    let root = scratch("publish-unchecked");
    dist(&root, OTHER);
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(1));
    assert!(err.contains("not an asset of"), "{err}");
    assert_eq!(gh_args(&root), "");
}

#[test]
fn publish_refuses_a_commit_a_newer_release_supersedes_before_any_write() {
    // Catches: Rule S missing from publish, or accepting `behind` (the commit is an
    // ancestor of the highest tag: a re-run of an old run would publish over a newer
    // release), `diverged`, or a compare GitHub cannot answer.
    for status in ["behind", "diverged", "weird"] {
        let root = scratch("publish-superseded");
        dist(&root, COMMIT);
        tags_standing(&root, &[TAG, "v0.2.0"], "v0.2.0", status);
        let (code, out, err) = publish(&root, None);
        assert_eq!(code, Some(1), "{status}: {out}");
        assert!(
            err.contains(&format!(
                "superseded: the highest release tag is v0.2.0 and {COMMIT} is not at or \
                 after it (compare says {status})"
            )),
            "{status}: {err}"
        );
        let args = gh_args(&root);
        assert!(!args.contains("release create"), "{status}: {args}");
        assert!(!args.contains("release upload"), "{status}: {args}");
        assert!(!args.contains("releases/tags"), "{status}: {args}");
        assert_eq!(released(&root), vec![], "{status}");
    }
}

#[test]
fn publish_goes_on_when_the_highest_tag_is_an_ancestor_of_the_commit() {
    // Control for the test above: the highest tag is older than the commit (`ahead`),
    // and a repository with no release tag at all is not superseded by one.
    for (tags, top, status) in [
        (vec!["v0.0.9"], "v0.0.9", "ahead"),
        (vec![], TAG, "identical"),
        (vec![TAG, "v0.1.0-rc1", "latest"], TAG, "identical"),
    ] {
        let root = scratch("publish-ahead");
        dist(&root, COMMIT);
        tags_standing(&root, &tags, top, status);
        let (code, _, err) = publish(&root, None);
        assert_eq!(code, Some(0), "{status}: {err}");
        assert_eq!(released(&root), verified(&root));
    }
}

#[test]
fn publish_refuses_a_tag_that_is_not_above_the_highest_before_any_write() {
    // Catches: Rule S comparing only the commit with the highest tag, so a caller could
    // publish `--tag v0.0.1` for an ahead commit while v0.0.5 exists; a tag equal to the
    // highest that does not name this commit; a lower tag when the highest names it; a
    // tag that is no release tag. A re-run (the highest tag names this commit and is
    // `--tag`) is the control, and it is in `a_rerun_changes_nothing`.
    for (top, status, tag) in [
        ("v0.0.5", "ahead", "v0.0.1"),
        ("v0.0.5", "ahead", "v0.0.5"),
        ("v0.0.5", "ahead", "latest"),
        ("v0.0.5", "identical", "v0.0.1"),
    ] {
        let root = scratch("publish-low-tag");
        let dist = dist(&root, COMMIT);
        tags_standing(&root, &[top], top, status);
        let sums = std::fs::read_to_string(dist.join("SHA256SUMS")).expect("sums");
        let (code, out, err) = publish_tag(&root, tag, &sha_of(&sums), None);
        assert_eq!(code, Some(1), "{tag} {status}: {out}");
        assert!(
            err.contains(&format!(
                "--tag {tag} is no release above the highest release tag {top}"
            )),
            "{tag} {status}: {err}"
        );
        let args = gh_args(&root);
        assert!(!args.contains("release create"), "{tag}: {args}");
        assert!(!args.contains("release upload"), "{tag}: {args}");
        assert!(!args.contains("releases/tags"), "{tag}: {args}");
        assert_eq!(released(&root), vec![], "{tag}");
    }
}

#[test]
fn a_failed_gh_release_fails_the_run() {
    // Catches: a create or upload whose failure is not reported, so the workflow goes
    // green with no release.
    let root = scratch("publish-fails");
    dist(&root, COMMIT);
    let (code, _, err) = publish(&root, Some("release-fails"));
    assert_eq!(code, Some(1));
    assert_eq!(
        err,
        "xtask: gh release create: xtask-fake: release failed\n"
    );
}

#[test]
fn publish_refuses_files_whose_set_differs_from_the_verified_one_before_asking_github() {
    // Catches: a publish that trusts the verified files without comparing their set to
    // the one `release verify` printed (kbf-cicd.md 8.1: "if anything is rebuilt, verify fails"). The
    // files pass `check` in every case, so only the set hash can refuse them.
    let root = scratch("publish-other-set");
    let dist = dist(&root, COMMIT);
    // `release verify` printed another set than these files.
    let other = sha_of("what verify printed");
    let (code, _, err) = publish_set(&root, &other, None);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("other bytes than it verified"), "{err}");
    assert!(err.contains(&other), "{err}");
    assert!(released(&root).is_empty(), "nothing was released");
    assert!(
        !gh_args(&root).contains("release create"),
        "GitHub was asked to create a release"
    );
    // The same files under their own set hash publish.
    let sums = std::fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
    let (code, _, err) = publish_set(&root, &sha_of(&sums), None);
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn publish_refuses_a_set_hash_that_is_not_a_hash() {
    let root = scratch("publish-bad-hash");
    dist(&root, COMMIT);
    let (code, _, err) = publish_set(&root, "abc", None);
    assert_eq!(code, Some(2), "{err}");
}

/// The deb of `platform` named `name`, added to `dist` as `pkg-asset` and the attest job
/// leave it: the file, its line in `SHA256SUMS`, and its bundle when `bundle`.
fn add_deb(dist: &Path, name: &str, bundle: bool) {
    let bytes = format!("{name} bytes");
    std::fs::write(dist.join(name), &bytes).expect("deb");
    let sha = sha_of(&bytes);
    let mut sums = std::fs::read_to_string(dist.join("SHA256SUMS")).expect("sums");
    sums.push_str(&format!("{sha}  {name}\n"));
    std::fs::write(dist.join("SHA256SUMS"), sums).expect("sums");
    if bundle {
        std::fs::write(
            dist.join(format!("{name}.sigstore.json")),
            bundle_text(&sha),
        )
        .expect("bundle");
    }
}

fn deb(commit: &str, platform: &str) -> String {
    format!("kbf-installer-{commit}-{platform}.deb")
}

#[test]
fn check_accepts_the_installer_package_of_the_commit_with_its_bundle() {
    // Catches: a `.deb` in SHA256SUMS refused as "not an asset of <commit>" (the release
    // of the first main push that lists one fails), or accepted with no bundle, and the
    // bundles line not saying that a package was counted.
    let root = scratch("check-deb");
    let dist = dist(&root, COMMIT);
    add_deb(&dist, &deb(COMMIT, "linux-x86_64"), true);
    add_deb(&dist, &deb(COMMIT, "linux-arm64"), true);
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "sums: 8 files match; the flipped copy does not\n\
             names: every listed file is an asset of {COMMIT}\n\
             bundles: each of the 3 tarballs and 2 package(s) has one provenance bundle\n"
        )
    );
}

fn image(commit: &str, platform: &str) -> String {
    format!("kbf-frontend-image-{commit}-{platform}.oci.tar")
}

#[test]
fn check_accepts_the_frontend_image_of_each_linux_arch_with_its_bundle() {
    // Catches: an `.oci.tar` in SHA256SUMS refused as "not an asset of <commit>" (the release
    // of the first main push that lists an image fails), accepted with no bundle, and the
    // bundles line not counting the image as a package.
    let root = scratch("check-image");
    let dist = dist(&root, COMMIT);
    add_deb(&dist, &image(COMMIT, "linux-x86_64"), true);
    add_deb(&dist, &image(COMMIT, "linux-arm64"), true);
    let (code, out, err) = check(&dist, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "sums: 8 files match; the flipped copy does not\n\
             names: every listed file is an asset of {COMMIT}\n\
             bundles: each of the 3 tarballs and 2 package(s) has one provenance bundle\n"
        )
    );
    let root = scratch("check-image-no-bundle");
    let held = dist_without_bundle(&root);
    let (code, out, err) = check(&held.0, COMMIT);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!(
            "xtask: {}: no provenance bundle {}.sigstore.json\n",
            held.1, held.1
        )
    );
}

#[test]
fn check_accepts_the_coordinator_image_of_each_linux_arch_and_refuses_it_on_the_mac() {
    // Catches: `kbf-coordinator-image-...oci.tar` refused as "not an asset of <commit>" (the
    // release of the first main push that lists it fails), accepted with no bundle, or
    // accepted for the Mac, which builds no container image.
    let root = scratch("check-coordinator-image");
    let listed = dist(&root, COMMIT);
    for platform in ["linux-x86_64", "linux-arm64"] {
        add_deb(
            &listed,
            &format!("kbf-coordinator-image-{COMMIT}-{platform}.oci.tar"),
            true,
        );
    }
    let (code, out, err) = check(&listed, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.contains("bundles: each of the 3 tarballs and 2 package(s) has one provenance bundle"),
        "{out}"
    );
    let mac = format!("kbf-coordinator-image-{COMMIT}-darwin-arm64.oci.tar");
    let root = scratch("check-coordinator-image-mac");
    let held = dist(&root, COMMIT);
    add_deb(&held, &mac, true);
    let (code, out, err) = check(&held, COMMIT);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(err, format!("xtask: {mac}: not an asset of {COMMIT}\n"));
}

/// A `dist` of the commit with the x86_64 frontend image listed and no bundle for it: the
/// directory and the image's name.
fn dist_without_bundle(root: &Path) -> (PathBuf, String) {
    let dist = dist(root, COMMIT);
    let name = image(COMMIT, "linux-x86_64");
    add_deb(&dist, &name, false);
    (dist, name)
}

#[test]
fn check_refuses_a_frontend_image_of_another_commit_arch_or_name() {
    // Catches: an image accepted for another commit's build, for an architecture the release
    // does not build (an unknown one, or the Mac's, which has no container image), under a
    // binary name that is not the frontend image's, or with no commit in its name at all.
    for foreign in [
        image(OTHER, "linux-arm64"),
        image(COMMIT, "linux-riscv"),
        image(COMMIT, "darwin-arm64"),
        format!("kbf-daemon-{COMMIT}-linux-arm64.oci.tar"),
        format!("-{COMMIT}-linux-arm64.oci.tar"),
        "kbf-frontend-image-linux-arm64.oci.tar".to_owned(),
    ] {
        let root = scratch("check-image-foreign");
        let dist = dist(&root, COMMIT);
        add_deb(&dist, &foreign, true);
        let (code, out, err) = check(&dist, COMMIT);
        assert_eq!(code, Some(1), "{foreign}: {out}");
        assert_eq!(
            err,
            format!("xtask: {foreign}: not an asset of {COMMIT}\n"),
            "{foreign}"
        );
    }
}

#[test]
fn check_accepts_the_macos_installer_package_of_the_commit_and_refuses_the_uncommitted_name() {
    // Catches: a `.pkg` in SHA256SUMS refused as "not an asset of <commit>" (the release of
    // the first main push that lists the Mac package fails), the package counted with no
    // bundle, and `pkg build`'s own name (`kbf-installer-<version>-macos-arm64.pkg`, which
    // names no commit) accepted as an asset.
    let root = scratch("check-pkg");
    let accepted = dist(&root, COMMIT);
    add_deb(
        &accepted,
        &format!("kbf-installer-{COMMIT}-darwin-arm64.pkg"),
        true,
    );
    let (code, out, err) = check(&accepted, COMMIT);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        out,
        format!(
            "sums: 7 files match; the flipped copy does not\n\
             names: every listed file is an asset of {COMMIT}\n\
             bundles: each of the 3 tarballs and 1 package(s) has one provenance bundle\n"
        )
    );
    let root = scratch("check-pkg-foreign");
    let foreign = dist(&root, COMMIT);
    add_deb(&foreign, "kbf-installer-0.1.0-macos-arm64.pkg", true);
    let (code, _, err) = check(&foreign, COMMIT);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("kbf-installer-0.1.0-macos-arm64.pkg: not an asset of"),
        "{err}"
    );
}

#[test]
fn check_refuses_a_package_with_no_bundle_a_stray_package_bundle_and_a_foreign_package() {
    // Catches: a package released with no bundle a host can verify it with; a bundle of
    // a package that is not listed; and a `.deb` that is not an asset of this commit:
    // another commit's, an unknown platform's, and nfpm's own name
    // (`kbf-installer_<version>_<arch>.deb`), which names no commit.
    let root = scratch("check-deb-refusals");
    let held = dist(&root, COMMIT);
    let name = deb(COMMIT, "linux-x86_64");
    add_deb(&held, &name, false);
    let (code, out, err) = check(&held, COMMIT);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {name}: no provenance bundle {name}.sigstore.json\n")
    );
    let stray = format!("{}.sigstore.json", deb(COMMIT, "linux-arm64"));
    std::fs::write(
        held.join(format!("{name}.sigstore.json")),
        bundle_text(&sha_of(&format!("{name} bytes"))),
    )
    .expect("bundle");
    std::fs::write(held.join(&stray), bundle_text(&sha_of("x"))).expect("stray");
    let (code, out, err) = check(&held, COMMIT);
    assert_eq!(code, Some(1), "{out}");
    assert_eq!(
        err,
        format!("xtask: {stray}: not the bundle of a listed tarball\n")
    );
    for foreign in [
        deb(OTHER, "linux-arm64"),
        deb(COMMIT, "linux-riscv"),
        "kbf-installer_0.1.0_amd64.deb".to_owned(),
        format!("-{COMMIT}-linux-arm64.deb"),
    ] {
        let root = scratch("check-deb-foreign");
        let dist = dist(&root, COMMIT);
        add_deb(&dist, &foreign, true);
        let (code, out, err) = check(&dist, COMMIT);
        assert_eq!(code, Some(1), "{foreign}: {out}");
        assert_eq!(
            err,
            format!("xtask: {foreign}: not an asset of {COMMIT}\n"),
            "{foreign}"
        );
    }
}

#[test]
fn publish_releases_the_package_and_its_bundle() {
    // Catches: a release that lists the package in SHA256SUMS and uploads neither it
    // nor its bundle (the names are the checked set plus the bundles of each file that
    // has one), so the operator path (verify the deb offline, then dpkg -i) has nothing
    // to start from.
    let root = scratch("publish-deb");
    let dist = dist(&root, COMMIT);
    let name = deb(COMMIT, "linux-arm64");
    add_deb(&dist, &name, true);
    let (code, _, err) = publish(&root, None);
    assert_eq!(code, Some(0), "{err}");
    let names: Vec<String> = released(&root).into_iter().map(|(n, _)| n).collect();
    assert!(names.contains(&name), "{names:?}");
    assert!(
        names.contains(&format!("{name}.sigstore.json")),
        "{names:?}"
    );
}
