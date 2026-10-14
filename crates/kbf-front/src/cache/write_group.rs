//! Write grouping (`docs/design/write-path.md`, section 3): concurrent uploads share
//! one segment, one PUT and one `PutBlobs` commit.
//!
//! A [`Group`] holds one **open segment** and the callers waiting on it. Each blob a
//! caller stores (not already present) is pushed into the open segment with a one-shot
//! reply channel; a digest already in it is a no-op push, and both callers wait on that
//! segment. The open segment is **sealed** when the next blob does not fit, or when its
//! oldest blob has waited [`WriteConfig::linger`]. A sealed segment is written by a
//! task of its own: a PUT (at most [`WriteConfig::puts_in_flight`] at once, each tried
//! up to [`WriteConfig::put_attempts`] times under a new key), the footer read back,
//! one `PutBlobs` listing every blob in it, and only then every caller waiting on it is
//! answered. A caller that goes away changes nothing: its blobs are written and
//! indexed with the segment.
//!
//! A blob of [`WriteConfig::own_segment_bytes`] or more (or one too large to share a
//! segment) is written as a segment of its own, past the group, so it neither waits for
//! the linger nor holds up small writes.
//!
//! [`Cache::admit`] bounds the bytes uploads hold: a ByteStream `Write` is admitted for
//! its blob's size before it buffers anything past its first message, a
//! `BatchUpdateBlobs` for its total, and the admission travels with the blobs into
//! their segments, so it is released only once they are written (or have failed), even
//! if the caller has gone.

