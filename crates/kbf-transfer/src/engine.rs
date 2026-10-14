//! [`Transfers`], one per daemon process, and [`Scope`], a lease's view of it.
//!
//! Every request takes one permit of `--transfer-in-flight` for as long as it is out:
//! a ByteStream read or write, a `PutRange`, or a FindMissing call. The permits are
//! granted in the order they were asked for (`tokio::sync::Semaphore` is FIFO), across
//! every lease, so a lease with thousands of blobs does not starve one with ten past
//! queue order.
//!
//! An upload (a ByteStream write or a `PutRange`) first takes a permit of the uploads'
//! share ([`upload_share`]: all but a quarter of the limit), and only then its request
//! permit. The server holds a write until its blob is durable, which on a slow store
//! is minutes, while a read holds its permit for milliseconds; FIFO alone lets the
//! permits drain to the writes and stay there, so one lease's upload would stop every
//! other lease's reads (issue #641). The quarter outside the share is only ever held by
//! reads and FindMissing calls.
//! A read into memory also holds its blob's size of `--transfer-bytes-in-flight` (a
//! blob larger than the budget waits for all of it and goes alone); a read streamed
//! into a file holds only its messages.
//!
//! A read that fails with UNAVAILABLE or a broken connection is retried, after a
//! backoff, up to `--transfer-retries` times, from the bytes it has already received
//! and hashed (`read_offset`); the connection is made again by the next request. A
//! write starts again from its first byte; one streamed from a file's chunks is not
//! retried, since its chunks are read once.

use std::collections::HashSet;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Semaphore, SemaphorePermit};
use tonic::Status;
use tonic::transport::{Channel, Endpoint};

use crate::cas::{Cas, CasError, Chunks, digest_of, label};
use crate::config::TransferConfig;
use crate::find::Finder;
use crate::flights::{Flights, Outcome, Publish};
use crate::spool::Spool;
use crate::stats::{Counts, EngineStats, TransferStats, UploadStats, add};
use crate::wire::{Wire, WriteFailed, call_error, committed, retryable};

/// The shared transfer engine of one daemon process
/// (`docs/design/worker-cache.md#4-the-shared-transfer-engine`): one connection to the
/// server's CAS, one in-flight limit and one byte budget for every lease. A runtime
/// takes a [`Scope`] per lease ([`Cas::lease`]); the engine itself is a [`Cas`] too,
/// whose transfers count for no lease.
#[derive(Debug)]
pub struct Transfers {
    inner: Arc<Inner>,
    unscoped: Scope,
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub wire: Wire,
    pub config: TransferConfig,
    requests: Semaphore,
    /// The uploads' share of `requests` ([`upload_share`]), taken before it.
    uploads: Semaphore,
    bytes: Semaphore,
    /// The byte budget, as permits.
    bytes_total: usize,
    flights: Flights,
    pub finder: Finder,
    /// Set by the first UNIMPLEMENTED from `Segments`: outputs go blob by blob.
    pub segments_off: AtomicBool,
}

impl Transfers {
    /// The engine over one HTTP/2 connection to `endpoint`, made on first use, with the
    /// stream window `config` names and a connection window eight times that.
    #[must_use]
    pub fn new(endpoint: Endpoint, config: TransferConfig) -> Self {
        let window = config.stream_window_bytes;
        let endpoint = endpoint
            .initial_stream_window_size(window)
            .initial_connection_window_size(window.saturating_mul(8));
        Self::over(endpoint.connect_lazy(), config)
    }

    /// The engine over `channel`, a connection already made (its windows are the
    /// channel's). A limit of 0 is taken as 1.
    #[must_use]
    pub fn over(channel: Channel, mut config: TransferConfig) -> Self {
        config.in_flight = config.in_flight.clamp(1, Semaphore::MAX_PERMITS);
        config.chunk_bytes = config.chunk_bytes.max(1);
        config.find_missing_chunk = config.find_missing_chunk.max(1);
        // `acquire_many` takes a u32.
        let bytes_total = config.bytes_in_flight.clamp(1, u32::MAX as usize);
        let inner = Arc::new(Inner {
            wire: Wire::new(channel, config.chunk_bytes),
            requests: Semaphore::new(config.in_flight),
            uploads: Semaphore::new(upload_share(config.in_flight)),
            bytes: Semaphore::new(bytes_total),
            bytes_total,
            flights: Flights::default(),
            finder: Finder::default(),
            segments_off: AtomicBool::new(false),
            config,
        });
        let unscoped = Scope::new(&inner, None);
        Self { inner, unscoped }
    }

