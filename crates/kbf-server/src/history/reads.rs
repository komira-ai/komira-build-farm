//! The reading half of history: a [`kbf_history::Reader`] over the objects and rows the
//! writer keeps, with its object store's type erased so the build pages need not be
//! generic over it, and the index of the runs the recorder still holds.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_history::{
    Allow, ChunkCursor, InstanceList, InvKey, InvocationPage, InvocationView, ListQuery,
    PageCursor, ReadError, Reader, RecordsPage, RunId, RunView, ShapeEntry, ShapePage, StatusEntry,
    StatusKind, StatusPage, Today,
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

    fn instance_names(&self) -> BoxFuture<'_, Result<InstanceList, ReadError>>;

    fn list_invocations<'a>(
        &'a self,
        q: &'a ListQuery<'a>,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<InvocationPage, ReadError>>;

    fn shapes<'a>(
        &'a self,
        instance: &'a str,
        after: Option<&'a PageCursor>,
        limit: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<ShapePage, ReadError>>;

    fn shape_cfgs<'a>(
        &'a self,
        instance: &'a str,
        shape: u64,
        after: Option<&'a PageCursor>,
        limit: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<ShapePage, ReadError>>;

    fn shape<'a>(
        &'a self,
        instance: &'a str,
        shape: u64,
        cfg: &'a str,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<ShapeEntry>, ReadError>>;

    fn status<'a>(
        &'a self,
        instance: &'a str,
        shape: u64,
        cfg: &'a str,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<StatusEntry>, ReadError>>;

    fn by_status<'a>(
        &'a self,
        kind: StatusKind,
        after: Option<&'a PageCursor>,
        limit: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<StatusPage, ReadError>>;
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

    fn instance_names(&self) -> BoxFuture<'_, Result<InstanceList, ReadError>> {
        Box::pin(self.instances(&|_| true))
    }

    fn list_invocations<'a>(
        &'a self,
        q: &'a ListQuery<'a>,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<InvocationPage, ReadError>> {
        Box::pin(Reader::list_invocations(self, q, allow))
    }

    fn shapes<'a>(
        &'a self,
        instance: &'a str,
        after: Option<&'a PageCursor>,
        limit: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<ShapePage, ReadError>> {
        Box::pin(Reader::shapes(self, instance, after, limit, allow))
    }

    fn shape_cfgs<'a>(
        &'a self,
        instance: &'a str,
        shape: u64,
        after: Option<&'a PageCursor>,
        limit: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<ShapePage, ReadError>> {
        Box::pin(Reader::shape_cfgs(
            self, instance, shape, after, limit, allow,
        ))
    }

    fn shape<'a>(
        &'a self,
        instance: &'a str,
        shape: u64,
        cfg: &'a str,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<ShapeEntry>, ReadError>> {
        Box::pin(Reader::shape(self, instance, shape, cfg, allow))
    }

    fn status<'a>(
        &'a self,
        instance: &'a str,
        shape: u64,
        cfg: &'a str,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<Option<StatusEntry>, ReadError>> {
        Box::pin(Reader::status(self, instance, shape, cfg, allow))
    }

    fn by_status<'a>(
        &'a self,
        kind: StatusKind,
        after: Option<&'a PageCursor>,
        limit: usize,
        allow: Allow<'a>,
    ) -> BoxFuture<'a, Result<StatusPage, ReadError>> {
        Box::pin(Reader::by_status(self, kind, after, limit, allow))
    }
}

/// What the build pages read history with: the reader, and where the live record of each
/// recently minted run is.
pub struct HistoryReads {
    source: Box<dyn Source>,
    runs: Arc<RunIndex>,
    today: Arc<Today>,
}

impl HistoryReads {
    pub(crate) fn new<O: ObjectStore>(
        reader: Reader<O>,
        runs: Arc<RunIndex>,
        today: Arc<Today>,
    ) -> Self {
        Self {
            source: Box::new(reader),
            runs,
            today,
        }
    }

    /// The runs of today the writer holds in memory.
    pub(crate) fn today(&self) -> &Today {
        &self.today
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

    /// Every instance that has an invocation list, whatever a caller may read.
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn instance_names(&self) -> Result<InstanceList, ReadError> {
        self.source.instance_names().await
    }

    /// [`Reader::list_invocations`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn list_invocations(
        &self,
        q: &ListQuery<'_>,
        allow: Allow<'_>,
    ) -> Result<InvocationPage, ReadError> {
        self.source.list_invocations(q, allow).await
    }

    /// [`Reader::shapes`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn shapes(
        &self,
        instance: &str,
        after: Option<&PageCursor>,
        limit: usize,
        allow: Allow<'_>,
    ) -> Result<ShapePage, ReadError> {
        self.source.shapes(instance, after, limit, allow).await
    }

    /// [`Reader::shape_cfgs`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn shape_cfgs(
        &self,
        instance: &str,
        shape: u64,
        after: Option<&PageCursor>,
        limit: usize,
        allow: Allow<'_>,
    ) -> Result<ShapePage, ReadError> {
        self.source
            .shape_cfgs(instance, shape, after, limit, allow)
            .await
    }

    /// [`Reader::shape`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn shape(
        &self,
        instance: &str,
        shape: u64,
        cfg: &str,
        allow: Allow<'_>,
    ) -> Result<Option<ShapeEntry>, ReadError> {
        self.source.shape(instance, shape, cfg, allow).await
    }

    /// [`Reader::status`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn status(
        &self,
        instance: &str,
        shape: u64,
        cfg: &str,
        allow: Allow<'_>,
    ) -> Result<Option<StatusEntry>, ReadError> {
        self.source.status(instance, shape, cfg, allow).await
    }

    /// [`Reader::by_status`].
    ///
    /// # Errors
    /// [`ReadError`].
    pub async fn by_status(
        &self,
        kind: StatusKind,
        after: Option<&PageCursor>,
        limit: usize,
        allow: Allow<'_>,
    ) -> Result<StatusPage, ReadError> {
        self.source.by_status(kind, after, limit, allow).await
    }
}

#[cfg(test)]
#[path = "reads_tests.rs"]
mod tests;
