//! The storage nodes a router calls: one [`Storage`] per map member.
//!
//! [`TlsNodes`] dials a member over mutual TLS and verifies it against **its own** name
//! from the map (the host of its address, which is the name its certificate carries),
//! never against one name for every node: a
//! daemon that dialled node-03 and accepted node-02's certificate would send its
//! blobs to whoever holds any certificate of the farm's CA. There is no parameter that
//! sets a name, so there is nothing to override.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_map::{Member, MemberId};
use kbf_storage::Storage;
use kbf_storage::StorageError;
use kbf_storage::service::RemoteStorage;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

/// How a router reaches a member.
pub trait Nodes: Send + Sync {
    /// The node `member` is, every call of it tagged with map `generation` (the one the
    /// caller routed by, so a node can tell the caller's map is old).
    ///
    /// # Errors
    /// The member cannot be dialled: its address is not one.
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError>;
}

/// How long a connection to a node may be silent before the router gives up on it, so that
/// a node that has stopped answering (its host is gone, a firewall drops its packets)
/// fails the call and the router goes on to the next owner, instead of the call waiting
/// for the operating system's TCP timeouts (minutes).
///
/// Nothing here bounds how long a request takes: a large read on a healthy connection is
/// answered by the HTTP/2 pings as long as the node is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    /// How long dialling may take, for the TCP connection and for the TLS handshake each.
    pub connect: Duration,
    /// How often a connection with a call on it is pinged (HTTP/2 PING).
    pub keepalive_interval: Duration,
    /// How long a ping may go unanswered before the connection is dropped and the calls
    /// on it fail.
    pub keepalive_timeout: Duration,
}

impl Default for Liveness {
    /// Ten seconds to dial, and a connection that has not answered a ping sent every ten
    /// seconds within ten more is dropped: a silent node is given up on within about
    /// twenty seconds.
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(10),
            keepalive_timeout: Duration::from_secs(10),
        }
    }
}

/// Storage nodes over mutual TLS, connected when first used.
#[derive(Debug)]
pub struct TlsNodes {
    /// The CA and this process's identity, and no domain name.
    tls: ClientTlsConfig,
    liveness: Liveness,
    /// One connection per member address; a member that moves is a new connection.
    channels: Mutex<HashMap<(MemberId, String), Channel>>,
}

impl TlsNodes {
    /// Nodes that present `identity` and trust only `ca`.
    #[must_use]
    pub fn new(ca: Certificate, identity: Identity) -> Self {
        Self {
            tls: ClientTlsConfig::new().ca_certificate(ca).identity(identity),
            liveness: Liveness::default(),
            channels: Mutex::default(),
        }
    }

    /// The same nodes, giving up on a silent one as `liveness` says rather than as
    /// [`Liveness::default`] does.
    #[must_use]
    pub fn with_liveness(mut self, liveness: Liveness) -> Self {
        self.liveness = liveness;
        self
    }

    /// How these nodes give up on a silent one: what [`TlsNodes::with_liveness`] set, or
    /// [`Liveness::default`]. Read by the daemon's tests to see that its flags arrived.
    #[must_use]
    pub fn liveness(&self) -> Liveness {
        self.liveness
    }

    /// The connection to `member`, verified against the host of its address. For a caller
    /// that speaks to a node without a [`Storage`] (the frontend's pass-through of a
    /// ByteStream).
    ///
    /// # Errors
    /// The member's address is not a host and port.
    pub fn channel(&self, member: &Member) -> Result<Channel, StorageError> {
        let key = (member.id, member.addr.clone());
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(channel) = channels.get(&key) {
            return Ok(channel.clone());
        }
        let endpoint = Endpoint::from_shared(format!("https://{}", member.addr))
            .map_err(|e| bad(member, "its address is not a host and port", &e))?;
        // No domain name is set: tonic then verifies the certificate against the host of
        // the endpoint's URI, which is the host of this member's address.
        let live = self.liveness;
        let channel = endpoint
            .connect_timeout(live.connect)
            .http2_keep_alive_interval(live.keepalive_interval)
            .keep_alive_timeout(live.keepalive_timeout)
            .tls_config(self.tls.clone().timeout(live.connect))
            .map_err(|e| bad(member, "its TLS configuration is not usable", &e))?
            .connect_lazy();
        channels.insert(key, channel.clone());
        Ok(channel)
    }
}

impl Nodes for TlsNodes {
    fn node(&self, member: &Member, generation: u64) -> Result<Arc<dyn Storage>, StorageError> {
        Ok(Arc::new(RemoteStorage::new(
            self.channel(member)?,
            generation,
        )))
    }
}

/// A member that cannot be dialled: the map or the configuration is wrong, not the node
/// down.
fn bad(member: &Member, what: &str, error: &dyn std::fmt::Display) -> StorageError {
    StorageError::Internal(format!(
        "member {} at {:?} cannot be dialled, {what}: {error}",
        member.id, member.addr
    ))
}
