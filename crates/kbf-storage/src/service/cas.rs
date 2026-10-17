//! `ContentAddressableStorage`: FindMissingBlobs over [`crate::Storage::find_missing`].

use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use kbf_proto::reapi::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetChunkMappingRequest, GetChunkMappingResponse, GetTreeRequest, GetTreeResponse,
    RegisterChunkMappingRequest, RegisterChunkMappingResponse, SpliceBlobRequest,
    SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse,
};
use tonic::{Request, Response, Status, Streaming};

use super::{Endpoint, MAX_FIND_MISSING_DIGESTS, StorageService, catch_up, generation, wire};

/// What every call but FindMissingBlobs answers.
fn not_served<T>(call: &str) -> Result<T, Status> {
    Err(Status::unimplemented(format!(
        "{call}: a storage node serves FindMissingBlobs, and blobs through ByteStream"
    )))
}

#[tonic::async_trait]
impl ContentAddressableStorage for StorageService {
    /// The digests the store does not hold, in the order asked. A request naming more than
    /// [`MAX_FIND_MISSING_DIGESTS`] is refused INVALID_ARGUMENT. While the node is catching
    /// up, a call with any digest missing is UNAVAILABLE as a whole: an answer that left
    /// out only the missing ones would tell the caller they are held.
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        self.admission.check(&request, Endpoint::FindMissing)?;
        self.check_map(generation::requested(&request)?).await?;
        let request = request.into_inner();
        if request.blob_digests.len() > MAX_FIND_MISSING_DIGESTS {
            return Err(Status::invalid_argument(format!(
                "FindMissingBlobs names {} digests; a call may name at most \
                 {MAX_FIND_MISSING_DIGESTS}, so ask in batches",
                request.blob_digests.len()
            )));
        }
        wire::check_digest_function(request.digest_function)?;
        let digests = request
            .blob_digests
            .iter()
            .map(|d| wire::digest(Some(d)))
            .collect::<Result<Vec<_>, _>>()?;
        let catching_up = self.catching_up();
        let missing = self.storage.find_missing(&digests).await?;
        if let Some(status) = catch_up::missing_gap(catching_up, missing.len(), digests.len()) {
            return Err(status);
        }
        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing.iter().map(wire::to_proto).collect(),
        }))
    }

    async fn batch_update_blobs(
        &self,
        _: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        not_served("BatchUpdateBlobs")
    }

    async fn batch_read_blobs(
        &self,
        _: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        not_served("BatchReadBlobs")
    }

    type GetTreeStream = futures::stream::Empty<Result<GetTreeResponse, Status>>;

    async fn get_tree(
        &self,
        _: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        not_served("GetTree")
    }

    async fn split_blob(
        &self,
        _: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        not_served("SplitBlob")
    }

    async fn splice_blob(
        &self,
        _: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        not_served("SpliceBlob")
    }

    type GetChunkMappingStream = futures::stream::Empty<Result<GetChunkMappingResponse, Status>>;

    async fn get_chunk_mapping(
        &self,
        _: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        not_served("GetChunkMapping")
    }

    async fn register_chunk_mapping(
        &self,
        _: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        not_served("RegisterChunkMapping")
    }
}
