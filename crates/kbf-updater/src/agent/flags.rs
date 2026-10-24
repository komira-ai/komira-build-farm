//! The flags of `kbf-updater agent`. Every setting is a flag; none is read from the
//! environment.
//!
//! ```text
//! kbf-updater agent --coordinator <dns name>:<port> --channel NAME
//!                   (--state-dir DIR | --client-cert FILE --client-key FILE --ca FILE)
//!                   [--frontend <dns name>:<port>] [--enroll-frontend <dns name>:<port>]
//!                   [--operator-api http://IP:PORT --token-file FILE] [--helper-socket PATH]
//!                   [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N]
//!                   [--health-poll-secs N] [--retry-secs N]
//! ```
//!
//! - `--coordinator`: where the coordinator is; its certificate must be for that name.
//! - `--state-dir`: the directory `kbf-updater enroll` filled (the key, the certificate and the
//!   CA are its files, [`kbf_enroll::store::Store`]'s). The agent renews the certificate at half
//!   its life and writes the new one there; it needs `--enroll-frontend` for that.
//! - `--client-cert`, `--client-key`, `--ca`: instead, files an operator placed: this host's
//!   updater certificate and key, and the farm CA that signed the coordinator's certificate.
//!   Nothing renews them. The certificate says which node this is and what the host runs;
//!   neither is a flag.
//! - `--frontend`: the frontend's worker listener, `<dns name>:<port>` (its `--worker-listen`;
//!   the name must be one its certificate covers). The node's drain, its undrain, its status
//!   and, for a storage host, its own storage health are four calls there, made over mutual
//!   TLS with the updater's own certificate and no token: the frontend serves them for the node
//!   in the certificate and for no other, and refuses everything else from an updater.
//! - `--enroll-frontend`: the frontend's enroll listener (its `--enroll-listen`), the address
//!   `kbf-updater enroll --frontend` was given. The agent renews its certificate there, with
//!   the key it enrolled with; the listener asks for no client certificate, so a host whose
//!   certificate lapsed during an outage still recovers.
//! - `--channel`: the release channel the helper's allowlist names; the version the
//!   coordinator names is looked up in it by the helper, never by URL.
//! - `--helper-socket`: the socket of `kbf-host-install --serve` (default
//!   `/run/kbf-host-install/local.sock`).
//! - `--operator-api`, `--token-file`: the frontend's loopback
//!   operator API, as in the roll mode, as an **opt-in fallback** for the drain and the
//!   node's health: the credential is the full operator token, which the design keeps off
//!   hosts. Giving it to an agent is a choice of the operator, made by giving both
//!   `--operator-api` and `--token-file`, and it is then used
//!   instead of the updater's certificate for those calls. Without either way (no `--frontend`
//!   and no token) the agent heartbeats and refuses every update order, before it fetches
//!   anything. The packaged units give no token.
//! - Times, in seconds: `--heartbeat-secs` (default 15), `--drain-deadline-secs` (600, how long
//!   the node's leases are waited for), `--health-timeout-secs` (300, how long the release on
//!   trial has to show healthy), `--health-poll-secs` (5) and `--retry-secs` (900, how long a
//!   target that failed is left alone before the same order is tried again).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_enroll::store::Store;
use kbf_types::Role;

use super::coordinator::{Endpoint, TlsFiles};
use crate::daemon::parse_operator_url;

pub const USAGE: &str = "usage: kbf-updater agent --coordinator DNSNAME:PORT --channel NAME \
(--state-dir DIR | --client-cert FILE --client-key FILE --ca FILE) [--frontend DNSNAME:PORT] \
[--enroll-frontend DNSNAME:PORT] \
[--operator-api http://IP:PORT --token-file FILE] \
[--helper-socket PATH] [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N] \
[--health-poll-secs N] [--retry-secs N]\n";

const FLAGS: [&str; 16] = [
    "--coordinator",
    "--state-dir",
    "--frontend",
    "--enroll-frontend",
    "--client-cert",
    "--client-key",
    "--ca",
    "--channel",
    "--operator-api",
    "--token-file",
    "--helper-socket",
    "--heartbeat-secs",
    "--drain-deadline-secs",
    "--health-timeout-secs",
    "--health-poll-secs",
    "--retry-secs",
];

/// Where the helper listens unless a flag says otherwise.
pub const DEFAULT_HELPER_SOCKET: &str = "/run/kbf-host-install/local.sock";

/// How long, and how often, the agent waits for things.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Between two heartbeats.
    pub heartbeat: Duration,
    /// How long the node's leases are waited for once it is drained.
    pub drain_deadline: Duration,
    /// How often they are looked at.
    pub drain_poll: Duration,
    /// How long a release on trial has to show healthy.
    pub health_timeout: Duration,
    /// How often it is looked at.
    pub health_poll: Duration,
    /// How long a target that failed is left alone.
    pub retry: Duration,
    /// How much of the helper's window is kept for going back: the health wait ends this
    /// long before the helper's own deadline.
    pub margin: Duration,
}

impl Timing {
    pub const DEFAULT: Self = Self {
        heartbeat: Duration::from_secs(15),
        drain_deadline: Duration::from_secs(600),
        drain_poll: Duration::from_secs(5),
        health_timeout: Duration::from_secs(300),
        health_poll: Duration::from_secs(5),
        retry: Duration::from_secs(900),
        margin: Duration::from_secs(10),
    };
}