    /// The engine as the lease `lease` sees it: its transfers are counted in the
    /// scope's [`TransferStats`], logged when the scope and every fetch it started are
    /// gone.
    #[must_use]
    pub fn scope(&self, lease: &LeaseId) -> Scope {
        Scope::new(&self.inner, Some(*lease))
    }

    /// The process's transfers now.
    #[must_use]
    pub fn stats(&self) -> EngineStats {
        let inner = &self.inner;
        EngineStats {
            requests_in_flight: inner.config.in_flight - inner.requests.available_permits(),
            bytes_in_flight: inner.bytes_total - inner.bytes.available_permits(),
            fetches: inner.flights.len(),
            find_missing_calls: inner.finder.calls.load(Ordering::Relaxed),
            find_missing_unavailable: inner.finder.unavailable.load(Ordering::Relaxed),
        }
    }
}

/// One lease's view of the engine: a [`Cas`] whose transfers count in its own
/// [`TransferStats`]. Its requests are futures that borrow it, so dropping a lease's
/// work drops its requests: one only queued never starts, and a fetch another caller
/// waits on goes on (`docs/design/worker-cache.md#44-limits-retries-cancellation`).
#[derive(Debug)]
pub struct Scope {
    pub(crate) inner: Arc<Inner>,
    pub(crate) counts: Arc<Counts>,
    /// The labels of digests [`Cas::missing`] answered missing that are not stored yet:
    /// their put does not ask again (`crate::find`).
    answered: Mutex<HashSet<String>>,
}

impl Scope {
    fn new(inner: &Arc<Inner>, lease: Option<LeaseId>) -> Self {
        Self {
            inner: Arc::clone(inner),
            counts: Arc::new(Counts::new(lease)),
            answered: Mutex::default(),
        }
    }

    fn answered(&self) -> MutexGuard<'_, HashSet<String>> {
        // Every change is one insert or one remove.
        self.answered.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What this scope has transferred so far.
    #[must_use]
    pub fn stats(&self) -> TransferStats {
        self.counts.stats()
    }

    /// The blob `digest` names, through the flight every caller of it shares.
    async fn shared(&self, digest: &Digest) -> Outcome {
        let inner = &self.inner;
        let start =
            |publish| Arc::clone(inner).fly(digest.clone(), Arc::clone(&self.counts), publish);
        let (waiter, merged) = inner.flights.join(digest, start);
        if merged {
            add(&self.counts.merged_reads, 1);
        }
        waiter.outcome(digest).await
    }

    /// Whether the server holds the blob, so its upload is skipped: `false` without a
    /// question for a digest [`Cas::missing`] answered missing, the answer used once.
    pub(crate) async fn present(&self, digest: &Digest) -> bool {
        if self.answered().remove(&label(digest)) {
            return false;
        }
        let present = self.inner.present(digest, &self.counts).await;
        if present {
            add(&self.counts.outputs_present, 1);
        }
        present
    }
}

impl Cas for Scope {
    type Lease = Self;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> Self {
        Self::new(&self.inner, Some(*lease))
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        Ok(Arc::unwrap_or_clone(self.shared(digest).await?))
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        let digest = digest_of(&bytes);
        if self.present(&digest).await {
            return Ok(digest);
        }
        self.send(&digest, &bytes).await
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        if self.present(&digest).await {
            return Ok(digest);
        }
        self.send_chunks(&digest, chunks).await
    }

    async fn put_spool(&self, spool: Arc<Spool>) -> Result<(), CasError> {
        self.upload_spool(spool).await
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        let small = u64::try_from(digest.size_bytes)
            .is_ok_and(|n| n <= self.inner.config.memory_blob_bytes);
        let written = if small {
            let bytes = self.shared(digest).await?;
            file.write_all(&bytes).await
        } else {
            let arrived = AtomicU64::new(0);
            self.inner
                .read(digest, &mut Sink::File(file), &self.counts, &arrived)
                .await?;
            Ok(())
        };
        let flushed = match written {
            Ok(()) => file.flush().await,
            Err(e) => Err(e),
        };
        flushed.map_err(|e| CasError::Write(label(digest), e.to_string()))
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        let missing = self.inner.missing(digests, &self.counts).await;
        let present = digests.len() - missing.len();
        add(&self.counts.outputs_present, present as u64);
        self.answered().extend(missing.iter().map(label));
        missing
    }

    fn upload_stats(&self) -> UploadStats {
        self.counts.upload_stats()
    }

    fn in_flight(&self) -> usize {
        self.inner.config.in_flight
    }
}

