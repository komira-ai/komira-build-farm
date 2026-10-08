//! `kbf-cell`: the M1 integration harness's helper (see the `kbf_it` library docs).
//!
//! Exit codes: 0 done (or the check passed), 1 the check failed, 2 the command could
//! not run.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use kbf_it::summary::{Tool, check_logs};

#[derive(Debug, Parser)]
#[command(
    name = "kbf-cell",
    version,
    about = "The kbf integration cell's helper"
)]
enum Cli {
    /// Writes a throwaway CA, a server certificate and a daemon certificate.
    Pki {
        /// The directory to write them to.
        #[arg(long)]
        dir: PathBuf,
        /// The node id the daemon certificate names (its DNS subjectAltName and common
        /// name). The daemon must say the same `--node-id`, or the server refuses it.
        #[arg(long, default_value = "cell-node-1")]
        node_id: String,
    },
    /// Runs the test-only daemon (local runtime, no isolation) until SIGINT.
    #[cfg(target_os = "linux")]
    Daemon(kbf_it::daemon::DaemonArgs),
    /// Checks a build and its rebuild after a clean: all remote, then all cache hits.
    Check {
        #[arg(long, value_enum)]
        tool: Tool,
        /// The first build's console log.
        #[arg(long)]
        first: PathBuf,
        /// The second build's console log.
        #[arg(long)]
        second: PathBuf,
    },
}

fn main() -> ExitCode {
    match Cli::parse() {
        Cli::Pki { dir, node_id } => match kbf_it::pki::write(&dir, &node_id) {
            Ok(files) => {
                println!("{files:?}");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        #[cfg(target_os = "linux")]
        Cli::Daemon(args) => daemon(args),
        Cli::Check {
            tool,
            first,
            second,
        } => {
            let read = |path: &PathBuf| {
                std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
            };
            let (first, second) = match read(&first).and_then(|f| Ok((f, read(&second)?))) {
                Ok(logs) => logs,
                Err(e) => return fail(&e),
            };
            let (report, passed) = check_logs(tool, &first, &second);
            print!("{report}");
            if passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn daemon(args: kbf_it::daemon::DaemonArgs) -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// Runs the daemon until SIGINT.
#[cfg(target_os = "linux")]
fn serve(args: kbf_it::daemon::DaemonArgs) -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = async {
        // Without a handler the process cannot stop cleanly; it still stops.
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::runtime::Runtime::new()?.block_on(kbf_it::daemon::run(args, shutdown))?;
    Ok(())
}

fn fail(e: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("kbf-cell: {e}");
    ExitCode::from(2)
}
