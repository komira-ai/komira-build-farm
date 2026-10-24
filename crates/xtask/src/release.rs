//! `cargo xtask release`: a GitHub release of the exact files the artifacts stage of a push
//! run of the pipeline on `main` built and attested for one commit (docs/artifacts.md).
//! Nothing is rebuilt; `.github/workflows/release.yml` runs these verbs.
//!
//! - `release locate --repo <owner/repo> --commit <sha> --run-id <id> [--output <file>] [--gh
//!   <program>]` ([`locate`]): refuses a commit that is not on `main` (GitHub's compare
//!   of `main...<sha>` is neither `identical` nor `behind`), then a run that is not a
//!   successful `push` run on `main` for that exact commit (the run id is the pipeline run
//!   that started the release; nothing is searched for), then a run that no
//!   longer holds every artifact a release needs ([`ARTIFACTS`]). Prints `run-id=<id>`,
//!   and appends it to `--output` (on Actions, `GITHUB_OUTPUT`).
//! - `release main-control --repo <owner/repo> --on-main <sha> --off-main <sha>`
//!   ([`main_control`]): the on-main check against GitHub, both ways: `--on-main` must
//!   pass and `--off-main` must be refused. CI runs it on pull requests into `main`
//!   with the base commit and the pull request's merge commit, so a check that accepts
//!   every commit (or none) fails there instead of at a release.
//! - `release check <dir> --commit <sha> [--without-version-facts]` ([`check`]): `<dir>`
//!   passes `verify-assets sums` (so each `kbf` tarball has its version facts listed; the
//!   flag, for the release before the build jobs recorded them, waives that and nothing
//!   else: [`Pairing::Waived`]), every file `SHA256SUMS`
//!   lists is `<binary>-<sha>-<platform>.tar.gz` or `.cdx.json`, or `kbf`'s
//!   `kbf-<sha>-<platform>.version.json` ([`crate::version_facts`]): the assets of that
//!   commit and no other, and each tarball has exactly
//!   one provenance bundle, `<tarball>.sigstore.json`, whose one subject is that
//!   tarball's digest ([`bundles`]), and no bundle is there for anything else.
//! - `release verify <dir> --repo-dir <checkout> --tag <tag> --commit <sha> [--main-ref
//!   <ref>] [--output <file>]` ([`verify`]): the tag names that commit, which is on main.
//!   It reads no verdict: the tag's message is an audit record that nothing gates on, and a
//!   tag without one verifies. Prints `set-hash=<hex>`, the hash of the set `<dir>`'s
//!   `SHA256SUMS` lists, which `publish --set-hash` must be given.
//! - `release identity <dir> --commit <sha> --out <file>` ([`identity`]): runs `kbf --version
//!   --json` from `<dir>`'s linux-x86_64 tarball of that commit and writes what it prints. It
//!   runs a binary: only after `check` and `verify-assets attestations` on `<dir>`.
//! - `release previous --tag <tag> --commit <sha> [--repo-dir <checkout>] [--output
//!   <file>]` ([`notes::previous_release`]): the nearest earlier stable `v*` tag that is
//!   an ancestor of the commit; prints `previous=` and `previous-commit=` (empty for a
//!   first release).
//! - `release notes <dir> ... --version-json <file> [--previous-version-json <file>] --out
//!   <file>` ([`notes`]): the same checks, then the release's notes: first "Before you
//!   click" (what is not reversible, as `kbf_gate::reversible` and the `protocol` of the two
//!   builds' `kbf --version --json` say and never from the record's own list, unknown counting as
//!   not reversible; the waivers; the tag's Beta record in a line), then the commits since the
//!   previous release, the record, its escapes and what is withdrawn. Without the
//!   identity of the previous release, when there is one, it refuses (fail closed).
//! - `release plan --repo <owner/repo> --sha <sha>
//!   [--repo-dir <checkout>]
//!   [--freeze <text>] [--output <file>] [--gh <program>]` ([`plan`]): the next version
//!   (`VERSION` and the release tags), the class of what changed since the last release (code, site, pipeline), and the
//!   refusals: a freeze, a base below an existing tag, a superseded commit ([`tags::superseded`]).
//!   Prints `key=value` lines and appends them to `--output`.
//! - `release fetch-previous --repo <owner/repo> --sha <sha> --platform <p> --binary <name>...
//!   --out <dir> [--trusted-root <file>] [--output <file>] [--gh <program>]` ([`fetch_previous`]):
//!   the release before `--sha` for the tests that need an N-1: the highest release tag
//!   that names a strict ancestor of it, its release downloaded and verified
//!   (checksums, then attestations), and only then the named binaries taken out into `--out`. With no
//!   such tag it prints `previous-source=none`; a failed call is an error, never "no tags".
//!   Prints `key=value` lines and appends them to `--output`.
//! - `release publish <dir> --repo <owner/repo> --tag <tag> --commit <sha> --run-id <id>
//!   --set-hash <hex> --notes-file <file> [--gh <program>]` ([`publish`]): `check`, then
//!   creates release `<tag>` with every file of `<dir>` (the bundles beside their
//!   tarballs) and the notes file, verbatim, as the start of its body. When the release exists,
//!   every asset it holds must be one of these files, byte for byte, or nothing is
//!   changed; the files it lacks are uploaded. A re-run therefore creates no second
//!   release and replaces no asset. It refuses first when the highest release tag is not
//!   at `--commit` or an ancestor of it ([`tags::superseded`]).
//! - `release bump-internal --version <v> --sha <sha> --internal-repo <owner/repo> --submodule
//!   <name> [--repo <owner/repo>] [--workdir <dir>] [--gh <program>] [--git <program>]`
//!   ([`bump`]): after the release is published, a pull request in the internal repository
//!   that moves its submodule to `--sha`, with auto-merge on, if the tag `v<v>` names `--sha`
//!   and the submodule is not already at or beyond it. The token is `GH_TOKEN`.
//! - `release tag --repo <owner/repo> --sha <sha> --tag <tag> --message-file <file>
//!   [--tagger <login>] [--output <file>] [--gh <program>]` ([`tag`]): the annotated tag
//!   `--tag` at `--sha` through the GitHub API, create-only, after the same stale-run
//!   guards as `publish`. A tag that already exists is reused only when it names this
//!   commit with this message. Prints `tag=` and `created=true|false`.
//! - `release verdict-write <dir> --commit <sha> --tag <tag> --run-id <id> --pipeline-run <id>
//!   --out <file> [...]` ([`verdict_write`]): the
//!   Beta record the tag's message carries, which nothing gates on.

