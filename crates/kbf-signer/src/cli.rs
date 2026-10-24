//! The `kbf-signer` command line.

use std::convert::Infallible;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use kbf_ca::{Policy, SystemEntropy};

use crate::ca_dir::{ensure, ensure_shared};
use crate::error::SignerError;
use crate::group::{Accounts, ClientGroup};
use crate::server::{SignerServer, bind, bind_for_group, harden, system_now};

/// `kbf-signer`'s flags. Configuration is flags: nothing is read from the environment.
#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(
    name = "kbf-signer",
    about = "Holds the farm's CA key and signs certificates for the frontend's account alone."
)]
pub struct Flags {
    /// The CA directory (`ca.key` mode 0600, `ca.pem`), owned by this account.
    #[arg(long, value_name = "DIR")]
    pub ca_dir: PathBuf,
    /// The socket to serve, in a directory of mode 0700 (made if missing).
    #[arg(long, value_name = "PATH")]
    pub socket: PathBuf,
    /// The user id of the frontend: the only peer served.
    #[arg(long, value_name = "UID")]
    pub frontend_uid: u32,
    /// A group whose members may use the signer's socket as well as the frontend's user:
    /// the socket's directory becomes mode 0750 and the socket 0660, and the CA
    /// directory 0750 with `ca.pem` 0640, all of this group; `ca.key` stays 0600. A peer
    /// is still served only if its user id is `--frontend-uid` or it belongs to the group.
    /// This account must be a member of the group. Without the flag the socket and the
    /// directories are closed to every other account.
    #[arg(long, value_name = "GROUP")]
    pub client_group: Option<String>,
    /// The longest certificate lifetime the signer agrees to, in seconds.
    #[arg(long, value_name = "SECS", default_value_t = Policy::DEFAULT_LIFETIME_S)]
    pub lifetime_secs: i64,
}

fn start(flags: &Flags) -> Result<Infallible, SignerError> {
    harden()?;
    let policy = Policy::new(flags.lifetime_secs)?;
    // A group that does not exist stops the start before anything is written.
    let group = flags
        .client_group
        .as_deref()
        .map(|name| ClientGroup::resolve(name, Accounts::system()))
        .transpose()?;
    // The signer owns ca.key, so the signer makes the CA at first start.
    let now = system_now();
    let (server, listener) = if let Some(group) = group {
        let gid = group.gid();
        ensure_shared(&flags.ca_dir, policy, now, &mut SystemEntropy, gid)?;
        let server = SignerServer::from_shared_dir(&flags.ca_dir, flags.frontend_uid, policy)?
            .with_client_group(group);
        (server, bind_for_group(&flags.socket, gid)?)
    } else {
        ensure(&flags.ca_dir, policy, now, &mut SystemEntropy)?;
        let server = SignerServer::from_dir(&flags.ca_dir, flags.frontend_uid, policy)?;
        (server, bind(&flags.socket)?)
    };
    Ok(server.serve(&listener)?)
}

/// Starts the signer and serves until killed; returns only to report why it could not.
#[must_use]
pub fn run(flags: &Flags) -> ExitCode {
    let Err(e) = start(flags);
    eprintln!("kbf-signer: {e}");
    ExitCode::FAILURE
}

/// The `kbf-signer` binary.
#[must_use]
pub fn main() -> ExitCode {
    run(&Flags::parse())
}
