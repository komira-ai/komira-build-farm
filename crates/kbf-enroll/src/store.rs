//! The node's key and certificate files (design sections 3.2 and 5.8).
//!
//! Layout under the state directory, `0700`:
//!
//! ```text
//! pki/ca.pem            the CA bundle, current first
//! pki/<role>.key        the node's private key for the role, 0600
//! pki/<role>.pem        its certificate
//! pki/node.json         ids, serials, the pin, the config generation (0600)
//! pki/staging/          an install in progress
//! ```
//!
//! **Every file is replaced in one step**: written beside the target, `fsync`ed, renamed
//! over it, the directory `fsync`ed. A reader sees the old file or the new one, never a
//! piece of either.
//!
//! **An install is staged, then committed.** Everything the answer holds is first written
//! under `staging/`, `node.json` last; then the files are renamed into place in a fixed
//! order, `node.json` last again, and the staging directory is removed. `staging/node.json`
//! is therefore the commit point: when it exists, every other staged file does, and
//! [`Store::recover`] finishes whatever a crash left undone. A renewal keeps the key, so
//! it replaces the CA bundle and certificates only, and a crash at any step leaves a
//! certificate that still matches the key. The keys and the requests of an enrollment in
//! flight are persisted in staging before the request is sent, so a retry after a lost
//! answer sends the same requests and the frontend, which keeps one use per request,
//! answers with the certificates it already issued.
//!
//! A private key is written nowhere else, and never with a mode wider than `0600`.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use kbf_proto::kbf::enroll::v1::{CredentialMode, NodeConfig};
use kbf_types::{CaPin, NodeId, Role, RoleSet};
use rcgen::KeyPair;
use serde::{Deserialize, Serialize};

use crate::answer::Cert;
use crate::csr;

const PKI: &str = "pki";
const STAGING: &str = "staging";
const NODE_JSON: &str = "node.json";
const CA_PEM: &str = "ca.pem";

/// Why a file operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The operating system refused.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory.
        path: String,
        /// What it said.
        source: std::io::Error,
    },
    /// A directory is not private to the service user.
    #[error("{0} must be a directory with mode 0700 owned by the service user")]
    Insecure(String),
    /// `node.json` or a key does not parse.
    #[error("{0} is damaged")]
    Damaged(String),
    /// A key could not be made or a request signed.
    #[error("key generation: {0}")]
    Key(#[from] rcgen::Error),
    /// `node.json` could not be encoded.
    #[error("encoding node.json: {0}")]
    Encode(#[from] serde_json::Error),
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// The part of the frontend's `NodeConfig` the node keeps in `node.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredConfig {
    /// Strictly increases at every change.
    pub generation: u64,
    /// The roles granted.
    pub roles: Vec<String>,
    /// The URL the daemon dials.
    pub worker_endpoint: String,
    /// What `--cas` is for a daemon-only node.
    pub cas_endpoint: String,
    /// The storage port.
    pub storage_port: u32,
    /// The cold store URL, never a key.
    pub cold_store: String,
    /// The `CredentialMode` number (an unknown one is kept as it came).
    pub credential_mode: i32,
    /// The least protocol the node may speak.
    pub min_protocol: u32,
    /// Seconds between looks at the certificate files.
    pub tls_reload_interval_s: u32,
    /// The frontend URL the frontend asks the node to adopt, if any.
    pub frontend_url: String,
}

impl StoredConfig {
    /// The keys of `config`.
    #[must_use]
    pub fn from_proto(config: &NodeConfig) -> Self {
        Self {
            generation: config.generation,
            roles: config.roles.clone(),
            worker_endpoint: config.worker_endpoint.clone(),
            cas_endpoint: config.cas_endpoint.clone(),
            storage_port: config.storage_port,
            cold_store: config.cold_store.clone(),
            credential_mode: config.credential_mode,
            min_protocol: config.min_protocol,
            tls_reload_interval_s: config.tls_reload_interval_s,
            frontend_url: config.frontend_url.clone(),
        }
    }

    /// How the object-store credentials come; `None` for a number this build does not know.
    #[must_use]
    pub fn credential_mode(&self) -> Option<CredentialMode> {
        CredentialMode::try_from(self.credential_mode).ok()
    }
}

/// One certificate the node holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCert {
    /// The role.
    pub role: String,
    /// Lower-case hex.
    pub serial: String,
    /// Unix seconds.
    pub not_before: i64,
    /// Unix seconds.
    pub not_after: i64,
}

