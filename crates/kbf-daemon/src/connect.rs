//! Finding a server to connect to, and how long to wait between tries.
//!
//! A daemon is given one or more `--server` URLs. Each names a host and a port; the
//! host may be an address, or a DNS name with a record per server. Every round of
//! connection attempts resolves every name again (a server that moved, or one added
//! to the name, is found without a restart) and tries each address it found in turn.
//! An attempt that ends before the server's `Welcome`, for any reason (a refused
//! connection, a TLS failure, a server answering `UNAVAILABLE`, no `Welcome` in time),
//! moves on to the next address at once. Only when every address of a round has
//! failed does the daemon wait, and it never stops trying:
//!
//! - the wait after the `n`th failed round in a row is `--reconnect-ms` times
//!   `2^(n-1)`, capped at `--reconnect-max-ms` (default [`RECONNECT_MAX`], 30 s), and
//!   jittered down by up to half, so daemons that lost one server do not all come back
//!   in step;
//! - a session that was welcomed resets the count: when it ends, the daemon waits
//!   `--reconnect-ms` (jittered the same way) and starts a new round, beginning with
//!   the address after the one whose session ended.
//!
//! Failures are logged at WARN at most once per [`WARN_EVERY`] while they repeat
//! (the first failure after a session always is); the others go to DEBUG, and each WARN
//! counts the attempts since the last session.

use std::collections::BTreeSet;
use std::hash::{BuildHasher, RandomState};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::time::Instant;
use tonic::codegen::http::Uri;

use crate::config::ConfigError;

/// The default longest wait between rounds of connection attempts.
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// While connection attempts keep failing, at most one is logged at WARN this often.
pub const WARN_EVERY: Duration = Duration::from_secs(60);

/// One `--server` URL: an `https` URL, its host (an address or a DNS name, without
/// the brackets of an IPv6 address) and its port (443 when the URL names none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub url: String,
    pub host: String,
    pub port: u16,
}

impl Server {
    /// Parses a `--server` URL. Only `https` is accepted: the daemon connects only over
    /// mutual TLS.
    pub fn parse(url: &str) -> Result<Self, ConfigError> {
        if !url.starts_with("https://") {
            return Err(ConfigError::NotHttps(url.to_owned()));
        }
        let bad = |reason: &str| ConfigError::BadServer {
            url: url.to_owned(),
            reason: reason.to_owned(),
        };
        let uri: Uri = url.parse().map_err(|_| bad("not a URL"))?;
        let host = uri
            .host()
            .filter(|h| !h.is_empty())
            .ok_or_else(|| bad("no host"))?;
        if uri.path() != "/" && !uri.path().is_empty() {
            return Err(bad("a server URL names no path"));
        }
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        Ok(Self {
            url: url.to_owned(),
            host: host.to_owned(),
            port: uri.port_u16().unwrap_or(443),
        })
    }
}

/// One address to try: the server URL it came from and the address its host resolved
/// to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub server: Server,
    pub addr: SocketAddr,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.server.url, self.addr)
    }
}

/// Turns a server's host and port into addresses. [`SystemResolver`] asks the
/// operating system; a test passes its own to change the answer between rounds.
pub trait Resolve: Send + Sync {
    fn resolve(&self, host: String, port: u16) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>>;
}

/// Resolves through the operating system's resolver (`getaddrinfo`), asked anew
/// each time. An address literal resolves to itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, host: String, port: u16) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
        Box::pin(async move { Ok(tokio::net::lookup_host((host, port)).await?.collect()) })
    }
}

/// Resolves every server, each within `within`: the addresses to try this round, in
/// the order of the servers and then of each name's records, each address once; and,
/// for each server that resolved to nothing, why.
pub async fn resolve_all(
    resolver: &Arc<dyn Resolve>,
    servers: &[Server],
    within: Duration,
) -> (Vec<Target>, Vec<(String, String)>) {
    let mut targets = Vec::new();
    let mut failed = Vec::new();
    let mut seen = BTreeSet::new();
    for server in servers {
        let answer =
            tokio::time::timeout(within, resolver.resolve(server.host.clone(), server.port));
        match answer.await {
            Ok(Ok(addrs)) if !addrs.is_empty() => {
                for addr in addrs {
                    if seen.insert(addr) {
                        targets.push(Target {
                            server: server.clone(),
                            addr,
                        });
                    }
                }
            }
            Ok(Ok(_)) => failed.push((server.url.clone(), "resolved to no address".to_owned())),
            Ok(Err(e)) => failed.push((server.url.clone(), format!("resolve: {e}"))),
            Err(_) => failed.push((
                server.url.clone(),
                format!("resolve: no answer within {within:?}"),
            )),
        }
    }
    (targets, failed)
}

/// `targets` in the order to try them: starting just after `last` when it is among
/// them, else from the first.
#[must_use]
pub fn rotate_after(mut targets: Vec<Target>, last: Option<SocketAddr>) -> Vec<Target> {
    if let Some(at) = last.and_then(|last| targets.iter().position(|t| t.addr == last)) {
        targets.rotate_left(at + 1);
    }
    targets
}

