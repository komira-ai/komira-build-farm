//! The two halves started and run together (the crate docs say how), for
//! `kbf all-in-one` (the server half) and `kbf storage-and-daemon` (the storage half,
//! `crate::storage_and_daemon`).

use std::fmt;
use std::future::Future;
use std::io::IsTerminal as _;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;

use kbf_server::ServeError;
use kbf_threads::Topology;
use tokio::runtime::Runtime;

use crate::LocalDataPlane;
use crate::cli::{Flags, command, parse};

/// Parses the command line, starts both halves and serves until the server half stops.
pub(crate) fn main() -> ExitCode {
    let matches = command().get_matches();
    let flags = parse(&matches).unwrap_or_else(|e| e.exit());
    run(&flags)
}

/// Starts both halves on the parsed flags and serves until the server half stops.
pub(crate) fn run(flags: &Flags) -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    exit("kbf-all-in-one", "server", serve(prepared(flags)))
}

/// The exit status of a run that ended as `served` did, its failure said on stderr
/// after `name`, `half` naming the half beside the daemon (`server` or `storage`).
pub(crate) fn exit(name: &str, half: &str, served: Result<(), Failed>) -> ExitCode {
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(failed) => {
            eprintln!("{name}: {}", failed.said(half));
            failed.code()
        }
    }
}

/// Why the process stopped other than on a signal. Which half is "that half" (the one
/// beside the daemon) is the caller's to say ([`Failed::said`]), so each failure is
/// made by a function, not a closure.
pub(crate) enum Failed {
    /// That half refused to start.
    Server(Box<dyn std::error::Error>),
    /// The daemon half refused to start.
    Daemon(kbf_node::Error),
    /// That half stopped serving on its own.
    Stopped(String),
}

impl Failed {
    pub(crate) fn server(e: impl Into<Box<dyn std::error::Error>>) -> Self {
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

    /// What failed, `half` naming the half beside the daemon.
    fn said(&self, half: &str) -> String {
        match self {
            Self::Server(e) => format!("{half}: {e}"),
            Self::Daemon(e) => format!("daemon: {e}"),
            Self::Stopped(e) => format!("the {half} half stopped: {e}"),
        }
    }
}

/// Both halves' flags as the mode runs them: given one cache when they share a disk
/// (`crate::one_cache`).
fn prepared(flags: &Flags) -> Flags {
    let mut flags = flags.clone();
    crate::one_cache::share(&mut flags.server, &mut flags.daemon);
    flags
}

/// The server half on a runtime of its own, then the daemon half over the in-process
/// plane; returns once the server half has stopped and the daemon half after it. Both
/// halves start from `flags`, as [`prepared`] made them: no half sees another copy.
fn serve(flags: Flags) -> Result<(), Failed> {
    let sizes = kbf_server::sizes_on(&flags.server, Topology::detect());
    sizes.log();
    let tokio = sizes.runtime().map_err(Failed::server)?;
    let started = tokio.block_on(kbf_server::start_interruptible(&flags.server));
    let server = started.map_err(Failed::server)?;
    println!("{}", server.start_line());
    let plane = Arc::new(LocalDataPlane::new(server.local(), tokio.handle().clone()));
    let serving = Box::pin(server.serve());
    with_daemon(&tokio, serving, in_process(plane), &flags.daemon)
}

/// What the half beside the daemon serves until it stops.
pub(crate) type Serving = Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>;

/// How the daemon half starts: from its flags, over the data plane its mode gives it.
pub(crate) type Starter =
    Box<dyn FnOnce(&kbf_node::Cli) -> Result<kbf_node::NodeHandle, kbf_node::Error>>;

/// The daemon half over `plane`, the in-process plane of its half.
pub(crate) fn in_process(plane: Arc<LocalDataPlane>) -> Starter {
    Box::new(move |cli| kbf_node::start_with(cli, move |_| Ok(Arc::clone(&plane))))
}

/// The daemon half of `daemon` started by `start`, beside the half (server or storage)
/// `serving` on `tokio`; returns once that half has stopped and the daemon half after
/// it. Not generic, so both modes run one copy of it.
pub(crate) fn with_daemon(
    tokio: &Runtime,
    serving: Serving,
    start: Starter,
    daemon: &kbf_node::Cli,
) -> Result<(), Failed> {
    // Dropped when the other half stops serving, which stops the daemon half.
    let (ended, other_ended) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio.spawn(async move {
        let _ended = ended;
        serving.await
    });
    let node = start(daemon).map_err(Failed::Daemon)?;
    node.serve_until(async move {
        let _ = other_ended.await;
    });
    let served = tokio.block_on(serving);
    let served = served.map_err(Failed::stopped)?;
    served.map_err(Failed::stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a half started from flags other than the ones given the one cache (the
    /// daemon half handed a copy made before the sharing, or the server half): with the
    /// hot store on the data directory's disk, both halves of the flags `serve` takes
    /// hold the same cache.
    #[test]
    fn both_halves_start_with_the_one_cache() {
        let dir = crate::one_cache::under_target("prepared-all-in-one");
        let at = |name: &str| dir.join(name).display().to_string();
        let argv = [
            "kbf-all-in-one".to_owned(),
            "--server=https://127.0.0.1:8981".to_owned(),
            "--ca-cert=ca.pem".to_owned(),
            "--cert=node.pem".to_owned(),
            "--key=node.key".to_owned(),
            "--node-id=node-1".to_owned(),
            "--driver=container".to_owned(),
            format!("--hot-store=file://{}", at("hot")),
            format!("--data-dir={}", at("data")),
        ];
        let flags = parse(&command().get_matches_from(argv)).expect("flags");
        let flags = prepared(&flags);
        let local = flags
            .server
            .local_cache
            .as_ref()
            .expect("the server half's");
        assert!(crate::one_cache::daemon_holds(local, &flags.daemon));
    }

    /// Catches a failure that exits with another status than its half's (a refused
    /// server start must exit 2, as `kbf-server`'s does, and a server half that stops
    /// serving must not exit 0), or that says nothing of which half failed.
    #[test]
    fn each_failure_exits_with_its_halfs_status() {
        let stopped = Failed::stopped("listener gone");
        assert_eq!(
            stopped.said("server"),
            "the server half stopped: listener gone"
        );
        assert_eq!(
            stopped.said("storage"),
            "the storage half stopped: listener gone"
        );
        assert_eq!(stopped.code(), ExitCode::FAILURE);
        let server = Failed::server("no store");
        assert_eq!(server.said("server"), "server: no store");
        assert_eq!(server.said("storage"), "storage: no store");
        assert_eq!(server.code(), ExitCode::from(2));
        let daemon = Failed::Daemon("no scratch".into());
        assert_eq!(daemon.said("storage"), "daemon: no scratch");
        assert_eq!(daemon.code(), ExitCode::FAILURE);
    }
}
