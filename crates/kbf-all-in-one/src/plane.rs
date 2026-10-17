//! [`LocalDataPlane`]: the daemon's data plane ([`kbf_transfer::Cas`]) as calls into the
//! server half of the same process.
//!
//! Every call runs on the server half's async runtime, through the server's
//! [`LocalStore`]: the same cache and the same `Segments` handlers a remote daemon's calls
//! reach through the worker listener, with no TLS, no protobuf encoding and no socket
//! between. What a remote daemon sends is unchanged; this plane sends nothing.
//!
//! - **Output files, as the walk reads them** ([`Cas::intake`], `crate::intake`): the
//!   container driver's walk hands each file's chunks to the plane as it hashes them, so
//!   each output file (and stdout and stderr) is read once; the plane stores them as
//!   below, from those bytes, in batches while the walk goes on.
//! - **Outputs** ([`Cas::put_records`], and [`Cas::put_spool`] through it): the walk
//!   hashed each file once. The plane asks the cache which digests it lacks, packs those
//!   records into segments and uploads exactly as the transfer engine plans them
//!   ([`kbf_transfer::plan`]), and hands each range of the records' bytes, read once from
//!   where the walk left them, to `Begin`, `PutRange` and `Commit`. Nothing is spooled
//!   and nothing is hashed here: with a write tier the server trusts the digests at the
//!   commit and its flusher hashes every record before the PUT, dropping one that does
//!   not match (#656); without one, `PutRange` hashes each record as its bytes arrive.
//!   An upload holding a record over [`MAX_UNSTAGED_BLOB_BYTES`] that the server has no
//!   write tier room for (`Begin` answers FAILED_PRECONDITION) is stored record by
//!   record instead, as the transfer engine falls back ([`kbf_transfer::put_each`]).
//! - **A whole blob** ([`Cas::put`], and [`Cas::put_chunks`] through it): hashed once,
//!   by the server's [`LocalStore::put`], under the admission a ByteStream `Write` takes.
//!   A blob of [`Cas::put_chunks`] over [`MAX_UNSTAGED_BLOB_BYTES`] is streamed to the
//!   object store as its chunks are read ([`LocalStore::put_streamed`]), as a ByteStream
//!   `Write` of one is: never gathered, never refused for its size.
//! - **Reads** ([`Cas::read_to`], the input cache's fills, and [`Cas::get`]): the
//!   storage's streamed read ([`LocalStore::read_stream`]), pumped on the server half's
//!   runtime a chunk at a time (at most two chunks between the halves, never the blob)
//!   into the file (or, for `get`, the `Vec`) as it arrives, and hashed here. A stream
//!   that runs past the digest's size stops at its first byte past it, answered
//!   [`CasError::Corrupt`] as the transfer engine answers one, so no storage fills the
//!   disk (or memory) with more than the blob asked for. A stream that ends with an
//!   error item is [`CasError::Unavailable`] (the same read may pass later); one that
//!   ends cleanly but short, or with other bytes, is [`CasError::Corrupt`].
//! - **FindMissing** ([`Cas::missing`]): the cache's own answer. A call that fails
//!   answers every digest missing, as the transfer engine's does.
//!
//! A server half that is gone (its runtime shut down) fails each call UNAVAILABLE.
//!
//! **Over any storage.** The plane knows the store only as [`kbf_storage::Storage`]
//! (`LocalStore::over`), so it is the one adapter from the daemon's `Cas` to the
//! storage boundary: `kbf all-in-one` gives it the server's cache, `kbf
//! storage-and-daemon` the storage node's (`crate::storage_and_daemon`), and over a
//! `kbf_storage::service::RemoteStorage` it reads and writes a storage node over gRPC
//! with the same answers (tests/storage_plane.rs runs one suite over both). "The server
//! half" above is whichever runtime the storage's calls run on. Unlike the transfer
//! engine, the plane neither retries a failed call nor resumes a broken read from where
//! it stopped: in-process there is no connection to lose. Before a daemon-only node
//! reads a storage node through it (av2-09), that retry belongs in the plane or in the
//! storage it is given.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use futures::{FutureExt as _, StreamExt as _, TryStreamExt as _, future, stream};
use kbf_front::MAX_UNSTAGED_BLOB_BYTES;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::segments_server::Segments;
use kbf_proto::worker::{BeginRequest, CommitRequest, PutRangeRequest, UploadSegment};
use kbf_segments::MAX_SEGMENT_BYTES;
use kbf_server::LocalStore;
use kbf_storage::{ReadStream, StorageError};
use kbf_transfer::{
    Cas, CasError, Chunks, DEFAULT_SEGMENT_UPLOADS, Intake, MAX_RANGE_BYTES, MAX_UPLOAD_RECORDS,
    Planned, Record, RecordHasher, SEGMENT_UPLOAD_BYTES, label, plan, proto_digest, put_each,
    read_pieces,
};
use kbf_types::{DigestFunction, LeaseId};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};
use tokio::runtime::Handle;
use tokio::sync::mpsc::Sender;
use tonic::{Code, Request, Status};

