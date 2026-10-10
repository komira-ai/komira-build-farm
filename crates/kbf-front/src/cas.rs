//! `ContentAddressableStorage`: FindMissingBlobs, BatchUpdateBlobs, BatchReadBlobs and
//! GetTree. The chunking calls (REAPI 2.12 and later) answer UNIMPLEMENTED, matching
//! the capabilities, which do not advertise them.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use kbf_auth::{Authorizers, authorize};
use kbf_objstore::ObjectStore;
use kbf_proto::reapi::content_addressable_storage_server::ContentAddressableStorage;
use kbf_proto::reapi::{
    self, BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, Directory, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetChunkMappingRequest, GetChunkMappingResponse, GetTreeRequest, GetTreeResponse,
    RegisterChunkMappingRequest, RegisterChunkMappingResponse, SpliceBlobRequest,
    SpliceBlobResponse, SplitBlobRequest, SplitBlobResponse, batch_read_blobs_response,
    batch_update_blobs_request, batch_update_blobs_response,
};
use kbf_types::Digest;
use prost::Message;
use tonic::{Request, Response, Status, Streaming};

use crate::cache::{Cache, CacheError, VerifiedBlob};
use crate::meta_log::MetaLog;
use crate::{MAX_BATCH_TOTAL_BYTES, wire};

/// Directories per `GetTreeResponse` when the client sets no smaller page size.
const TREE_PAGE_DIRECTORIES: usize = 1_000;

/// The `ContentAddressableStorage` service over a [`Cache`]. FindMissingBlobs is
/// authorized by [`Authorizers::cas_find_missing`]; BatchReadBlobs, GetTree, SplitBlob
/// and GetChunkMapping by [`Authorizers::cas_get`]; BatchUpdateBlobs, SpliceBlob and
/// RegisterChunkMapping by [`Authorizers::cas_put`]; each against the request's
/// instance name, before anything else in the request is read.
#[derive(Debug)]
pub struct CasService<M, O> {
    cache: Arc<Cache<M, O>>,
    authorizers: Arc<Authorizers>,
}

impl<M, O> CasService<M, O> {
    /// The service over `cache`, every call allowed.
    #[must_use]
    pub fn new(cache: Arc<Cache<M, O>>) -> Self {
        Self::with_authorizers(cache, Arc::new(Authorizers::allow_all()))
    }

    /// The service over `cache`, each call authorized by `authorizers`.
    #[must_use]
    pub const fn with_authorizers(cache: Arc<Cache<M, O>>, authorizers: Arc<Authorizers>) -> Self {
        Self { cache, authorizers }
    }
}

/// The gRPC path of a `ContentAddressableStorage` method, as calls are logged.
macro_rules! cas_call {
    ($method:literal) => {
        concat!(
            "/build.bazel.remote.execution.v2.ContentAddressableStorage/",
            $method
        )
    };
}

type Stream<T> = stream::Iter<std::vec::IntoIter<Result<T, Status>>>;