impl Scope {
    /// Writes `bytes`, the blob `digest` names, with one ByteStream write, retried on
    /// UNAVAILABLE or a broken connection; FindMissing is not asked.
    pub(crate) async fn send(&self, digest: &Digest, bytes: &[u8]) -> Result<Digest, CasError> {
        let inner = &self.inner;
        let mut attempt = 0;
        loop {
            let permit = inner.upload_request().await;
            add(&self.counts.write_calls, 1);
            let written = inner.wire.write(digest, bytes).await;
            drop(permit);
            match written {
                Ok(size) => {
                    add(&self.counts.written_bytes, bytes.len() as u64);
                    add(&self.counts.written_blobs, 1);
                    return committed(digest, size);
                }
                Err(status) => self.retry(digest, &status, &mut attempt).await?,
            }
        }
    }

    /// Writes the blob `digest` names from `chunks` with one ByteStream write, not
    /// retried (the chunks are read once); FindMissing is not asked.
    pub(crate) async fn send_chunks(
        &self,
        digest: &Digest,
        chunks: Chunks,
    ) -> Result<Digest, CasError> {
        let permit = self.inner.upload_request().await;
        add(&self.counts.write_calls, 1);
        let written = self.inner.wire.write_chunks(digest, chunks).await;
        drop(permit);
        match written {
            Ok(size) => {
                add(&self.counts.written_bytes, u64::try_from(size).unwrap_or(0));
                add(&self.counts.written_blobs, 1);
                committed(digest, size)
            }
            Err(WriteFailed::Call(status)) => Err(call_error(digest, &status)),
            Err(WriteFailed::Read(e)) => Err(CasError::Read(label(digest), e.to_string())),
        }
    }

    /// After a failed call: waits out the backoff and returns to retry it, or returns
    /// the call's error once it is not worth another attempt.
    async fn retry(
        &self,
        digest: &Digest,
        status: &Status,
        attempt: &mut u32,
    ) -> Result<(), CasError> {
        if self.inner.retry(status, attempt).await {
            add(&self.counts.retries, 1);
            Ok(())
        } else {
            Err(call_error(digest, status))
        }
    }
}

impl Cas for Transfers {
    type Lease = Scope;

    fn lease(self: &Arc<Self>, lease: &LeaseId) -> Scope {
        self.scope(lease)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.unscoped.get(digest).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.unscoped.put(bytes).await
    }

    async fn put_chunks(&self, digest: Digest, chunks: Chunks) -> Result<Digest, CasError> {
        self.unscoped.put_chunks(digest, chunks).await
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        self.unscoped.read_to(digest, file).await
    }

    async fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        self.unscoped.missing(digests).await
    }

    async fn put_spool(&self, spool: Arc<Spool>) -> Result<(), CasError> {
        self.unscoped.put_spool(spool).await
    }

    fn upload_stats(&self) -> UploadStats {
        self.unscoped.upload_stats()
    }

    fn in_flight(&self) -> usize {
        self.unscoped.in_flight()
    }
}

/// Where a read's bytes go.
enum Sink<'a> {
    Memory(&'a mut Vec<u8>),
    File(&'a mut tokio::fs::File),
}

impl Sink<'_> {
    async fn put(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Memory(bytes) => {
                bytes.extend_from_slice(data);
                Ok(())
            }
            Self::File(file) => file.write_all(data).await,
        }
    }
}

/// Why one attempt at a read stopped.
enum Broke {
    Call(Status),
    Sink(std::io::Error),
    /// The server sent more bytes than the digest says.
    Long,
}

