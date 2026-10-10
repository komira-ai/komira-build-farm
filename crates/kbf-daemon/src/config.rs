//! Configuration: command-line flags, and the TLS material they name.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tonic::transport::{Certificate, ClientTlsConfig, Identity};

/// T: a daemon fences its leases this long after the newest acknowledged heartbeat
/// (`docs/design/scheduler.md#fencing-g-and-t`). The scheduler re-dispatches after
/// G = 60 s; safety needs T + 5 s < G.
pub const FENCE_AFTER: Duration = Duration::from_secs(40);

/// How long the daemon waits at most before it compares the fence with the
/// suspend-counting clock again. Timers stop while the machine is suspended, so a
/// resume is noticed within this much running time (issue #78).
pub const RECHECK_EVERY: Duration = Duration::from_secs(1);

/// The session flags of the `kbf-daemon` command line: where to connect, as whom, and
/// how this node is labelled. The binary (crate `kbf-node`) adds the driver flags.
#[derive(Clone, Debug, clap::Args)]
pub struct Args {
    /// A kbf-server worker listener to connect to, as an `https://host:port` URL;
    /// repeatable, or several separated by commas. The host may be a DNS name with a
    /// record per server: every round of attempts resolves it again and tries each
    /// address. The daemon never stops trying (see `--reconnect-max-ms`).
    #[arg(long = "server", required = true, value_delimiter = ',')]
    pub servers: Vec<String>,
    /// PEM file of the CA certificates that sign server certificates.
    #[arg(long)]
    pub ca_cert: PathBuf,
    /// PEM file of this daemon's client certificate chain.
    #[arg(long)]
    pub cert: PathBuf,
    /// PEM file of this daemon's private key.
    #[arg(long)]
    pub key: PathBuf,
    /// The name to verify in the server certificate, when it is not the URL's host.
    #[arg(long)]
    pub tls_server_name: Option<String>,
    /// A stable name for this node, unique within the cell.
    #[arg(long)]
    pub node_id: String,
    /// The wait, in milliseconds, after a session ends and after the first round of
    /// connection attempts that all failed. It doubles with each further failed round.
    #[arg(long, default_value_t = 1000)]
    pub reconnect_ms: u64,
    /// The longest wait between rounds of connection attempts, in milliseconds. Each
    /// wait is jittered down by up to half.
    #[arg(long, default_value_t = 30_000)]
    pub reconnect_max_ms: u64,
    /// A node label, `key=value`, reported as `label.<key>`; repeatable. A Mac in
    /// komira's pool runs with `--label pool=darwin-sized`.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
}

impl Args {
    /// The node report entries the labels add: `(label.<key>, value)`.
    #[must_use]
    pub fn label_entries(&self) -> Vec<(String, String)> {
        self.labels
            .iter()
            .map(|(key, value)| (format!("label.{key}"), value.clone()))
            .collect()
    }
}

/// A `--label` value: `key=value`, the key made of ASCII letters, digits, `.`, `_`
/// and `-`, the value non-empty and without whitespace.
fn parse_label(text: &str) -> Result<(String, String), String> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not key=value"))?;
    let key_ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !key_ok {
        return Err(format!(
            "label key {key:?} must be ASCII letters, digits, '.', '_' or '-'"
        ));
    }
    if value.is_empty() || value.contains(char::is_whitespace) {
        return Err(format!(
            "label value {value:?} must be non-empty and contain no whitespace"
        ));
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// The TLS files a daemon authenticates with: the server's CA, and its own
/// certificate and key.
#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub ca_cert: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub server_name: Option<String>,
}

/// Why configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("server URL {0:?} is not https; the daemon connects only over mutual TLS")]
    NotHttps(String),
    #[error("server URL {url:?}: {reason}")]
    BadServer { url: String, reason: String },
    #[error("no server URL given")]
    NoServer,
}

impl TlsFiles {
    /// Reads the files into a client TLS configuration that presents the daemon's
    /// certificate and trusts only the given CA.
    pub fn load(&self) -> Result<ClientTlsConfig, ConfigError> {
        let ca = Certificate::from_pem(read(&self.ca_cert)?);
        let identity = Identity::from_pem(read(&self.cert)?, read(&self.key)?);
        let mut tls = ClientTlsConfig::new().ca_certificate(ca).identity(identity);
        if let Some(name) = &self.server_name {
            tls = tls.domain_name(name.clone());
        }
        Ok(tls)
    }
}

fn read(path: &Path) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })
}

