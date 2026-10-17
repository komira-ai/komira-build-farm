//! The `linux` kit's settings: its closed key set, the defaults, and the checks every
//! value passes before it reaches a unit.

use std::path::Path;

use kbf_types::{ImageError, ImageRef};

use super::modes::Mode;
use crate::config::{Config, ConfigError};
use kbf_host::{HostPath, PathError};

/// The keys `templates/linux/config.example` documents; any other is refused.
pub const KEYS: &[&str] = &[
    "KBF_HOST_NAME",
    "KBF_LISTEN_ADDRESS",
    "KBF_NODE_ID",
    "KBF_REAPI_PORT",
    "KBF_WORKER_PORT",
    "KBF_API_PORT",
    "KBF_STORAGE_PORT",
    "KBF_RAM_GIB",
    "MINIO_RESERVATION_GIB",
    "KBF_SERVER_RESERVATION_GIB",
    "KBF_OS_HEADROOM_GIB",
    "KBF_DAEMON_MEMORY_MIN_MIB",
    "MINIO_UNIT",
    "MINIO_S3_ENDPOINT",
    "KBF_S3_BUCKET",
    "KBF_S3_PREFIX",
    "KBF_META_DIR",
    "KBF_HOT_DIR",
    "KBF_DATA_DIR",
    "KBF_IMAGES",
    "KBF_USER",
    "KBF_SERVER_USER",
    "KBF_ETC",
    "KBF_BIN_DIR",
    "KBF_SWAP_FILE",
    "KBF_SWAP_SIZE",
    "KBF_FRONTEND_NAME",
    "KBF_STORAGE_URL",
    "KBF_REAPI_CERT",
    "KBF_REAPI_KEY",
    "KBF_API_TOKEN_FILE",
    "KBF_STATE_DIR",
    "KBF_RESERVE_CPUS",
    "KBF_LEASE_PLACEMENT",
];

/// The converged units' S3 endpoint when `MINIO_S3_ENDPOINT` is not set: the host's own
/// MinIO.
pub const LOOPBACK_MINIO: &str = "http://127.0.0.1:9000";

/// Where a daemon puts each lease among the node's CPUs: kbf-daemon's `--lease-placement`
/// values, a closed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeasePlacement {
    /// Any CPU the daemon may use.
    None,
    /// One NUMA node per lease (needs `cpuset` delegated).
    NumaNode,
}

impl LeasePlacement {
    /// The flag's value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LeasePlacement::None => "none",
            LeasePlacement::NumaNode => "numa-node",
        }
    }

    fn parse(value: &str) -> Option<LeasePlacement> {
        match value {
            "none" => Some(LeasePlacement::None),
            "numa-node" => Some(LeasePlacement::NumaNode),
            _ => None,
        }
    }
}

/// The least RAM, in GiB, left to the kernel, systemd and the daemon.
const MIN_OS_HEADROOM_GIB: u64 = 4;

/// The settings, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxConfig {
    pub host_name: String,
    pub listen_address: String,
    pub node_id: String,
    pub reapi_port: u16,
    pub worker_port: u16,
    pub api_port: u16,
    /// `kbf storage`'s `--storage-listen` port, on `KBF_LISTEN_ADDRESS`.
    pub storage_port: u16,
    pub ram_gib: u64,
    pub minio_reservation_gib: u64,
    pub server_reservation_gib: u64,
    pub os_headroom_gib: u64,
    pub daemon_memory_min_mib: u64,
    /// RAM minus what MinIO, kbf-server and the OS keep: the daemon's
    /// `--actions-memory-max-gib` (docs/deploy/storage-node.md, "Memory on a converged
    /// host"). At least 1.
    pub actions_gib: u64,
    pub minio_unit: String,
    /// The S3 endpoint of the bucket, when `MINIO_S3_ENDPOINT` is set. The converged
    /// units default to the host's own MinIO ([`LOOPBACK_MINIO`]); `--mode storage`
    /// refuses without it ([`LinuxConfig::for_mode`]).
    pub minio_s3_endpoint: Option<String>,
    pub s3_bucket: String,
    /// The key prefix in the bucket, empty (kbf's own default, `kbf/`) or `a/b/` with a
    /// trailing `/`. A fresh prefix is a fresh cold store beside the old one.
    pub s3_prefix: String,
    /// kbf-server's `--meta-dir`: its metadata log, a single-voter Raft log.
    pub meta_dir: HostPath,
    /// `kbf storage`'s `--hot-store` directory: the SSD tier in front of the cold store.
    pub hot_dir: HostPath,
    /// kbf-daemon's `--data-dir`: lease directories, the input cache and the trees.
    pub data_dir: HostPath,
    /// The pinned images, in the bare `<repo>@sha256:<digest>` form (no `docker://`).
    pub images: Vec<ImageRef>,
    pub user: String,
    pub server_user: String,
    pub etc: HostPath,
    pub bin_dir: HostPath,
    pub swap_file: HostPath,
    pub swap_size: String,
    /// The one name daemons and storage nodes dial for the frontend (`--server`,
    /// `--frontend`): `KBF_FRONTEND_NAME`, else `KBF_HOST_NAME` (a frontend on this host).
    /// `host_name` stays the node's own identity.
    pub frontend_name: String,
    /// The frontend's `--storage`: an `https://` URL of a storage node; only the
    /// frontend unit uses it.
    pub storage_url: Option<String>,
    /// The REAPI listener's certificate chain and key (`--reapi-tls-cert`, `--reapi-tls-key`):
    /// a certificate the farm's CA did not issue, such as a `tailscale cert` pair, so the
    /// file names are the host's to choose.
    pub reapi_cert: HostPath,
    pub reapi_key: HostPath,
    /// The operator API's bearer token (`--api-token-file`): a file the operator puts
    /// there, the server user's alone. install-units never makes it.
    pub api_token_file: HostPath,
    /// The directory of the frontend's SQLite state store, `state.db` in it.
    pub state_dir: HostPath,
    /// The daemon's `--reserve-cpus`; none leaves kbf's default.
    pub reserve_cpus: Option<u64>,
    /// The daemon's `--lease-placement`; none leaves kbf's default.
    pub lease_placement: Option<LeasePlacement>,
}