use std::collections::BTreeSet;
use std::mem;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kbf_meta::{Applied, Command, Epoch, Location, MetaState, ObjectId, StoreId};
use kbf_objstore::{ByteRange, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_segments::layout::{TRAILER_LEN, footer_len};
use kbf_segments::{Footer, SegmentWriter, WriteError};
use kbf_types::Digest;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use super::tier::Staging;
use super::{Cache, CacheError, VerifiedBlob, refuse_oversized, unexpected};
use crate::meta_log::{MetaLog, MetaLogError};
use crate::metrics::{CacheMetrics, command_label};

/// The wait before the first retry of a failed segment PUT. Each later wait is
/// [`PUT_BACKOFF_FACTOR`] times the one before: 100 ms, then 400 ms.
pub const PUT_BACKOFF: Duration = Duration::from_millis(100);

/// How much longer each wait between PUT attempts is than the one before.
pub const PUT_BACKOFF_FACTOR: u32 = 4;

/// The most bytes one [`Cache::admit`] call takes and counts: a semaphore hands out at
/// most `u32::MAX` permits at once. A caller asking for more holds more memory than the
/// bound counts, so `kbf-server` refuses a `--segment-upload-max-bytes` over this.
pub const MAX_ADMISSION_BYTES: u64 = u32::MAX as u64;

/// The most bytes batch reads hold at once ([`Cache::admit_read`]): a read of `n` bytes
/// of blobs holds the blobs, the response's copy of them and the encoded message, so it
/// counts [`READ_COPIES`] times `n`, until its response has been sent. At 1 GiB per
/// batch that is two full batches at a time.
pub const READ_BYTES_IN_FLIGHT: u64 = 6 << 30;

/// How many copies of a batch read's data the server holds while answering it.
pub const READ_COPIES: u64 = 3;

/// The most bytes of gRPC messages the server's listeners hold decoded or being
/// decoded for one call each at once (`kbf-server`'s in-flight layer, which counts the
/// length a message's own prefix declares): without it, [`MAX_CONCURRENT_STREAMS`]
/// calls of [`MAX_MESSAGE_BYTES`](crate::MAX_MESSAGE_BYTES) each could make the server
/// hold that many gigabytes before [`Cache::admit`] sees any. A call holds its share
/// until its response is sent.
pub const MESSAGE_BYTES_IN_FLIGHT: u64 = 4 << 30;

/// The most HTTP/2 streams one connection may have open on either of the server's
/// listeners. A `BatchUpdateBlobs` request is decoded (up to
/// [`MAX_MESSAGE_BYTES`](crate::MAX_MESSAGE_BYTES)) before [`Cache::admit`] sees it,
/// and so is a ByteStream `Write`'s first message; [`MESSAGE_BYTES_IN_FLIGHT`] bounds
/// the sum of those messages across all streams and connections (the server's
/// in-flight layer), so what is held outside the admission bound is at most that, plus
/// this many [`STREAM_WINDOW_BYTES`] of data received but not yet read.
pub const MAX_CONCURRENT_STREAMS: u32 = 200;

/// The HTTP/2 flow-control window of each stream on either listener: how much a
/// client may send on a stream before the server reads it.
pub const STREAM_WINDOW_BYTES: u32 = 1 << 20;

/// The HTTP/2 flow-control window of each connection on either listener: every
/// stream's window at once. A `Write` waiting for admission is not read, so its unread
/// data holds part of the connection's window; with a smaller window (the transport's
/// default is 1 MiB), the waiting streams of one connection can take all of it, and
/// the admitted streams, which must finish before anyone else is admitted, never
/// receive another byte.
pub const CONNECTION_WINDOW_BYTES: u32 = MAX_CONCURRENT_STREAMS * STREAM_WINDOW_BYTES;

/// How the cache groups and bounds uploads (`docs/design/write-path.md`, section 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteConfig {
    /// How long the oldest blob of an open segment waits for others before the segment
    /// is sealed and written (`--write-linger`).
    pub linger: Duration,
    /// Blobs of this many bytes or more are written as a segment of their own, past
    /// the group (`--write-own-segment-bytes`).
    pub own_segment_bytes: u64,
    /// The most bytes admitted uploads hold at once ([`Cache::admit`],
    /// `--write-bytes-in-flight`). A caller over the bound waits; a caller asking for
    /// more than the whole bound waits for all of it. `kbf-server` refuses a bound
    /// below [`MAX_BLOB_BYTES`](crate::MAX_BLOB_BYTES).
    pub bytes_in_flight: u64,
    /// The most segment PUTs, each with its footer read-back, in flight at once
    /// (`--write-puts-in-flight`). A segment sealed while all are busy waits.
    pub puts_in_flight: NonZeroUsize,
    /// How many times a segment PUT is tried (`--store-put-attempts`). Only a PUT that
    /// did not reach the store or a 5xx answer is tried again, after [`PUT_BACKOFF`]
    /// (growing by [`PUT_BACKOFF_FACTOR`]), each time under a new object id.
    pub put_attempts: NonZeroU32,
}

impl WriteConfig {
    /// The defaults of `docs/design/write-path.md`: a 2 ms linger, own segments from
    /// 16 MiB, 2 GiB admitted, 4 PUTs in flight, 3 attempts per PUT.
    pub const DEFAULT: Self = Self {
        linger: Duration::from_millis(2),
        own_segment_bytes: 16 << 20,
        bytes_in_flight: 2 << 30,
        puts_in_flight: NonZeroUsize::MIN.saturating_add(3),
        put_attempts: NonZeroU32::MIN.saturating_add(2),
    };
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Bytes admitted under [`WriteConfig::bytes_in_flight`] by [`Cache::admit`], released
/// when the last clone is dropped. The cache keeps a clone with each blob it is
/// writing, so the bytes stay counted until their segment is settled. The default
/// admits nothing.
#[derive(Clone, Debug, Default)]
pub struct Admitted {
    /// Held for its drop, which releases the bytes.
    _permit: Option<Arc<OwnedSemaphorePermit>>,
}

/// Bytes admitted for a batch read ([`Cache::admit_read`]), freed when the last clone
/// is dropped. Put in a response's extensions, it is held until the response body has
/// been sent: `kbf-server`'s in-flight layer moves it onto the body.
#[derive(Clone, Debug)]
pub struct ResponseHold {
    /// Held for its drop, which releases the bytes.
    _permit: Arc<Option<OwnedSemaphorePermit>>,
}

/// The open segment, the bounds, and the ByteStream buffer gauge of one cache.
#[derive(Debug)]
pub(super) struct Group {
    config: WriteConfig,
    /// What [`Cache::admit`] takes, in bytes; the bound clamped to what a semaphore holds.
    admission: Arc<Semaphore>,
    bound: u64,
    /// What [`Cache::admit_read`] takes.
    read_admission: Arc<Semaphore>,
    puts: Semaphore,
    state: Mutex<State>,
    buffered: AtomicU64,
    buffered_peak: AtomicU64,
}

#[derive(Debug, Default)]
struct State {
    open: Option<Open>,
    next: u64,
}

/// The segment blobs are being pushed into, and its id within this group.
#[derive(Debug)]
struct Open {
    id: u64,
    segment: Segment,
}

/// A segment and everyone waiting on it: one reply channel and one admission per push.
#[derive(Debug)]
struct Segment {
    writer: SegmentWriter,
    packed: BTreeSet<Digest>,
    waiters: Vec<oneshot::Sender<Result<(), CacheError>>>,
    held: Vec<Admitted>,
    /// A segment of one blob, past the group: on the write tier, a block of its own.
    own: bool,
}

impl Segment {
    fn new(limit: u64, own: bool) -> Self {
        Self {
            writer: SegmentWriter::new(limit),
            packed: BTreeSet::new(),
            waiters: Vec::new(),
            held: Vec::new(),
            own,
        }
    }

