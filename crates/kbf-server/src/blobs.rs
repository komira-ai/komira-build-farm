//! The daemons' blob path on the worker listener: `ByteStream` (Read, Write and
//! QueryWriteStatus) and `ContentAddressableStorage` (FindMissingBlobs,
//! BatchUpdateBlobs, BatchReadBlobs, GetTree) over the farm's cache, so a daemon reads
//! inputs and writes outputs over the same mutual TLS as its worker stream and needs no
//! path to REAPI.
//!
//! Every call of both services is admitted before it is served, by one rule
//! ([`admit`], over [`crate::identity`]): the listener serves mutual TLS, the call's
//! client certificate names exactly one node, and neither the certificate (serial,
//! public key) nor that node is on the deny list, looked at again for this call. Before
//! that, the listener's role table ([`crate::worker_roles`]) took the call from a
//! certificate naming the daemon role alone. A
//! streamed request (a Write, a RegisterChunkMapping) is refused before its first
//! message is read. Under plain text every call is refused UNAUTHENTICATED. An
//! admitted call is served as the REAPI listener serves it; the chunking calls answer
//! UNIMPLEMENTED there as here.
//!
//! `kbf.worker.v1.Segments` ([`WorkerSegments`]: segment upload and segment-range
//! reads, `docs/design/write-path.md`, section 4) is admitted by the same rule.
//!
//! `Execution`, `ActionCache` and `Capabilities` are not in the role table, so this
//! listener refuses them PERMISSION_DENIED.

use kbf_front::{ByteStreamService, CasService, MetaLog, SegmentsService};
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, WriteRequest, WriteResponse,
};
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetChunkMappingRequest, GetTreeRequest, RegisterChunkMappingRequest,
    RegisterChunkMappingResponse, SpliceBlobRequest, SpliceBlobResponse, SplitBlobRequest,
    SplitBlobResponse,
};
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use tonic::{Request, Response, Status, Streaming};

use crate::identity::Peers;

/// Admits the call `request` is, as the module docs say, or says why not (and logs it).
async fn admit<T>(peers: &Peers, request: &Request<T>, call: &'static str) -> Result<(), Status> {
    let certs = kbf_tls::peer_certs(request);
    let leaf = certs.as_deref().and_then(|chain| chain.first());
    let admitted = peers.admit_call(leaf.map(AsRef::as_ref)).await;
    admitted.map(drop).inspect_err(|refused| {
        let (code, reason) = (refused.code(), refused.message());
        tracing::warn!(call, ?code, reason, "blob call refused");
    })
}

/// `ByteStream` for daemons, each call admitted as the module docs say.
#[derive(Debug)]
pub struct WorkerBlobs<M, O> {
    inner: ByteStreamService<M, O>,
    peers: Peers,
}

impl<M, O> WorkerBlobs<M, O> {
    /// The service over `inner`, admitting calls as `peers` (the worker listener's).
    pub const fn new(inner: ByteStreamService<M, O>, peers: Peers) -> Self {
        Self { inner, peers }
    }
}

#[tonic::async_trait]
impl<M, O> ByteStream for WorkerBlobs<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    type ReadStream = <ByteStreamService<M, O> as ByteStream>::ReadStream;

    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        admit(&self.peers, &request, "Read").await?;
        self.inner.read(request).await
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        admit(&self.peers, &request, "Write").await?;
        self.inner.write(request).await
    }

    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        admit(&self.peers, &request, "QueryWriteStatus").await?;
        self.inner.query_write_status(request).await
    }
}

/// `ContentAddressableStorage` for daemons, each call admitted as the module docs say.
#[derive(Debug)]
pub struct WorkerCas<M, O> {
    inner: CasService<M, O>,
    peers: Peers,
}

impl<M, O> WorkerCas<M, O> {
    /// The service over `inner`, admitting calls as `peers` (the worker listener's).
    pub const fn new(inner: CasService<M, O>, peers: Peers) -> Self {
        Self { inner, peers }
    }
}

#[tonic::async_trait]
impl<M, O> ContentAddressableStorage for WorkerCas<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        admit(&self.peers, &request, "FindMissingBlobs").await?;
        self.inner.find_missing_blobs(request).await
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        admit(&self.peers, &request, "BatchUpdateBlobs").await?;
        self.inner.batch_update_blobs(request).await
    }

    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        admit(&self.peers, &request, "BatchReadBlobs").await?;
        self.inner.batch_read_blobs(request).await
    }

    type GetTreeStream = <CasService<M, O> as ContentAddressableStorage>::GetTreeStream;

    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        admit(&self.peers, &request, "GetTree").await?;
        self.inner.get_tree(request).await
    }

    async fn split_blob(
        &self,
        request: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        admit(&self.peers, &request, "SplitBlob").await?;
        self.inner.split_blob(request).await
    }

    type GetChunkMappingStream =
        <CasService<M, O> as ContentAddressableStorage>::GetChunkMappingStream;

    async fn get_chunk_mapping(
        &self,
        request: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        admit(&self.peers, &request, "GetChunkMapping").await?;
        self.inner.get_chunk_mapping(request).await
    }

    async fn splice_blob(
        &self,
        request: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        admit(&self.peers, &request, "SpliceBlob").await?;
        self.inner.splice_blob(request).await
    }

    async fn register_chunk_mapping(
        &self,
        request: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        admit(&self.peers, &request, "RegisterChunkMapping").await?;
        self.inner.register_chunk_mapping(request).await
    }
}

/// `kbf.worker.v1.Segments` for daemons, each call admitted as the module docs say.
#[derive(Debug)]
pub struct WorkerSegments<M, O> {
    inner: SegmentsService<M, O>,
    peers: Peers,
}

impl<M, O> WorkerSegments<M, O> {
    /// The service over `inner`, admitting calls as `peers` (the worker listener's).
    pub const fn new(inner: SegmentsService<M, O>, peers: Peers) -> Self {
        Self { inner, peers }
    }
}

#[tonic::async_trait]
impl<M, O> Segments for WorkerSegments<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        admit(&self.peers, &request, "Begin").await?;
        self.inner.begin(request).await
    }

    async fn put_range(
        &self,
        request: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        admit(&self.peers, &request, "PutRange").await?;
        self.inner.put_range(request).await
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        admit(&self.peers, &request, "Commit").await?;
        self.inner.commit(request).await
    }

    async fn locate(
        &self,
        request: Request<LocateRequest>,
    ) -> Result<Response<LocateResponse>, Status> {
        admit(&self.peers, &request, "Locate").await?;
        self.inner.locate(request).await
    }

    async fn read_range(
        &self,
        request: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        admit(&self.peers, &request, "ReadRange").await?;
        self.inner.read_range(request).await
    }
}