/// Why the settings were refused.
#[derive(Debug, thiserror::Error)]
pub enum LinuxError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{key} holds a character the units cannot carry: {value}")]
    Chars { key: &'static str, value: String },
    #[error("{key} must be a whole number, not '{value}'")]
    NotNumber { key: &'static str, value: String },
    #[error("KBF_LEASE_PLACEMENT={0}: must be none or numa-node")]
    Placement(String),
    #[error("KBF_S3_PREFIX={value}: {why}")]
    S3Prefix { value: String, why: &'static str },
    #[error(
        "KBF_STORAGE_URL={0}: must be an https:// URL, the frontend's mutual TLS to a storage node"
    )]
    StorageUrl(String),
    #[error("{key} must be a port number from 1 to 65535, not '{value}'")]
    NotPort { key: &'static str, value: String },
    #[error("{key}: {source}")]
    Path {
        key: &'static str,
        source: PathError,
    },
    #[error("KBF_LISTEN_ADDRESS={0}: bind the host's pinned address, not every interface")]
    AnyAddress(String),
    #[error("MINIO_UNIT={0} is not a .service unit name")]
    MinioUnit(String),
    #[error("{a} and {b} are both {port}: each listener of a host needs its own port")]
    SamePort {
        a: &'static str,
        b: &'static str,
        port: u16,
    },
    #[error("{key} must be set for --mode {mode}: {why}")]
    Required {
        key: &'static str,
        mode: &'static str,
        why: &'static str,
    },
    #[error("KBF_OS_HEADROOM_GIB={0}: keep at least {MIN_OS_HEADROOM_GIB} GiB for the OS")]
    Headroom(u64),
    #[error(
        "KBF_RAM_GIB={ram} leaves {left} GiB for builds after MinIO {minio}, kbf-server {server} and the OS {os}"
    )]
    NoRoom {
        ram: u64,
        minio: u64,
        server: u64,
        os: u64,
        left: i128,
    },
    #[error(
        "image '{0}': write the bare <repo>@sha256:<digest>; render-units adds docker:// for kbf-daemon"
    )]
    ImageScheme(String),
    #[error(transparent)]
    Image(#[from] ImageError),
}

impl LinuxConfig {
    /// Read and check the file at `path`.
    pub fn load(path: &Path) -> Result<LinuxConfig, LinuxError> {
        LinuxConfig::from_config(&Config::load(path, KEYS)?)
    }

    /// Check `text`, a config file's contents.
    pub fn parse(text: &str) -> Result<LinuxConfig, LinuxError> {
        LinuxConfig::from_config(&Config::parse(text, KEYS)?)
    }