    /// Records that `digest` was pushed, for `waiter`, under `admitted`.
    fn wait(
        &mut self,
        digest: Digest,
        waiter: oneshot::Sender<Result<(), CacheError>>,
        admitted: &Admitted,
    ) {
        self.packed.insert(digest);
        self.waiters.push(waiter);
        self.held.push(admitted.clone());
    }
}

impl Group {
    pub(super) fn new(config: WriteConfig) -> Self {
        let bound = config.bytes_in_flight.min(Semaphore::MAX_PERMITS as u64);
        Self {
            config,
            // `bound` is at most `MAX_PERMITS`, a `usize`.
            admission: Arc::new(Semaphore::new(bound as usize)),
            bound,
            read_admission: Arc::new(Semaphore::new(READ_BYTES_IN_FLIGHT as usize)),
            puts: Semaphore::new(config.puts_in_flight.get()),
            state: Mutex::new(State::default()),
            buffered: AtomicU64::new(0),
            buffered_peak: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Bytes a ByteStream `Write` holds while it receives a blob, counted toward
/// [`Cache::write_buffer_peak`] until dropped.
#[derive(Debug)]
pub(crate) struct Buffering<'a> {
    group: &'a Group,
    held: u64,
}

impl Buffering<'_> {
    /// Counts `n` more bytes held.
    pub(crate) fn add(&mut self, n: u64) {
        self.held += n;
        let now = self.group.buffered.fetch_add(n, Ordering::Relaxed) + n;
        self.group.buffered_peak.fetch_max(now, Ordering::Relaxed);
    }
}

impl Drop for Buffering<'_> {
    fn drop(&mut self) {
        self.group.buffered.fetch_sub(self.held, Ordering::Relaxed);
    }
}

/// What a blob of a store call is waiting for.
enum Answer {
    /// Already present: touched, not written.
    Ready,
    /// In a segment being written.
    Waiting(oneshot::Receiver<Result<(), CacheError>>),
}

/// The store side of the cache: the metadata log, the bucket, how objects are named,
/// and the cache's metrics. Shared with the tasks that write segments, so a segment
/// written after its callers have gone is still timed.
#[derive(Debug)]
pub(super) struct Writer<M, O> {
    pub(super) meta: M,
    pub(super) objects: O,
    pub(super) prefix: KeyPrefix,
    pub(super) epoch: Epoch,
    pub(super) metrics: CacheMetrics,
    /// Shared with the write tier, which numbers its blocks' objects from it too.
    pub(super) next_seq: Arc<AtomicU64>,
    /// The local write tier, once [`Cache::attach_tier`] attached one.
    pub(super) tier: OnceLock<Staging>,
}

impl<M, O> Writer<M, O> {
    pub(super) fn new(meta: M, objects: O, prefix: KeyPrefix, epoch: Epoch) -> Self {
        Self {
            meta,
            objects,
            prefix,
            epoch,
            metrics: CacheMetrics::default(),
            next_seq: Arc::new(AtomicU64::new(1)),
            tier: OnceLock::new(),
        }
    }