/// The wait between rounds: doubling from `base` with each failed round in a row, at
/// most `max`, then jittered into its upper half.
#[derive(Clone, Debug)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    failed_rounds: u32,
}

impl Backoff {
    /// `max` below `base` is taken as `base`.
    #[must_use]
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max: max.max(base),
            failed_rounds: 0,
        }
    }

    /// Failed rounds since the last welcomed session.
    #[must_use]
    pub fn failed_rounds(&self) -> u32 {
        self.failed_rounds
    }

    /// A round failed: the wait before the next, from `unit` in `[0, 1)`.
    pub fn failed(&mut self, unit: f64) -> Duration {
        self.failed_rounds = self.failed_rounds.saturating_add(1);
        let doublings = (self.failed_rounds - 1).min(31);
        let wait = self.base.saturating_mul(1 << doublings).min(self.max);
        jittered(wait, unit)
    }

    /// A welcomed session ended: the count starts over, and the wait is `base`'s.
    pub fn welcomed(&mut self, unit: f64) -> Duration {
        self.failed_rounds = 0;
        jittered(self.base, unit)
    }
}

/// `wait` less up to half of it: `wait * (1 - unit / 2)` for `unit` in `[0, 1)`.
fn jittered(wait: Duration, unit: f64) -> Duration {
    wait.mul_f64(1.0 - unit.clamp(0.0, 1.0) / 2.0)
}

/// A number in `[0, 1)` from a hasher the standard library keys from the operating
/// system's random source: jitter needs no stronger randomness.
#[must_use]
pub fn unit_random() -> f64 {
    let bits = RandomState::new().hash_one(0u8) >> 11;
    #[allow(clippy::cast_precision_loss)] // 53 bits fit an f64 exactly
    let unit = bits as f64 / (1u64 << 53) as f64;
    unit
}

/// Decides which connection failures are logged at WARN: the first after a session
/// (or since the daemon started), then at most one per `every`.
#[derive(Clone, Debug)]
pub struct WarnLimit {
    every: Duration,
    last: Option<Instant>,
    /// Failed attempts since the last welcomed session.
    attempts: u64,
}

impl WarnLimit {
    #[must_use]
    pub fn new(every: Duration) -> Self {
        Self {
            every,
            last: None,
            attempts: 0,
        }
    }

    /// An attempt failed at `now`: the count of attempts failed since the last
    /// session, and whether this one is logged at WARN.
    pub fn failed(&mut self, now: Instant) -> (u64, bool) {
        self.attempts += 1;
        let warn = self
            .last
            .is_none_or(|last| now.duration_since(last) >= self.every);
        if warn {
            self.last = Some(now);
        }
        (self.attempts, warn)
    }

    /// A session was welcomed: the next failure is logged at WARN again.
    pub fn welcomed(&mut self) {
        self.last = None;
        self.attempts = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str, addr: &str) -> Target {
        Target {
            server: Server::parse(url).expect("url"),
            addr: addr.parse().expect("addr"),
        }
    }

    /// Catches: a plain-text URL taken, a port other than the URL's (or 443 when it
    /// names none), an IPv6 host kept in brackets (which no resolver takes), and a URL
    /// with no host or with a path taken.
    #[test]
    fn server_urls_are_parsed() {
        let s = Server::parse("https://farm.test:7070").expect("name");
        assert_eq!((s.host.as_str(), s.port), ("farm.test", 7070));
        let s = Server::parse("https://farm.test").expect("no port");
        assert_eq!(s.port, 443);
        let s = Server::parse("https://[::1]:7070/").expect("v6");
        assert_eq!((s.host.as_str(), s.port), ("::1", 7070));
        assert!(matches!(
            Server::parse("http://farm.test:7070"),
            Err(ConfigError::NotHttps(_))
        ));
        for bad in [
            "https://",
            "https://:7070",
            "https://farm.test:7070/x",
            "https://a b",
        ] {
            assert!(
                matches!(Server::parse(bad), Err(ConfigError::BadServer { .. })),
                "{bad}"
            );
        }
    }

    /// Catches: a wait that does not double per failed round, exceeds the maximum,
    /// overflows after many rounds, is not reset by a session, or is jittered outside
    /// its upper half.
    #[test]
    fn the_wait_doubles_to_its_cap_and_a_session_resets_it() {
        let ms = Duration::from_millis;
        let mut b = Backoff::new(ms(100), ms(1000));
        let waits: Vec<Duration> = (0..6).map(|_| b.failed(0.0)).collect();
        assert_eq!(
            waits,
            [ms(100), ms(200), ms(400), ms(800), ms(1000), ms(1000)]
        );
        for _ in 0..100 {
            assert_eq!(b.failed(0.0), ms(1000), "capped forever");
        }
        assert_eq!(b.failed_rounds(), 106);
        assert_eq!(b.welcomed(0.0), ms(100));
        assert_eq!(b.failed_rounds(), 0);
        assert_eq!(b.failed(0.0), ms(100), "the count started over");
        // Jitter takes off up to half: the second round's 200 ms, nearly halved.
        let low = b.failed(0.999_999);
        assert!(low >= ms(100) && low < ms(101), "{low:?}");
        assert_eq!(Backoff::new(ms(100), ms(1000)).failed(0.5), ms(75));
        // A maximum below the base is the base.
        assert_eq!(Backoff::new(ms(100), ms(10)).failed(0.0), ms(100));
        let mut d = Backoff::new(Duration::from_secs(1), RECONNECT_MAX);
        let longest = (0..50).map(|_| d.failed(0.0)).max();
        assert_eq!(longest, Some(Duration::from_secs(30)));
    }

