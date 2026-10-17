//! How a storage node reaches the others: one lazily connected channel per address,
//! over TLS with the node's own certificate when the farm uses it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::StorageError;

/// How long a connection attempt may take before the peer is judged unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Idle and in-flight connections are probed this often, and a peer that does not answer
/// within the same time is gone: a dead replica fails a write in seconds instead of
/// whenever the kernel gives up on the socket.
const KEEP_ALIVE: Duration = Duration::from_secs(10);

/// The channels to the other storage nodes of the map.
pub struct Peers {
    tls: Option<ClientTlsConfig>,
    channels: Mutex<BTreeMap<String, Channel>>,
}

impl Peers {
    /// Plain-text channels: one-box trials and tests, as `Admission::Open`.
    #[must_use]
    pub fn plain() -> Self {
        Self {
            tls: None,
            channels: Mutex::new(BTreeMap::new()),
        }
    }

    /// TLS channels, `tls` carrying the roots to trust and this node's own certificate:
    /// the one whose role is `storage` (`urn:kbf:role:storage`, with serverAuth and
    /// clientAuth), so the peer's `Endpoint::WriteLeg` admits the call.
    #[must_use]
    pub fn tls(tls: ClientTlsConfig) -> Self {
        Self {
            tls: Some(tls),
            channels: Mutex::new(BTreeMap::new()),
        }
    }

    /// The channel to the member dialled at `addr` (`host:port`), made on first use and
    /// connected on the first call, so a peer that is down is an error of the call and
    /// not of this lookup.
    ///
    /// # Errors
    /// [`StorageError::Internal`] if `addr` is not an address a channel can be made to
    /// (the map holds one that was never dialable).
    pub(crate) fn channel(&self, addr: &str) -> Result<Channel, StorageError> {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(channel) = channels.get(addr) {
            return Ok(channel.clone());
        }
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        let mut endpoint = Endpoint::from_shared(format!("{scheme}://{addr}"))
            .map_err(|e| StorageError::Internal(format!("member address {addr:?}: {e}")))?
            .connect_timeout(CONNECT_TIMEOUT)
            .http2_keep_alive_interval(KEEP_ALIVE)
            .keep_alive_timeout(KEEP_ALIVE)
            .keep_alive_while_idle(true);
        if let Some(tls) = &self.tls {
            endpoint = endpoint
                .tls_config(tls.clone())
                .map_err(|e| StorageError::Internal(format!("TLS for {addr:?}: {e}")))?;
        }
        let channel = endpoint.connect_lazy();
        channels.insert(addr.to_owned(), channel.clone());
        Ok(channel)
    }
}
