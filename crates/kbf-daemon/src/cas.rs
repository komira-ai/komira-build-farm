//! The blobs a lease reads and writes: the [`Cas`] trait and its helpers, from
//! `kbf-transfer`, whose [`kbf_transfer::Transfers`] is the store the daemon binary's
//! runtimes read and write through (one per process, shared by every lease).
//!
//! A runtime fetches an action, its command and its input tree through a `Cas`, and
//! stores outputs, stdout and stderr through it; a driver hashes an action's outputs
//! once and stores them with [`Cas::put_records`] (the native driver gathers them in a
//! [`Spool`] first). Blob bytes never travel on the
//! `kbf.worker.v1` stream; they go over the engine's own connection.

use kbf_proto::reapi::Digest;
pub use kbf_transfer::cas::*;
pub use kbf_transfer::{
    FileSource, OPEN_DIRECTORY, OPEN_FILE, Record, RecordHasher, Source, Spool, Spooled, Stamp,
    descend, proto_digest,
};

/// A [`Cas`] as the input tree's walks read it: one checked fetch, behind a pointer, so
/// those walks are compiled once whatever the CAS (a line a walk runs is then counted
/// once, not once per CAS type a test or a binary instantiates it with).
pub(crate) trait Fetch: Send + Sync {
    fn fetch<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, CasError>>;
}

/// A [`Fetch`] the input cache fills files through as well: the walks' fetch, and a
/// blob written into a file as it arrives.
pub(crate) trait Fill: Fetch {
    /// [`Cas::read_to`]: the blob written into `file` as it arrives, checked against
    /// `digest`; never held whole by the caller.
    fn fetch_to<'a>(
        &'a self,
        digest: &'a Digest,
        file: &'a mut tokio::fs::File,
    ) -> futures::future::BoxFuture<'a, Result<(), CasError>>;
}

impl<C: Cas> Fetch for C {
    fn fetch<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> futures::future::BoxFuture<'a, Result<Vec<u8>, CasError>> {
        Box::pin(fetch(self, digest))
    }
}

impl<C: Cas> Fill for C {
    fn fetch_to<'a>(
        &'a self,
        digest: &'a Digest,
        file: &'a mut tokio::fs::File,
    ) -> futures::future::BoxFuture<'a, Result<(), CasError>> {
        Box::pin(self.read_to(digest, file))
    }
}
