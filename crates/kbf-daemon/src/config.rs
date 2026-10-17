//! Configuration: command-line flags, and the TLS material they name.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_route::Liveness;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

/// T: a daemon fences its leases this long after the newest acknowledged heartbeat
/// (`docs/design/scheduler.md#fencing-g-and-t`). The scheduler re-dispatches after
/// G = 60 s; safety needs T + 5 s < G.
pub const FENCE_AFTER: Duration = Duration::from_secs(40);

/// How long the daemon waits at most before it compares the fence with the
/// suspend-counting clock again. Timers stop while the machine is suspended, so a
/// resume is noticed within this much running time (issue #78).
pub const RECHECK_EVERY: Duration = Duration::from_secs(1);

/// HTTP/2 keepalive on the daemon's channels to the server (its worker stream and its
/// blob channel): a PING every `interval`, idle or not, and the connection closed when
/// one is not acknowledged within `timeout`. A connection that died without a FIN or
/// RST (a dropped NAT entry, a peer that lost power) then fails its calls within
/// `interval + timeout`, instead of leaving a call waiting for an answer that cannot
/// come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeepAlive {
    pub interval: Duration,
    pub timeout: Duration,
}

impl KeepAlive {
    /// A PING every 20 s, closed after 20 s without its acknowledgement.
    pub const DEFAULT: Self = Self {
        interval: Duration::from_secs(20),
        timeout: Duration::from_secs(20),
    };

    /// `endpoint` with this keepalive.
    #[must_use]
    pub fn apply(self, endpoint: Endpoint) -> Endpoint {
        endpoint
            .http2_keep_alive_interval(self.interval)
            .keep_alive_timeout(self.timeout)
            .keep_alive_while_idle(true)
    }
}

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
    /// The longest, in milliseconds, that dialling a storage node may take: for its TCP
    /// connection and for its TLS handshake each. Must be 1 to 3600000 (one hour).
    #[arg(long, default_value_t = STORAGE_LIVENESS_DEFAULT_MS)]
    pub storage_dial_timeout_ms: u64,
    /// How often, in milliseconds, a connection to a storage node with a call on it is
    /// pinged (HTTP/2 PING). Must be 1 to 3600000 (one hour).
    #[arg(long, default_value_t = STORAGE_LIVENESS_DEFAULT_MS)]
    pub storage_ping_interval_ms: u64,
    /// How long, in milliseconds, a ping to a storage node may go unanswered before the
    /// connection is dropped and the calls on it fail over to the next owner. Must be 1
    /// to 3600000 (one hour) and no more than `--storage-ping-interval-ms`.
    #[arg(long, default_value_t = STORAGE_LIVENESS_DEFAULT_MS)]
    pub storage_ping_timeout_ms: u64,
}

/// The default of each `--storage-*-ms` flag: [`Liveness::default`]'s ten seconds.
const STORAGE_LIVENESS_DEFAULT_MS: u64 = 10_000;

/// The largest value of each `--storage-*-ms` flag: one hour. A value beyond it is a
/// typo (seconds or microseconds given as milliseconds) or a silent node waited on for
/// longer than any call is worth.
const STORAGE_LIVENESS_MAX_MS: u64 = 3_600_000;

