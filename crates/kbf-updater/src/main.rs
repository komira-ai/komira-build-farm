//! `kbf-updater`: installs signed software sets on a node, as root, for the genuine
//! `kbf-daemon` only (`docs/design/fleet-updates-security.md` S4.1). Configuration is by
//! flags only (`kbf-updater --help`); everything a set must satisfy is pinned here at
//! provisioning: the root key, the pool and platform, the daemon's uid and binary.
//!
//! ```text
//! kbf-updater --root-key-file /etc/kbf/root.pub --pool linux-x86 --os linux --arch x86_64 \
//!   --daemon-path /usr/local/libexec/kbf/kbf-daemon --daemon-uid 990 --socket-gid 991 \
//!   --artifacts-dir /var/lib/kbf/artifacts --bin-dir /usr/local/libexec/kbf
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// Installs signed software sets on this node, for the genuine kbf-daemon only.
#[derive(Debug, Parser)]
#[command(name = "kbf-updater", version)]
struct Cli {
    /// The offline root key's public half, hex, in a root-owned file.
    #[arg(long)]
    root_key_file: PathBuf,
    /// The pool this node belongs to; sets for any other pool are refused.
    #[arg(long)]
    pool: String,
    /// This node's `os` (`linux`).
    #[arg(long)]
    os: String,
    /// This node's `arch` (`x86_64` or `arm64`).
    #[arg(long)]
    arch: String,
    /// The installed kbf-daemon binary; a caller's executable must hash to it.
    #[arg(long)]
    daemon_path: PathBuf,
    /// The daemon's uid; a caller must run as it.
    #[arg(long)]
    daemon_uid: u32,
    /// The helpers' dedicated group, which owns the socket and its directory.
    #[arg(long)]
    socket_gid: u32,
    /// The socket; its directory is made mode 0750.
    #[arg(long, default_value = "/run/kbf-updater/kbf-updater.sock")]
    socket: PathBuf,
    /// Root-only: the state file and the staging directory.
    #[arg(long, default_value = "/var/lib/kbf-updater")]
    state_dir: PathBuf,
    /// Where the daemon leaves artifacts, each named by its SHA-256.
    #[arg(long)]
    artifacts_dir: PathBuf,
    /// Where kbf's binaries are installed.
    #[arg(long)]
    bin_dir: PathBuf,
}

#[cfg(target_os = "linux")]
fn start(cli: Cli) -> Result<std::convert::Infallible, String> {
    use kbf_updater::apply::{AptSnapshot, SystemRunner};
    use kbf_updater::caller::DaemonPin;
    use kbf_updater::server::{Settings, read_root_key, run};
    use kbf_updater::set::{Pin, Platform};
    use kbf_updater::updater::Config;

    let settings = Settings {
        socket: cli.socket,
        socket_gid: cli.socket_gid,
        daemon: DaemonPin {
            uid: cli.daemon_uid,
            path: cli.daemon_path,
        },
        config: Config {
            root_key: read_root_key(&cli.root_key_file)?,
            pin: Pin {
                pool: cli.pool,
                platform: Platform {
                    os: cli.os,
                    arch: cli.arch,
                },
            },
            state_dir: cli.state_dir,
            artifacts_dir: cli.artifacts_dir,
        },
        proc_root: PathBuf::from("/proc"),
    };
    run(settings, AptSnapshot::new(SystemRunner, cli.bin_dir))
}

/// Macs need actions to run as per-lease users before the helper may ship there (S4.3).
#[cfg(not(target_os = "linux"))]
fn start(_cli: Cli) -> Result<std::convert::Infallible, String> {
    Err("kbf-updater runs on Linux only for now; on macOS it waits for per-lease users".into())
}

fn main() -> ExitCode {
    let Err(e) = start(Cli::parse());
    eprintln!("kbf-updater: {e}");
    ExitCode::FAILURE
}