/// `node.json`: what the node knows of its enrollment. Holds no secret: the pin is public.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The node id.
    pub node_id: String,
    /// The SHA-256 of the pinned root, hex.
    pub pin: String,
    /// One per role.
    pub certs: Vec<StoredCert>,
    /// When to first try to renew, unix seconds.
    pub renew_after: i64,
    /// The configuration of the last answer.
    pub config: StoredConfig,
}

impl Node {
    /// The record of an accepted answer.
    #[must_use]
    pub fn new(
        node_id: &NodeId,
        pin: &CaPin,
        certs: &[Cert],
        renew_after: i64,
        config: StoredConfig,
    ) -> Self {
        Self {
            node_id: node_id.to_string(),
            pin: hex::encode(pin.as_bytes()),
            certs: certs
                .iter()
                .map(|c| StoredCert {
                    role: c.role.name().to_owned(),
                    serial: c.serial.clone(),
                    not_before: c.not_before,
                    not_after: c.not_after,
                })
                .collect(),
            renew_after,
            config,
        }
    }

    /// The roles whose certificates the node holds.
    #[must_use]
    pub fn roles(&self) -> Vec<Role> {
        Role::ALL
            .into_iter()
            .filter(|r| self.certs.iter().any(|c| c.role == r.name()))
            .collect()
    }

    /// The pin as a value.
    ///
    /// # Errors
    /// `node.json` holds a pin that is not 32 bytes of hex.
    pub fn pin(&self) -> Result<CaPin, StoreError> {
        let bytes = hex::decode(&self.pin)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok());
        bytes
            .map(CaPin::from_bytes)
            .ok_or_else(|| StoreError::Damaged(NODE_JSON.to_owned()))
    }

    /// The earliest expiry among the certificates.
    #[must_use]
    pub fn earliest_not_after(&self) -> i64 {
        self.certs
            .iter()
            .map(|c| c.not_after)
            .min()
            .unwrap_or_default()
    }
}

/// A key and request made before an enrollment is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The role.
    pub role: Role,
    /// The SubjectPublicKeyInfo DER of the key.
    pub spki: Vec<u8>,
    /// The PKCS #10 request.
    pub csr_der: Vec<u8>,
}

/// The node's state directory.
#[derive(Clone, Debug)]
pub struct Store {
    pki: PathBuf,
}

/// Writes `bytes` to `dir/name` in one step with exactly `mode`.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<(), StoreError> {
    let target = dir.join(name);
    let temp = dir.join(format!(".{name}.tmp"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temp)
        .map_err(io(&temp))?;
    // `mode` above is masked by the umask, and an old temp file keeps its mode.
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io(&temp))?;
    file.write_all(bytes).map_err(io(&temp))?;
    file.sync_all().map_err(io(&temp))?;
    std::fs::rename(&temp, &target).map_err(io(&target))?;
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io(dir))
}

/// The bytes of a file, `None` if it is not there.
fn read_opt(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path)(e)),
    }
}

/// `path` as a private directory, made if missing.
fn private_dir(path: &Path) -> Result<(), StoreError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(io(path))?;
    let meta = std::fs::symlink_metadata(path).map_err(io(path))?;
    let euid = rustix::process::geteuid().as_raw();
    if meta.is_dir() && meta.uid() == euid && meta.mode() & 0o777 == 0o700 {
        Ok(())
    } else {
        Err(StoreError::Insecure(path.display().to_string()))
    }
}

impl Store {
    /// Opens (making if missing) `state_dir` and its `pki/` as private directories.
    ///
    /// # Errors
    /// A directory cannot be made, or exists but is not `0700`, owned by the service
    /// user and a real directory.
    pub fn open(state_dir: &Path) -> Result<Self, StoreError> {
        private_dir(state_dir)?;
        let pki = state_dir.join(PKI);
        private_dir(&pki)?;
        Ok(Self { pki })
    }

    fn staging(&self) -> PathBuf {
        self.pki.join(STAGING)
    }

    /// Where the role's private key is.
    #[must_use]
    pub fn key_path(&self, role: Role) -> PathBuf {
        self.pki.join(format!("{}.key", role.name()))
    }

    /// Where the role's certificate is.
    #[must_use]
    pub fn cert_path(&self, role: Role) -> PathBuf {
        self.pki.join(format!("{}.pem", role.name()))
    }

    /// Where the CA bundle is.
    #[must_use]
    pub fn ca_path(&self) -> PathBuf {
        self.pki.join(CA_PEM)
    }