    /// The key of object `id`: `<prefix>cas/<epoch>/<seq>`, each as 16 hex digits.
    pub(super) fn object_key(&self, id: ObjectId) -> Result<ObjectKey, CacheError> {
        let (epoch, seq) = (id.epoch().get(), id.seq());
        Ok(self.prefix.key(&format!("cas/{epoch:016x}/{seq:016x}"))?)
    }

    fn allocate(&self) -> ObjectId {
        ObjectId::new(self.epoch, self.next_seq.fetch_add(1, Ordering::Relaxed))
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// The same cache, grouping and bounding uploads as `config` says instead of
    /// [`WriteConfig::DEFAULT`].
    #[must_use]
    pub fn with_writes(mut self, config: WriteConfig) -> Self {
        self.group = Arc::new(Group::new(config));
        self
    }

    /// Waits until `bytes` more fit under [`WriteConfig::bytes_in_flight`] and takes
    /// them. A call asking for more than the bound, or than [`MAX_ADMISSION_BYTES`],
    /// takes and counts only that much. Nothing is refused: a caller over the bound waits.
    pub async fn admit(&self, bytes: u64) -> Admitted {
        let n = bytes.min(self.group.bound).min(MAX_ADMISSION_BYTES);
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        let permit = Arc::clone(&self.group.admission)
            .acquire_many_owned(n)
            .await
            .ok();
        Admitted {
            _permit: permit.map(Arc::new),
        }
    }

    /// Waits until a batch read of `bytes` of blobs fits under [`READ_BYTES_IN_FLIGHT`]
    /// ([`READ_COPIES`] times `bytes`, at most what one semaphore call takes) and takes
    /// it. The returned hold frees the bytes when its last clone is dropped, which the
    /// caller hands to the response ([`ResponseHold`]) so it lasts until the message is
    /// sent.
    pub async fn admit_read(&self, bytes: u64) -> ResponseHold {
        let n = bytes
            .saturating_mul(READ_COPIES)
            .min(READ_BYTES_IN_FLIGHT)
            .min(u64::from(u32::MAX));
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        let permit = Arc::clone(&self.group.read_admission)
            .acquire_many_owned(n)
            .await
            .ok();
        ResponseHold {
            _permit: Arc::new(permit),
        }
    }

    /// The bytes [`Cache::admit_read`] has handed out and not had back.
    #[must_use]
    pub fn read_bytes_admitted(&self) -> u64 {
        READ_BYTES_IN_FLIGHT - self.group.read_admission.available_permits() as u64
    }

    /// The bytes [`Cache::admit`] has handed out and not yet had back: what uploads
    /// hold under [`WriteConfig::bytes_in_flight`] now.
    #[must_use]
    pub fn bytes_admitted(&self) -> u64 {
        self.group.bound - self.group.admission.available_permits() as u64
    }

    /// The most bytes ByteStream `Write`s have held in their receive buffers at once
    /// since this cache was made: from the first byte past the first message until the
    /// whole blob has arrived.
    pub fn write_buffer_peak(&self) -> u64 {
        self.group.buffered_peak.load(Ordering::Relaxed)
    }

    /// A gauge of the bytes one ByteStream `Write` buffers.
    pub(crate) fn buffering(&self) -> Buffering<'_> {
        Buffering {
            group: &self.group,
            held: 0,
        }
    }