    fn from_config(c: &Config) -> Result<LinuxConfig, LinuxError> {
        for &key in KEYS {
            if let Some(value) = c.get(key) {
                plain(key, value)?;
            }
        }
        // An unset or empty key takes its default.
        let or = |key: &'static str, default: &str| -> String {
            c.get(key)
                .filter(|v| !v.is_empty())
                .unwrap_or(default)
                .to_owned()
        };
        let number = |key: &'static str, default: &str| whole(key, &or(key, default));
        let port = |key: &'static str, default: &str| port(key, &or(key, default));
        let path = |key: &'static str, default: &str| {
            HostPath::new(&or(key, default)).map_err(|source| LinuxError::Path { key, source })
        };

        let listen_address = c.require("KBF_LISTEN_ADDRESS")?.to_owned();
        if matches!(listen_address.as_str(), "0.0.0.0" | "::") {
            return Err(LinuxError::AnyAddress(listen_address));
        }
        let minio_unit = or("MINIO_UNIT", "minio.service");
        if minio_unit
            .strip_suffix(".service")
            .is_none_or(str::is_empty)
        {
            return Err(LinuxError::MinioUnit(minio_unit));
        }
        let ram_gib = whole("KBF_RAM_GIB", c.require("KBF_RAM_GIB")?)?;
        let minio = number("MINIO_RESERVATION_GIB", "32")?;
        let server = number("KBF_SERVER_RESERVATION_GIB", "16")?;
        let os = number("KBF_OS_HEADROOM_GIB", "8")?;
        if os < MIN_OS_HEADROOM_GIB {
            return Err(LinuxError::Headroom(os));
        }
        let left = i128::from(ram_gib) - i128::from(minio) - i128::from(server) - i128::from(os);
        let actions_gib =
            u64::try_from(left)
                .ok()
                .filter(|&n| n >= 1)
                .ok_or(LinuxError::NoRoom {
                    ram: ram_gib,
                    minio,
                    server,
                    os,
                    left,
                })?;
        let images = or("KBF_IMAGES", "")
            .split_whitespace()
            .map(image)
            .collect::<Result<_, _>>()?;
        let ports = [
            ("KBF_REAPI_PORT", port("KBF_REAPI_PORT", "8980")?),
            ("KBF_WORKER_PORT", port("KBF_WORKER_PORT", "8981")?),
            ("KBF_API_PORT", port("KBF_API_PORT", "8982")?),
            ("KBF_STORAGE_PORT", port("KBF_STORAGE_PORT", "8983")?),
        ];
        for (i, &(a, p)) in ports.iter().enumerate() {
            if let Some(&(b, _)) = ports[i + 1..].iter().find(|(_, q)| *q == p) {
                return Err(LinuxError::SamePort { a, b, port: p });
            }
        }
        let [reapi_port, worker_port, api_port, storage_port] = ports.map(|(_, p)| p);
        let etc = path("KBF_ETC", "/etc/kbf")?;
        let host_name = c.require("KBF_HOST_NAME")?.to_owned();
        let lease_placement = match or("KBF_LEASE_PLACEMENT", "").as_str() {
            "" => None,
            v => Some(LeasePlacement::parse(v).ok_or_else(|| LinuxError::Placement(v.to_owned()))?),
        };
        let reserve_cpus = match or("KBF_RESERVE_CPUS", "").as_str() {
            "" => None,
            v => Some(whole("KBF_RESERVE_CPUS", v)?),
        };
        let storage_url = match or("KBF_STORAGE_URL", "").as_str() {
            "" => None,
            v if v.strip_prefix("https://").is_some_and(|h| !h.is_empty()) => Some(v.to_owned()),
            v => return Err(LinuxError::StorageUrl(v.to_owned())),
        };
        Ok(LinuxConfig {
            frontend_name: or("KBF_FRONTEND_NAME", &host_name),
            storage_url,
            reapi_cert: path("KBF_REAPI_CERT", &format!("{etc}/reapi.pem"))?,
            reapi_key: path("KBF_REAPI_KEY", &format!("{etc}/reapi.key"))?,
            api_token_file: path("KBF_API_TOKEN_FILE", &format!("{etc}/api-token"))?,
            state_dir: path("KBF_STATE_DIR", "/var/lib/kbf-server/state")?,
            reserve_cpus,
            lease_placement,
            s3_prefix: s3_prefix(&or("KBF_S3_PREFIX", ""))?,
            host_name,
            listen_address,
            node_id: c.require("KBF_NODE_ID")?.to_owned(),
            reapi_port,
            worker_port,
            api_port,
            storage_port,
            ram_gib,
            minio_reservation_gib: minio,
            server_reservation_gib: server,
            os_headroom_gib: os,
            daemon_memory_min_mib: number("KBF_DAEMON_MEMORY_MIN_MIB", "256")?,
            actions_gib,
            minio_unit,
            minio_s3_endpoint: c
                .get("MINIO_S3_ENDPOINT")
                .filter(|v| !v.is_empty())
                .map(str::to_owned),
            s3_bucket: or("KBF_S3_BUCKET", "kbf-cas"),
            meta_dir: path("KBF_META_DIR", "/var/lib/kbf-server/raft")?,
            hot_dir: path("KBF_HOT_DIR", "/var/lib/kbf-server/hot")?,
            data_dir: path("KBF_DATA_DIR", "/var/lib/kbf")?,
            images,
            user: or("KBF_USER", "kbf"),
            server_user: or("KBF_SERVER_USER", "kbf-server"),
            etc,
            bin_dir: path("KBF_BIN_DIR", "/usr/local/bin")?,
            swap_file: path("KBF_SWAP_FILE", "/swapfile")?,
            swap_size: or("KBF_SWAP_SIZE", "128G"),
        })
    }

    /// Refuse settings that leave out a key `mode`'s unit needs: `MINIO_S3_ENDPOINT` for
    /// `storage`, whose cold store is a bucket on MinIO elsewhere (the converged units'
    /// default, the host's own MinIO, is a guess there).
    ///
    /// # Errors
    /// [`LinuxError::Required`] for the first key left out.
    pub fn for_mode(&self, mode: Mode) -> Result<&LinuxConfig, LinuxError> {
        if mode == Mode::Frontend && self.storage_url.is_none() {
            return Err(LinuxError::Required {
                key: "KBF_STORAGE_URL",
                mode: mode.name(),
                why: "`kbf frontend` requires --storage, a storage node's https:// URL",
            });
        }
        if mode == Mode::Storage && self.minio_s3_endpoint.is_none() {
            return Err(LinuxError::Required {
                key: "MINIO_S3_ENDPOINT",
                mode: mode.name(),
                why: "the storage node's cold store is the bucket at that endpoint, and no \
                      MinIO runs beside it to default to",
            });
        }
        Ok(self)
    }

    /// The listen address as the host part of a `host:port`: an IPv6 literal goes in
    /// brackets.
    pub fn listen_host(&self) -> String {
        if self.listen_address.contains(':') {
            format!("[{}]", self.listen_address)
        } else {
            self.listen_address.clone()
        }
    }
}