pub mod attest_gh;
pub mod bump;
pub mod fetch_previous;
mod git;
pub mod identity;
mod identity_file;
pub mod notes;
pub mod plan;
pub mod tag;
pub mod tags;
pub mod verdict_write;
pub mod verify;

use std::io::Write;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::Error;
use crate::build_commit::full_commit;
use crate::bundles;
use crate::fetch::Sha256;
use crate::package::{Platform, read};
use crate::pkg_image_asset;
use crate::sums::{self, SUMS};
use crate::version_facts::{self, Pairing};

/// The branch a released commit must be on.
pub const MAIN: &str = "main";

/// The workflow whose jobs, called by the pipeline on a push to `main`, build and attest the
/// assets (the signer a deployment names).
pub const ARTIFACTS_WORKFLOW: &str = "artifacts.yml";

/// The artifacts such a run leaves, all of which a release needs: the files, the sums,
/// and the provenance bundles of each platform group's tarballs (the `attest-*` jobs').
pub const ARTIFACTS: [&str; 6] = [
    "assets-darwin-arm64",
    "assets-linux-x86_64",
    "assets-linux-arm64",
    "sha256sums",
    "bundles-darwin-arm64",
    "bundles-linux",
];

/// `gh api <path>`: the JSON it printed, or `None` when GitHub answered 404.
pub(crate) fn api(gh: &Path, path: &str) -> Result<Option<Value>, Error> {
    let out = Command::new(gh)
        .args(["api", path])
        .output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        if err.contains("(HTTP 404)") {
            return Ok(None);
        }
        return Err(Error::Failed(format!("gh api {path}: {}", err.trim_end())));
    }
    let json = serde_json::from_slice(&out.stdout);
    json.map(Some)
        .map_err(|e| Error::Failed(format!("gh api {path}: not JSON: {e}")))
}