    /// Stores verified blobs, as [`Cache::store_each`] does, and fails if any blob
    /// failed (with the first such blob's error).
    ///
    /// # Errors
    /// As [`Cache::store_each`], or the first failed blob's error.
    pub async fn store_admitted(
        &self,
        blobs: Vec<VerifiedBlob>,
        admitted: Admitted,
    ) -> Result<(), CacheError> {
        self.store_each(blobs, admitted)
            .await?
            .into_iter()
            .collect()
    }

    /// Stores verified blobs and answers for each, in order. Blobs already present are
    /// touched, not stored again; the rest are written with whatever other callers are
    /// writing (module docs), and each is answered once its segment's `PutBlobs` has
    /// committed, or failed. A blob is never answered `Ok` before it is durable.
    /// `admitted` is held until every blob's segment is settled.
    ///
    /// # Errors
    /// [`CacheError::Invalid`], before anything is stored, if any blob is larger than
    /// [`MAX_BLOB_BYTES`](crate::MAX_BLOB_BYTES); the metadata log could not say which
    /// blobs are present. A blob whose segment failed has that failure as its answer:
    /// UNAVAILABLE when the store or the log could not be reached, INTERNAL when the
    /// store kept something other than what was written.
    pub async fn store_each(
        &self,
        blobs: Vec<VerifiedBlob>,
        admitted: Admitted,
    ) -> Result<Vec<Result<(), CacheError>>, CacheError> {
        refuse_oversized(&blobs)?;
        let started = Instant::now();
        let stored = self.store_fresh(blobs, admitted).await;
        self.writer.metrics.store_blobs.since(started);
        stored
    }

    /// [`Cache::store_each`] of blobs no larger than the limit.
    async fn store_fresh(
        &self,
        blobs: Vec<VerifiedBlob>,
        admitted: Admitted,
    ) -> Result<Vec<Result<(), CacheError>>, CacheError> {
        let held = self.held(&blobs).await?;
        let answers = self.submit(blobs, &held, &admitted);
        drop(admitted);
        let mut out = Vec::with_capacity(answers.len());
        for answer in answers {
            out.push(match answer {
                Answer::Ready => Ok(()),
                Answer::Waiting(reply) => reply.await.unwrap_or_else(|_| {
                    Err(CacheError::Internal(
                        "the segment's write ended without an answer".to_owned(),
                    ))
                }),
            });
        }
        Ok(out)
    }

    /// Hands each blob not in `held` to its segment: its own, or the open one.
    fn submit(
        &self,
        blobs: Vec<VerifiedBlob>,
        held: &BTreeSet<Digest>,
        admitted: &Admitted,
    ) -> Vec<Answer> {
        let mut guard = self.group.lock();
        let state = &mut *guard;
        blobs
            .into_iter()
            .map(|blob| {
                if held.contains(&blob.digest) {
                    return Answer::Ready;
                }
                let (waiter, reply) = oneshot::channel();
                if blob.digest.size_bytes >= self.group.config.own_segment_bytes {
                    self.write_own(&blob, waiter, admitted);
                } else {
                    self.join(state, &blob, waiter, admitted);
                }
                Answer::Waiting(reply)
            })
            .collect()
    }

    /// Pushes `blob` into the open segment (opening one if there is none), sealing the
    /// open segment first if `blob` does not fit in it.
    fn join(
        &self,
        state: &mut State,
        blob: &VerifiedBlob,
        waiter: oneshot::Sender<Result<(), CacheError>>,
        admitted: &Admitted,
    ) {
        loop {
            let open = state
                .open
                .get_or_insert_with(|| self.open_segment(&mut state.next));
            match open.segment.writer.push(&blob.bytes) {
                Ok(_) => return open.segment.wait(blob.digest, waiter, admitted),
                Err(WriteError::Full { .. }) => {
                    let fresh = self.open_segment(&mut state.next);
                    let full = mem::replace(open, fresh);
                    tokio::spawn(settle(
                        Arc::clone(&self.group),
                        Arc::clone(&self.writer),
                        full.segment,
                    ));
                }
                // Too large for any segment of this size: a segment of its own.
                Err(_) => return self.write_own(blob, waiter, admitted),
            }
        }
    }