impl Inner {
    /// A request permit. The semaphore is never closed, so it is always granted.
    pub(crate) async fn request(&self) -> Option<SemaphorePermit<'_>> {
        self.requests.acquire().await.ok()
    }

    /// A request permit for an upload: a permit of the uploads' share first, then the
    /// request permit, both held until the returned value is dropped. Neither semaphore
    /// is ever closed, so both are always granted.
    pub(crate) async fn upload_request(&self) -> UploadPermit<'_> {
        let share = self.uploads.acquire().await.ok();
        let request = self.request().await;
        UploadPermit {
            _share: share,
            _request: request,
        }
    }

    /// After a failed call: whether to try again, having waited out the backoff
    /// (`attempt` counts the retries so far).
    pub(crate) async fn retry(&self, status: &Status, attempt: &mut u32) -> bool {
        if !retryable(status) || *attempt >= self.config.retries {
            return false;
        }
        *attempt += 1;
        tokio::time::sleep(backoff(*attempt)).await;
        true
    }

    /// The task of a flight: the read, and then the outcome for every waiter; or, once
    /// every waiter has left, giving up (`crate::flights`).
    async fn fly(self: Arc<Self>, digest: Digest, counts: Arc<Counts>, publish: Publish) {
        let flight = Arc::clone(&publish.flight);
        let mut read = pin!(self.read_to_memory(&digest, &counts, &flight.arrived));
        let outcome = loop {
            tokio::select! {
                outcome = &mut read => break outcome,
                () = flight.deserted() => {
                    if self.flights.give_up(&publish) {
                        return;
                    }
                }
            }
        };
        self.flights.land(publish, outcome);
    }

    /// The blob `digest` names, read into memory within the byte budget.
    async fn read_to_memory(
        &self,
        digest: &Digest,
        counts: &Counts,
        arrived: &AtomicU64,
    ) -> Outcome {
        let size = usize::try_from(digest.size_bytes).unwrap_or(0);
        let want = u32::try_from(size.clamp(1, self.bytes_total)).unwrap_or(u32::MAX);
        // The semaphore is never closed, so the permits are always granted.
        let _budget = self.bytes.acquire_many(want).await.ok();
        let mut bytes = Vec::new();
        self.read(digest, &mut Sink::Memory(&mut bytes), counts, arrived)
            .await?;
        Ok(Arc::new(bytes))
    }

    /// Reads the blob `digest` names into `sink`, resuming a broken read from the
    /// bytes received, and checks it against the digest. `arrived` follows the bytes
    /// received.
    async fn read(
        &self,
        digest: &Digest,
        sink: &mut Sink<'_>,
        counts: &Counts,
        arrived: &AtomicU64,
    ) -> Result<(), CasError> {
        let blob = label(digest);
        let mut progress = Progress {
            at: 0,
            size: u64::try_from(digest.size_bytes).unwrap_or(0),
            hasher: Sha256::new(),
        };
        let mut attempt = 0;
        loop {
            let read = self
                .read_from(digest, &mut progress, sink, counts, arrived)
                .await;
            let status = match read {
                Ok(()) => break,
                Err(Broke::Call(status)) => status,
                Err(Broke::Sink(e)) => return Err(CasError::Write(blob, e.to_string())),
                Err(Broke::Long) => {
                    let size = progress.size;
                    return Err(CasError::Corrupt(blob, format!("more than {size} bytes")));
                }
            };
            if !self.retry(&status, &mut attempt).await {
                return Err(call_error(digest, &status));
            }
            add(&counts.retries, 1);
        }
        let actual = Digest {
            hash: hex::encode(progress.hasher.finalize()),
            size_bytes: progress.at as i64,
        };
        if actual != *digest {
            return Err(CasError::Corrupt(blob, label(&actual)));
        }
        Ok(())
    }

    /// One attempt at a read, from `progress.at` on: each message is hashed, handed to
    /// `sink` and counted, and `progress` moves past it.
    async fn read_from(
        &self,
        digest: &Digest,
        progress: &mut Progress,
        sink: &mut Sink<'_>,
        counts: &Counts,
        arrived: &AtomicU64,
    ) -> Result<(), Broke> {
        let _permit = self.request().await;
        add(&counts.read_calls, 1);
        let offset = i64::try_from(progress.at).unwrap_or(i64::MAX);
        let mut messages = self.wire.read(digest, offset).await.map_err(Broke::Call)?;
        while let Some(message) = messages.message().await.map_err(Broke::Call)? {
            let len = message.data.len() as u64;
            if progress.at + len > progress.size {
                return Err(Broke::Long);
            }
            progress.hasher.update(&message.data);
            sink.put(&message.data).await.map_err(Broke::Sink)?;
            progress.at += len;
            add(&counts.read_bytes, len);
            arrived.store(progress.at, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// The permits of an upload ([`Inner::upload_request`]), released when dropped.
pub(crate) struct UploadPermit<'a> {
    _share: Option<SemaphorePermit<'a>>,
    _request: Option<SemaphorePermit<'a>>,
}

/// How many of `in_flight` request permits uploads may hold at once: all but a quarter
/// (at least one kept for reads and FindMissing calls), or the one permit when that is
/// all there is. 48 of the default 64.
pub(crate) fn upload_share(in_flight: usize) -> usize {
    let kept = (in_flight / 4).max(1).min(in_flight.saturating_sub(1));
    in_flight - kept
}

/// How far a read has got: the bytes received of the blob's `size`, and their hash.
struct Progress {
    at: u64,
    size: u64,
    hasher: Sha256,
}

/// The wait before retry number `attempt` (from 1): 100 ms, doubling, at most 3.2 s.
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(50 << attempt.min(6))
}

#[cfg(test)]
mod tests {
    use super::upload_share;

    /// Catches a share that keeps nothing for reads (the whole limit), or that leaves
    /// uploads no permit when the limit is one or two.
    #[test]
    fn uploads_share_all_but_a_quarter_of_the_limit() {
        let shares: Vec<usize> = [1, 2, 3, 4, 16, 64].map(upload_share).to_vec();
        assert_eq!(shares, [1, 1, 2, 3, 12, 48]);
    }
}