/// Appends `lines` to `output` (on Actions, `GITHUB_OUTPUT`), one per line.
fn append_output(output: &Path, lines: &[String]) -> Result<(), Error> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output)
        .and_then(|mut f| writeln!(f, "{}", lines.join("\n")))
        .map_err(Error::spawn(format!("append to {}", output.display())))
}

/// Runs `gh <args>`; an error holds what it printed on stderr.
fn gh_run(gh: &Path, args: &[&str]) -> Result<(), Error> {
    let what = format!("gh {}", args[..2].join(" "));
    let out = Command::new(gh)
        .args(args)
        .output()
        .map_err(Error::spawn(gh.display().to_string()))?;
    let err = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Failed(format!("{what}: {}", err.trim_end())))
}

/// Whether `commit` is on `main`: `main` holds it (compared, `main` is identical to it
/// or ahead of it). An unknown commit is not.
pub fn on_main(gh: &Path, repo: &str, commit: &str) -> Result<bool, Error> {
    let compared = api(gh, &format!("repos/{repo}/compare/{MAIN}...{commit}"))?;
    let status = compared.as_ref().and_then(|v| v["status"].as_str());
    Ok(matches!(status, Some("identical" | "behind")))
}

/// The `release locate` verb: the lines to print. `run_id` is the pipeline run that built
/// `commit` (on Actions, the id of the run the release workflow was started by).
pub fn locate(
    gh: &Path,
    repo: &str,
    commit: &str,
    run_id: u64,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    full_commit("--commit", commit)?;
    if !on_main(gh, repo, commit)? {
        return Err(Error::Failed(format!(
            "{commit} is not on {MAIN}: a release is cut only from a commit on {MAIN}"
        )));
    }
    let id = run_id;
    let run = api(gh, &format!("repos/{repo}/actions/runs/{id}"))?;
    let is_the_push_run = run.as_ref().is_some_and(|r| {
        r["head_sha"] == commit
            && r["head_branch"] == MAIN
            && r["event"] == "push"
            && r["conclusion"] == "success"
    });
    if !is_the_push_run {
        return Err(Error::Failed(format!(
            "run {id} is not a successful push run on {MAIN} for {commit}"
        )));
    }
    let artifacts = api(
        gh,
        &format!("repos/{repo}/actions/runs/{id}/artifacts?per_page=100"),
    )?;
    let kept: Vec<&str> = artifacts
        .as_ref()
        .and_then(|v| v["artifacts"].as_array())
        .into_iter()
        .flatten()
        .filter(|a| a["expired"] == false)
        .filter_map(|a| a["name"].as_str())
        .collect();
    let missing: Vec<String> = ARTIFACTS
        .iter()
        .filter(|a| !kept.contains(*a))
        .map(|a| format!("run {id} holds no unexpired artifact {a}"))
        .collect();
    if !missing.is_empty() {
        return Err(Error::Refused(missing));
    }
    let line = format!("run-id={id}");
    if let Some(output) = output {
        append_output(output, std::slice::from_ref(&line))?;
    }
    Ok(vec![
        format!("{commit} is on {MAIN}; run {id} built it"),
        line,
    ])
}

