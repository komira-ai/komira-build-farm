//! `cargo xtask <verb>`: kbf's CI logic, in Rust with tests, so each workflow step is one
//! command (the alias is in `.cargo/config.toml`).
//!
//! - `build-commit check <binary> <commit>` and `build-commit stamps <commit>`
//!   ([`build_commit`]): kbf-server names the commit it was built from.
//! - `budget-diff --base <rev>` ([`budget`]): the interpreter budget may only shrink; a
//!   row the working tree's budget file has and `<rev>`'s does not is refused. CI runs
//!   it on pull requests, with the base branch as `<rev>`.
//! - `sign-darwin <binary> --minos <version> [--summary <file>]` and `darwin-asset
//!   <tarball> <name>` ([`darwin`]): the macOS binary signed ad hoc with the hardened
//!   runtime and exactly the entitlements the allow list ([`entitlements`]) gives it,
//!   and checked again as it ships. macOS only.
//! - `fetch --url <url> --sha256 <hex> --out <path> [--extract <format> [--member <path>]]`
//!   ([`fetch`]): a pinned tool, downloaded over HTTPS and refused unless its SHA-256
//!   matches, before anything is unpacked or written.
//! - `package --binary <path> --sbom <path> --platform <platform> --commit <sha> --out
//!   <dir>` ([`package`]): one binary as a release asset, a tarball that is the same
//!   bytes on every runner, and its SBOM, with their lines in `SHA256SUMS.<platform>`.
//! - `sums assemble <dir>` and `verify-assets {sums,attestations} <dir>` ([`sums`]): the
//!   one `SHA256SUMS` over every platform's assets, and the checks the attest job and
//!   a deployment make of it, each with a control that must fail.
//! - `release {locate,main-control,check,publish}` ([`release`]): a GitHub release of
//!   the files a push run of the artifacts workflow on `main` built and attested for a
//!   commit on `main`, checked again and never rebuilt; a re-run adds nothing twice and
//!   replaces nothing.
//! - `podman-tests [--sudo <path>] [--podman <path>] [--runtime-dir <path>]`
//!   ([`podman_tests`]): the container driver's real-Podman tests on a hosted Linux
//!   runner, each test target in its own `Delegate=yes` system unit.
//! - `sudo-test -p <package> --test <name> [--sudo <path>] [-- <test args>]`
//!   ([`sudo_test`]): one integration test target built as this user, then its
//!   `#[ignore]` tests run as root through `sudo -n`.
//!
//! Exit status: 0 when the check passed, 1 when it failed (one reason per line on
//! stderr), 2 on a usage error.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

pub mod budget;
pub mod build_commit;
pub mod darwin;
pub mod entitlements;
pub mod fetch;
pub mod package;
pub mod podman_tests;
pub mod release;
pub mod sudo_test;
pub mod sums;

