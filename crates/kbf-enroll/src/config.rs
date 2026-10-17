//! `kbf.toml`: the node's config file (design section 4).
//!
//! One TOML file with a closed key set. Parsing is `serde` with `deny_unknown_fields`;
//! [`Config::parse`] then validates every field and reports **every** problem, each with
//! the key and the line it is on, not the first. There is no environment expansion, no
//! `~` and no include. The file holds no secret: the token is a path.
//!
//! Checks that need the file system (owner and mode of the directories, symlinks, the
//! one-filesystem rule) are [`Config::check_paths`], separate so that parsing a file is
//! a pure function of its text and the [`Host`] it is read on.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use kbf_types::{NodeId, RoleSet};
use serde::Deserialize;
use toml::Spanned;

/// The state directory when `dirs.state` is not set.
pub const DEFAULT_STATE_DIR: &str = if cfg!(target_os = "macos") {
    "/var/db/kbf"
} else {
    "/var/lib/kbf"
};

/// The token file when `enroll.token_file` is not set.
pub const DEFAULT_TOKEN_FILE: &str = if cfg!(target_os = "macos") {
    "/var/db/kbf/enroll.token"
} else {
    "/var/lib/kbf/enroll.token"
};

/// The longest host name: a DNS name.
const MAX_HOST_LEN: usize = 253;
/// The longest label of a DNS name.
const MAX_LABEL_LEN: usize = 63;
/// The longest zone label.
const MAX_ZONE_LEN: usize = 63;
/// The port of an `https` URL that names none.
const HTTPS_PORT: u16 = 443;

/// The tables and keys of the file. Any other key is an error.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    frontend: Option<RawFrontend>,
    enroll: Option<RawEnroll>,
    node: Option<RawNode>,
    dirs: Option<RawDirs>,
    storage: Option<RawStorage>,
    daemon: Option<RawDaemon>,
    tls: Option<RawTls>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFrontend {
    url: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnroll {
    token_file: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNode {
    id: Option<Spanned<String>>,
    advertise: Option<Spanned<String>>,
    zone: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDirs {
    state: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    hot_dir: Option<Spanned<String>>,
    meta_dir: Option<Spanned<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDaemon {
    data_dir: Option<Spanned<String>>,
    reserve_cpus: Option<Spanned<i64>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    trust: Option<Spanned<String>>,
}

/// What a node knows about the machine it runs on, passed in so parsing reads nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// The host name as the machine reports it (the short form is derived).
    pub hostname: String,
    /// The CPU count, for the range of `daemon.reserve_cpus`.
    pub cpus: usize,
}

/// One thing wrong with the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The key, as written in the file (empty when the line has no key).
    pub key: String,
    /// The 1-based line, when the problem is in the text.
    pub line: Option<usize>,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(f, "line {line}: ")?;
        }
        if !self.key.is_empty() {
            write!(f, "{}: ", self.key)?;
        }
        f.write_str(&self.message)
    }
}

/// Every problem of a file, never empty.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
pub struct Problems(pub Vec<Problem>);

/// A host name or an address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostName {
    /// A DNS name, lower-cased.
    Dns(String),
    /// An address literal.
    Ip(IpAddr),
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dns(name) => f.write_str(name),
            Self::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]"),
            Self::Ip(ip) => write!(f, "{ip}"),
        }
    }
}

/// `frontend.url`: `https://host[:port]` and nothing after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontendUrl {
    /// The host, lower-cased.
    pub host: HostName,
    /// The port, 443 when the URL names none.
    pub port: u16,
}

impl FrontendUrl {
    /// The URL in its one spelling, which a channel dials.
    #[must_use]
    pub fn to_url(&self) -> String {
        format!("https://{}:{}", self.host, self.port)
    }
}

/// `node.advertise`: where a storage node says it can be reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertise {
    /// The host.
    pub host: HostName,
    /// The port.
    pub port: u16,
}