/// The `release main-control` verb: the line to print.
pub fn main_control(gh: &Path, repo: &str, on: &str, off: &str) -> Result<String, Error> {
    full_commit("--on-main", on)?;
    full_commit("--off-main", off)?;
    let mut problems = Vec::new();
    if !on_main(gh, repo, on)? {
        problems.push(format!("control failed: {on}, on {MAIN}, was refused"));
    }
    if on_main(gh, repo, off)? {
        problems.push(format!(
            "control failed: {off}, not on {MAIN}, was accepted"
        ));
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(format!(
        "on-{MAIN} check: {on} is accepted and {off} is refused"
    ))
}

/// Whether `name` is an asset of `commit`: `<binary>-<commit>-<platform>` and
/// `.tar.gz`, `.cdx.json`, `.deb`, `.pkg` or `.oci.tar` (the packages; and the frontend or coordinator image,
/// [`crate::pkg_image_asset`], which exists for the Linux platforms only; nfpm's own `kbf-installer_<version>_
/// <arch>.deb` and `pkg build`'s `kbf-installer-<version>-macos-arm64.pkg` name no commit and are
/// not one), or the version facts of `kbf` for a platform ([`version_facts::is_facts_of`]).
fn is_asset_of(name: &str, commit: &str) -> bool {
    if version_facts::is_facts_of(name, commit) {
        return true;
    }
    let stem = name
        .strip_suffix(".tar.gz")
        .or_else(|| name.strip_suffix(".cdx.json"))
        .or_else(|| name.strip_suffix(".deb"))
        .or_else(|| name.strip_suffix(".pkg"))
        .or_else(|| name.strip_suffix(pkg_image_asset::SUFFIX));
    let parts = stem.and_then(|s| s.split_once(&format!("-{commit}-")));
    let is_image = name.ends_with(pkg_image_asset::SUFFIX);
    parts.is_some_and(|(binary, platform)| {
        let platform = Platform::named(platform);
        !binary.is_empty()
            && platform.is_some()
            && (!is_image
                || (pkg_image_asset::Image::named(binary).is_some()
                    && matches!(platform, Some(Platform::LinuxX86_64 | Platform::LinuxArm64))))
    })
}

/// The `release check` verb: the lines to print, then every file name of `dir`
/// (`SHA256SUMS` first, then the listed files, then the bundles).
fn checked(
    dir: &Path,
    commit: &str,
    pairing: Pairing,
) -> Result<(Vec<String>, Vec<String>), Error> {
    full_commit("--commit", commit)?;
    let sums_line = sums::verify_sums_with(dir, Sha256::of, pairing)?;
    let lines = sums::read_sums(&dir.join(SUMS))?;
    let foreign: Vec<String> = lines
        .iter()
        .filter(|l| !is_asset_of(&l.name, commit))
        .map(|l| format!("{}: not an asset of {commit}", l.name))
        .collect();
    if !foreign.is_empty() {
        return Err(Error::Refused(foreign));
    }
    let unpaired = bundles::pairing(dir, &lines)?;
    if !unpaired.is_empty() {
        return Err(Error::Refused(unpaired));
    }
    let mut names = vec![SUMS.to_owned()];
    names.extend(lines.iter().map(|l| l.name.clone()));
    let tarballs: Vec<&str> = lines
        .iter()
        .map(|l| l.name.as_str())
        .filter(|n| n.ends_with(".tar.gz"))
        .collect();
    let packages: Vec<&str> = lines
        .iter()
        .map(|l| l.name.as_str())
        .filter(|n| bundles::is_package(n))
        .collect();
    names.extend(
        tarballs
            .iter()
            .chain(&packages)
            .map(|t| bundles::name_of(t)),
    );
    let and_packages = if packages.is_empty() {
        String::new()
    } else {
        format!(" and {} package(s)", packages.len())
    };
    let printed = vec![
        sums_line,
        format!("names: every listed file is an asset of {commit}"),
        format!(
            "bundles: each of the {} tarballs{and_packages} has one provenance bundle",
            tarballs.len()
        ),
    ];
    Ok((printed, names))
}

/// The `release check` verb: the lines to print. `pairing` is [`Pairing::Waived`] only for
/// the release before the build jobs recorded version facts (`--without-version-facts`).
pub fn check(dir: &Path, commit: &str, pairing: Pairing) -> Result<Vec<String>, Error> {
    Ok(checked(dir, commit, pairing)?.0)
}

/// The most characters GitHub takes in a release body.
const BODY_CAP: usize = 125_000;

/// The notes in `path`, which `release publish` requires: a missing, unreadable, non-UTF-8
/// or blank file is a refusal, never a quiet fall back to another body.
pub fn read_notes(path: &Path) -> Result<String, Error> {
    let text = std::fs::read_to_string(path).map_err(Error::spawn(format!(
        "read --notes-file {}",
        path.display()
    )))?;
    if text.trim().is_empty() {
        return Err(Error::Failed(format!(
            "--notes-file {} is empty: a release is made with its notes",
            path.display()
        )));
    }
    Ok(text)
}

/// The commit tag `tag` names, in full.
pub(crate) fn commit_of(repo: &Path, tag: &str) -> Result<String, Error> {
    git::commit_of(repo, tag)
}

/// Which release `release publish` makes or completes.
pub struct Release<'a> {
    /// The repository, `<owner>/<repo>`.
    pub repo: &'a str,
    /// The tag, which must exist (`gh release create --verify-tag`).
    pub tag: &'a str,
    /// The commit the tag names, whose assets these are.
    pub commit: &'a str,
    /// The run of the artifacts workflow that built and attested them.
    pub run_id: u64,
    /// The set hash `release verify` printed: the files must
    /// be that set, or nothing is published.
    pub set_hash: &'a kbf_gate::SetHash,
    /// The notes `release notes` wrote: the first part of the release's body.
    pub notes: &'a str,
    /// The `gh` program.
    pub gh: &'a Path,
    /// An empty directory, for the assets the release already holds.
    pub scratch: &'a Path,
}