/// The settings of an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub coordinator: Endpoint,
    pub files: TlsFiles,
    pub channel: String,
    pub helper_socket: PathBuf,
    /// The frontend's worker listener: where the node's own drain, status and storage health
    /// are asked.
    pub frontend: Option<Endpoint>,
    /// The frontend's enroll listener: where the certificate is renewed.
    pub enroll_frontend: Option<Endpoint>,
    /// The enrollment's state directory, if the certificate is the agent's to renew.
    pub state_dir: Option<PathBuf>,
    /// The frontend's operator API for the drain and the health, if the operator gave the agent
    /// the credential for it.
    pub node_api: Option<NodeApi>,
    pub timing: Timing,
}

/// The frontend's loopback operator API and the credential for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeApi {
    pub addr: SocketAddr,
    pub token_file: PathBuf,
}

/// The `--flag value` and `--flag=value` pairs of `args`, each flag of `known` given at most
/// once.
///
/// # Errors
/// An argument that is no known flag, a flag with no value, a flag given twice.
pub(super) fn collect<'k>(
    args: &[String],
    known: &[&'k str],
) -> Result<BTreeMap<&'k str, String>, String> {
    let mut found = BTreeMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let Some(&flag) = known.iter().find(|f| **f == name) else {
            return Err(format!("unknown argument {name}"));
        };
        let value = match inline {
            Some(value) => value,
            None => rest
                .next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))?,
        };
        if found.insert(flag, value).is_some() {
            return Err(format!("{flag} is given twice"));
        }
    }
    Ok(found)
}

/// A number of seconds from 1 to a day.
fn secs(flag: &str, text: &str) -> Result<Duration, String> {
    text.parse::<u64>()
        .ok()
        .filter(|n| (1..=86_400).contains(n))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{flag} must be a number of seconds from 1 to 86400"))
}

/// Read the flags (after `agent`). Each is given once, as `--flag value` or `--flag=value`.
///
/// # Errors
/// What is wrong with them, in a line.
pub fn parse_flags(args: &[String]) -> Result<Config, String> {
    let mut found = collect(args, &FLAGS)?;
    let mut take = |flag: &str| {
        found
            .remove(flag)
            .ok_or_else(|| format!("{flag} is required"))
    };
    let coordinator =
        Endpoint::parse(&take("--coordinator")?).map_err(|why| format!("--coordinator: {why}"))?;
    let channel = take("--channel")?;
    if !channel_ok(&channel) {
        return Err("--channel must be a channel name: [a-z0-9][a-z0-9-]{0,31}".to_owned());
    }
    let frontend = found
        .remove("--frontend")
        .map(|text| Endpoint::parse(&text).map_err(|why| format!("--frontend: {why}")))
        .transpose()?;
    let enroll_frontend = found
        .remove("--enroll-frontend")
        .map(|text| Endpoint::parse(&text).map_err(|why| format!("--enroll-frontend: {why}")))
        .transpose()?;
    let state_dir = found.remove("--state-dir").map(PathBuf::from);
    let files = tls_files(
        state_dir.as_deref(),
        [
            found.remove("--client-cert"),
            found.remove("--client-key"),
            found.remove("--ca"),
        ],
    )?;
    if state_dir.is_some() && enroll_frontend.is_none() {
        return Err(
            "--state-dir needs --enroll-frontend: the certificate is renewed there".to_owned(),
        );
    }
    let api = found.remove("--operator-api");
    let token = found.remove("--token-file");
    let node_api = match (api, token) {
        (Some(api), Some(token)) => Some(NodeApi {
            addr: parse_operator_url(&api)?,
            token_file: PathBuf::from(token),
        }),
        (None, None) => None,
        _ => {
            return Err(
                "--operator-api and --token-file are given together, or not at all".to_owned(),
            );
        }
    };
    let helper_socket = found
        .remove("--helper-socket")
        .map_or_else(|| PathBuf::from(DEFAULT_HELPER_SOCKET), PathBuf::from);
    let mut timing = Timing::DEFAULT;
    for (flag, slot) in [
        ("--heartbeat-secs", &mut timing.heartbeat),
        ("--drain-deadline-secs", &mut timing.drain_deadline),
        ("--health-timeout-secs", &mut timing.health_timeout),
        ("--health-poll-secs", &mut timing.health_poll),
        ("--retry-secs", &mut timing.retry),
    ] {
        if let Some(text) = found.remove(flag) {
            *slot = secs(flag, &text)?;
        }
    }
    Ok(Config {
        coordinator,
        files,
        channel,
        helper_socket,
        frontend,
        enroll_frontend,
        state_dir,
        node_api,
        timing,
    })
}

/// The files of this host's TLS: those of `state_dir` (`kbf-updater enroll` made them), or the
/// three an operator placed, and never a mixture.
fn tls_files(state_dir: Option<&Path>, placed: [Option<String>; 3]) -> Result<TlsFiles, String> {
    match (state_dir, placed) {
        (Some(dir), [None, None, None]) => {
            let store = Store::unopened(dir);
            Ok(TlsFiles {
                cert: store.cert_path(Role::Updater),
                key: store.key_path(Role::Updater),
                ca: store.ca_path(),
            })
        }
        (None, [Some(cert), Some(key), Some(ca)]) => Ok(TlsFiles {
            cert: PathBuf::from(cert),
            key: PathBuf::from(key),
            ca: PathBuf::from(ca),
        }),
        _ => Err(
            "give --state-dir, or all of --client-cert, --client-key and --ca (and not \
                  both)"
                .to_owned(),
        ),
    }
}

/// The grammar of a channel name, as the helper checks it (`[a-z0-9][a-z0-9-]{0,31}`).
fn channel_ok(text: &str) -> bool {
    let lower_or_digit = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let Some((first, rest)) = text.split_at_checked(1) else {
        return false;
    };
    text.len() <= 32
        && first.bytes().all(lower_or_digit)
        && rest.bytes().all(|b| lower_or_digit(b) || b == b'-')
}
