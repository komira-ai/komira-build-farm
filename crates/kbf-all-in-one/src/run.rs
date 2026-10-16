//! The two halves started and run together (the crate docs say how).

use std::fmt;
use std::io::IsTerminal as _;
use std::process::ExitCode;
use std::sync::Arc;

use kbf_threads::Topology;

use crate::cli::{Flags, command, parse};
use crate::{LocalControlPlane, LocalDataPlane};

/// Parses the command line, starts both halves and serves until the server half stops.
pub(crate) fn main() -> ExitCode {
    let matches = command().get_matches();
    let flags = parse(&matches).unwrap_or_else(|e| e.exit());
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    match serve(&flags) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failed) => {
            eprintln!("kbf-all-in-one: {failed}");
            failed.code()
        }
    }
}

/// Why the process stopped other than on a signal.
enum Failed {
    /// The server half refused to start.
    Server(Box<dyn std::error::Error>),
    /// The daemon half refused to start.
    Daemon(kbf_node::Error),
    /// The server half stopped serving on its own.
    Stopped(String),
}

impl Failed {
    fn server(e: impl Into<Box<dyn std::error::Error>>) -> Self {
        Self::Server(e.into())
    }

    fn stopped(e: impl fmt::Display) -> Self {
        Self::Stopped(e.to_string())
    }

    /// The exit status: 2 for a start the server refused, as `kbf-server` exits; the
    /// daemon's own for a start it refused; 1 for a server half that stopped.
    fn code(&self) -> ExitCode {
        match self {
            Self::Server(_) => ExitCode::from(2),
            Self::Daemon(e) => kbf_node::exit_code(e),
            Self::Stopped(_) => ExitCode::FAILURE,
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(e) => write!(f, "server: {e}"),
            Self::Daemon(e) => write!(f, "daemon: {e}"),
            Self::Stopped(e) => write!(f, "the server half stopped: {e}"),
        }
    }
}

/// The server half on a runtime of its own, its worker listener serving remote daemons,
/// then the daemon half over the in-process planes, data and control; returns once the
/// server half has stopped and the daemon half after it.
fn serve(flags: &Flags) -> Result<(), Failed> {
    let sizes = kbf_server::sizes_on(&flags.server, Topology::detect());
    sizes.log();
    let tokio = sizes.runtime().map_err(Failed::server)?;
    let started = tokio.block_on(kbf_server::start(&flags.server, kbf_server::interrupted));
    let server = started.map_err(Failed::server)?;
    println!("{}", server.start_line());
    let plane = Arc::new(LocalDataPlane::new(server.local(), tokio.handle().clone()));
    let control = LocalControlPlane::new(server.local_worker(), tokio.handle().clone());
    // Dropped when the server half stops serving, which stops the daemon half.
    let (ended, server_ended) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio.spawn(async move {
        let _ended = ended;
        server.serve().await
    });
    let data = move |_: &kbf_node::Cli| Ok(Arc::clone(&plane));
    let node = kbf_node::start_with(&flags.daemon, data, Arc::new(control));
    let node = node.map_err(Failed::Daemon)?;
    node.serve_until(async move {
        let _ = server_ended.await;
    });
    let served = tokio.block_on(serving).map_err(Failed::stopped)?;
    served.map_err(Failed::stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a failure that exits with another status than its half's (a refused
    /// server start must exit 2, as `kbf-server`'s does, and a server half that stops
    /// serving must not exit 0), or that says nothing of which half failed.
    #[test]
    fn each_failure_exits_with_its_halfs_status() {
        let stopped = Failed::stopped("listener gone");
        assert_eq!(
            stopped.to_string(),
            "the server half stopped: listener gone"
        );
        assert_eq!(stopped.code(), ExitCode::FAILURE);
        let server = Failed::server("no store");
        assert_eq!(server.to_string(), "server: no store");
        assert_eq!(server.code(), ExitCode::from(2));
        let daemon = Failed::Daemon("no scratch".into());
        assert_eq!(daemon.to_string(), "daemon: no scratch");
        assert_eq!(daemon.code(), ExitCode::FAILURE);
    }
}
