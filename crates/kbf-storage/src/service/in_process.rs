//! [`InProcess`]: a storage node's own [`StorageService`] called as a [`Storage`], without
//! a connection.
//!
//! A process that is a storage node and also a caller of the farm (`kbf storage-and-daemon`)
//! reaches the one member that is itself through this, and every other member through
//! [`super::RemoteStorage`]. The two answer alike because they run the same code: a write
//! takes the service's own write body ([`StorageService::write_in`]: the generation check,
//! the chain's plan, the blob streamed to the replica when this node is the primary, the
//! staging and the commit order of [`crate::replicate`]), so a blob this node is the
//! primary of is stored here and on its replica, never as one copy. A caller that skipped
//! the service and called the inner [`Storage`] would write that one copy and judge no
//! map.
//!
//! **The generation** is the caller's: [`InProcess::at`] takes the generation of the map
//! the caller routed by, the number a [`super::RemoteStorage`] sends in the
//! [`super::MAP_GENERATION_HEADER`] entry, and every call is judged by it before it reads
//! anything: STALE_MAP for a caller behind the node, a wait for the map for one ahead of it
//! ([`StorageService::check_map`]).
//!
//! **What is not run.** The caller is this process, so there is no certificate to admit
//! ([`super::Admission`]) and no wire message to decode: the handlers' admission and
//! translations are skipped, their checks of the map and their store calls are not. The
//! listener's own gate (a node with no map answers nothing) is a layer of the listener, not
//! of the service, and is the caller's to apply. `FindMissingBlobs` is not bounded to
//! [`super::MAX_FIND_MISSING_DIGESTS`]: that bounds what a message can ask of the node, and
//! this caller holds the slice already.
//!
//! **A node that is catching up** ([`super::CatchUp`]) answers a miss as its handlers do:
//! the rules are [`catch_up`]'s, asked here and by the handlers alike, so a read, a
//! `FindMissing`, an action-cache lookup or a closure-blob refusal that the `Read` and
//! `ActionStore` handlers would answer UNAVAILABLE is UNAVAILABLE here, with the same text.
//!
//! **Reads** go through the same generation check and then the store ([`Storage::read_range`],
//! verified as a whole before the first byte, as the `Read` handler does): the check is the
//! service's, so a stale read is refused here as it is over the wire.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures::{StreamExt as _, stream};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{
    BeginRequest, BeginResponse, CommitRequest, CommitResponse, LocateRequest, LocateResponse,
    PutRangeRequest, PutRangeResponse, ReadRangeRequest, ReadRangeResponse,
};
use kbf_types::Digest;
use tonic::{Request, Response, Status};

use super::client::{lift, write_messages};
use super::{StorageService, catch_up};
use crate::{Answer, PutStream, ReadStream, Storage, StorageError};

/// A node's [`StorageService`], called in-process ([`StorageService::in_process`]). Cheap
/// to clone.
#[derive(Clone)]
pub struct InProcess {
    service: Arc<StorageService>,
    uploads: Arc<AtomicU64>,
}