use crate::intake::Taking;

/// The daemon's data plane as calls into the server half of this process. Clones share
/// the server half's store and this plane's own `Segments` service.
#[derive(Clone)]
pub struct LocalDataPlane {
    store: LocalStore,
    /// This plane's own `Segments` service: its uploads, no one else's.
    segments: Arc<dyn Segments>,
    /// The server half's runtime, which every call runs on.
    server: Handle,
}

impl LocalDataPlane {
    /// The plane over the server's `store`, its calls run on the server half's runtime
    /// `server`.
    #[must_use]
    pub fn new(store: LocalStore, server: Handle) -> Self {
        let segments = store.segments();
        Self {
            store,
            segments,
            server,
        }
    }

    /// The server half's runtime.
    pub(crate) fn server(&self) -> &Handle {
        &self.server
    }

    /// The server half's store.
    pub(crate) fn store(&self) -> &LocalStore {
        &self.store
    }

    /// Runs `call` on the server half's runtime and returns its answer; UNAVAILABLE when
    /// that runtime is gone.
    async fn call<T: Send + 'static>(
        &self,
        call: impl Future<Output = Result<T, Status>> + Send + 'static,
    ) -> Result<T, Status> {
        match self.server.spawn(call).await {
            Ok(answer) => answer,
            Err(gone) => Err(Status::unavailable(format!(
                "the server half did not answer: {gone}"
            ))),
        }
    }

    /// Which of `digests` the cache does not hold, in the order asked.
    async fn absent(
        &self,
        digests: Vec<kbf_types::Digest>,
    ) -> Result<Vec<kbf_types::Digest>, Status> {
        let store = self.store.clone();
        self.call(async move { store.find_missing(&digests).await.map_err(Status::from) })
            .await
    }

    /// The blob `digest` streamed into `into` and flushed (the module docs, **Reads**):
    /// a chunk at a time, stopped at the first byte past the digest's size, hashed. Not
    /// generic, so `get` and `read_to` run one copy of it.
    async fn receive(
        &self,
        digest: &Digest,
        into: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<(), CasError> {
        let blob = label(digest);
        let Some(parsed) = parse(digest) else {
            return Err(CasError::Missing(blob));
        };
        let (chunks, mut arriving) = tokio::sync::mpsc::channel(1);
        let store = self.store.clone();
        let opened = self.call(async move {
            let stream = store.read_stream(&parsed).await.map_err(Status::from)?;
            tokio::spawn(pump(stream, chunks));
            Ok(())
        });
        match opened.await {
            Ok(()) => {}
            Err(status) if status.code() == Code::NotFound => return Err(CasError::Missing(blob)),
            Err(status) => return Err(unavailable(&blob, &status)),
        }
        let size = parsed.size_bytes;
        let mut at = 0_u64;
        let mut hasher = RecordHasher::new();
        // Dropping `arriving` on an early return stops the pump, and the stream with it.
        while let Some(chunk) = arriving.recv().await {
            let chunk = chunk.map_err(|e| unavailable(&blob, &Status::from(e)))?;
            at = at.saturating_add(chunk.len() as u64);
            if at > size {
                return Err(CasError::Corrupt(blob, format!("more than {size} bytes")));
            }
            hasher.update(&chunk);
            let written = into.write_all(&chunk).await;
            written.map_err(|e| CasError::Write(blob.clone(), e.to_string()))?;
        }
        let (read, _) = hasher.finish();
        if read != parsed {
            return Err(CasError::Corrupt(blob, label(&proto_digest(&read))));
        }
        let flushed = into.flush().await;
        flushed.map_err(|e| CasError::Write(blob, e.to_string()))
    }

    /// One upload: `Begin` with the segments' footers, each range of the records'
    /// bytes read from their sources, and `Commit`, which answers once the records are
    /// staged (or stored) and indexed.
    async fn upload(&self, upload: Vec<Planned>) -> Result<(), CasError> {
        let begin = BeginRequest {
            segments: upload
                .iter()
                .map(|s| UploadSegment {
                    length: s.length(),
                    footer: s.footer(),
                })
                .collect(),
        };
        let segments = Arc::clone(&self.segments);
        let begun = self.call(async move { segments.begin(Request::new(begin)).await });
        let id = match begun.await {
            Ok(begun) => begun.into_inner().upload,
            // A record too large to hold in memory and no write tier with room for it:
            // each record by itself, a large one streamed to the store.
            Err(status) if status.code() == Code::FailedPrecondition => {
                let records = upload.into_iter().flat_map(|s| s.records);
                return put_each(self, records.map(|(r, _)| r).collect()).await;
            }
            Err(status) => return Err(not_stored(status)),
        };
        for (index, segment) in upload.iter().enumerate() {
            for (start, end) in segment.ranges(MAX_RANGE_BYTES) {
                let pieces = segment.pieces(start, end);
                let bytes = read_pieces(pieces, end - start).await.map_err(not_read)?;
                let request = PutRangeRequest {
                    upload: id,
                    segment: u32::try_from(index).unwrap_or(u32::MAX),
                    offset: start,
                    bytes,
                };
                let segments = Arc::clone(&self.segments);
                let put = self.call(async move { segments.put_range(Request::new(request)).await });
                put.await.map_err(not_stored)?;
            }
        }
        let segments = Arc::clone(&self.segments);
        let commit = CommitRequest { upload: id };
        let committed = self.call(async move { segments.commit(Request::new(commit)).await });
        committed.await.map(drop).map_err(not_stored)
    }

    /// Streams the blob `digest`, its bytes `chunks`, to the object store on the
    /// server half's runtime, unless the cache holds it: [`Cas::put_chunks`] of a blob
    /// too large to gather.
    async fn put_streamed(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        let blob = label(&digest);
        let Some(parsed) = parse(&digest) else {
            return Err(CasError::Unavailable(
                blob,
                "not a SHA-256 digest".to_owned(),
            ));
        };
        if self.absent(vec![parsed]).await.is_ok_and(|a| a.is_empty()) {
            return Ok(digest);
        }
        // A chunk that could not be read fails the upload as a read, not as the store.
        let unread = Arc::new(Mutex::new(None));
        let noted = Arc::clone(&unread);
        let bytes = chunks
            .map(move |chunk| {
                chunk.map(Bytes::from).inspect_err(|e| {
                    *noted.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
                })
            })
            .boxed();
        let store = self.store.clone();
        let size = parsed.size_bytes;
        let claim = future::ready(Some(parsed)).boxed();
        let upload = self
            .server
            .spawn(async move { store.put_streamed(size, bytes, claim).await });
        let stored = match upload.await {
            Ok(stored) => stored,
            Err(gone) => {
                let why = format!("the server half did not answer: {gone}");
                return Err(CasError::Unavailable(blob, why));
            }
        };
        if let Some(why) = unread.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Err(CasError::Read(blob, why));
        }
        match stored {
            Ok(_) => Ok(digest),
            Err(StorageError::DigestMismatch { actual, .. }) => {
                Err(CasError::Corrupt(blob, label(&proto_digest(&actual))))
            }
            Err(e) => Err(CasError::Unavailable(blob, e.to_string())),
        }
    }
}