/// Why a verb failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("not a commit: {0:?} (expected 12 to 40 lowercase hex digits)")]
    NotACommit(String),
    #[error("--version is {version:?}; expected it to end +{}", &commit[..12])]
    WrongCommit { version: String, commit: String },
    #[error("control: {version:?} also passed for {}", &other[..12])]
    ControlPassed { version: String, other: String },
    #[error("{what}: {why}")]
    Spawn { what: String, why: String },
    #[error("{0}")]
    Failed(String),
    #[error("{}", .0.join("\n"))]
    Refused(Vec<String>),
    #[error(transparent)]
    Fetch(#[from] fetch::Error),
    #[error(transparent)]
    Entitlements(#[from] entitlements::Error),
    /// Flags that do not fit together (exit status 2, like a clap error).
    #[error("{0}")]
    Usage(String),
}

impl Error {
    /// The error for a program that could not be started, or a file not read: `what`
    /// and the system's reason.
    fn spawn(what: String) -> impl FnOnce(std::io::Error) -> Error {
        move |e| Error::Spawn {
            what,
            why: e.to_string(),
        }
    }
}

#[derive(Parser)]
#[command(name = "cargo xtask", about = "kbf's CI steps")]
struct Cli {
    /// The repository root (default: the workspace this xtask was built from).
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// kbf-server's `--version` names the commit it was built from.
    #[command(subcommand)]
    BuildCommit(BuildCommit),
    /// Refuse a row the budget file gains against `--base`.
    BudgetDiff {
        /// The revision to compare with (on a pull request, the base branch).
        #[arg(long)]
        base: String,
    },
    /// Download a pinned file, check its SHA-256 and install it (mode 0755).
    Fetch {
        /// Where to download it from (HTTPS; redirects must stay on HTTPS).
        #[arg(long)]
        url: String,
        /// The SHA-256 of the download (before unpacking), 64 lowercase hex digits.
        #[arg(long)]
        sha256: fetch::Sha256,
        /// The file to install; its directory is created when missing.
        #[arg(long)]
        out: PathBuf,
        /// How the download is packed (default: it is the file).
        #[arg(long, value_enum)]
        extract: Option<fetch::Format>,
        /// The file to take out of a tar (`--extract tar.gz` or `tar.xz`): its path
        /// inside the archive.
        #[arg(long)]
        member: Option<PathBuf>,
    },
    /// Package one binary and its SBOM as release assets in `--out`.
    Package {
        /// The binary; its file name is the asset's name.
        #[arg(long)]
        binary: PathBuf,
        /// Its CycloneDX SBOM.
        #[arg(long)]
        sbom: PathBuf,
        /// The platform it was built for.
        #[arg(long, value_enum)]
        platform: package::Platform,
        /// The commit it was built from (on Actions, `GITHUB_SHA`).
        #[arg(long)]
        commit: String,
        /// The directory the assets go in; created when missing.
        #[arg(long)]
        out: PathBuf,
    },
    /// Sign a macOS binary in place, ad hoc with the hardened runtime and the
    /// entitlements the allow list gives its file name, and check it (macOS only).
    SignDarwin {
        /// The binary; it must accept `--version`.
        binary: PathBuf,
        /// The minimum macOS it was built for (on Actions, `MACOSX_DEPLOYMENT_TARGET`).
        #[arg(long)]
        minos: String,
        /// Append a line on the signature to this file (on Actions, `GITHUB_STEP_SUMMARY`).
        #[arg(long)]
        summary: Option<PathBuf>,
    },
    /// Check the macOS binary `<name>` as it ships in `<tarball>` (macOS only).
    DarwinAsset { tarball: PathBuf, name: String },
    /// The commit's one `SHA256SUMS`.
    #[command(subcommand)]
    Sums(Sums),
    /// Check a commit's assets as a deployment is to.
    #[command(subcommand)]
    VerifyAssets(VerifyAssets),
    /// A GitHub release of a `main` commit's attested assets, never rebuilt.
    #[command(subcommand)]
    Release(ReleaseVerb),
    /// Run the container driver's real-Podman tests on a hosted Linux runner, each
    /// test target in its own Delegate=yes system unit as this user.
    PodmanTests {
        /// The sudo program; it is run with `-n`, so it never prompts.
        #[arg(long, default_value = "sudo")]
        sudo: PathBuf,
        /// The podman program.
        #[arg(long, default_value = "podman")]
        podman: PathBuf,
        /// This user's runtime directory, where its systemd user manager's bus appears
        /// (default: /run/user/<uid>).
        #[arg(long)]
        runtime_dir: Option<PathBuf>,
    },
    /// Build one integration test target, then run its ignored tests as root.
    SudoTest {
        /// The package the target is in.
        #[arg(short = 'p', long)]
        package: String,
        /// The integration test target (a file in the package's `tests/`).
        #[arg(long)]
        test: String,
        /// The sudo program; it is run with `-n`, so it never prompts.
        #[arg(long, default_value = "sudo")]
        sudo: PathBuf,
        /// Arguments for the test binary (after `--`), such as a test name.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum Sums {
    /// Merge `<dir>/SHA256SUMS.<platform>` into `<dir>/SHA256SUMS`, and remove them.
    Assemble {
        dir: PathBuf,
        /// Refuse a result that differs from this file.
        #[arg(long)]
        expect: Option<PathBuf>,
        /// Append the result to this file (on Actions, `GITHUB_STEP_SUMMARY`).
        #[arg(long)]
        summary: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum VerifyAssets {
    /// `<dir>/SHA256SUMS` lists every other file in `<dir>` once, and each matches.
    Sums { dir: PathBuf },
    /// Every asset `<dir>/SHA256SUMS` lists has the attestations a deployment checks.
    Attestations {
        dir: PathBuf,
        /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
        #[arg(long)]
        repo: String,
        /// The signing workflow, `<owner>/<repo>/<path>@<ref>` (on Actions,
        /// `GITHUB_WORKFLOW_REF`).
        #[arg(long)]
        workflow_ref: String,
        /// The branch the attestations were signed on, `refs/heads/<branch>` (on a push,
        /// `GITHUB_REF`). The default is main's: a release verifies as main.
        #[arg(long, default_value = "refs/heads/main")]
        source_ref: String,
        /// The `gh` program.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
}

#[derive(Subcommand)]
enum ReleaseVerb {
    /// Refuse a commit not on main; find the successful push run of artifacts.yml on
    /// main that built it, and print `run-id=<id>`.
    Locate {
        /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
        #[arg(long)]
        repo: String,
        /// The commit to release, a full SHA.
        #[arg(long)]
        commit: String,
        /// Append `run-id=<id>` to this file (on Actions, `GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
        /// The `gh` program.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
    /// The on-main check against GitHub: `--on-main` passes and `--off-main` is refused.
    MainControl {
        /// The repository, `<owner>/<repo>`.
        #[arg(long)]
        repo: String,
        /// A commit on main (on a pull request, its base commit).
        #[arg(long)]
        on_main: String,
        /// A commit not on main (on a pull request, its merge commit).
        #[arg(long)]
        off_main: String,
        /// The `gh` program.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
    /// `<dir>` passes `verify-assets sums` and lists only assets of `--commit`.
    Check {
        dir: PathBuf,
        /// The commit the assets were built from, a full SHA.
        #[arg(long)]
        commit: String,
    },
    /// Create release `--tag` with every file of `<dir>`, or add the files it lacks;
    /// an asset it holds must be one of them, byte for byte.
    Publish {
        dir: PathBuf,
        /// The repository, `<owner>/<repo>`.
        #[arg(long)]
        repo: String,
        /// The release's tag, which must exist (on Actions, `GITHUB_REF_NAME`).
        #[arg(long)]
        tag: String,
        /// The commit the tag names, a full SHA.
        #[arg(long)]
        commit: String,
        /// The artifacts workflow run that built and attested the files.
        #[arg(long)]
        run_id: u64,
        /// The `gh` program.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
}

#[derive(Subcommand)]
enum BuildCommit {
    /// `<binary> --version` names `<commit>`, and not the control commit.
    Check {
        binary: PathBuf,
        commit: String,
        /// Also `<binary> --version --json` (kbf): it names `<commit>` and a release
        /// build.
        #[arg(long)]
        json: bool,
    },
    /// A plain build, two override stamps and a malformed override, from the checkout.
    Stamps { commit: String },
}

/// The workspace this xtask was built from.
fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The budget file as `rev` holds it, read with `git show` in `root`.
fn budget_at(root: &Path, rev: &str) -> Result<String, Error> {
    let spec = format!("{rev}:{}", budget::BUDGET);
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &spec])
        .output()
        .map_err(Error::spawn("git".to_owned()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "git show {spec}: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `budget-diff --base <rev>`: the line it prints when nothing was added.
fn budget_diff(root: &Path, rev: &str) -> Result<String, Error> {
    let base = budget_at(root, rev)?;
    let path = root.join(budget::BUDGET);
    let head = std::fs::read_to_string(&path).map_err(Error::spawn(path.display().to_string()))?;
    let added = budget::new_rows(&base, &head).map_err(Error::Refused)?;
    if added.is_empty() {
        return Ok(format!("ok: {} adds no row to {rev}'s", budget::BUDGET));
    }
    Err(Error::Refused(
        added
            .iter()
            .map(|r| {
                format!(
                    "{}:{}: a new {} row for `{}`; the budget only shrinks: port it to \
                     Rust instead",
                    budget::BUDGET,
                    r.line,
                    r.section.header(),
                    r.key
                )
            })
            .collect(),
    ))
}

/// The cargo to run: the one that started this xtask, which sets `CARGO` to itself for
/// the programs it runs.
fn cargo_program() -> OsString {
    std::env::var_os("CARGO").unwrap_or(OsString::from("cargo"))
}

/// Runs the verb `cli` names: the lines to print when it passed.
fn run(cli: Cli) -> Result<Vec<String>, Error> {
    let root = cli.root.unwrap_or_else(default_root);
    match cli.verb {
        Verb::BuildCommit(BuildCommit::Check {
            binary,
            commit,
            json,
        }) => Ok(vec![build_commit::check(&binary, &commit, json)?]),
        Verb::BuildCommit(BuildCommit::Stamps { commit }) => {
            build_commit::stamps(&cargo_program(), &root, &commit)
        }
        Verb::SudoTest {
            package,
            test,
            sudo,
            args,
        } => sudo_test::run(&sudo_test::SudoTest {
            cargo: &cargo_program(),
            sudo: &sudo,
            root: &root,
            package: &package,
            test: &test,
            args: &args,
        }),
        Verb::PodmanTests {
            sudo,
            podman,
            runtime_dir,
        } => podman_tests::run(&podman_tests::PodmanTests {
            cargo: &cargo_program(),
            sudo: &sudo,
            podman: &podman,
            root: &root,
            runtime_dir: runtime_dir.as_deref(),
        }),
        Verb::BudgetDiff { base } => Ok(vec![budget_diff(&root, &base)?]),
        Verb::Fetch {
            url,
            sha256,
            out,
            extract,
            member,
        } => {
            let unpack = fetch::Unpack::new(extract, member).map_err(Error::Usage)?;
            let roots = fetch::host_roots();
            Ok(vec![fetch::fetch(&url, &sha256, &unpack, &out, &roots)?])
        }
        Verb::Package {
            binary,
            sbom,
            platform,
            commit,
            out,
        } => package::package(&binary, &sbom, platform, &commit, &out),
        #[cfg(target_os = "macos")]
        Verb::SignDarwin {
            binary,
            minos,
            summary,
        } => darwin::sign(
            &binary,
            &root.join(entitlements::ALLOWLIST),
            &minos,
            summary.as_deref(),
        ),
        #[cfg(target_os = "macos")]
        Verb::DarwinAsset { tarball, name } => {
            darwin::asset(&tarball, &name, &root.join(entitlements::ALLOWLIST))
        }
        #[cfg(not(target_os = "macos"))]
        Verb::SignDarwin { .. } => Err(darwin::needs_macos("sign-darwin")),
        #[cfg(not(target_os = "macos"))]
        Verb::DarwinAsset { .. } => Err(darwin::needs_macos("darwin-asset")),
        Verb::Sums(Sums::Assemble {
            dir,
            expect,
            summary,
        }) => sums::assemble(&dir, expect.as_deref(), summary.as_deref()),
        Verb::VerifyAssets(VerifyAssets::Sums { dir }) => {
            Ok(vec![sums::verify_sums(&dir, fetch::Sha256::of)?])
        }
        Verb::Release(ReleaseVerb::Locate {
            repo,
            commit,
            output,
            gh,
        }) => release::locate(&gh, &repo, &commit, output.as_deref()),
        Verb::Release(ReleaseVerb::MainControl {
            repo,
            on_main,
            off_main,
            gh,
        }) => Ok(vec![release::main_control(
            &gh, &repo, &on_main, &off_main,
        )?]),
        Verb::Release(ReleaseVerb::Check { dir, commit }) => release::check(&dir, &commit),
        Verb::Release(ReleaseVerb::Publish {
            dir,
            repo,
            tag,
            commit,
            run_id,
            gh,
        }) => {
            // The release's assets as it holds them, of this process alone.
            let scratch =
                std::env::temp_dir().join(format!("xtask-release-{}", std::process::id()));
            std::fs::create_dir_all(&scratch)
                .map_err(Error::spawn(format!("create {}", scratch.display())))?;
            let r = release::Release {
                repo: &repo,
                tag: &tag,
                commit: &commit,
                run_id,
                gh: &gh,
                scratch: &scratch,
            };
            let published = release::publish(&dir, &r);
            // Nothing in it is needed after the run, whatever its result.
            let _ = std::fs::remove_dir_all(&scratch);
            published
        }
        Verb::VerifyAssets(VerifyAssets::Attestations {
            dir,
            repo,
            workflow_ref,
            source_ref,
            gh,
        }) => {
            // The flipped copy's directory, of this process alone.
            let scratch = std::env::temp_dir().join(format!("xtask-verify-{}", std::process::id()));
            std::fs::create_dir_all(&scratch)
                .map_err(Error::spawn(format!("create {}", scratch.display())))?;
            let a = sums::Attestations {
                repo: &repo,
                workflow_ref: &workflow_ref,
                source_ref: &source_ref,
                gh: &gh,
                scratch: &scratch,
            };
            let verified = sums::verify_attestations(&dir, &a);
            // Nothing in it is needed after the run, whatever its result.
            let _ = std::fs::remove_dir_all(&scratch);
            verified
        }
    }
}

/// The program: parses the arguments, runs the verb and prints its result.
pub fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xtask: {e}");
            match e {
                Error::Usage(_) => ExitCode::from(2),
                _ => ExitCode::FAILURE,
            }
        }
    }
}
