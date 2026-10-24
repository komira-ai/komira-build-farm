//! `cargo xtask <verb>`: kbf's CI logic, in Rust with tests, so each workflow step is one
//! command (the alias is in `.cargo/config.toml`).
//!
//! - `build-commit check <binary> <commit>`, `build-commit check-asset <tarball>
//!   <commit>` and `build-commit stamps <commit>` ([`build_commit`]): kbf-server names the
//!   commit it was built from, and `kbf` does in the tarball that ships.
//! - `advisories --repo <owner>/<repo> [--commit <sha>]` ([`advisories`]): `cargo deny check
//!   advisories`, kept in one GitHub issue (opened, updated, or commented on and closed when
//!   none remain) instead of failing a pull request. Exit 0 whatever it found; 1 when
//!   cargo-deny or `gh` failed. `advisories.yml` runs it daily and on every push to `main`.
//! - `deploy-farm --url <url> --version <v> [--deadline-min 60] [--stall-min 20]` ([`deploy_farm`]): after a
//!   release is published, set it as the update coordinator's desired version (CI's OIDC token)
//!   and wait until the farm converges; exit 1 when it is blocked, stalled or out of time. An
//!   empty `--url` prints a notice and exits 0.
//! - `budget-diff --base <rev>` ([`budget`]): the interpreter budget may only shrink; a
//!   row the working tree's budget file has and `<rev>`'s does not is refused, unless
//!   git sees the row's path renamed from a path `<rev>` has a row for. CI runs
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
//! - `installer-tarball --binary <path> --platform <platform> --commit <sha> --units
//!   <dir> --trust <dir> --sysusers <file> --tmpfiles <file> --needrestart <file> --out <dir>`
//!   ([`installer_tarball`]): the files the Debian package ships, at the same paths and
//!   modes (one list, [`pkg_linux::files`]), in a tarball that is the same bytes on every
//!   runner, for `packaging/install.sh`.
//! - `pkg-config`, `pkg-build`, `pkg-install-test` ([`pkg_linux`], [`pkg_install`]): the
//!   `kbf-installer` deb and rpm and the `kbf` deb and rpm (`/usr/bin/kbf` and the units of
//!   the modes): the nfpm configuration of both from the inputs, a package built twice to
//!   prove the bytes, and, for `kbf-installer`, an install, remove and purge in a container.
//! - `pkg-trust` and `pkg-asset` ([`pkg_trust`], [`pkg_asset`]): the trust directory of the
//!   package from the pinned `gh`, `gh`'s trusted root and the branch the package is
//!   attested on, and the built deb as the release asset
//!   `kbf-installer-<commit>-<platform>.deb`, refused unless it holds the binary of the
//!   attested tarball and trusts the branch it is attested for, and holds nothing of the
//!   `kbf` package.
//! - `pkg-kbf-asset` ([`pkg_kbf_asset`]): the `kbf` deb as the release asset
//!   `kbf-<commit>-<platform>.deb`, refused unless its `/usr/bin/kbf` is the `kbf` of the
//!   attested tarball, it depends on no `kbf-installer` and replaces `kbf-installer`, and it
//!   holds the units of the modes and nothing else.
//! - `pkg-image-asset` ([`pkg_image_asset`]): the frontend or coordinator OCI image tarball as the
//!   release asset `kbf-<service>-image-<commit>-<platform>.oci.tar`, refused unless its program
//!   is the attested `kbf` (frontend) or the built `kbf-coordinator`, it runs as a user that is
//!   not root with the service's entrypoint, and it is built for the platform.
//! - [`pkg_version`] and [`pkg_size`] (no verb yet): the version a `kbf-installer` package
//!   carries, in the one form a deb and an rpm put in the same order, and the most a package
//!   that carries a kbf release may weigh. `pkg-config` and `pkg-asset` take them up with
//!   the release.
//! - `pkg-macos-asset` and `pkg-macos-install-test` ([`pkg_macos_asset`],
//!   [`pkg_macos_install`]): the Mac `.pkg` as the release asset
//!   `kbf-installer-<commit>-darwin-arm64.pkg`, refused unless it holds the attested
//!   binary, the pinned `gh`, a trusted root and trusts the branch it is attested for; and
//!   its install as root on a Mac, every file checked and nothing started.
//! - `pkg build --binary <path> --launchd-plist <path> [--trust <path>]... --version <v>
//!   --out <dir>` and `pkg cask` ([`pkg`]): kbf-installer for a Mac, a `.pkg` that is
//!   the same bytes on every host and installs a root-owned tree and a LaunchDaemon that
//!   is not loaded, and the Homebrew cask for it (no formula: contract section 7).
//! - `sums assemble <dir>` and `verify-assets {sums,attestations} <dir>` ([`sums`]): the
//!   one `SHA256SUMS` over every platform's assets, and the checks the attest job and
//!   a deployment make of it, each with a control that must fail.
//! - Each tarball's provenance bundle, `<tarball>.sigstore.json` ([`bundles`]): the
//!   attestation a host verifies the tarball with offline. `verify-assets
//!   attestations` and `release check` hold a directory to one bundle per tarball,
//!   each that tarball's (its one subject is the tarball's digest); `verify-assets
//!   attestations` also has `gh` verify each tarball against its bundle.
//! - `release {locate,main-control,check,verify,notes,plan,identity,previous,fetch-previous,publish,bump-internal}` ([`release`]): a GitHub release of
//!   the files the artifacts stage of a push run of the pipeline on `main` built and attested for a
//!   commit on `main`, checked again and never rebuilt; a re-run adds nothing twice and
//!   replaces nothing.
//! - `release version-facts --binary <kbf> --commit <sha> --platform <p> --out <dir>`
//!   ([`version_facts`]): the `kbf` just built, run with `--version --json`, and what it
//!   printed kept unchanged as the release asset `kbf-<commit>-<platform>.version.json`
//!   with its line in `SHA256SUMS.<platform>`. The release set holds it for every `kbf`
//!   tarball (`verify-assets sums`, `release check`), because only a runner of a platform
//!   can run its binary.
//! - `crun-tests [--sudo <path>]` ([`crun_tests`]): the container driver's real-`crun`
//!   tests on a hosted Linux runner, each test target in its own `Delegate=yes` system
//!   unit.
//! - `sudo-test -p <package> --test <name> [--sudo <path>] [-- <test args>]`
//!   ([`sudo_test`]): one integration test target built as this user, then its
//!   `#[ignore]` tests run as root through `sudo -n`.
//!
//! Exit status: 0 when the check passed, 1 when it failed (one reason per line on
//! stderr), 2 on a usage error.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

