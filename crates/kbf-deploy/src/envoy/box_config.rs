//! The box topology's settings: one Envoy in front of two kbf instances on one host.
//! Its closed key set is separate from the front's ([`super::config::KEYS`]);
//! `templates/envoy/box.example` documents every key.

use std::path::Path;

use crate::config::{Config, ConfigError};

/// Every key the box config must set; any other is refused.
pub const BOX_KEYS: &[&str] = &[
    "KBF_BOX_HOST",
    "KBF_BOX_REAPI_PORT",
    "KBF_BOX_WORKER_PORT",
    "KBF_BOX_ADMIN_PORT",
    "KBF_BOX_BACKEND_PORT_BASE",
    "KBF_BOX_CERT_FILE",
    "KBF_BOX_KEY_FILE",
    "KBF_BOX_CA_FILE",
    "KBF_BOX_REAPI_MAX_STREAMS",
    "KBF_BOX_STREAM_CEILING",
    "KBF_BOX_LONGEST_ACTION_SECS",
];

/// The offsets of one instance's loopback ports from its base: REAPI, worker, API.
pub const REAPI_OFFSET: u16 = 0;
pub const WORKER_OFFSET: u16 = 1;
pub const API_OFFSET: u16 = 2;
/// Instance `b`'s ports are instance `a`'s plus this.
pub const SECOND_INSTANCE: u16 = 10;

/// The extra time after the longest action that Envoy's old epoch is kept, in seconds.
pub const PARENT_SLACK_SECS: u64 = 900;

/// One kbf instance's loopback ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instance {
    pub reapi: u16,
    pub worker: u16,
    pub api: u16,
}

/// The box settings, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoxConfig {
    /// The DNS name the kbf instances' REAPI certificate carries; Envoy's SNI and name
    /// check on the hop to them, and the Host of the health checks.
    pub host: String,
    /// The public REAPI port (clients' TLS ends in Envoy).
    pub reapi_port: u16,
    /// The public worker port (the daemons' mutual TLS passes through Envoy).
    pub worker_port: u16,
    /// Envoy's admin port, on loopback.
    pub admin_port: u16,
    /// Instance `a`'s first loopback port; `b`'s is [`SECOND_INSTANCE`] above.
    pub backend_port_base: u16,
    /// The certificate chain and key Envoy shows REAPI clients.
    pub cert_file: String,
    pub key_file: String,
    /// The CA the instances' certificates chain to.
    pub ca_file: String,
    /// kbf's `--reapi-max-streams`: the streams one upstream connection may carry.
    pub reapi_max_streams: u32,
    /// The concurrent streams Envoy must hold open across the REAPI cluster.
    pub stream_ceiling: u32,
    /// The longest action the farm runs, in seconds.
    pub longest_action_secs: u64,
}

/// Why the box settings were refused.
#[derive(Debug, thiserror::Error)]
pub enum BoxError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{key}='{value}' is not {want}")]
    Shape {
        key: &'static str,
        value: String,
        want: &'static str,
    },
    #[error("{a} and {b} are both port {port}")]
    PortTwice { a: String, b: String, port: u16 },
    #[error("KBF_BOX_BACKEND_PORT_BASE={0}: the second instance's ports run past 65535")]
    BasePastRange(u16),
}

impl BoxConfig {
    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<BoxConfig, BoxError> {
        BoxConfig::from_config(&Config::load(path, BOX_KEYS)?)
    }

    /// Check `text`, a config file's contents.
    pub fn parse(text: &str) -> Result<BoxConfig, BoxError> {
        BoxConfig::from_config(&Config::parse(text, BOX_KEYS)?)
    }