    /// A new open segment, and the task that seals it at the linger.
    fn open_segment(&self, next: &mut u64) -> Open {
        let id = *next;
        *next += 1;
        let (group, writer) = (Arc::clone(&self.group), Arc::clone(&self.writer));
        tokio::spawn(async move {
            tokio::time::sleep(group.config.linger).await;
            let sealed = group.lock().open.take_if(|open| open.id == id);
            if let Some(open) = sealed {
                settle(group, writer, open.segment).await;
            }
        });
        Open {
            id,
            segment: Segment::new(self.segment_limit, false),
        }
    }

    /// Writes `blob` as a segment of one record, past the group.
    fn write_own(
        &self,
        blob: &VerifiedBlob,
        waiter: oneshot::Sender<Result<(), CacheError>>,
        admitted: &Admitted,
    ) {
        let size = blob.digest.size_bytes.saturating_add(footer_len(1));
        let mut segment = Segment::new(size, true);
        // An empty segment sized for this record always takes it.
        let _ = segment.writer.push(&blob.bytes);
        segment.wait(blob.digest, waiter, admitted);
        let (group, writer) = (Arc::clone(&self.group), Arc::clone(&self.writer));
        tokio::spawn(settle(group, writer, segment));
    }
}

/// Writes a sealed segment, commits it, and answers everyone waiting on it.
async fn settle<M: MetaLog, O: ObjectStore>(
    group: Arc<Group>,
    writer: Arc<Writer<M, O>>,
    segment: Segment,
) {
    let Segment {
        writer: records,
        packed,
        waiters,
        held,
        own,
    } = segment;
    if packed.is_empty() {
        // The open segment a blob too large for it left behind: nothing to write.
        return;
    }
    let body = Bytes::from(records.finish());
    // On the write tier when one is attached and takes it, else PUT.
    let answer = match writer.stage(body.clone(), own).await {
        Ok(true) => Ok(()),
        Ok(false) => match writer.put_segment(&group, body, &packed).await {
            Ok(placed) => writer.commit_segment(placed).await,
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    for waiter in waiters {
        let _ = waiter.send(answer.clone());
    }
    drop(held);
}

impl<M: MetaLog, O: ObjectStore> Writer<M, O> {
    /// Commits `command` to the metadata log, timed by command.
    pub(super) async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        let histogram = &self.metrics.commit[command_label(&command)];
        let started = Instant::now();
        let applied = self.meta.commit(command).await;
        histogram.since(started);
        applied
    }

    /// Runs `f` against the metadata log's state, timed.
    pub(super) async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        let started = Instant::now();
        let answer = self.meta.query(f).await;
        self.metrics.query.since(started);
        answer
    }

    /// One range GET of a blob being read, timed. The footer read-back of a segment
    /// write is not a read and does not come here.
    pub(super) async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let started = Instant::now();
        let bytes = self.objects.get_range(key, range).await;
        self.metrics.object_get.since(started);
        bytes
    }

    /// Writes one segment, reads its footer back from the store, and returns where
    /// each packed blob is, as the stored footer says.
    async fn put_segment(
        &self,
        group: &Group,
        body: Bytes,
        packed: &BTreeSet<Digest>,
    ) -> Result<Vec<(Digest, Location)>, CacheError> {
        let (segment, key, footer) = self.store_segment(group, body, packed.len()).await?;
        packed
            .iter()
            .map(|d| {
                let entry = footer.find(d).ok_or_else(|| {
                    CacheError::Internal(format!("segment {key} came back without blob {d}"))
                })?;
                Ok((
                    *d,
                    Location {
                        store: StoreId::CONFIGURED,
                        object: segment,
                        offset: entry.offset,
                    },
                ))
            })
            .collect()
    }