/// `tls.trust`: how the first contact with the frontend is verified (design section 5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The enroll listener presents a chain holding the certificate whose hash is the
    /// token's pin. The default.
    Pin,
    /// The system roots verify the name; the pin is checked against the CA bundle in
    /// the answer. Written down on purpose.
    Public,
}

/// A validated `kbf.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `frontend.url`.
    pub frontend: FrontendUrl,
    /// `enroll.token_file`.
    pub token_file: PathBuf,
    /// `node.id`.
    pub node_id: NodeId,
    /// `node.advertise`; the default (`<node.id>:<storage port>`) needs the storage port
    /// the frontend sends, so it is not filled here.
    pub advertise: Option<Advertise>,
    /// `node.zone`, empty when unset.
    pub zone: String,
    /// `dirs.state`.
    pub state_dir: PathBuf,
    /// `storage.hot_dir`.
    pub hot_dir: PathBuf,
    /// `storage.meta_dir`.
    pub meta_dir: PathBuf,
    /// `daemon.data_dir`.
    pub data_dir: PathBuf,
    /// `daemon.reserve_cpus`; `None` is kbf's default.
    pub reserve_cpus: Option<u32>,
    /// `tls.trust`.
    pub trust: Trust,
    storage_table: bool,
    daemon_table: bool,
}

/// Accumulates problems with their lines.
struct Check<'a> {
    text: &'a str,
    problems: Vec<Problem>,
}

impl Check<'_> {
    fn fail(&mut self, key: &str, offset: Option<usize>, message: impl Into<String>) {
        self.problems.push(Problem {
            key: key.to_owned(),
            line: offset.map(|o| line_of(self.text, o)),
            message: message.into(),
        });
    }

    /// The value of a string key, or `None` after recording why it is refused.
    fn value<T>(
        &mut self,
        key: &str,
        raw: Option<&Spanned<String>>,
        parse: impl FnOnce(&str) -> Result<T, String>,
    ) -> Option<T> {
        let raw = raw?;
        match parse(raw.get_ref()) {
            Ok(value) => Some(value),
            Err(message) => {
                self.fail(key, Some(raw.span().start), message);
                None
            }
        }
    }
}

/// The 1-based line of a byte offset.
fn line_of(text: &str, offset: usize) -> usize {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

/// The key written on the line holding `offset`, if the line is `key = ...`.
fn key_at(text: &str, offset: usize) -> String {
    let offset = offset.min(text.len());
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line = text[start..].lines().next().unwrap_or_default();
    line.split_once('=')
        .map(|(key, _)| key.trim().to_owned())
        .unwrap_or_default()
}

fn parse_problem(text: &str, error: &toml::de::Error) -> Problem {
    let offset = error.span().map_or(0, |span| span.start);
    Problem {
        key: key_at(text, offset),
        line: Some(line_of(text, offset)),
        message: error.message().to_owned(),
    }
}

/// A DNS name: labels of `a-z0-9-` (case folded), 1 to 63 characters, not starting or
/// ending with `-`, at most 253 bytes in all.
fn dns_name(text: &str) -> Result<String, String> {
    let name = text.to_ascii_lowercase();
    if name.len() > MAX_HOST_LEN {
        return Err(format!("the host is longer than {MAX_HOST_LEN} bytes"));
    }
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    };
    if name.split('.').all(label_ok) {
        Ok(name)
    } else {
        Err(format!(
            "{text:?} is not a DNS name or an address (labels of a-z, 0-9 and -, no wildcard)"
        ))
    }
}

/// A host: an IPv4 or bracketed IPv6 literal, or a DNS name.
fn host_name(text: &str) -> Result<HostName, String> {
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        return inner
            .parse::<Ipv6Addr>()
            .map(|ip| HostName::Ip(IpAddr::V6(ip)))
            .map_err(|_| format!("{text:?} is not an IPv6 address"));
    }
    match text.parse::<IpAddr>() {
        Ok(ip) => Ok(HostName::Ip(ip)),
        Err(_) => dns_name(text).map(HostName::Dns),
    }
}