    /// `node.json`, or `None` before the node is enrolled.
    ///
    /// # Errors
    /// The file cannot be read or does not parse.
    pub fn load(&self) -> Result<Option<Node>, StoreError> {
        let path = self.pki.join(NODE_JSON);
        read_opt(&path)?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|_| StoreError::Damaged(path.display().to_string()))
            })
            .transpose()
    }

    /// Finishes an install a crash cut short (when `staging/node.json` exists), then
    /// loads `node.json`.
    ///
    /// # Errors
    /// As [`Store::load`] and [`Store::commit`].
    pub fn recover(&self) -> Result<Option<Node>, StoreError> {
        if self.staging().join(NODE_JSON).exists() {
            self.commit()?;
        }
        self.load()
    }

    /// The keys and requests for `roles`: those a previous attempt persisted, so a retry
    /// sends the same bytes, else new ones, persisted before they are returned.
    ///
    /// # Errors
    /// A file cannot be read or written, or a key cannot be made.
    pub fn pending(&self, roles: RoleSet) -> Result<Vec<Pending>, StoreError> {
        let staging = self.staging();
        private_dir(&staging)?;
        roles
            .iter()
            .map(|role| {
                let key_file = staging.join(format!("{}.key", role.name()));
                let csr_file = staging.join(format!("{}.csr", role.name()));
                let kept = (read_opt(&key_file)?, read_opt(&csr_file)?);
                if let (Some(key), Some(csr_der)) = kept {
                    let key = std::str::from_utf8(&key)
                        .ok()
                        .and_then(|pem| KeyPair::from_pem(pem).ok())
                        .ok_or_else(|| StoreError::Damaged(key_file.display().to_string()))?;
                    return Ok(Pending {
                        role,
                        spki: csr::spki_of(&key),
                        csr_der,
                    });
                }
                let key = csr::new_key()?;
                let csr_der = csr::request(&key, None)?;
                // The request first, the key last: a key in staging always has its request.
                write_atomic(&staging, &format!("{}.csr", role.name()), &csr_der, 0o600)?;
                write_atomic(
                    &staging,
                    &format!("{}.key", role.name()),
                    key.serialize_pem().as_bytes(),
                    0o600,
                )?;
                Ok(Pending {
                    role,
                    spki: csr::spki_of(&key),
                    csr_der,
                })
            })
            .collect()
    }

    /// The installed private key for `role`.
    ///
    /// # Errors
    /// The file cannot be read or is not a key.
    pub fn key_pair(&self, role: Role) -> Result<KeyPair, StoreError> {
        let path = self.key_path(role);
        let pem = std::fs::read_to_string(&path).map_err(io(&path))?;
        KeyPair::from_pem(&pem).map_err(|_| StoreError::Damaged(path.display().to_string()))
    }

    /// Stages an accepted answer: the certificates, the bundle and `node.json`, in that
    /// order, into `staging/`. Nothing installed changes.
    ///
    /// # Errors
    /// A file cannot be written.
    pub fn stage(&self, certs: &[Cert], ca_bundle: &[u8], node: &Node) -> Result<(), StoreError> {
        let staging = self.staging();
        private_dir(&staging)?;
        write_atomic(&staging, CA_PEM, ca_bundle, 0o644)?;
        for cert in certs {
            write_atomic(
                &staging,
                &format!("{}.pem", cert.role.name()),
                cert.pem.as_bytes(),
                0o644,
            )?;
        }
        let json = serde_json::to_vec_pretty(node)?;
        write_atomic(&staging, NODE_JSON, &json, 0o600)
    }

    /// Moves the staged files into place, `node.json` last, then removes `staging/`.
    ///
    /// # Errors
    /// A file cannot be moved.
    pub fn commit(&self) -> Result<(), StoreError> {
        self.commit_steps(usize::MAX)
    }

    /// [`Store::commit`], stopping after `steps` renames: the points a crash can fall at.
    fn commit_steps(&self, steps: usize) -> Result<(), StoreError> {
        let staging = self.staging();
        let path = staging.join(NODE_JSON);
        let bytes =
            read_opt(&path)?.ok_or_else(|| StoreError::Damaged(path.display().to_string()))?;
        let node: Node = serde_json::from_slice(&bytes)
            .map_err(|_| StoreError::Damaged(path.display().to_string()))?;
        let mut names = vec![CA_PEM.to_owned()];
        for role in node.roles() {
            names.push(format!("{}.key", role.name()));
            names.push(format!("{}.pem", role.name()));
        }
        names.push(NODE_JSON.to_owned());
        let mut left = steps;
        for name in names {
            let from = staging.join(&name);
            if left == 0 {
                return Ok(());
            }
            if from.exists() {
                std::fs::rename(&from, self.pki.join(&name)).map_err(io(&from))?;
                left -= 1;
            }
        }
        sync_dir(&self.pki)?;
        std::fs::remove_dir_all(&staging).map_err(io(&staging))
    }
}

#[cfg(test)]
mod tests;