/// Everything a [`crate::Daemon`] needs besides its runtime and node report.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// The servers' worker listeners, each an `https` URL (see [`crate::connect`]).
    pub servers: Vec<String>,
    pub tls: TlsFiles,
    pub node_id: String,
    /// T, normally [`FENCE_AFTER`]. Tests shorten it.
    pub fence_after: Duration,
    /// The wait after a session ends, and the first wait after a failed round of
    /// connection attempts.
    pub reconnect_after: Duration,
    /// The longest wait between rounds of connection attempts, normally
    /// [`crate::connect::RECONNECT_MAX`].
    pub reconnect_max: Duration,
    /// How long to wait for Welcome after opening a stream.
    pub welcome_timeout: Duration,
    /// The longest wait before the fence is checked again, normally
    /// [`RECHECK_EVERY`]. Tests lengthen it to show which event checked it.
    pub recheck_every: Duration,
}

impl DaemonConfig {
    /// A configuration for one server URL, with the production fence time.
    #[must_use]
    pub fn new(server: String, tls: TlsFiles, node_id: String) -> Self {
        Self {
            servers: vec![server],
            tls,
            node_id,
            fence_after: FENCE_AFTER,
            reconnect_after: Duration::from_secs(1),
            reconnect_max: crate::connect::RECONNECT_MAX,
            welcome_timeout: Duration::from_secs(10),
            recheck_every: RECHECK_EVERY,
        }
    }

    /// The configuration the command line describes.
    #[must_use]
    pub fn from_args(args: &Args) -> Self {
        let tls = TlsFiles {
            ca_cert: args.ca_cert.clone(),
            cert: args.cert.clone(),
            key: args.key.clone(),
            server_name: args.tls_server_name.clone(),
        };
        let mut config = Self::new(String::new(), tls, args.node_id.clone());
        config.servers.clone_from(&args.servers);
        config.reconnect_after = Duration::from_millis(args.reconnect_ms);
        config.reconnect_max = Duration::from_millis(args.reconnect_max_ms);
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(clap::Parser)]
    struct Line {
        #[command(flatten)]
        args: Args,
    }

    fn parse(extra: &[&str]) -> Result<Args, clap::Error> {
        let base = [
            "kbf-daemon",
            "--server=https://front:7070",
            "--ca-cert=ca.pem",
            "--cert=c.pem",
            "--key=c.key",
            "--node-id=mac-1",
        ];
        <Line as clap::Parser>::try_parse_from(base.iter().chain(extra)).map(|l| l.args)
    }

    /// Catches: a label flag that is not repeatable, reports under another key than
    /// `label.<key>`, or accepts a key or value the matcher could not compare exactly
    /// (empty, spaced, or without `=`).
    #[test]
    fn labels_are_checked_and_reported_under_label_keys() {
        let args = parse(&["--label", "pool=darwin-sized", "--label=rack=r1"]).expect("labels");
        assert_eq!(
            args.label_entries(),
            [
                ("label.pool".to_owned(), "darwin-sized".to_owned()),
                ("label.rack".to_owned(), "r1".to_owned())
            ]
        );
        assert!(parse(&[]).expect("no labels").label_entries().is_empty());
        for bad in ["pool", "=x", "po ol=x", "pool=", "pool=a b", "p/l=x"] {
            assert!(parse(&["--label", bad]).is_err(), "{bad:?}");
        }
    }

    /// Catches: flags that do not reach the configuration (a reconnect wait or its
    /// maximum ignored, TLS files swapped), and a server list that keeps only one of
    /// several `--server` flags or comma-separated URLs.
    #[test]
    fn the_flags_reach_the_configuration() {
        let args = parse(&[
            "--reconnect-ms=250",
            "--reconnect-max-ms=9000",
            "--tls-server-name=front",
        ])
        .expect("flags");
        let config = DaemonConfig::from_args(&args);
        assert_eq!(config.servers, ["https://front:7070"]);
        assert_eq!(config.reconnect_max, Duration::from_millis(9000));
        assert_eq!(config.node_id, "mac-1");
        assert_eq!(config.reconnect_after, Duration::from_millis(250));
        assert_eq!(config.tls.ca_cert, PathBuf::from("ca.pem"));
        assert_eq!(config.tls.cert, PathBuf::from("c.pem"));
        assert_eq!(config.tls.key, PathBuf::from("c.key"));
        assert_eq!(config.tls.server_name.as_deref(), Some("front"));
        assert_eq!(config.fence_after, FENCE_AFTER);
        assert_eq!(config.recheck_every, RECHECK_EVERY);

        let defaults = DaemonConfig::from_args(&parse(&[]).expect("defaults"));
        assert_eq!(defaults.reconnect_after, Duration::from_secs(1));
        assert_eq!(defaults.reconnect_max, Duration::from_secs(30));

        let several = parse(&[
            "--server=https://b:1,https://c:2",
            "--server",
            "https://d:3",
        ])
        .expect("several servers");
        assert_eq!(
            DaemonConfig::from_args(&several).servers,
            [
                "https://front:7070",
                "https://b:1",
                "https://c:2",
                "https://d:3"
            ]
        );
    }
}