#[tonic::async_trait]
impl<M: MetaLog, O: ObjectStore + 'static> ContentAddressableStorage for CasService<M, O> {
    /// Every requested digest is answered: a digest that does not parse fails the whole
    /// call rather than being dropped, because a client reads an omitted digest as
    /// present and never uploads it.
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let find_missing = &*self.authorizers.cas_find_missing;
        let call = cas_call!("FindMissingBlobs");
        authorize(find_missing, &caller, call, &request.instance_name).await?;
        wire::check_digest_function(request.digest_function)?;
        let digests = wire::digests(&request.blob_digests)?;
        let missing = self.cache.find_missing(&digests).await?;
        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing.iter().map(wire::digest_to_proto).collect(),
        }))
    }

    /// Verifies each blob against its digest and stores the good ones together (one
    /// segment, one commit per blob). A bad blob fails alone, with INVALID_ARGUMENT.
    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = cas_call!("BatchUpdateBlobs");
        authorize(
            &*self.authorizers.cas_put,
            &caller,
            call,
            &request.instance_name,
        )
        .await?;
        wire::check_digest_function(request.digest_function)?;
        let total: usize = request.requests.iter().map(|r| r.data.len()).sum();
        if total > MAX_BATCH_TOTAL_BYTES {
            return Err(Status::invalid_argument(format!(
                "batch carries {total} bytes; at most {MAX_BATCH_TOTAL_BYTES} are allowed"
            )));
        }
        let checked: Vec<(Option<reapi::Digest>, Result<VerifiedBlob, Status>)> = request
            .requests
            .into_iter()
            .map(|r| (r.digest.clone(), verify_upload(r)))
            .collect();
        let good: Vec<VerifiedBlob> = checked
            .iter()
            .filter_map(|(_, c)| c.as_ref().ok().cloned())
            .collect();
        let stored = self.cache.store_blobs(good).await.map_err(Status::from);
        let responses = checked
            .into_iter()
            .map(|(digest, c)| {
                let status = match (c, &stored) {
                    (Ok(_), Ok(())) => wire::rpc_ok(),
                    (Ok(_), Err(e)) => wire::rpc_status(e),
                    (Err(e), _) => wire::rpc_status(&e),
                };
                batch_update_blobs_response::Response {
                    digest,
                    status: Some(status),
                }
            })
            .collect();
        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    /// Reads each blob; each answers alone (NOT_FOUND only when the blob is not held).
    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = cas_call!("BatchReadBlobs");
        authorize(
            &*self.authorizers.cas_get,
            &caller,
            call,
            &request.instance_name,
        )
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
        let mut responses = Vec::with_capacity(request.digests.len());
        for asked in &request.digests {
            let read = match wire::digest(Some(asked)) {
                Ok(d) => self.cache.read_blob(&d).await.map_err(Status::from),
                Err(e) => Err(e),
            };
            let (data, status) = match read {
                Ok(bytes) => (bytes.to_vec(), wire::rpc_ok()),
                Err(e) => (Vec::new(), wire::rpc_status(&e)),
            };
            responses.push(batch_read_blobs_response::Response {
                digest: Some(asked.clone()),
                data,
                compressor: reapi::compressor::Value::Identity as i32,
                status: Some(status),
            });
        }
        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = Stream<GetTreeResponse>;

    /// Every directory reachable from the root, breadth first, each distinct digest
    /// once. A missing child is left out with its subtree (REAPI's rule); a missing
    /// root is NOT_FOUND. The page token is the number of directories already sent.
    ///
    /// Each call walks the tree again from the root and skips what earlier pages sent.
    /// The token is an offset, not a snapshot: if a directory in the tree is collected
    /// or uploaded between pages, later pages can repeat or leave out a directory.
    /// Pages from one call come from one walk and are consistent with each other.
    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let request = request.into_inner();
        let call = cas_call!("GetTree");
        authorize(
            &*self.authorizers.cas_get,
            &caller,
            call,
            &request.instance_name,
        )
        .await?;
        wire::check_digest_function(request.digest_function)?;
        let root = wire::digest(request.root_digest.as_ref())?;
        let skip = if request.page_token.is_empty() {
            0
        } else {
            request
                .page_token
                .parse::<usize>()
                .map_err(|_| Status::invalid_argument("page_token is not one this server gave"))?
        };
        let page = usize::try_from(request.page_size)
            .ok()
            .filter(|n| *n > 0)
            .map_or(TREE_PAGE_DIRECTORIES, |n| n.min(TREE_PAGE_DIRECTORIES));
        let directories = self.tree(root).await?;
        Ok(Response::new(stream::iter(pages(directories, skip, page))))
    }

    /// UNIMPLEMENTED, once authorized as a read.
    async fn split_blob(
        &self,
        request: Request<SplitBlobRequest>,
    ) -> Result<Response<SplitBlobResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let call = cas_call!("SplitBlob");
        authorize(&*self.authorizers.cas_get, &caller, call, instance).await?;
        Err(chunking_unimplemented())
    }

    type GetChunkMappingStream = Stream<GetChunkMappingResponse>;

    /// UNIMPLEMENTED, once authorized as a read.
    async fn get_chunk_mapping(
        &self,
        request: Request<GetChunkMappingRequest>,
    ) -> Result<Response<Self::GetChunkMappingStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let call = cas_call!("GetChunkMapping");
        authorize(&*self.authorizers.cas_get, &caller, call, instance).await?;
        Err(chunking_unimplemented())
    }

    /// UNIMPLEMENTED, once authorized as a write.
    async fn splice_blob(
        &self,
        request: Request<SpliceBlobRequest>,
    ) -> Result<Response<SpliceBlobResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let instance = &request.get_ref().instance_name;
        let call = cas_call!("SpliceBlob");
        authorize(&*self.authorizers.cas_put, &caller, call, instance).await?;
        Err(chunking_unimplemented())
    }

    /// UNIMPLEMENTED, once authorized as a write against the first message's instance
    /// name (the empty name when the stream sends none).
    async fn register_chunk_mapping(
        &self,
        request: Request<Streaming<RegisterChunkMappingRequest>>,
    ) -> Result<Response<RegisterChunkMappingResponse>, Status> {
        let caller = kbf_auth::metadata(&request);
        let first = request.into_inner().message().await?;
        let instance = first.map(|m| m.instance_name).unwrap_or_default();
        let call = cas_call!("RegisterChunkMapping");
        authorize(&*self.authorizers.cas_put, &caller, call, &instance).await?;
        Err(chunking_unimplemented())
    }
}

