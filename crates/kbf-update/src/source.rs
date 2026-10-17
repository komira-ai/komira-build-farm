//! Where the bytes come from: the frontend's release service, behind a trait so the
//! client's tests serve them from memory and the daemon serves them over its existing
//! mutual-TLS channel.

use futures::future::BoxFuture;
use futures::stream::BoxStream;

/// The chunks of one blob, in order.
pub type Chunks = BoxStream<'static, Result<Vec<u8>, FetchError>>;

/// Why a fetch failed.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The connection or the service failed; trying again may work.
    #[error("{0}")]
    Transport(String),
    /// The service does not have the blob; trying again will not help.
    #[error("the frontend does not hold it: {0}")]
    Missing(String),
}

/// A release service.
pub trait Source: Send + Sync {
    /// The blob whose SHA-256 is `sha256` (64 lowercase hex digits), from its first byte.
    fn fetch(&self, sha256: &str) -> BoxFuture<'_, Result<Chunks, FetchError>>;
}