/// `host[:port]`, the port 1 to 65535 when written.
fn host_port(text: &str) -> Result<(HostName, Option<u16>), String> {
    // A bracketed IPv6 literal contains colons; its port follows the `]`.
    let split = match text.rfind(']') {
        Some(close) => text[close + 1..]
            .strip_prefix(':')
            .map(|port| (&text[..=close], Some(port)))
            .or_else(|| (close + 1 == text.len()).then_some((text, None))),
        None => Some(
            text.rsplit_once(':')
                .map_or((text, None), |(host, port)| (host, Some(port))),
        ),
    };
    let (host, port) = split.ok_or_else(|| format!("{text:?} has text after the address"))?;
    let port = port
        .map(|p| match p.parse::<u16>() {
            Ok(n) if n != 0 && p.bytes().all(|c| c.is_ascii_digit()) => Ok(n),
            _ => Err("the port must be a number from 1 to 65535".to_owned()),
        })
        .transpose()?;
    Ok((host_name(host)?, port))
}

fn parse_url(text: &str) -> Result<FrontendUrl, String> {
    let rest = text
        .strip_prefix("https://")
        .ok_or("the scheme must be exactly https")?;
    if rest.contains('@') {
        return Err("the URL must not carry user information".to_owned());
    }
    if rest.contains(['/', '?', '#']) {
        return Err("the URL must not carry a path, query or fragment".to_owned());
    }
    let (host, port) = host_port(rest)?;
    Ok(FrontendUrl {
        host,
        port: port.unwrap_or(HTTPS_PORT),
    })
}

fn parse_advertise(text: &str, frontend: Option<&FrontendUrl>) -> Result<Advertise, String> {
    let (host, port) = host_port(text)?;
    let port = port.ok_or("the address must be host:port")?;
    let refused = match &host {
        HostName::Dns(name) if name == "localhost" => Some("loopback"),
        HostName::Ip(ip) if ip.is_loopback() => Some("loopback"),
        HostName::Ip(ip) if ip.is_unspecified() => Some("unspecified"),
        HostName::Ip(ip) if ip.is_multicast() => Some("multicast"),
        HostName::Ip(IpAddr::V4(ip)) if ip.is_link_local() => Some("link-local"),
        HostName::Ip(IpAddr::V6(ip)) if ip.is_unicast_link_local() => Some("link-local"),
        _ => None,
    };
    if let Some(kind) = refused {
        return Err(format!("the advertised host must not be {kind}"));
    }
    if frontend.is_some_and(|f| f.host == host) {
        return Err("the advertised host must not be a name of the frontend".to_owned());
    }
    Ok(Advertise { host, port })
}

fn parse_zone(text: &str) -> Result<String, String> {
    let ok = text.len() <= MAX_ZONE_LEN
        && text.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        });
    if ok {
        Ok(text.to_owned())
    } else {
        Err(format!(
            "a zone is at most {MAX_ZONE_LEN} bytes of a-z, 0-9, '.', '_' and '-'"
        ))
    }
}

/// An absolute path; `no_dotdot` also refuses a `..` component.
fn parse_path(text: &str, no_dotdot: bool) -> Result<PathBuf, String> {
    let path = PathBuf::from(text);
    if !path.is_absolute() {
        return Err("the path must be absolute".to_owned());
    }
    if no_dotdot && path.components().any(|c| c == Component::ParentDir) {
        return Err("the path must not contain ..".to_owned());
    }
    Ok(path)
}

/// The short host name, lower-cased, as a node id; the error says to set `node.id`.
fn default_node_id(hostname: &str) -> Result<NodeId, String> {
    let short = hostname.split('.').next().unwrap_or_default();
    short.to_ascii_lowercase().parse::<NodeId>().map_err(|e| {
        format!("the host name {hostname:?} is not a usable node id ({e}); set node.id")
    })
}