impl Release<'_> {
    /// The release's body: the notes, verbatim, then where the files come from and how to
    /// verify one, online and (for a tarball) offline with the bundle released beside it.
    fn body(&self) -> Result<String, Error> {
        let repo = self.repo;
        let body = format!(
            "{notes}\n## The files\n\nThe files run {id} of the pipeline built and attested for {commit} on \
             {MAIN} (https://github.com/{repo}/actions/runs/{id}), unchanged: a release \
             rebuilds nothing.\n\nVerify an asset before use:\n\n```text\ngh attestation \
             verify <asset> --repo {repo} --signer-workflow \
             {repo}/.github/workflows/{ARTIFACTS_WORKFLOW} --source-ref refs/heads/{MAIN} \
             --deny-self-hosted-runners\n```\n\nEach tarball and each package (`.deb` or `.pkg`) also has its provenance bundle, \
             `<asset>{SUFFIX}`, which verifies it with no network but a trusted root \
             (`gh attestation trusted-root > trusted_root.jsonl`):\n\n```text\ngh attestation \
             verify <asset> --bundle <asset>{SUFFIX} --custom-trusted-root trusted_root.jsonl \
             --repo {repo} --signer-workflow {repo}/.github/workflows/{ARTIFACTS_WORKFLOW} \
             --cert-oidc-issuer {ISSUER} --source-ref refs/heads/{MAIN} --source-digest \
             {commit} --predicate-type {PROVENANCE} --deny-self-hosted-runners\n```\n",
            notes = self.notes.trim_end(),
            id = self.run_id,
            commit = self.commit,
            SUFFIX = bundles::SUFFIX,
            ISSUER = bundles::ISSUER,
            PROVENANCE = bundles::PROVENANCE,
        );
        let len = body.chars().count();
        if len > BODY_CAP {
            return Err(Error::Failed(format!(
                "the release body is {len} characters; GitHub takes {BODY_CAP}"
            )));
        }
        Ok(body)
    }
}

