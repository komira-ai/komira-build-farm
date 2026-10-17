//! The daemon's entry: what the `kbf-daemon` binary and `kbf daemon` both run once the
//! flags are parsed.

use std::io::IsTerminal;
use std::process::ExitCode;

use crate::{Cli, exit_code, start};

/// Installs the log (to stderr, in color only on a terminal), starts the daemon on the
/// parsed flags and serves until a signal. Exits 0 on a clean stop; a start or a serve
/// that fails prints why and exits as [`exit_code`] says.
#[must_use]
pub fn run(cli: &Cli) -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    match start(cli).and_then(crate::NodeHandle::serve) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-daemon: {e}");
            exit_code(&e)
        }
    }
}