impl Config {
    /// Reads `text` as a `kbf.toml` on `host`.
    ///
    /// # Errors
    /// Every problem found, with its key and line; a file that is not TOML, has an
    /// unknown key or a wrong type reports that one problem, because nothing else in it
    /// can be trusted.
    pub fn parse(text: &str, host: &Host) -> Result<Self, Problems> {
        let raw: Raw =
            toml::from_str(text).map_err(|error| Problems(vec![parse_problem(text, &error)]))?;
        let mut check = Check {
            text,
            problems: Vec::new(),
        };

        let url_raw = raw.frontend.as_ref().and_then(|f| f.url.as_ref());
        let frontend = if url_raw.is_some() {
            check.value("frontend.url", url_raw, parse_url)
        } else {
            check.fail("frontend.url", None, "is required");
            None
        };
        let token_file = check
            .value(
                "enroll.token_file",
                raw.enroll.as_ref().and_then(|e| e.token_file.as_ref()),
                |t| parse_path(t, false),
            )
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TOKEN_FILE));

        let node = raw.node.as_ref();
        let id_raw = node.and_then(|n| n.id.as_ref());
        let node_id = match id_raw {
            Some(_) => check.value("node.id", id_raw, |t| {
                t.parse::<NodeId>().map_err(|e| e.to_string())
            }),
            None => match default_node_id(&host.hostname) {
                Ok(id) => Some(id),
                Err(message) => {
                    check.fail("node.id", None, message);
                    None
                }
            },
        };
        let advertise = check.value(
            "node.advertise",
            node.and_then(|n| n.advertise.as_ref()),
            |t| parse_advertise(t, frontend.as_ref()),
        );
        let zone = check
            .value("node.zone", node.and_then(|n| n.zone.as_ref()), parse_zone)
            .unwrap_or_default();

        let dir = |check: &mut Check<'_>, key, raw: Option<&Spanned<String>>, default: &Path| {
            check
                .value(key, raw, |t| parse_path(t, true))
                .unwrap_or_else(|| default.to_path_buf())
        };
        let state_dir = dir(
            &mut check,
            "dirs.state",
            raw.dirs.as_ref().and_then(|d| d.state.as_ref()),
            Path::new(DEFAULT_STATE_DIR),
        );
        let storage = raw.storage.as_ref();
        let hot_dir = dir(
            &mut check,
            "storage.hot_dir",
            storage.and_then(|s| s.hot_dir.as_ref()),
            &state_dir.join("hot"),
        );
        let meta_dir = dir(
            &mut check,
            "storage.meta_dir",
            storage.and_then(|s| s.meta_dir.as_ref()),
            &state_dir.join("meta"),
        );
        let daemon = raw.daemon.as_ref();
        let data_dir = dir(
            &mut check,
            "daemon.data_dir",
            daemon.and_then(|d| d.data_dir.as_ref()),
            &state_dir.join("data"),
        );

        let reserve_cpus = daemon.and_then(|d| d.reserve_cpus.as_ref()).and_then(|v| {
            let most = host.cpus.saturating_sub(1);
            match u32::try_from(*v.get_ref()) {
                Ok(n) if n as usize <= most => Some(n),
                _ => {
                    check.fail(
                        "daemon.reserve_cpus",
                        Some(v.span().start),
                        format!("must be from 0 to {most} on a host with {} CPUs", host.cpus),
                    );
                    None
                }
            }
        });

        let trust_raw = raw.tls.as_ref().and_then(|t| t.trust.as_ref());
        let trust = check
            .value("tls.trust", trust_raw, |t| match t {
                "pin" => Ok(Trust::Pin),
                "public" => Ok(Trust::Public),
                _ => Err("must be \"pin\" or \"public\"".to_owned()),
            })
            .unwrap_or(Trust::Pin);
        if let (Some(url), Trust::Public, Some(raw_url)) = (&frontend, trust, url_raw)
            && matches!(url.host, HostName::Ip(_))
        {
            check.fail(
                "frontend.url",
                Some(raw_url.span().start),
                "an IP address host needs tls.trust = \"pin\"",
            );
        }

        match (frontend, node_id) {
            (Some(frontend), Some(node_id)) if check.problems.is_empty() => Ok(Self {
                frontend,
                token_file,
                node_id,
                advertise,
                zone,
                state_dir,
                hot_dir,
                meta_dir,
                data_dir,
                reserve_cpus,
                trust,
                storage_table: raw.storage.is_some(),
                daemon_table: raw.daemon.is_some(),
            }),
            _ => Err(Problems(check.problems)),
        }
    }

    /// Warnings once the roles are known (they come from the token, not the file): a
    /// `[storage]` or `[daemon]` table for a role the frontend did not grant, and an
    /// `advertise` on a node granted no storage role.
    #[must_use]
    pub fn role_warnings(&self, granted: RoleSet) -> Vec<String> {
        use kbf_types::Role;
        let mut out = Vec::new();
        if self.storage_table && !granted.contains(Role::Storage) {
            out.push("[storage] is set but the frontend granted no storage role".to_owned());
        }
        if self.daemon_table && !granted.contains(Role::Daemon) {
            out.push("[daemon] is set but the frontend granted no daemon role".to_owned());
        }
        if self.advertise.is_some() && !granted.contains(Role::Storage) {
            out.push("node.advertise is set but the frontend granted no storage role".to_owned());
        }
        out
    }

    /// The checks that read the file system, for directories that exist: no symlink in
    /// the path (one a root-owned system alias like macOS's `/var` is not), owned by
    /// `euid`, mode `0700`; and `storage.hot_dir` on the same file system as
    /// `daemon.data_dir`, which share one cache.
    #[must_use]
    pub fn check_paths(&self, euid: u32) -> Vec<Problem> {
        let mut out = Vec::new();
        for (key, path) in [
            ("dirs.state", &self.state_dir),
            ("storage.hot_dir", &self.hot_dir),
            ("storage.meta_dir", &self.meta_dir),
            ("daemon.data_dir", &self.data_dir),
        ] {
            if let Some(message) = dir_problem(path, euid) {
                out.push(Problem {
                    key: key.to_owned(),
                    line: None,
                    message,
                });
            }
        }
        if let (Ok(hot), Ok(data)) = (
            std::fs::metadata(&self.hot_dir),
            std::fs::metadata(&self.data_dir),
        ) && hot.dev() != data.dev()
        {
            out.push(Problem {
                key: "storage.hot_dir".to_owned(),
                line: None,
                message: "must be on the same file system as daemon.data_dir".to_owned(),
            });
        }
        out
    }
}

/// What is wrong with `path` as a node directory, if it exists. A symlink in the path is
/// refused unless root owns it and it is not the last component: a system alias such as
/// macOS's `/var` is not something the service user can plant.
fn dir_problem(path: &Path, euid: u32) -> Option<String> {
    let mut walked = PathBuf::new();
    let (mut is_dir, mut uid, mut mode) = (false, euid, 0o700);
    for component in path.components() {
        walked.push(component);
        let meta = std::fs::symlink_metadata(&walked).ok()?;
        if meta.file_type().is_symlink() && (meta.uid() != 0 || walked == path) {
            return Some(format!("{} is a symlink", walked.display()));
        }
        (is_dir, uid, mode) = (meta.is_dir(), meta.uid(), meta.mode() & 0o777);
    }
    if !is_dir {
        return Some("is not a directory".to_owned());
    }
    if uid != euid {
        return Some(format!("is owned by uid {uid}, not {euid}"));
    }
    (mode != 0o700).then(|| format!("has mode {mode:04o}, not 0700"))
}

#[cfg(test)]
mod tests;