impl Args {
    /// How long the daemon waits on a silent storage node, from the `--storage-*-ms`
    /// flags.
    ///
    /// # Errors
    /// A value is zero or over one hour, or the ping timeout is longer than the ping interval (a ping
    /// would then be sent before the last one is given up on).
    pub fn storage_liveness(&self) -> Result<Liveness, ConfigError> {
        for (flag, ms) in [
            ("--storage-dial-timeout-ms", self.storage_dial_timeout_ms),
            ("--storage-ping-interval-ms", self.storage_ping_interval_ms),
            ("--storage-ping-timeout-ms", self.storage_ping_timeout_ms),
        ] {
            if ms == 0 {
                return Err(ConfigError::ZeroDuration(flag));
            }
            if ms > STORAGE_LIVENESS_MAX_MS {
                return Err(ConfigError::DurationTooLong(flag));
            }
        }
        if self.storage_ping_timeout_ms > self.storage_ping_interval_ms {
            return Err(ConfigError::PingTimeoutAfterInterval {
                timeout_ms: self.storage_ping_timeout_ms,
                interval_ms: self.storage_ping_interval_ms,
            });
        }
        Ok(Liveness {
            connect: Duration::from_millis(self.storage_dial_timeout_ms),
            keepalive_interval: Duration::from_millis(self.storage_ping_interval_ms),
            keepalive_timeout: Duration::from_millis(self.storage_ping_timeout_ms),
        })
    }

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
    #[error("{0} must be at least 1")]
    ZeroDuration(&'static str),
    #[error("{0} must be at most 3600000 (one hour)")]
    DurationTooLong(&'static str),
    #[error(
        "--storage-ping-timeout-ms {timeout_ms} is longer than \
         --storage-ping-interval-ms {interval_ms}"
    )]
    PingTimeoutAfterInterval { timeout_ms: u64, interval_ms: u64 },
}

impl TlsFiles {
    /// Reads the files into a client TLS configuration that presents the daemon's
    /// certificate and trusts only the given CA.
    pub fn load(&self) -> Result<ClientTlsConfig, ConfigError> {
        let (ca, identity) = self.trust()?;
        let mut tls = ClientTlsConfig::new().ca_certificate(ca).identity(identity);
        if let Some(name) = &self.server_name {
            tls = tls.domain_name(name.clone());
        }
        Ok(tls)
    }
}