    /// Stores one whole segment of `records` records: under one of `group`'s
    /// [`WriteConfig::puts_in_flight`] slots, a PUT tried as [`Writer::put`] says, then
    /// its footer read back. Returns the object, its key and the footer the store holds.
    /// Grouped segments and the segments of a `Segments` upload are both stored here.
    ///
    /// # Errors
    /// Every PUT attempt failed; the read-back failed, INTERNAL when it reached the
    /// store and disagrees with what was written.
    pub(super) async fn store_segment(
        &self,
        group: &Group,
        body: Bytes,
        records: usize,
    ) -> Result<(ObjectId, ObjectKey, Footer), CacheError> {
        let len = body.len() as u64;
        self.metrics.segment_blobs.observe(records as u64);
        self.metrics.segment_bytes.observe(len);
        let _slot = group.puts.acquire().await;
        let (object, key) = self.put(group.config.put_attempts, body).await?;
        let started = Instant::now();
        let footer = self.read_footer(&key, len).await;
        self.metrics.segment_footer.since(started);
        Ok((object, key, footer.map_err(read_back)?))
    }

    /// PUTs `body` under a new object id, and again under another after a failure that
    /// did not reach the store or a 5xx answer, up to `attempts` times in all. A retry
    /// never reuses a key: a first PUT the store kept but reported failed cannot fail
    /// the retry as `AlreadyExists`, and is left an orphan.
    async fn put(
        &self,
        attempts: NonZeroU32,
        body: Bytes,
    ) -> Result<(ObjectId, ObjectKey), CacheError> {
        let mut backoff = PUT_BACKOFF;
        let mut attempt = 1;
        loop {
            let object = self.allocate();
            let key = self.object_key(object)?;
            let started = Instant::now();
            let put = self.objects.put_new(&key, body.clone(), None).await;
            self.metrics.segment_put.since(started);
            match put {
                Ok(()) => return Ok((object, key)),
                Err(e) if attempt < attempts.get() && transient(&e) => {
                    tracing::warn!(%key, %e, attempt, "segment PUT failed; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(PUT_BACKOFF_FACTOR);
                    attempt += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub(super) async fn read_footer(
        &self,
        key: &ObjectKey,
        len: u64,
    ) -> Result<Footer, CacheError> {
        let tail = |n: u64| {
            len.checked_sub(n)
                .and_then(|start| ByteRange::new(start, n))
                .ok_or_else(|| CacheError::Internal(format!("segment {key} is {len} bytes")))
        };
        let trailer = self
            .objects
            .get_range(key, tail(TRAILER_LEN as u64)?)
            .await?;
        let footer_len = Footer::len_from_trailer(&trailer)?;
        let bytes = self.objects.get_range(key, tail(footer_len)?).await?;
        Ok(Footer::parse(&bytes, len)?)
    }

    /// Commits where each blob of one or more segments is, as one `PutBlobs`.
    pub(super) async fn commit_segment(
        &self,
        placed: Vec<(Digest, Location)>,
    ) -> Result<(), CacheError> {
        let started = Instant::now();
        let committed = self.commit(Command::PutBlobs(placed)).await;
        self.metrics.segment_commit.since(started);
        match committed? {
            Applied::Blobs(Ok(_)) => Ok(()),
            Applied::Blobs(Err(e)) => Err(CacheError::Internal(e.to_string())),
            other => Err(unexpected(&other)),
        }
    }
}

/// Whether a failed PUT may be tried again: it did not complete (connect, reset,
/// timeout), or the store answered 5xx (`SlowDown`, `InternalError`, ...).
const fn transient(e: &ObjectStoreError) -> bool {
    matches!(
        e,
        ObjectStoreError::Transport(_)
            | ObjectStoreError::Service {
                status: 500..=599,
                ..
            }
    )
}

/// A footer read-back that reached the store but disagrees with what was written (no
/// such object, a short one, or a trailer or footer that does not parse) is INTERNAL: a
/// store that acknowledges a PUT and then returns something else is a fault to surface,
/// not a blip to retry. A read-back that did not reach the store stays UNAVAILABLE.
fn read_back(e: CacheError) -> CacheError {
    match e {
        e @ (CacheError::Store(
            ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. },
        )
        | CacheError::Segment(_)) => {
            CacheError::Internal(format!("segment read back does not match: {e}"))
        }
        e => e,
    }
}