/// Rule S compares the commit with the highest tag only, so `--tag` is checked too: it
/// is the highest tag when it already names this commit (a re-run of the same release),
/// and otherwise it must be strictly above the highest existing release tag.
fn tag_is_current(tag: &str, standing: &tags::Standing) -> Result<(), Error> {
    let given = tags::Version::parse_tag(tag);
    let (top, ok) = match standing {
        tags::Standing::Untagged => return Ok(()),
        tags::Standing::Is(top) => (top, given == Some(*top)),
        tags::Standing::Ahead(top) => (top, given.is_some_and(|v| v > *top)),
    };
    if ok {
        return Ok(());
    }
    Err(Error::Refused(vec![format!(
        "--tag {tag} is no release above the highest release tag {}: a release tag \
         is v<MAJOR>.<MINOR>.<PATCH>, strictly above the highest unless it names this commit",
        top.tag()
    )]))
}

/// The `release publish` verb: the lines to print.
pub fn publish(dir: &Path, r: &Release) -> Result<Vec<String>, Error> {
    let (mut printed, names) = checked(dir, r.commit, Pairing::Required)?;
    let held_set = verify::set_hash_of(dir)?;
    if &held_set != r.set_hash {
        return Err(Error::Refused(vec![format!(
            "the files in {} are set {}, `release verify` printed set {}: other bytes than it verified",
            dir.display(),
            held_set.as_str(),
            r.set_hash.as_str()
        )]));
    }
    // Refused before GitHub is asked anything (not even the tags), whether or not the
    // release exists.
    r.body()?;
    // Rule S, before the first read of the release: an old run does nothing.
    let standing = tags::superseded(r.gh, r.repo, r.commit)?;
    tag_is_current(r.tag, &standing)?;
    let paths: Vec<String> = names
        .iter()
        .map(|n| dir.join(n).display().to_string())
        .collect();
    let path_args = paths.iter().map(String::as_str);
    let held = api(r.gh, &format!("repos/{}/releases/tags/{}", r.repo, r.tag))?;
    let Some(held) = held else {
        let notes = r.body()?;
        let mut args = vec!["release", "create", r.tag, "--repo", r.repo, "--verify-tag"];
        args.extend(["--title", r.tag, "--notes", &notes]);
        args.extend(path_args);
        gh_run(r.gh, &args)?;
        printed.push(format!(
            "created release {} with {} files",
            r.tag,
            names.len()
        ));
        return Ok(printed);
    };
    let held: Vec<&str> = held["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str())
        .collect();
    if !held.is_empty() {
        let scratch = r.scratch.display().to_string();
        gh_run(
            r.gh,
            &[
                "release", "download", r.tag, "--repo", r.repo, "--dir", &scratch,
            ],
        )?;
    }
    let mut problems = Vec::new();
    for name in &held {
        if !names.iter().any(|n| n == name) {
            problems.push(format!(
                "release {} holds {name}, not an asset of {}",
                r.tag, r.commit
            ));
        } else if read(&r.scratch.join(name))? != read(&dir.join(name))? {
            problems.push(format!(
                "release {} holds {name} with other bytes than the verified file",
                r.tag
            ));
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    let missing: Vec<&str> = names
        .iter()
        .zip(paths.iter())
        .filter(|(n, _)| !held.contains(&n.as_str()))
        .map(|(_, p)| p.as_str())
        .collect();
    if !missing.is_empty() {
        let mut args = vec!["release", "upload", r.tag, "--repo", r.repo];
        args.extend(missing.iter().copied());
        gh_run(r.gh, &args)?;
    }
    printed.push(format!(
        "release {}: {} files already held byte for byte, {} uploaded",
        r.tag,
        held.len(),
        missing.len()
    ));
    Ok(printed)
}
