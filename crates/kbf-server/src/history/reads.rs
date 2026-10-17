//! The reading half of history: a [`kbf_history::Reader`] over the objects and rows the
//! writer keeps, with its object store's type erased so the build pages need not be
//! generic over it, and the index of the runs the recorder still holds.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_history::records::FlogStream;
use kbf_history::{
    Allow, ChunkCursor, FailureLog, InvKey, InvocationView, ReadError, Reader, RecordsPage, RunId,
    RunView,
};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

use super::tap::RunIndex;

/// An object store the writer and the reader both hold.
pub(crate) struct Shared<O>(pub(crate) Arc<O>);

impl<O> Clone for Shared<O> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<O: ObjectStore> ObjectStore for Shared<O> {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> impl Future<Output = Result<(), ObjectStoreError>> + Send {
        self.0.put_new(key, body, retain_until)
    }

    fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> impl Future<Output = Result<Bytes, ObjectStoreError>> + Send {
        self.0.get_range(key, range)
    }

    fn may_hold(
        &self,
        key: &ObjectKey,
    ) -> impl Future<Output = Result<bool, ObjectStoreError>> + Send {
        self.0.may_hold(key)
    }

    fn delete(&self, key: &ObjectKey) -> impl Future<Output = Result<(), ObjectStoreError>> + Send {
        self.0.delete(key)
    }

    fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> impl Future<Output = Result<ListPage, ObjectStoreError>> + Send {
        self.0.list(prefix, after, max_keys)
    }
}

/// [`Reader`]'s calls with the object store's type erased.
trait Source: Send + Sync {
    fn instances<'a>(
        &'a self,
        inv: &'a InvKey,
    ) -> BoxFuture<'a, Result<Option<Vec<String>>, ReadError>>;

    fn invocation<'a>(
        &'a self,
        inv: &'a InvKey,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<InvocationView>, ReadError>>;

    fn records<'a>(
        &'a self,
        inv: &'a InvKey,
        after: Option<&'a ChunkCursor>,
        chunks: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<RecordsPage>, ReadError>>;

    fn run<'a>(
        &'a self,
        id: RunId,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<RunView>, ReadError>>;

    fn failure_log<'a>(
        &'a self,
        id: RunId,
        stream: FlogStream,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<FailureLog>, ReadError>>;

    fn read_log<'a>(
        &'a self,
        log: &'a FailureLog,
        first: u64,
        last: u64,
    ) -> BoxFuture<'a, Result<Bytes, ReadError>>;
}

impl<O: ObjectStore> Source for Reader<O> {
    fn instances<'a>(
        &'a self,
        inv: &'a InvKey,
    ) -> BoxFuture<'a, Result<Option<Vec<String>>, ReadError>> {
        Box::pin(self.invocation_instances(inv))
    }

    fn invocation<'a>(
        &'a self,
        inv: &'a InvKey,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<InvocationView>, ReadError>> {
        Box::pin(Reader::invocation(self, inv, allow))
    }

    fn records<'a>(
        &'a self,
        inv: &'a InvKey,
        after: Option<&'a ChunkCursor>,
        chunks: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<RecordsPage>, ReadError>> {
        Box::pin(Reader::records(self, inv, after, chunks, allow))
    }

    fn run<'a>(
        &'a self,
        id: RunId,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<RunView>, ReadError>> {
        Box::pin(Reader::run(self, id, allow))
    }

    fn failure_log<'a>(
        &'a self,
        id: RunId,
        stream: FlogStream,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<FailureLog>, ReadError>> {
        Box::pin(Reader::failure_log(self, id, stream, allow))
    }

    fn read_log<'a>(
        &'a self,
        log: &'a FailureLog,
        first: u64,
        last: u64,
    ) -> BoxFuture<'a, Result<Bytes, ReadError>> {
        Box::pin(Reader::read_log(self, log, first, last))
    }
}

/// What the build pages read history with: the reader, and where the live record of each
/// recently minted run is.
pub struct HistoryReads {
    source: Box<dyn Source>,
    runs: Arc<RunIndex>,
}

impl HistoryReads {
    pub(crate) fn new<O: ObjectStore>(reader: Reader<O>, runs: Arc<RunIndex>) -> Self {
        Self {
            source: Box::new(reader),
            runs,
        }
    }

    /// Where the live record of each recently minted run is.
    pub(crate) fn runs(&self) -> &RunIndex {
        &self.runs
    }

    /// [`Reader::invocation_instances`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn instances(&self, inv: &InvKey) -> Result<Option<Vec<String>>, ReadError> {
        self.source.instances(inv).await
    }

    /// [`Reader::invocation`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn invocation(
        &self,
        inv: &InvKey,
        allow: Allow<'_>,
    ) -> Result<Option<InvocationView>, ReadError> {
        self.source.invocation(inv, allow).await
    }

    /// [`Reader::records`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn records(
        &self,
        inv: &InvKey,
        after: Option<&ChunkCursor>,
        chunks: usize,
        allow: Allow<'_>,
    ) -> Result<Option<RecordsPage>, ReadError> {
        self.source.records(inv, after, chunks, allow).await
    }

    /// [`Reader::run`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn run(&self, id: RunId, allow: Allow<'_>) -> Result<Option<RunView>, ReadError> {
        self.source.run(id, allow).await
    }

    /// [`Reader::failure_log`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn failure_log(
        &self,
        id: RunId,
        stream: FlogStream,
        allow: Allow<'_>,
    ) -> Result<Option<FailureLog>, ReadError> {
        self.source.failure_log(id, stream, allow).await
    }

    /// [`Reader::read_log`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn read_log(
        &self,
        log: &FailureLog,
        first: u64,
        last: u64,
    ) -> Result<Bytes, ReadError> {
        self.source.read_log(log, first, last).await
    }
}

#[cfg(test)]
#[path = "reads_tests.rs"]
mod tests;