impl InProcess {
    pub(super) fn new(service: Arc<StorageService>) -> Self {
        Self {
            service,
            uploads: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The node as a [`Storage`] whose every call is judged by map `generation` (the one
    /// the caller routed by; 0: no map seen).
    #[must_use]
    pub fn at(&self, generation: u64) -> Arc<dyn Storage> {
        Arc::new(Called {
            this: self.clone(),
            generation,
        })
    }
}

/// `error` of a read of `digest`, as the `Read` handler would answer it while the node is
/// catching up ([`catch_up::read_miss`]).
fn read_error(catching_up: bool, error: StorageError, digest: &Digest) -> StorageError {
    match catch_up::read_miss(catching_up, &error, digest) {
        Some(status) => lift(&status, None),
        None => error,
    }
}

/// `error` of an action-cache write, as the `ActionStore` handlers would answer it while the
/// node is catching up ([`catch_up::retryable`]).
fn write_error(catching_up: bool, error: StorageError) -> StorageError {
    let status = match &error {
        StorageError::ActionWrite(refused) => catch_up::retryable(catching_up, refused),
        _ => None,
    };
    status.map_or(error, |status| lift(&status, None))
}

/// The node, for one caller routed by `generation`.
struct Called {
    this: InProcess,
    generation: u64,
}

impl Called {
    /// Whether a call routed by this generation may be served now.
    async fn judged(&self) -> Result<&StorageService, StorageError> {
        self.this.service.check_map(self.generation).await?;
        Ok(&self.this.service)
    }

    /// [`Storage::put`], or [`Storage::put_unowned`] if `unowned`: hashed here and run
    /// through the service's write, the same messages a [`super::RemoteStorage`] sends.
    fn put_as(&self, bytes: Bytes, unowned: bool) -> Answer<'_, Digest> {
        Box::pin(async move {
            let digest = kbf_segments::sha256(&bytes);
            let upload = self.this.uploads.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "uploads/in-process-{upload}/blobs/{}/{}",
                digest.hash_hex(),
                digest.size_bytes
            );
            let messages = stream::iter(write_messages(name, bytes).map(Ok)).boxed();
            self.this
                .service
                .write_in(self.generation, None, unowned, messages)
                .await
                .map_err(|status| lift(&status, None))?;
            Ok(digest)
        })
    }
}

impl Storage for Called {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async move {
            let service = self.judged().await?;
            let catching_up = service.catching_up();
            let missing = service.storage.find_missing(digests).await?;
            match catch_up::missing_gap(catching_up, missing.len(), digests.len()) {
                Some(status) => Err(lift(&status, None)),
                None => Ok(missing),
            }
        })
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async move {
            let service = self.judged().await?;
            let catching_up = service.catching_up();
            let read = service.storage.read_blob(digest).await;
            read.map_err(|e| read_error(catching_up, e, digest))
        })
    }

    fn read_range<'a>(
        &'a self,
        digest: &'a Digest,
        offset: u64,
        limit: u64,
    ) -> Answer<'a, ReadStream> {
        Box::pin(async move {
            let service = self.judged().await?;
            let catching_up = service.catching_up();
            let read = service.storage.read_range(digest, offset, limit).await;
            read.map_err(|e| read_error(catching_up, e, digest))
        })
    }

    /// Run through the service's write ([`Called::put_as`]).
    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.put_as(bytes, false)
    }

    fn put_unowned(&self, bytes: Bytes) -> Answer<'_, Digest> {
        self.put_as(bytes, true)
    }
    /// Always `None`, as a [`super::RemoteStorage`]'s: a streamed write is
    /// [`Storage::put`]'s messages.
    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    fn segments(&self) -> Arc<dyn Segments> {
        Arc::new(Judged {
            this: self.this.clone(),
            generation: self.generation,
        })
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        Box::pin(async move {
            let service = self.judged().await?;
            let catching_up = service.catching_up();
            let entry = service.storage.action_result(action).await?;
            match catch_up::entry_miss(catching_up, entry.is_some(), action) {
                Some(status) => Err(lift(&status, None)),
                None => Ok(entry),
            }
        })
    }

    fn prepare_action_result<'a>(&'a self, result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        Box::pin(async move {
            let service = self.judged().await?;
            let catching_up = service.catching_up();
            let record = service.storage.prepare_action_result(result).await;
            record.map_err(|e| write_error(catching_up, e))
        })
    }

    fn commit_action_record(
        &self,
        role: Role,
        action: Digest,
        record: ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async move {
            let service = self.judged().await?;
            let catching_up = service.catching_up();
            let committed = service
                .storage
                .commit_action_record(role, action, record)
                .await;
            committed.map_err(|e| write_error(catching_up, e))
        })
    }
}

/// The node's `Segments` handlers for one caller routed by a generation: the store's own,
/// after the service's check.
struct Judged {
    this: InProcess,
    generation: u64,
}

#[tonic::async_trait]
impl Segments for Judged {
    async fn begin(
        &self,
        request: Request<BeginRequest>,
    ) -> Result<Response<BeginResponse>, Status> {
        self.this.service.check_map(self.generation).await?;
        self.this.service.segments.begin(request).await
    }

    async fn put_range(
        &self,
        request: Request<PutRangeRequest>,
    ) -> Result<Response<PutRangeResponse>, Status> {
        self.this.service.check_map(self.generation).await?;
        self.this.service.segments.put_range(request).await
    }

    async fn commit(
        &self,
        request: Request<CommitRequest>,
    ) -> Result<Response<CommitResponse>, Status> {
        self.this.service.check_map(self.generation).await?;
        self.this.service.segments.commit(request).await
    }

    async fn locate(
        &self,
        request: Request<LocateRequest>,
    ) -> Result<Response<LocateResponse>, Status> {
        self.this.service.check_map(self.generation).await?;
        self.this.service.segments.locate(request).await
    }

    async fn read_range(
        &self,
        request: Request<ReadRangeRequest>,
    ) -> Result<Response<ReadRangeResponse>, Status> {
        self.this.service.check_map(self.generation).await?;
        self.this.service.segments.read_range(request).await
    }
}