    fn from_config(c: &Config) -> Result<BoxConfig, BoxError> {
        let host = text(
            c,
            "KBF_BOX_HOST",
            "a DNS name (DNS labels joined by .)",
            |v| super::config::fits(super::config::Shape::DnsName, v),
        )?;
        let cfg = BoxConfig {
            host,
            reapi_port: number(c, "KBF_BOX_REAPI_PORT", 1, 65535)?,
            worker_port: number(c, "KBF_BOX_WORKER_PORT", 1, 65535)?,
            admin_port: number(c, "KBF_BOX_ADMIN_PORT", 1, 65535)?,
            backend_port_base: number(c, "KBF_BOX_BACKEND_PORT_BASE", 1, 65535)?,
            cert_file: path(c, "KBF_BOX_CERT_FILE")?,
            key_file: path(c, "KBF_BOX_KEY_FILE")?,
            ca_file: path(c, "KBF_BOX_CA_FILE")?,
            reapi_max_streams: number(c, "KBF_BOX_REAPI_MAX_STREAMS", 1, 32767)?,
            stream_ceiling: number(c, "KBF_BOX_STREAM_CEILING", 1, 100_000_000)?,
            longest_action_secs: number(c, "KBF_BOX_LONGEST_ACTION_SECS", 1, 31_536_000)?,
        };
        cfg.distinct_ports()?;
        Ok(cfg)
    }

    /// The two instances, `a` then `b`.
    pub fn instances(&self) -> [Instance; 2] {
        let at = |base: u16| Instance {
            reapi: base + REAPI_OFFSET,
            worker: base + WORKER_OFFSET,
            api: base + API_OFFSET,
        };
        let a = self.backend_port_base;
        [at(a), at(a + SECOND_INSTANCE)]
    }

    /// Envoy's `--parent-shutdown-time-s`: the longest action plus [`PARENT_SLACK_SECS`].
    pub fn parent_shutdown_secs(&self) -> u64 {
        self.longest_action_secs + PARENT_SLACK_SECS
    }

    /// The nine ports the box binds on loopback or publicly, none twice.
    fn distinct_ports(&self) -> Result<(), BoxError> {
        if self.backend_port_base > 65535 - SECOND_INSTANCE - API_OFFSET {
            return Err(BoxError::BasePastRange(self.backend_port_base));
        }
        let mut named: Vec<(String, u16)> = vec![
            ("KBF_BOX_REAPI_PORT".to_owned(), self.reapi_port),
            ("KBF_BOX_WORKER_PORT".to_owned(), self.worker_port),
            ("KBF_BOX_ADMIN_PORT".to_owned(), self.admin_port),
        ];
        for (name, i) in [("a", self.instances()[0]), ("b", self.instances()[1])] {
            named.push((format!("instance {name} REAPI"), i.reapi));
            named.push((format!("instance {name} worker"), i.worker));
            named.push((format!("instance {name} API"), i.api));
        }
        for (i, (a, pa)) in named.iter().enumerate() {
            if let Some((b, _)) = named[i + 1..].iter().find(|(_, pb)| pb == pa) {
                return Err(BoxError::PortTwice {
                    a: a.clone(),
                    b: b.clone(),
                    port: *pa,
                });
            }
        }
        Ok(())
    }
}

fn text(
    c: &Config,
    key: &'static str,
    want: &'static str,
    ok: impl Fn(&str) -> bool,
) -> Result<String, BoxError> {
    let value = c.require(key)?;
    if ok(value) {
        Ok(value.to_owned())
    } else {
        Err(BoxError::Shape {
            key,
            value: value.to_owned(),
            want,
        })
    }
}

/// An absolute path of printable characters other than space and quotes.
fn path(c: &Config, key: &'static str) -> Result<String, BoxError> {
    text(
        c,
        key,
        "an absolute path (no spaces, quotes or control characters)",
        |v| {
            v.starts_with('/')
                && v.bytes()
                    .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
        },
    )
}

/// A decimal number (digits only) from `min` to `max`.
fn number<T: TryFrom<u64>>(
    c: &Config,
    key: &'static str,
    min: u64,
    max: u64,
) -> Result<T, BoxError> {
    let value = c.require(key)?;
    let parsed = Some(value)
        .filter(|v| !v.is_empty() && v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| (min..=max).contains(n))
        .and_then(|n| T::try_from(n).ok());
    parsed.ok_or_else(|| BoxError::Shape {
        key,
        value: value.to_owned(),
        want: "a decimal number in the range the key allows (see box.example)",
    })
}