    /// Catches: jitter that is not in `[0, 1)`, or the same every time (no jitter).
    #[test]
    fn the_jitter_source_spreads_over_the_unit_interval() {
        let draws: Vec<f64> = (0..64).map(|_| unit_random()).collect();
        assert!(draws.iter().all(|u| (0.0..1.0).contains(u)), "{draws:?}");
        assert!(draws.iter().any(|u| *u != draws[0]), "{draws:?}");
    }

    /// Catches: every failure logged at WARN (a dead server floods the log), a WARN
    /// never repeated while failures go on (an operator sees one line, hours old), and
    /// a session that does not make the next failure a WARN.
    #[test]
    fn failures_warn_at_most_once_a_period() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut w = WarnLimit::new(Duration::from_secs(60));
        assert_eq!(w.failed(at(0)), (1, true));
        assert_eq!(w.failed(at(1)), (2, false));
        assert_eq!(w.failed(at(59)), (3, false));
        assert_eq!(w.failed(at(60)), (4, true));
        assert_eq!(w.failed(at(61)), (5, false));
        w.welcomed();
        assert_eq!(w.failed(at(62)), (1, true));
    }

    /// Catches: a round that starts again at the address whose session just ended
    /// (after a failover the daemon would dial the old leader first), or that drops
    /// or repeats an address.
    #[test]
    fn a_round_starts_after_the_last_address() {
        let all = vec![
            target("https://farm.test:1", "192.0.2.1:1"),
            target("https://farm.test:1", "192.0.2.2:1"),
            target("https://farm.test:1", "192.0.2.3:1"),
        ];
        let order = |last: Option<&str>| -> Vec<String> {
            rotate_after(all.clone(), last.map(|l| l.parse().expect("addr")))
                .iter()
                .map(|t| t.addr.to_string())
                .collect()
        };
        assert_eq!(order(None), ["192.0.2.1:1", "192.0.2.2:1", "192.0.2.3:1"]);
        assert_eq!(
            order(Some("192.0.2.2:1")),
            ["192.0.2.3:1", "192.0.2.1:1", "192.0.2.2:1"]
        );
        assert_eq!(
            order(Some("192.0.2.3:1")),
            ["192.0.2.1:1", "192.0.2.2:1", "192.0.2.3:1"]
        );
        assert_eq!(
            order(Some("192.0.2.99:1")),
            ["192.0.2.1:1", "192.0.2.2:1", "192.0.2.3:1"]
        );
    }

    struct Fixed(io::Result<Vec<SocketAddr>>);

    impl Resolve for Fixed {
        fn resolve(&self, _: String, _: u16) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
            let answer = match &self.0 {
                Ok(addrs) => Ok(addrs.clone()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            };
            Box::pin(async move { answer })
        }
    }

    /// Catches: an address two names share tried twice in a round, a name that fails
    /// to resolve not reported (or taken for an empty round silently), and the
    /// system resolver not taking an address literal.
    #[tokio::test]
    async fn every_name_is_resolved_and_each_address_kept_once() {
        let servers = [
            Server::parse("https://a.test:1").expect("a"),
            Server::parse("https://b.test:1").expect("b"),
        ];
        let addrs: Vec<SocketAddr> = ["192.0.2.1:1", "192.0.2.2:1"]
            .iter()
            .map(|a| a.parse().expect("addr"))
            .collect();
        let resolver: Arc<dyn Resolve> = Arc::new(Fixed(Ok(addrs)));
        let (targets, failed) = resolve_all(&resolver, &servers, Duration::from_secs(1)).await;
        let got: Vec<String> = targets.iter().map(ToString::to_string).collect();
        assert_eq!(
            got,
            [
                "https://a.test:1 (192.0.2.1:1)",
                "https://a.test:1 (192.0.2.2:1)"
            ]
        );
        assert!(failed.is_empty());

        let broken: Arc<dyn Resolve> = Arc::new(Fixed(Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no such host",
        ))));
        let (targets, failed) = resolve_all(&broken, &servers, Duration::from_secs(1)).await;
        assert!(targets.is_empty());
        assert_eq!(failed.len(), 2);
        assert!(failed[0].1.contains("no such host"), "{failed:?}");

        let system: Arc<dyn Resolve> = Arc::new(SystemResolver);
        let literal = [Server::parse("https://127.0.0.1:7070").expect("literal")];
        let (targets, failed) = resolve_all(&system, &literal, Duration::from_secs(5)).await;
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].addr, "127.0.0.1:7070".parse().expect("addr"));
    }
}
