//! The cold-store keys a storage node holds in memory only (design section 4.3).
//!
//! In `KEYS` credential mode the frontend does not put the object store's keys in
//! `NodeConfig`. A storage node that has enrolled calls `ColdStore.GetKeys` on the worker
//! listener over its mutual-TLS identity, hands the keys to the object-store client
//! in-process, and fetches them again at every start and when the store refuses them.
//! They are never a file, an argument, an environment variable or a log line: the type
//! prints `Keys(..)`, overwrites its bytes when dropped, and [`harden_process`] turns off
//! core dumps for the process.

use kbf_proto::kbf::enroll::v1::GetKeysRequest;
use kbf_proto::kbf::enroll::v1::cold_store_client::ColdStoreClient;
use kbf_types::Role;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};
use zeroize::Zeroize;

use crate::client::CallError;
use crate::store::Store;

/// The object store's access key, held in memory.
pub struct ObjectStoreKeys {
    access_key_id: String,
    secret_access_key: String,
}

impl ObjectStoreKeys {
    /// The access key id.
    #[must_use]
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// The secret access key.
    #[must_use]
    pub fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }
}

impl std::fmt::Debug for ObjectStoreKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(..)")
    }
}

impl Drop for ObjectStoreKeys {
    fn drop(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
    }
}

/// Why the keys could not be fetched.
#[derive(Debug, thiserror::Error)]
pub enum KeysError {
    /// The node holds no storage identity, or its files do not read.
    #[error("storage identity: {0}")]
    Identity(String),
    /// The worker endpoint is not a URL, or the connection failed.
    #[error("worker endpoint: {0}")]
    Transport(#[from] tonic::transport::Error),
    /// The connection or the call failed; `Unavailable` is worth retrying.
    #[error(transparent)]
    Call(#[from] CallError),
}

/// Fetches the keys from the worker listener at `endpoint` (an `https` URL), as the
/// node's storage identity in `store`, trusting the CA bundle it holds.
///
/// # Errors
/// [`KeysError`]; a revoked node is `Call(Status(PermissionDenied))`.
pub async fn fetch(store: &Store, endpoint: &str) -> Result<ObjectStoreKeys, KeysError> {
    let read = |path: std::path::PathBuf| {
        std::fs::read(&path).map_err(|e| KeysError::Identity(format!("{}: {e}", path.display())))
    };
    let ca = read(store.ca_path())?;
    let cert = read(store.cert_path(Role::Storage))?;
    let key = read(store.key_path(Role::Storage))?;
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(cert, key));
    let channel = Endpoint::from_shared(endpoint.to_owned())?
        .tls_config(tls)?
        .connect()
        .await?;
    let reply = ColdStoreClient::new(channel)
        .get_keys(GetKeysRequest {})
        .await
        .map_err(CallError::from)?
        .into_inner();
    Ok(ObjectStoreKeys {
        access_key_id: reply.access_key_id,
        secret_access_key: reply.secret_access_key,
    })
}

/// Makes the process refuse to leave its memory in a file: no core dump, and where the
/// system has the control, not dumpable. Call it before the keys are fetched.
///
/// # Errors
/// The limit cannot be set.
pub fn harden_process() -> std::io::Result<()> {
    rustix::process::setrlimit(
        rustix::process::Resource::Core,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    #[cfg(target_os = "linux")]
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)?;
    Ok(())
}
