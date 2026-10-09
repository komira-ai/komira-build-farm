//! `kbf-guest`: runs one command per boot for the host, inside a VM guest.
//!
//! ```text
//! kbf-guest serve --unix-socket <path> --token-file <inputs>/kbf-guest.token \
//!   --inputs <inputs share> --outputs <outputs share>
//! kbf-guest session
//! ```
//!
//! `session` prints the launchd session type and exits; it is how CI checks what a
//! LaunchAgent and a LaunchDaemon see. Every subcommand refuses to run as root.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kbf_guest::server::{Agent, Config, HELLO_TIMEOUT};
use kbf_guest::session;
use kbf_guest::wire::TOKEN_LEN;

/// The agent inside a VM guest: runs one command per boot for the host.
#[derive(Debug, Parser)]
#[command(name = "kbf-guest", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Serve the host on a Unix socket.
    Serve {
        /// The socket to create; it must not exist.
        #[arg(long)]
        unix_socket: PathBuf,
        /// The per-boot token, 64 hex digits, written by the host.
        #[arg(long)]
        token_file: PathBuf,
        /// The inputs share; the command's working directory is inside it.
        #[arg(long)]
        inputs: PathBuf,
        /// The outputs share; the command's stdout and stderr go there.
        #[arg(long)]
        outputs: PathBuf,
    },
    /// Print the launchd session type (`Aqua`, `Background`, `System`; empty off
    /// macOS) and exit.
    Session,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Err(e) = session::refuse_root_here() {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    match cli.command {
        Cmd::Session => {
            println!("{}", session::manager_name());
            ExitCode::SUCCESS
        }
        Cmd::Serve {
            unix_socket,
            token_file,
            inputs,
            outputs,
        } => match serve(&unix_socket, &token_file, inputs, outputs) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kbf-guest: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn serve(
    socket: &std::path::Path,
    token_file: &std::path::Path,
    inputs: PathBuf,
    outputs: PathBuf,
) -> Result<(), String> {
    let text = std::fs::read_to_string(token_file)
        .map_err(|e| format!("reading {}: {e}", token_file.display()))?;
    let token: [u8; TOKEN_LEN] = hex::decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| {
            format!(
                "{} does not hold {} hex digits",
                token_file.display(),
                TOKEN_LEN * 2
            )
        })?;
    let listener =
        UnixListener::bind(socket).map_err(|e| format!("binding {}: {e}", socket.display()))?;
    let mut agent = Agent::new(Config {
        token,
        inputs,
        outputs,
        session: session::manager_name(),
        hello_timeout: HELLO_TIMEOUT,
    });
    let served = agent.serve(&listener);
    served.map_err(|e| format!("accepting: {e}"))
}
