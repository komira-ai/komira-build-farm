//! How a frontend learns what a storage node owes (`docs/design/architecture-v2.md`,
//! section 8: the repair record is kept in the node's journal "and, through a frontend,
//! in the state store, so the record survives the primary").
//!
//! A storage node does not reach the state store; the frontend does, and a node reaches the
//! frontend over its registration stream. That stream is another change, so this is the
//! seam it will fill: [`RepairLedger`] is told of each copy owed and settled. It is
//! **never waited for**: the journal is what makes a one-copy write safe to answer, and a
//! frontend that is slow or away must not stop writes.

use futures::future::BoxFuture;
use kbf_types::Digest;

use super::RepairRecord;

/// Where a node reports what it owes. Reports are best effort: an implementation that
/// cannot deliver one logs and goes on.
pub trait RepairLedger: Send + Sync {
    /// `record` is owed.
    fn owed<'a>(&'a self, record: &'a RepairRecord) -> BoxFuture<'a, ()>;

    /// `digest` is no longer owed.
    fn settled<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, ()>;
}

/// A node with no frontend to tell: reports go nowhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoLedger;

impl RepairLedger for NoLedger {
    fn owed<'a>(&'a self, _: &'a RepairRecord) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn settled<'a>(&'a self, _: &'a Digest) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}
