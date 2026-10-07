//! `kbf-daemon`: the worker daemon. See the library docs for what v0 does.
//!
//! Configuration is by flags only (`kbf-daemon --help`). Exits non-zero when the node
//! cannot be detected or the configuration cannot be loaded; otherwise runs until
//! killed.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_daemon::{Args, Daemon, DaemonConfig, FakeRuntime, NodeReport, Runtime, RuntimeKind};

fn main() -> ExitCode {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let runtime = match args.runtime {
        RuntimeKind::Fake => Arc::new(FakeRuntime::new(Duration::ZERO)),
    };
    match serve(&args, runtime) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the daemon until the process is killed; returns an error that stopped it from
/// starting.
fn serve<R: Runtime>(args: &Args, runtime: Arc<R>) -> Result<(), Box<dyn std::error::Error>> {
    let report = NodeReport::detect(&[runtime.driver()])?;
    let daemon = Daemon::new(DaemonConfig::from_args(args), runtime, report)?;
    let tokio = tokio::runtime::Runtime::new()?;
    tokio.block_on(daemon.run(std::future::pending()));
    Ok(())
}