/// `digest` as the server's digest; `None` when it is not a SHA-256 digest with a size,
/// which no store holds.
fn parse(digest: &Digest) -> Option<kbf_types::Digest> {
    kbf_types::Digest::parse(DigestFunction::Sha256, &label(digest)).ok()
}

/// Sends each item of `stream` down `chunks`, one at a time, until it ends or the
/// reader goes away.
async fn pump(mut stream: ReadStream, chunks: Sender<Result<Bytes, StorageError>>) {
    while let Some(chunk) = stream.next().await {
        if chunks.send(chunk).await.is_err() {
            return;
        }
    }
}

/// A read the server half could not answer.
fn unavailable(blob: &str, status: &Status) -> CasError {
    CasError::Unavailable(blob.to_owned(), status.message().to_owned())
}

/// An upload of outputs the server refused or did not answer.
fn not_stored(status: Status) -> CasError {
    CasError::Unavailable("outputs".to_owned(), status.message().to_owned())
}

/// Outputs whose bytes could not be read, or are no longer the ones the walk hashed.
fn not_read(e: std::io::Error) -> CasError {
    CasError::Read("outputs".to_owned(), e.to_string())
}

impl Cas for LocalDataPlane {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    /// The whole blob, in memory: [`Cas`] makes this a `Vec`. The daemon calls it only
    /// for what it decodes (the action, its command, the input tree's directories,
    /// bounded by the message size); every input file is a [`Cas::read_to`]. Read as
    /// `read_to` reads, into the `Vec`: never more than the digest's size, hashed here.
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let mut bytes = Vec::new();
        self.receive(digest, &mut bytes).await?;
        Ok(bytes)
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        self.receive(digest, file).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let blob = format!("of {} bytes", bytes.len());
        let store = self.store.clone();
        let stored = self.call(async move { store.put(bytes).await.map_err(Status::from) });
        match stored.await {
            Ok(digest) => Ok(proto_digest(&digest)),
            Err(status) => Err(CasError::Unavailable(blob, status.message().to_owned())),
        }
    }

    /// A blob over [`MAX_UNSTAGED_BLOB_BYTES`] (a log the intake did not take, or a
    /// record of an upload the server sent back blob by blob) is streamed to the object
    /// store as its chunks are read ([`LocalStore::put_streamed`]), never gathered,
    /// unless the cache holds it already; the store checks it against `digest` before
    /// it completes it. A smaller one is gathered and stored by [`Cas::put`].
    async fn put_chunks(&self, digest: Digest, mut chunks: Chunks) -> Result<Digest, CasError> {
        let blob = label(&digest);
        if u64::try_from(digest.size_bytes).unwrap_or(u64::MAX) > MAX_UNSTAGED_BLOB_BYTES {
            return self.put_streamed(digest, chunks).await;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| CasError::Read(blob.clone(), e.to_string()))?;
            bytes.extend_from_slice(&chunk);
        }
        let stored = self.put(bytes).await?;
        if stored != digest {
            return Err(CasError::Corrupt(blob, label(&stored)));
        }
        Ok(stored)
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        // A digest that does not parse is held by no store: ask nothing, as a
        // FindMissing the server refuses answers every digest missing.
        let Some(parsed) = digests.iter().map(parse).collect::<Option<Vec<_>>>() else {
            return digests.to_vec();
        };
        match self.absent(parsed).await {
            Ok(absent) => absent.iter().map(proto_digest).collect(),
            Err(_) => digests.to_vec(),
        }
    }

    async fn put_records(&self, records: Vec<Record>) -> Result<(), CasError> {
        let asked: Vec<kbf_types::Digest> = records.iter().map(|r| r.digest).collect();
        // A FindMissing that fails sends every record, as the transfer engine does.
        let absent: BTreeSet<kbf_types::Digest> = self
            .absent(asked.clone())
            .await
            .unwrap_or(asked)
            .into_iter()
            .collect();
        let missing: Vec<Record> = records
            .into_iter()
            .filter(|r| absent.contains(&r.digest))
            .collect();
        let uploads = plan(
            &missing,
            MAX_SEGMENT_BYTES,
            SEGMENT_UPLOAD_BYTES,
            MAX_UPLOAD_RECORDS,
        );
        // The first failure stops the rest; the uploads committed before it stay.
        stream::iter(uploads)
            .map(|upload| self.upload(upload))
            .buffer_unordered(DEFAULT_SEGMENT_UPLOADS)
            .try_for_each(|()| future::ready(Ok(())))
            .await
    }

    fn intake(&self) -> Option<Arc<dyn Intake>> {
        Some(Arc::new(Taking::new(self.clone())))
    }
}

#[cfg(test)]
#[path = "plane_tests.rs"]
mod tests;