/// A `KBF_S3_PREFIX`, as the path of the cold store's URI: empty, or `a/b/` with the
/// trailing `/` added. Plain segments only (letters, digits, `. _ -`): no empty, `.` or
/// `..` segment, no leading `/`.
fn s3_prefix(value: &str) -> Result<String, LinuxError> {
    let refuse = |why| {
        Err(LinuxError::S3Prefix {
            value: value.to_owned(),
            why,
        })
    };
    if value.is_empty() {
        return Ok(String::new());
    }
    if value.starts_with('/') {
        return refuse("write it without a leading /");
    }
    let body = value.strip_suffix('/').unwrap_or(value);
    for segment in body.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return refuse("no empty, . or .. segment");
        }
        if !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        {
            return refuse("only letters, digits and . _ - between the slashes");
        }
    }
    Ok(format!("{body}/"))
}

/// Refuse a value a rendered unit cannot carry as written: only letters, digits, spaces
/// and `. _ : / = , + @ -`. That leaves out `%` (systemd expands `%` specifiers), `$`
/// (it expands variables in `ExecStart=`), `\`, quotes, `;` and newlines.
fn plain(key: &'static str, value: &str) -> Result<(), LinuxError> {
    let ok = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " ._:/=,+@-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(LinuxError::Chars {
            key,
            value: value.to_owned(),
        })
    }
}

fn whole(key: &'static str, value: &str) -> Result<u64, LinuxError> {
    value
        .parse()
        .ok()
        .filter(|_| value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(LinuxError::NotNumber {
            key,
            value: value.to_owned(),
        })
}

fn port(key: &'static str, value: &str) -> Result<u16, LinuxError> {
    value
        .parse()
        .ok()
        .filter(|&p| p != 0 && value.bytes().all(|b| b.is_ascii_digit()))
        .ok_or(LinuxError::NotPort {
            key,
            value: value.to_owned(),
        })
}

/// A `KBF_IMAGES` entry: the bare `<repo>@sha256:<digest>`, which must pass
/// kbf-daemon's own `--image` parser once `docker://` is put in front.
pub fn image(entry: &str) -> Result<ImageRef, LinuxError> {
    if entry.starts_with("docker://") {
        return Err(LinuxError::ImageScheme(entry.to_owned()));
    }
    Ok(ImageRef::parse(&format!("docker://{entry}"))?)
}
