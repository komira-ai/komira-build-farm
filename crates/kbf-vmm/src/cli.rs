//! The command line.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// The VM helper for macOS VM leases: feasibility probes and the base-image builder.
///
/// Every command that touches the hypervisor runs on macOS only. It prints one JSON
/// object on stdout.
#[derive(Debug, Parser)]
#[command(name = "kbf-vmm", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Run one feasibility gate and print its result as one JSON object.
    Probe(ProbeArgs),
    /// Time clone, boot and teardown, cold and warm, and print median and max as JSON.
    ProbeBench(BenchArgs),
    /// Build a base VM bundle.
    #[command(subcommand)]
    Image(ImageCmd),
    /// Run one VM from a spec file; events are JSON lines on stdout; stdin closing stops
    /// the VM. The probes start this as a child process.
    #[command(hide = true)]
    VmRun {
        #[arg(long)]
        spec: PathBuf,
    },
}

/// The gates `probe` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Gate {
    /// Clone a base bundle: same machine identifier twice at once; a new identifier.
    G1,
    /// Which channel carries bytes from a guest process to the host.
    G2,
    /// Whether mounting a custom virtiofs tag in the guest needs root.
    G3,
    /// Start a VM from a launch-daemon child, nobody logged in, as a throwaway uid.
    #[value(alias = "p5")]
    G5,
    /// Why a booted guest never reaches its own userspace: two guests at once, every
    /// helper event timed, CPU and disk writes sampled, the clones kept for `image
    /// harvest` (give `--guest-binary`, `--graphics`, `--keep`).
    E1,
    /// Whether a root offline write into a guest volume works.
    P0,
    /// The third-VM limit error, and the slot after SIGKILL of a helper.
    P2,
    /// Whether an ad hoc hardened-runtime signature with the entitlement runs a VM.
    P6,
}

/// What every probe needs.
#[derive(Debug, Args)]
pub struct Common {
    /// Where clones, shared folders and logs go. Must be on the volume of the base
    /// bundle: `clonefile` does not cross volumes.
    #[arg(long)]
    pub work_dir: PathBuf,
    /// The prepared base bundle (default: `<work-dir>/base`).
    #[arg(long)]
    pub base: Option<PathBuf>,
    /// A `kbf-guest` built for macOS, copied into the folder shared with each guest.
    #[arg(long)]
    pub guest_binary: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ProbeArgs {
    #[arg(value_enum, ignore_case = true)]
    pub gate: Gate,
    #[command(flatten)]
    pub common: Common,
    /// The user id G5 starts the helper as (default 59001); needs root.
    #[arg(long)]
    pub run_as_uid: Option<u32>,
    /// Give the second VM of the pair a graphics device (E1: the first stays headless).
    #[arg(long)]
    pub graphics: bool,
    /// Leave the clones and their shares in the work directory when the probe ends, so
    /// `image harvest` can read the guest's disk.
    #[arg(long)]
    pub keep: bool,
}

#[derive(Debug, Args)]
pub struct BenchArgs {
    #[command(flatten)]
    pub common: Common,
    /// Runs per measure, cold and warm each.
    #[arg(long, default_value_t = 20)]
    pub runs: u32,
}

#[derive(Debug, Subcommand)]
pub enum ImageCmd {
    /// Install macOS from a restore image into a new base bundle.
    Base {
        /// The restore image: an https URL or a path to an .ipsw.
        #[arg(long)]
        ipsw: String,
        /// The SHA-256 the restore image must have (64 hex digits). A mismatch is refused.
        #[arg(long)]
        sha256: String,
        /// The new bundle directory; it must not exist.
        #[arg(long)]
        out: PathBuf,
        /// The guest disk's size.
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u64).range(32..=4096))]
        disk_gib: u64,
        /// As root, write the Setup Assistant marker and the probe launch daemons into
        /// the installed image (probe P0's question).
        #[arg(long)]
        offline_setup: bool,
    },
    /// As root, write the probe launch daemons and the Setup Assistant marker into an
    /// existing base bundle (the `--offline-setup` step, for a bundle built without it).
    Setup {
        /// The base bundle directory.
        #[arg(long)]
        base: PathBuf,
    },
    /// As root, copy a fixed allowlist of diagnostics (the probe daemons' logs, the local
    /// marker, the unified log's data files, crash reports) out of a stopped guest's disk
    /// into a new directory. The disk is attached read-only; nothing else is read.
    Harvest {
        /// The bundle (a clone kept by `probe e1 --keep`).
        #[arg(long)]
        bundle: PathBuf,
        /// The new directory to copy into; it must not exist.
        #[arg(long)]
        out: PathBuf,
    },
}