impl TlsFiles {
    /// The CA the daemon trusts and the identity it presents, and no server name: what
    /// a connection to a storage node is made from. Each node is verified against its
    /// own name from the map (`kbf_route::TlsNodes`), so `--tls-server-name`, which names
    /// the frontend, is not read.
    ///
    /// # Errors
    /// A file cannot be read.
    pub fn trust(&self) -> Result<(Certificate, Identity), ConfigError> {
        let ca = Certificate::from_pem(read(&self.ca_cert)?);
        let identity = Identity::from_pem(read(&self.cert)?, read(&self.key)?);
        Ok((ca, identity))
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
    /// HTTP/2 keepalive on the worker stream's channel, normally
    /// [`KeepAlive::DEFAULT`]; the binary gives its blob channel the same.
    pub keep_alive: KeepAlive,
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
            keep_alive: KeepAlive::DEFAULT,
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

    /// Catches a channel without HTTP/2 keepalive (the 15-hour upload hang): over a
    /// connection whose peer takes every byte and never answers, as a dead peer behind
    /// a live middlebox does, a call must fail once a PING goes unacknowledged. Without
    /// keepalive it waits for ever, here until the test's 10 s.
    #[tokio::test]
    async fn keep_alive_fails_a_call_on_a_silent_connection() {
        use kbf_proto::google::bytestream::QueryWriteStatusRequest;
        use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
        use tokio::io::AsyncReadExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        // The peer takes the one connection's bytes until it closes.
        let peer = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.expect("accept");
            let mut taken = [0u8; 4096];
            while peer.read(&mut taken).await.unwrap_or(0) > 0 {}
        });
        let keep_alive = KeepAlive {
            interval: Duration::from_millis(100),
            timeout: Duration::from_millis(100),
        };
        let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("url");
        let mut client = ByteStreamClient::new(keep_alive.apply(endpoint).connect_lazy());
        let call = client.query_write_status(QueryWriteStatusRequest::default());
        let ended = tokio::time::timeout(Duration::from_secs(10), call)
            .await
            .expect("the unanswered PING ends the call within 10 s");
        assert!(ended.is_err(), "{ended:?}");
        drop(client);
        tokio::time::timeout(Duration::from_secs(10), peer)
            .await
            .expect("the peer sees the connection close")
            .expect("the peer task");
    }

    /// Catches a daemon configured without keepalive.
    #[test]
    fn the_daemon_keeps_its_channels_alive() {
        let config = DaemonConfig::from_args(&parse(&[]).expect("flags"));
        assert_eq!(config.keep_alive, KeepAlive::DEFAULT);
        assert_eq!(
            KeepAlive::DEFAULT,
            KeepAlive {
                interval: Duration::from_secs(20),
                timeout: Duration::from_secs(20),
            }
        );
    }

    /// Catches storage-liveness flags whose defaults drifted from the library's, or that
    /// do not reach the value: each flag must set its own field, in milliseconds.
    #[test]
    fn the_storage_liveness_flags_default_to_the_libraries_and_reach_it() {
        assert_eq!(
            parse(&[])
                .expect("defaults")
                .storage_liveness()
                .expect("ok"),
            Liveness::default()
        );
        let args = parse(&[
            "--storage-dial-timeout-ms=300",
            "--storage-ping-interval-ms=200",
            "--storage-ping-timeout-ms=100",
        ])
        .expect("flags");
        assert_eq!(
            args.storage_liveness().expect("valid"),
            Liveness {
                connect: Duration::from_millis(300),
                keepalive_interval: Duration::from_millis(200),
                keepalive_timeout: Duration::from_millis(100),
            }
        );
        // A timeout equal to the interval is allowed: it is the default.
        parse(&["--storage-ping-timeout-ms=10000"])
            .expect("flags")
            .storage_liveness()
            .expect("equal");
    }

    /// Catches a zero that would make every dial or ping fail at once, and a ping
    /// timeout longer than the interval, each refused with a message naming the flag.
    #[test]
    fn the_storage_liveness_flags_refuse_zero_and_a_timeout_past_the_interval() {
        for flag in [
            "--storage-dial-timeout-ms",
            "--storage-ping-interval-ms",
            "--storage-ping-timeout-ms",
        ] {
            let error = parse(&[&format!("{flag}=0")])
                .expect("parses")
                .storage_liveness()
                .expect_err("zero");
            assert_eq!(error.to_string(), format!("{flag} must be at least 1"));
        }
        let error = parse(&["--storage-ping-timeout-ms=10001"])
            .expect("parses")
            .storage_liveness()
            .expect_err("timeout past interval");
        assert_eq!(
            error.to_string(),
            "--storage-ping-timeout-ms 10001 is longer than --storage-ping-interval-ms 10000"
        );
        assert!(parse(&["--storage-dial-timeout-ms=ten"]).is_err());
    }

    /// Catches a missing or off-by-one upper bound: one hour (3600000 ms) is accepted for
    /// each flag, one millisecond more is refused naming the flag. (The ping timeout is
    /// held to the interval too, so its bound is tried with the interval at the bound.)
    #[test]
    fn the_storage_liveness_flags_accept_one_hour_and_refuse_a_millisecond_more() {
        let flags = [
            "--storage-dial-timeout-ms",
            "--storage-ping-interval-ms",
            "--storage-ping-timeout-ms",
        ];
        for flag in flags {
            // The timeout may not pass the interval, so its accepted case lifts both; the
            // refused case leaves the interval alone, so it is the flag that is named.
            let lifted = |ms: u64| {
                let mut args = vec![format!("{flag}={ms}")];
                if flag == "--storage-ping-timeout-ms" && ms <= 3_600_000 {
                    args.push(format!("--storage-ping-interval-ms={ms}"));
                }
                args
            };
            let parsed = |ms: u64| {
                let args = lifted(ms);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                parse(&args).expect("parses").storage_liveness()
            };
            parsed(3_600_000).expect("one hour is allowed");
            let error = parsed(3_600_001).expect_err("over an hour");
            assert_eq!(
                error.to_string(),
                format!("{flag} must be at most 3600000 (one hour)")
            );
        }
    }
}