impl<M: MetaLog, O: ObjectStore> CasService<M, O> {
    /// The directories under `root`, breadth first, each distinct digest once.
    async fn tree(&self, root: Digest) -> Result<Vec<Directory>, Status> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::from([root]);
        let mut queue = VecDeque::from([root]);
        while let Some(digest) = queue.pop_front() {
            let is_root = digest == root;
            let bytes = match self.cache.read_blob(&digest).await {
                Ok(bytes) => bytes,
                Err(CacheError::NotFound(_)) if !is_root => continue,
                Err(e) => return Err(e.into()),
            };
            let directory = match Directory::decode(bytes) {
                Ok(d) => d,
                Err(e) if is_root => {
                    return Err(Status::invalid_argument(format!(
                        "tree root {digest} is not a Directory: {e}"
                    )));
                }
                Err(_) => continue,
            };
            for child in &directory.directories {
                let child = wire::digest(child.digest.as_ref())?;
                if seen.insert(child) {
                    queue.push_back(child);
                }
            }
            out.push(directory);
        }
        Ok(out)
    }
}

/// Splits `directories` after the first `skip` into responses of at most `page`
/// directories, each naming where the next starts.
fn pages(
    directories: Vec<Directory>,
    skip: usize,
    page: usize,
) -> Vec<Result<GetTreeResponse, Status>> {
    let total = directories.len();
    let rest: Vec<Directory> = directories.into_iter().skip(skip).collect();
    let mut out = Vec::new();
    let mut sent = skip.min(total);
    for chunk in rest.chunks(page) {
        sent += chunk.len();
        out.push(Ok(GetTreeResponse {
            directories: chunk.to_vec(),
            next_page_token: if sent < total {
                sent.to_string()
            } else {
                String::new()
            },
        }));
    }
    if out.is_empty() {
        out.push(Ok(GetTreeResponse::default()));
    }
    out
}

fn verify_upload(r: batch_update_blobs_request::Request) -> Result<VerifiedBlob, Status> {
    let digest = wire::digest(r.digest.as_ref())?;
    if r.compressor != reapi::compressor::Value::Identity as i32 {
        return Err(Status::invalid_argument(format!(
            "blob {digest}: compressor {} is not supported",
            r.compressor
        )));
    }
    Ok(VerifiedBlob::new(digest, Bytes::from(r.data))?)
}

fn chunking_unimplemented() -> Status {
    Status::unimplemented("blob chunking is not supported; see the capabilities")
}
