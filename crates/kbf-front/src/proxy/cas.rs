//! `ContentAddressableStorage` through the frontend, at the tonic level: the batch calls
//! name digests that live on different nodes, so the frontend decodes them, splits them
//! by owner and merges the answers. Each message is bounded by the gRPC message limit,
//! so holding one is bounded by construction. Single blobs, however large, go through
//! the raw ByteStream path ([`super::bytestream`]).

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use kbf_auth::{Authorizer, Authorizers, authorize};
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetChunkMappingRequest, GetChunkMappingResponse, GetTreeRequest, GetTreeResponse,
    RegisterChunkMappingRequest, RegisterChunkMappingResponse, SpliceBlobRequest,
    SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse, batch_read_blobs_response,
    batch_update_blobs_request, batch_update_blobs_response,
};
use tonic::{Request, Response, Status, Streaming};

use super::owners::Owners;
use crate::{MAX_BATCH_TOTAL_BYTES, wire};

/// Blobs of one batch in flight at once.
const BATCH_PARALLELISM: usize = 32;

macro_rules! cas_call {
    ($method:literal) => {
        concat!(
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/",
            $method
        )
    };
}

/// The `ContentAddressableStorage` service over the storage nodes. Each call is
/// authorized as the cache's service authorizes it ([`crate::CasService`]).
pub(super) struct ProxyCas {
    pub owners: Owners,
    pub authorizers: Arc<Authorizers>,
}

type Boxed<T> = BoxStream<'static, Result<T, Status>>;

impl ProxyCas {
    async fn allow(
        &self,
        authorizer: &Arc<dyn Authorizer>,
        call: &'static str,
        caller: &kbf_auth::AuthenticationMetadata,
        instance: &str,
    ) -> Result<(), Status> {
        authorize(&**authorizer, caller, call, instance).await
    }

    /// Stores one batch item on its writer, or says why not.
    async fn store_one(&self, item: batch_update_blobs_request::Request) -> Result<(), Status> {
        let digest = wire::digest(item.digest.as_ref())?;
        if item.compressor != reapi::compressor::Value::Identity as i32 {
            return Err(Status::invalid_argument(format!(
                "blob {digest}: compressor {} is not supported",
                item.compressor
            )));
        }
        let actual = kbf_segments::sha256(&item.data);
        if actual != digest {
            return Err(Status::invalid_argument(format!(
                "blob {digest}: the data is {actual}"
            )));
        }
        self.owners.put(&digest, Bytes::from(item.data)).await
    }

    fn unimplemented<T>(call: &str) -> Result<T, Status> {
        Err(Status::unimplemented(format!(
            "{call} is not served through the frontend proxy"
        )))
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for ProxyCas {
    /// Asks each node about the digests it owns, together, and answers in the order
    /// asked.
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let authorizer = &self.authorizers.cas_find_missing;
        let call = cas_call!("FindMissingBlobs");
        self.allow(authorizer, call, &caller, &request.instance_name)
            .await?;
        wire::check_digest_function(request.digest_function)?;
        let asked = wire::digests(&request.blob_digests)?;
        let missing = self.owners.find_missing(&asked).await?;
        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: asked
                .iter()
                .filter(|d| missing.contains(d))
                .map(wire::digest_to_proto)
                .collect(),
        }))
    }

    /// Verifies each blob against its digest and stores it on its primary; each answers
    /// alone, in request order.
    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let authorizer = &self.authorizers.cas_put;
        let call = cas_call!("BatchUpdateBlobs");
        self.allow(authorizer, call, &caller, &request.instance_name)
            .await?;
        wire::check_digest_function(request.digest_function)?;
        let total: usize = request.requests.iter().map(|r| r.data.len()).sum();
        if total > MAX_BATCH_TOTAL_BYTES {
            return Err(Status::invalid_argument(format!(
                "batch carries {total} bytes; at most {MAX_BATCH_TOTAL_BYTES} are allowed"
            )));
        }
        let responses = stream::iter(request.requests)
            .map(|item| async move {
                let digest = item.digest.clone();
                let stored = self.store_one(item).await;
                batch_update_blobs_response::Response {
                    digest,
                    status: Some(stored.map_or_else(|e| wire::rpc_status(&e), |()| wire::rpc_ok())),
                }
            })
            .buffered(BATCH_PARALLELISM)
            .collect::<Vec<_>>()
            .await;
        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    /// Reads each blob from the first node that has it; each answers alone, in request
    /// order.
    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let authorizer = &self.authorizers.cas_get;
        let call = cas_call!("BatchReadBlobs");
        self.allow(authorizer, call, &caller, &request.instance_name)
            .await?;
        wire::check_digest_function(request.digest_function)?;
        let total = request
            .digests
            .iter()
            .map(|d| u64::try_from(d.size_bytes).unwrap_or(0))
            .fold(0u64, u64::saturating_add);
        if total > MAX_BATCH_TOTAL_BYTES as u64 {
            return Err(Status::invalid_argument(format!(
                "batch asks for {total} bytes; at most {MAX_BATCH_TOTAL_BYTES} are allowed"
            )));
        }
        let responses = stream::iter(request.digests)
            .map(|asked| async move {
                let read = match wire::digest(Some(&asked)) {
                    Ok(digest) => self.owners.read_blob(&digest).await,
                    Err(e) => Err(e),
                };
                let (data, status) = match read {
                    Ok(bytes) => (bytes.to_vec(), wire::rpc_ok()),
                    Err(e) => (Vec::new(), wire::rpc_status(&e)),
                };
                batch_read_blobs_response::Response {
                    digest: Some(asked),
                    data,
                    compressor: reapi::compressor::Value::Identity as i32,
                    status: Some(status),
                }
            })
            .buffered(BATCH_PARALLELISM)
            .collect::<Vec<_>>()
            .await;
        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = Boxed<GetTreeResponse>;

    /// UNIMPLEMENTED for now, once authorized as a read: the walk reads directories
    /// from every owner.
    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        self.allow(
            &self.authorizers.cas_get,
            cas_call!("GetTree"),
            &caller,
            instance,
        )
        .await?;
        Self::unimplemented("GetTree")
    }

    /// UNIMPLEMENTED, once authorized as a read.
    async fn split_blob(
        &self,
        request: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        self.allow(
            &self.authorizers.cas_get,
            cas_call!("SplitBlob"),
            &caller,
            instance,
        )
        .await?;
        Self::unimplemented("SplitBlob")
    }

    type GetChunkMappingStream = Boxed<GetChunkMappingResponse>;

    /// UNIMPLEMENTED, once authorized as a read.
    async fn get_chunk_mapping(
        &self,
        request: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let call = cas_call!("GetChunkMapping");
        self.allow(&self.authorizers.cas_get, call, &caller, instance)
            .await?;
        Self::unimplemented("GetChunkMapping")
    }

    /// UNIMPLEMENTED, once authorized as a write.
    async fn splice_blob(
        &self,
        request: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        self.allow(
            &self.authorizers.cas_put,
            cas_call!("SpliceBlob"),
            &caller,
            instance,
        )
        .await?;
        Self::unimplemented("SpliceBlob")
    }

    /// UNIMPLEMENTED, once authorized as a write against the first message's instance.
    async fn register_chunk_mapping(
        &self,
        request: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let first = request.into_inner().message().await?;
        let instance = first.map(|m| m.instance_name).unwrap_or_default();
        let call = cas_call!("RegisterChunkMapping");
        self.allow(&self.authorizers.cas_put, call, &caller, &instance)
            .await?;
        Self::unimplemented("RegisterChunkMapping")
    }
}