pub mod advisories;
pub mod budget;
pub mod build_commit;
pub mod bundles;
pub mod crun_tests;
pub mod darwin;
pub mod deploy_farm;
pub mod entitlements;
pub mod fetch;
pub mod installer_tarball;
pub mod package;
pub mod pkg;
pub mod pkg_asset;
pub mod pkg_image_asset;
pub mod pkg_install;
pub mod pkg_kbf_asset;
pub mod pkg_linux;
pub mod pkg_macos_asset;
#[cfg(target_os = "macos")]
pub mod pkg_macos_install;
pub mod pkg_size;
pub mod pkg_trust;
pub mod pkg_version;
pub mod release;
pub mod sudo_test;
pub mod sums;
pub mod version_facts;

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
    /// Keep the advisories `cargo deny` reports in one GitHub issue.
    Advisories(advisories::Args),
    /// Set a published release as the farm's desired version on the update coordinator and
    /// wait for the farm to converge.
    DeployFarm(deploy_farm::Args),
    /// Refuse a row the budget file gains against `--base`.
    BudgetDiff {
        /// The revision to compare with (on a pull request, the base branch).
        #[arg(long)]
        base: String,
    },
    /// Append `SOURCE_DATE_EPOCH=<seconds>`, the committer time of `--commit`, to `--output`
    /// (on Actions, `GITHUB_ENV`), so a later step builds an image that carries the commit's
    /// time and no clock of the build's.
    SourceDateEpoch {
        /// The commit (`$GITHUB_SHA`), 12 to 40 lowercase hex digits.
        #[arg(long)]
        commit: String,
        /// The file to append the line to.
        #[arg(long)]
        output: PathBuf,
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
        /// The file to take out of a tar or zip (`--extract tar.gz`, `tar.xz` or `zip`): its
        /// path inside the archive.
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
    /// Write the payload `packaging/install.sh` places (installer, unit files, trust
    /// files) as a tarball in `--out`.
    InstallerTarball {
        /// The `kbf-installer` binary.
        #[arg(long)]
        binary: PathBuf,
        /// The platform it was built for (Linux only).
        #[arg(long, value_enum)]
        platform: package::Platform,
        /// The commit it was built from (on Actions, `GITHUB_SHA`).
        #[arg(long)]
        commit: String,
        /// The directory of `kbf-*.service` files (and `kbf.slice`).
        #[arg(long)]
        units: PathBuf,
        /// The directory of the three trust files.
        #[arg(long)]
        trust: PathBuf,
        /// The sysusers.d file.
        #[arg(long)]
        sysusers: PathBuf,
        /// The tmpfiles.d file.
        #[arg(long)]
        tmpfiles: PathBuf,
        /// The needrestart drop-in.
        #[arg(long)]
        needrestart: PathBuf,
        /// The directory the tarball goes in; created when missing.
        #[arg(long)]
        out: PathBuf,
    },
    /// Write the nfpm configuration and the file listing of the kbf-installer packages, and of
    /// the kbf packages in `<out>/kbf`.
    PkgConfig {
        /// The kbf-installer binary.
        #[arg(long)]
        binary: PathBuf,
        /// The kbf binary the kbf packages carry at `/usr/bin/kbf` (mode 0755).
        #[arg(long)]
        kbf: PathBuf,
        /// The directory of `kbf-*.service` files: the units of the modes go in the kbf
        /// packages, the others in the kbf-installer packages.
        #[arg(long)]
        units: PathBuf,
        /// The directory of the three trust files.
        #[arg(long)]
        trust: PathBuf,
        /// The sysusers.d file.
        #[arg(long)]
        sysusers: PathBuf,
        /// The tmpfiles.d file.
        #[arg(long)]
        tmpfiles: PathBuf,
        /// The needrestart drop-in.
        #[arg(long)]
        needrestart: PathBuf,
        /// The platform the binary was built for (`linux-x86_64`, `linux-arm64`).
        #[arg(long, value_enum)]
        platform: package::Platform,
        /// The package version.
        /// (One of the two is required; the verb says so, exit 2, as clap does for a bad flag.)
        #[arg(long, conflicts_with = "version_of")]
        version: Option<String>,
        /// Take the version from this binary's `--version` (`<version>+<12 of the commit>`).
        #[arg(long)]
        version_of: Option<PathBuf>,
        /// The directory `nfpm.yaml` and `listing.txt` go in; created when missing. The kbf
        /// package's pair goes in `<out>/kbf`.
        #[arg(long)]
        out: PathBuf,
    },
    /// Build the deb and the rpm with nfpm, and prove a second build is the same bytes.
    PkgBuild {
        /// The pinned nfpm program.
        #[arg(long)]
        nfpm: PathBuf,
        /// The `nfpm.yaml` of `pkg-config`.
        #[arg(long)]
        config: PathBuf,
        /// The directory the packages go in; created when missing.
        #[arg(long)]
        out: PathBuf,
        /// The time the packages carry, in seconds since the epoch.
        #[arg(long, default_value_t = pkg_linux::SOURCE_DATE_EPOCH)]
        source_date_epoch: u64,
    },
    /// Write the trust directory of the kbf-installer packages: the pinned `gh`, the
    /// Sigstore trusted root, and an `installer.conf` trusting the branch given.
    PkgTrust {
        /// The pinned `gh` program (copied, mode 0755).
        #[arg(long)]
        gh: PathBuf,
        /// The trusted root `gh attestation trusted-root` wrote (JSON lines).
        #[arg(long)]
        trusted_root: PathBuf,
        /// The branch the package is attested on: `refs/heads/<branch>`.
        #[arg(long)]
        source_ref: String,
        /// The directory the three files go in; created when missing.
        #[arg(long)]
        out: PathBuf,
    },
    /// The kbf-installer deb as a release asset, refused unless it holds the attested
    /// binary and trusts the branch it is attested for.
    PkgAsset {
        /// The deb nfpm built, or the directory holding the one (with `--verify-only`: the
        /// asset itself).
        #[arg(long)]
        package: PathBuf,
        /// The platform it was built for (`linux-x86_64`, `linux-arm64`).
        #[arg(long, value_enum)]
        platform: package::Platform,
        /// The commit it was built from (`$GITHUB_SHA`).
        #[arg(long)]
        commit: String,
        /// The `kbf-installer-<commit>-<platform>.tar.gz` whose binary the deb must hold.
        #[arg(long)]
        binary_tarball: PathBuf,
        /// The branch the deb must trust, and is attested for (`$GITHUB_REF`).
        #[arg(long)]
        expect_ref: String,
        /// The pinned `gh` the deb's must be byte for byte (optional with --verify-only).
        #[arg(long)]
        gh: Option<PathBuf>,
        /// The directory the asset and its line of `SHA256SUMS.<platform>` go in.
        #[arg(
            long,
            required_unless_present = "verify_only",
            conflicts_with = "verify_only"
        )]
        out: Option<PathBuf>,
        /// Only check `--package`, which must be named as the asset; write nothing.
        #[arg(long)]
        verify_only: bool,
    },
    /// The kbf deb as a release asset, refused unless its `/usr/bin/kbf` is the attested
    /// `kbf`, it depends on no kbf-installer and replaces it, and it holds the units of the
    /// modes and nothing else.
    PkgKbfAsset {
        /// The deb nfpm built, or the directory holding the one (with `--verify-only`: the
        /// asset itself).
        #[arg(long)]
        package: PathBuf,
        /// The platform it was built for (`linux-x86_64`, `linux-arm64`).
        #[arg(long, value_enum)]
        platform: package::Platform,
        /// The commit it was built from (`$GITHUB_SHA`).
        #[arg(long)]
        commit: String,
        /// The `kbf-<commit>-<platform>.tar.gz` whose `kbf` the deb must hold.
        #[arg(long)]
        kbf_tarball: PathBuf,
        /// The directory the asset and its line of `SHA256SUMS.<platform>` go in.
        #[arg(
            long,
            required_unless_present = "verify_only",
            conflicts_with = "verify_only"
        )]
        out: Option<PathBuf>,
        /// Only check `--package`, which must be named as the asset; write nothing.
        #[arg(long)]
        verify_only: bool,
    },
    /// The frontend or coordinator OCI image tarball as a release asset, refused unless its
    /// program is the attested `kbf` (or the built `kbf-coordinator`), its entrypoint is the
    /// service's, it runs as a user that is not root and it is built for the platform.
    PkgImageAsset {
        /// The image tarball `docker buildx build --output type=oci` wrote (with
        /// `--verify-only`: the asset itself).
        #[arg(long)]
        image: PathBuf,
        /// The service the image runs.
        #[arg(long, value_enum)]
        service: pkg_image_asset::Image,
        /// The platform it was built for (`linux-x86_64`, `linux-arm64`).
        #[arg(long, value_enum)]
        platform: package::Platform,
        /// The commit it was built from (`$GITHUB_SHA`).
        #[arg(long)]
        commit: String,
        /// The frontend's `kbf-<commit>-<platform>.tar.gz` whose `kbf` the image must hold.
        #[arg(long, conflicts_with = "binary")]
        kbf_tarball: Option<PathBuf>,
        /// The coordinator's `kbf-coordinator` the image must hold (it has no tarball; not
        /// with `--verify-only`, where the build's file is not at hand).
        #[arg(long, conflicts_with = "verify_only")]
        binary: Option<PathBuf>,
        /// The directory the asset and its line of `SHA256SUMS.<platform>` go in.
        #[arg(
            long,
            required_unless_present = "verify_only",
            conflicts_with = "verify_only"
        )]
        out: Option<PathBuf>,
        /// Only check `--image`, which must be named as the asset; write nothing.
        #[arg(long)]
        verify_only: bool,
    },
    /// Install a package into a fresh container and check it never starts or enables.
    PkgInstallTest {
        /// The package, or the directory holding the one package of its format.
        #[arg(long)]
        package: PathBuf,
        /// Its format.
        #[arg(long, value_enum)]
        format: pkg_install::Format,
        /// The `listing.txt` of `pkg-config`.
        #[arg(long)]
        listing: PathBuf,
        /// The directory holding the Dockerfile of the container.
        #[arg(long)]
        image_dir: PathBuf,
        /// A packaged unit and the file it needs to start: `<unit>=<file>`.
        #[arg(long)]
        gate: Vec<pkg_install::Gate>,
        /// A command to run in the container after the install and what its output must
        /// contain: `<program> <argument>..::<text>` (the command exits 0).
        #[arg(long)]
        probe: Vec<pkg_install::Probe>,
        /// The docker program.
        #[arg(long, default_value = "docker")]
        docker: PathBuf,
    },
    /// The kbf-installer `.pkg` as a release asset, refused unless it holds the attested
    /// binary, the pinned gh, a trusted root and trusts the branch it is attested for.
    PkgMacosAsset {
        /// The `.pkg` `pkg build` wrote, or the directory holding the one (with
        /// `--verify-only`: the asset itself).
        #[arg(long)]
        package: PathBuf,
        /// The commit it was built from (`$GITHUB_SHA`).
        #[arg(long)]
        commit: String,
        /// The `kbf-installer-<commit>-darwin-arm64.tar.gz` whose binary the package must hold.
        #[arg(long)]
        binary_tarball: PathBuf,
        /// The branch the package must trust, and is attested for (`$GITHUB_REF`).
        #[arg(long)]
        expect_ref: String,
        /// The pinned `gh` the package's must be byte for byte (optional with --verify-only).
        #[arg(long)]
        gh: Option<PathBuf>,
        /// The directory the asset and its line of `SHA256SUMS.darwin-arm64` go in.
        #[arg(
            long,
            required_unless_present = "verify_only",
            conflicts_with = "verify_only"
        )]
        out: Option<PathBuf>,
        /// Only check `--package`, which must be named as the asset; write nothing.
        #[arg(long)]
        verify_only: bool,
    },
    /// Install the `.pkg` on this Mac as root and check what it placed and that nothing
    /// started (macOS only).
    PkgMacosInstallTest {
        /// The `.pkg`, or the directory holding the one.
        #[arg(long)]
        package: PathBuf,
        /// A command to run after the install and what its output must contain:
        /// `<program> <argument>..::<text>` (the command exits 0).
        #[arg(long)]
        probe: Vec<pkg_install::Probe>,
    },
    /// kbf-installer for a Mac: its `.pkg`, and the Homebrew cask.
    #[command(subcommand)]
    Pkg(PkgVerb),
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
    /// Run the container driver's real-crun tests on a hosted Linux runner, each
    /// test target in its own Delegate=yes system unit as this user.
    CrunTests {
        /// The sudo program; it is run with `-n`, so it never prompts.
        #[arg(long, default_value = "sudo")]
        sudo: PathBuf,
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
enum PkgVerb {
    /// Write `kbf-installer-<version>-macos-arm64.pkg` in `--out`.
    Build {
        /// The kbf-installer binary, signed ad hoc.
        #[arg(long)]
        binary: PathBuf,
        /// The LaunchDaemon property list; refused unless the job is gated on the
        /// configuration file.
        #[arg(long)]
        launchd_plist: PathBuf,
        /// A trust file to install in /usr/local/kbf-installer/trust (repeatable).
        #[arg(long)]
        trust: Vec<PathBuf>,
        /// The release's version: letters, digits and `. _ + -`.
        /// (One of the two is required; the verb says so, exit 2.)
        #[arg(long, conflicts_with = "version_of")]
        version: Option<String>,
        /// Take the version from this binary's `--version` (`<version>+<12 of the commit>`).
        #[arg(long)]
        version_of: Option<PathBuf>,
        /// The directory the package goes in; created when missing.
        #[arg(long)]
        out: PathBuf,
    },
    /// Write the Homebrew cask that installs a package `pkg build` wrote.
    Cask {
        /// The package, named as `pkg build` names it.
        #[arg(long)]
        pkg: PathBuf,
        #[arg(long)]
        version: String,
        /// Where Homebrew downloads the package from (https:// or file:///).
        #[arg(long)]
        url: String,
        /// The cask file to write; it must not exist.
        #[arg(long)]
        out: PathBuf,
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
    /// `<dir>/SHA256SUMS` lists every other file in `<dir>` once, and each matches
    /// (a provenance bundle is not listed: it is made after the sums).
    Sums { dir: PathBuf },
    /// Every asset `<dir>/SHA256SUMS` lists has the attestations a deployment checks,
    /// and every tarball verifies against its own bundle as a host does.
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
        /// The commit the files were built from, a full SHA (on Actions, `GITHUB_SHA`;
        /// a release passes the tag's commit): each tarball's bundle must attest it.
        #[arg(long)]
        commit: String,
        /// A `trusted_root.jsonl` from `gh attestation trusted-root`. With it the bundles
        /// are verified with no network, as a host's installer does.
        #[arg(long)]
        trusted_root: Option<PathBuf>,
        /// The `gh` program.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
}

#[derive(Subcommand)]
enum ReleaseVerb {
    /// Refuse a commit not on main, and a run that is not the successful push run on main
    /// that built it; print `run-id=<id>`.
    Locate {
        /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
        #[arg(long)]
        repo: String,
        /// The commit to release, a full SHA.
        #[arg(long)]
        commit: String,
        /// The pipeline run that built it (on Actions, the id of the run that started the
        /// release).
        #[arg(long)]
        run_id: u64,
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
    /// `<dir>` passes `verify-assets sums`, lists only assets of `--commit`, and holds
    /// exactly one provenance bundle (`<tarball>.sigstore.json`) per tarball.
    Check {
        dir: PathBuf,
        /// The commit the assets were built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// Do not require the version facts of each `kbf` tarball: for the release made
        /// before the build jobs recorded them (the previous release, read by the first
        /// release that has them). Facts without their tarball are still refused.
        #[arg(long)]
        without_version_facts: bool,
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
        /// The pipeline run that built and attested the files.
        #[arg(long)]
        run_id: u64,
        /// The set hash of the verified files, as `release verify` printed it.
        #[arg(long)]
        set_hash: String,
        /// The release's notes (`release notes --out`): the release body, followed by how
        /// to verify the files. Required: no default body replaces it.
        #[arg(long)]
        notes_file: PathBuf,
        /// The `gh` program.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
    },
    /// The tag names this commit, which is on main. Reads no verdict: the tag's message is
    /// an audit record nothing gates on. Prints `set-hash=<hex>` of the files.
    Verify {
        #[command(flatten)]
        subject: SubjectArgs,
        /// Append `set-hash=<hex>` to this file (on Actions, `GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// What a push would release: the next version, the push's class, and the freeze and
    /// superseded refusals. Writes nothing but `--output`.
    Plan(release::plan::PlanArgs),
    /// The release before `--sha`: the highest release tag that names a strict ancestor of it,
    /// downloaded and verified (checksums, then attestations), and its named binaries taken
    /// out into `--out`. With no such tag, prints `previous-source=none`.
    FetchPrevious(release::fetch_previous::FetchPreviousArgs),
    /// Open the pull request in the internal repository that moves its submodule to the
    /// released commit, with auto-merge on (the token is `GH_TOKEN`).
    BumpInternal(release::bump::BumpArgs),
    /// Create the annotated release tag at `--sha` through the GitHub API: create-only,
    /// after the stale-run guards; a tag that exists is reused only when it is the same.
    Tag(release::tag::TagArgs),
    /// Write the Beta record a release tag carries as its message (audit only).
    VerdictWrite(release::verdict_write::WriteArgs),
    /// Run the `kbf` just built with `--version --json` and keep what it printed as the
    /// release asset `kbf-<commit>-<platform>.version.json`, refused unless it is the
    /// release build of `--commit`.
    VersionFacts(version_facts::Args),
    /// The same checks, then write the release's notes.
    Notes {
        #[command(flatten)]
        subject: SubjectArgs,
        /// `kbf --version --json` of this release's binary, run from an attested tarball.
        #[arg(long)]
        version_json: PathBuf,
        /// The same for the previous release (`release previous` names it); required
        /// exactly when there is one.
        #[arg(long)]
        previous_version_json: Option<String>,
        /// Write the notes here.
        #[arg(long)]
        out: PathBuf,
    },
    /// Run `kbf --version --json` from the linux-x86_64 tarball of `--commit` in `<dir>` and
    /// write what it prints. Runs a binary: only on files already checked and attested.
    Identity {
        dir: PathBuf,
        /// The commit the tarball was built from, a full SHA.
        #[arg(long)]
        commit: String,
        /// Write the identity here.
        #[arg(long)]
        out: PathBuf,
    },
    /// The release before `--tag`, if any: prints `previous=<tag>` and
    /// `previous-commit=<sha>` (both empty for a first release).
    Previous {
        /// A checkout that holds the tags and main's history.
        #[arg(long, default_value = ".")]
        repo_dir: PathBuf,
        /// The release tag.
        #[arg(long)]
        tag: String,
        /// The commit being released, a full SHA.
        #[arg(long)]
        commit: String,
        /// Append the two lines to this file (on Actions, `GITHUB_OUTPUT`).
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

/// What `release verify` and `release notes` check a release against.
#[derive(clap::Args)]
struct SubjectArgs {
    /// The verified release files (`SHA256SUMS` among them).
    dir: PathBuf,
    /// A checkout that holds the tag object and main's history.
    #[arg(long, default_value = ".")]
    repo_dir: PathBuf,
    /// The release tag, annotated.
    #[arg(long)]
    tag: String,
    /// The commit being released, a full SHA.
    #[arg(long)]
    commit: String,
    /// The ref of main in `--repo-dir`.
    #[arg(long, default_value = "origin/main")]
    main_ref: String,
}

impl SubjectArgs {
    fn subject(&self) -> release::verify::Subject<'_> {
        release::verify::Subject {
            repo_dir: &self.repo_dir,
            tag: &self.tag,
            commit: &self.commit,
            dir: &self.dir,
            main_ref: &self.main_ref,
        }
    }
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
    /// The `kbf` in the release tarball `<tarball>` (`<name>-<commit>-<platform>.tar.gz`)
    /// names `<commit>` and a release build, as `check --json` finds, run on the bytes
    /// that ship.
    CheckAsset { tarball: PathBuf, commit: String },
    /// A plain build, two override stamps and a malformed override, from the checkout.
    Stamps { commit: String },
}

/// The workspace this xtask was built from.
fn default_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// What `git <args>` prints in `root`.
fn git_stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(Error::spawn("git".to_owned()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(out.stdout)
}

/// The `source-date-epoch` verb: appends `SOURCE_DATE_EPOCH=<committer time>` of `commit`,
/// as `root` holds it, to `output`, and returns the line.
fn source_date_epoch(root: &Path, commit: &str, output: &Path) -> Result<String, Error> {
    if !build_commit::is_commit(commit) {
        return Err(Error::NotACommit(commit.to_owned()));
    }
    let said = git_stdout(root, &["show", "-s", "--format=%ct", commit])?;
    let seconds = String::from_utf8_lossy(&said).trim().to_owned();
    let line = format!("SOURCE_DATE_EPOCH={seconds}");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output)
        .map_err(Error::spawn(format!("open {}", output.display())))?;
    std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())
        .map_err(Error::spawn(format!("append to {}", output.display())))?;
    Ok(line)
}

/// What the image of `service` must hold, from the flags: the frontend wants `--kbf-tarball`
/// always, the coordinator wants `--binary` unless only verifying.
fn image_reference<'a>(
    service: pkg_image_asset::Image,
    tarball: Option<&'a Path>,
    binary: Option<&'a Path>,
    verify_only: bool,
) -> Result<Option<pkg_image_asset::Reference<'a>>, Error> {
    use pkg_image_asset::{Image, Reference};
    match (service, tarball, binary, verify_only) {
        (Image::Frontend, Some(t), None, _) => Ok(Some(Reference::KbfTarball(t))),
        (Image::Coordinator, None, Some(b), false) => Ok(Some(Reference::Binary(b))),
        (Image::Coordinator, None, None, true) => Ok(None),
        (Image::Frontend, ..) => Err(Error::Failed(
            "the frontend image is checked against --kbf-tarball, and only that".to_owned(),
        )),
        (Image::Coordinator, ..) => Err(Error::Failed(
            "the coordinator image is checked against --binary (not with --verify-only, \
             which makes the other checks alone)"
                .to_owned(),
        )),
    }
}

/// The budget file as `rev` holds it, read with `git show` in `root`.
fn budget_at(root: &Path, rev: &str) -> Result<String, Error> {
    let spec = format!("{rev}:{}", budget::BUDGET);
    let out = git_stdout(root, &["show", &spec])?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The paths git sees renamed between `rev` and `HEAD` in `root`: new path to old.
fn renames_since(root: &Path, rev: &str) -> Result<BTreeMap<String, String>, Error> {
    let args = ["diff", "--find-renames", "--name-status", "-z", rev, "HEAD"];
    Ok(budget::parse_renames(&git_stdout(root, &args)?))
}

/// `budget-diff --base <rev>`: the line it prints when nothing was added. A path git
/// renamed since `rev` keeps its row.
fn budget_diff(root: &Path, rev: &str) -> Result<String, Error> {
    let base = budget_at(root, rev)?;
    let renames = renames_since(root, rev)?;
    let path = root.join(budget::BUDGET);
    let head = std::fs::read_to_string(&path).map_err(Error::spawn(path.display().to_string()))?;
    let added = budget::new_rows_renamed(&base, &head, &renames).map_err(Error::Refused)?;
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
        Verb::BuildCommit(BuildCommit::CheckAsset { tarball, commit }) => {
            let scratch_in = root.join("target");
            Ok(vec![build_commit::check_asset(
                &scratch_in,
                &tarball,
                &commit,
            )?])
        }
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
        Verb::CrunTests { sudo } => crun_tests::run(&crun_tests::CrunTests {
            cargo: &cargo_program(),
            sudo: &sudo,
            root: &root,
        }),
        Verb::Advisories(args) => advisories::run(&args, &root, &cargo_program()),
        Verb::DeployFarm(args) => deploy_farm::run(&args),
        Verb::BudgetDiff { base } => Ok(vec![budget_diff(&root, &base)?]),
        Verb::SourceDateEpoch { commit, output } => {
            source_date_epoch(&root, &commit, &output).map(|line| vec![line])
        }
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
        Verb::InstallerTarball {
            binary,
            platform,
            commit,
            units,
            trust,
            sysusers,
            tmpfiles,
            needrestart,
            out,
        } => installer_tarball::installer_tarball(
            platform,
            &commit,
            &pkg_linux::Sources {
                binary: &binary,
                units: &units,
                trust: &trust,
                sysusers: &sysusers,
                tmpfiles: &tmpfiles,
                needrestart: &needrestart,
            },
            &out,
        ),
        Verb::PkgConfig {
            binary,
            kbf,
            units,
            trust,
            sysusers,
            tmpfiles,
            needrestart,
            platform,
            version,
            version_of,
            out,
        } => {
            let version = match (version, version_of) {
                (Some(version), _) => version,
                (None, Some(of)) => pkg_asset::binary_version(&of)?,
                (None, None) => return Err(Error::Usage("--version or --version-of".to_owned())),
            };
            pkg_linux::write_config(
                &pkg_linux::Inputs {
                    binary: &binary,
                    kbf: &kbf,
                    units: &units,
                    trust: &trust,
                    sysusers: &sysusers,
                    tmpfiles: &tmpfiles,
                    needrestart: &needrestart,
                    platform,
                    version: &version,
                },
                &out,
            )
        }
        Verb::PkgBuild {
            nfpm,
            config,
            out,
            source_date_epoch,
        } => pkg_linux::build(
            &mut pkg_linux::nfpm_process(&nfpm),
            &pkg_linux::Build {
                config: &config,
                out: &out,
                epoch: source_date_epoch,
            },
        ),
        Verb::PkgTrust {
            gh,
            trusted_root,
            source_ref,
            out,
        } => pkg_trust::write(&gh, &trusted_root, &source_ref, &out),
        Verb::PkgAsset {
            package,
            platform,
            commit,
            binary_tarball,
            expect_ref,
            gh,
            out,
            verify_only,
        } => pkg_asset::asset(
            &pkg_asset::sole(&package, "deb")?,
            &commit,
            &match (&out, verify_only) {
                (Some(out), false) => pkg_asset::Mode::Add(out),
                _ => pkg_asset::Mode::VerifyOnly,
            },
            &pkg_asset::Check {
                platform,
                binary_tarball: &binary_tarball,
                expect_ref: &expect_ref,
                gh: gh.as_deref(),
            },
        ),
        Verb::PkgKbfAsset {
            package,
            platform,
            commit,
            kbf_tarball,
            out,
            verify_only,
        } => pkg_kbf_asset::asset(
            &pkg_asset::sole(&package, "deb")?,
            &commit,
            &match (&out, verify_only) {
                (Some(out), false) => pkg_asset::Mode::Add(out),
                _ => pkg_asset::Mode::VerifyOnly,
            },
            &pkg_kbf_asset::Check {
                platform,
                kbf_tarball: &kbf_tarball,
            },
        ),
        Verb::PkgImageAsset {
            image,
            service,
            platform,
            commit,
            kbf_tarball,
            binary,
            out,
            verify_only,
        } => {
            let reference = image_reference(
                service,
                kbf_tarball.as_deref(),
                binary.as_deref(),
                verify_only,
            )?;
            pkg_image_asset::asset(
                &image,
                &commit,
                &match (&out, verify_only) {
                    (Some(out), false) => pkg_asset::Mode::Add(out),
                    _ => pkg_asset::Mode::VerifyOnly,
                },
                &pkg_image_asset::Check {
                    image: service,
                    platform,
                    reference,
                },
            )
        }
        Verb::PkgMacosAsset {
            package,
            commit,
            binary_tarball,
            expect_ref,
            gh,
            out,
            verify_only,
        } => pkg_macos_asset::asset(
            &pkg_asset::sole(&package, "pkg")?,
            &commit,
            &match (&out, verify_only) {
                (Some(out), false) => pkg_asset::Mode::Add(out),
                _ => pkg_asset::Mode::VerifyOnly,
            },
            &pkg_macos_asset::Check {
                binary_tarball: &binary_tarball,
                expect_ref: &expect_ref,
                gh: gh.as_deref(),
            },
        ),
        #[cfg(target_os = "macos")]
        Verb::PkgMacosInstallTest { package, probe } => {
            pkg_macos_install::run_test(&pkg_asset::sole(&package, "pkg")?, &probe)
        }
        #[cfg(not(target_os = "macos"))]
        Verb::PkgMacosInstallTest { .. } => Err(darwin::needs_macos("pkg-macos-install-test")),
        Verb::PkgInstallTest {
            package,
            format,
            listing,
            image_dir,
            gate,
            probe,
            docker,
        } => pkg_install::run(
            &mut pkg_install::process(&docker),
            &pkg_install::InstallTest {
                package: &pkg_asset::sole(
                    &package,
                    match format {
                        pkg_install::Format::Deb => "deb",
                        pkg_install::Format::Rpm => "rpm",
                    },
                )?,
                format,
                listing: &listing,
                image_dir: &image_dir,
                gates: &gate,
                probes: &probe,
                container: &format!("kbf-pkg-test-{}", std::process::id()),
            },
        ),
        Verb::Pkg(PkgVerb::Build {
            binary,
            launchd_plist,
            trust,
            version,
            version_of,
            out,
        }) => {
            let version = match (version, version_of) {
                (Some(version), _) => version,
                (None, Some(of)) => pkg_asset::binary_version(&of)?,
                (None, None) => return Err(Error::Usage("--version or --version-of".to_owned())),
            };
            pkg::build(&pkg::Build {
                binary: &binary,
                plist: &launchd_plist,
                trust: &trust,
                version: &version,
                out: &out,
            })
        }
        Verb::Pkg(PkgVerb::Cask {
            pkg,
            version,
            url,
            out,
        }) => pkg::cask(&pkg, &version, &url, &out),
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
            run_id,
            output,
            gh,
        }) => release::locate(&gh, &repo, &commit, run_id, output.as_deref()),
        Verb::Release(ReleaseVerb::MainControl {
            repo,
            on_main,
            off_main,
            gh,
        }) => Ok(vec![release::main_control(
            &gh, &repo, &on_main, &off_main,
        )?]),
        Verb::Release(ReleaseVerb::Check {
            dir,
            commit,
            without_version_facts,
        }) => {
            let pairing = if without_version_facts {
                version_facts::Pairing::Waived
            } else {
                version_facts::Pairing::Required
            };
            release::check(&dir, &commit, pairing)
        }
        Verb::Release(ReleaseVerb::Verify { subject, output }) => {
            let verified = release::verify::verify(&subject.subject())?;
            let line = format!("set-hash={}", verified.set_hash.as_str());
            if let Some(output) = output {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&output)
                    .and_then(|mut f| {
                        std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes())
                    })
                    .map_err(Error::spawn(format!("append to {}", output.display())))?;
            }
            Ok(vec![
                format!(
                    "tag {} names {} on {}; the files are set {}",
                    subject.tag,
                    subject.commit,
                    subject.main_ref,
                    verified.set_hash.as_str()
                ),
                line,
            ])
        }
        Verb::Release(ReleaseVerb::Plan(args)) => release::plan::run(&args),
        Verb::Release(ReleaseVerb::FetchPrevious(args)) => release::fetch_previous::run(&args),
        Verb::Release(ReleaseVerb::BumpInternal(args)) => release::bump::run(&args),
        Verb::Release(ReleaseVerb::Tag(args)) => release::tag::run(&args),
        Verb::Release(ReleaseVerb::VersionFacts(args)) => version_facts::run(&args),
        Verb::Release(ReleaseVerb::VerdictWrite(args)) => release::verdict_write::run(&args),
        Verb::Release(ReleaseVerb::Identity { dir, commit, out }) => {
            // The binary's unpacking place, of this process alone.
            let scratch =
                std::env::temp_dir().join(format!("xtask-identity-{}", std::process::id()));
            std::fs::create_dir_all(&scratch)
                .map_err(Error::spawn(format!("create {}", scratch.display())))?;
            let said = release::identity::identity(&dir, &commit, &scratch);
            let _ = std::fs::remove_dir_all(&scratch);
            let said = said?;
            std::fs::write(&out, format!("{said}\n"))
                .map_err(Error::spawn(format!("write {}", out.display())))?;
            Ok(vec![format!(
                "wrote the identity of {commit} to {}",
                out.display()
            )])
        }
        Verb::Release(ReleaseVerb::Previous {
            repo_dir,
            tag,
            commit,
            output,
        }) => {
            build_commit::full_commit("--commit", &commit)?;
            let found = release::notes::previous_release(&repo_dir, &tag, &commit)?;
            let at = match &found {
                Some(prev) => release::commit_of(&repo_dir, prev)?,
                None => String::new(),
            };
            let lines = vec![
                format!("previous={}", found.unwrap_or_default()),
                format!("previous-commit={at}"),
            ];
            if let Some(output) = output {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&output)
                    .and_then(|mut f| {
                        std::io::Write::write_all(
                            &mut f,
                            format!("{}\n", lines.join("\n")).as_bytes(),
                        )
                    })
                    .map_err(Error::spawn(format!("append to {}", output.display())))?;
            }
            Ok(lines)
        }
        Verb::Release(ReleaseVerb::Notes {
            subject,
            version_json,
            previous_version_json,
            out,
        }) => {
            let previous = previous_version_json
                .filter(|p| !p.is_empty())
                .map(PathBuf::from);
            let ids = release::notes::Identities {
                current: &version_json,
                // The workflow passes an empty path when there is no previous release.
                previous: previous.as_deref(),
            };
            let text = release::notes::notes(&subject.subject(), &ids)?;
            std::fs::write(&out, &text)
                .map_err(Error::spawn(format!("write {}", out.display())))?;
            Ok(vec![format!(
                "wrote the notes of {} to {}",
                subject.tag,
                out.display()
            )])
        }
        Verb::Release(ReleaseVerb::Publish {
            dir,
            repo,
            tag,
            commit,
            run_id,
            set_hash,
            notes_file,
            gh,
        }) => {
            let notes = release::read_notes(&notes_file)?;
            let set_hash = kbf_gate::SetHash::parse(&set_hash)
                .map_err(|e| Error::Usage(format!("--set-hash: {e}")))?;
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
                set_hash: &set_hash,
                notes: &notes,
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
            commit,
            trusted_root,
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
                commit: &commit,
                trusted_root: trusted_root.as_deref(),
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

/// Whether `args` (what follows `cargo xtask`) are a command line this program takes; the
/// error is the parser's message. Nothing is run: a test of the lines a workflow runs uses it.
pub fn check_args(args: &[&str]) -> Result<(), String> {
    let argv = std::iter::once("xtask").chain(args.iter().copied());
    let parsed = Cli::try_parse_from(argv);
    parsed.map(drop).map_err(|e| e.to_string())
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
