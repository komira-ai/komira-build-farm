//! The daemons' blob path on the worker listener: `ByteStream` (Read, Write and
//! QueryWriteStatus) over the farm's cache, so a daemon reads inputs and writes outputs
//! over the same mutual TLS as its worker stream and needs no path to REAPI.
//!
//! Every call is admitted before it is served, by the rules of [`crate::identity`]: the
//! listener serves mutual TLS, the call's client certificate names exactly one node,
//! and neither the certificate (serial, public key) nor that node is on the deny list,
//! looked at again for this call. A Write is refused before its first message is read.
//! Under plain text every call is refused UNAUTHENTICATED.
//!
//! Only `ByteStream` is served here, because it is all a daemon's CAS client calls:
//! `Execution`, `ActionCache`, `Capabilities` and `ContentAddressableStorage` answer
//! UNIMPLEMENTED on this listener.

use kbf_front::{ByteStreamService, MetaLog};
use kbf_objstore::ObjectStore;
use kbf_proto::google::bytestream::byte_stream_server::ByteStream;
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, WriteRequest, WriteResponse,
};
use tonic::{Request, Response, Status, Streaming};

use crate::identity::Peers;

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

    /// Admits the call `request` is, or says why not (and logs it).
    async fn admit<T>(&self, request: &Request<T>, call: &'static str) -> Result<(), Status> {
        let certs = request.peer_certs();
        let leaf = certs.as_deref().and_then(|chain| chain.first());
        let admitted = self.peers.admit_call(leaf.map(AsRef::as_ref)).await;
        admitted.map(drop).inspect_err(|refused| {
            let (code, reason) = (refused.code(), refused.message());
            tracing::warn!(call, ?code, reason, "blob call refused");
        })
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
        self.admit(&request, "Read").await?;
        self.inner.read(request).await
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        self.admit(&request, "Write").await?;
        self.inner.write(request).await
    }

    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        self.admit(&request, "QueryWriteStatus").await?;
        self.inner.query_write_status(request).await
    }
}
