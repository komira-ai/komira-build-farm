//! A failed run's tails, command rendering and log copies: what the tap hands a small
//! pool of workers instead of offering the run to the writer at once.
//!
//! - **Off the tap, off the acknowledgement path.** The tap runs on the recorder's task
//!   and never waits ([`kbf_front::Tap`]), so it only puts the run on a bounded queue
//!   ([`TailOptions::queue`]) that a pool of [`TailOptions::workers`] tasks drains. A full
//!   queue drops the tails, not the run: the tap offers it to the writer with none, and
//!   counts `kbf_history_dropped_total{stage="tail"}`. A run that did not fail never
//!   comes here: it is offered at once.
//! - **Under the read budget.** Each log is read whole (the cache verifies a blob against
//!   its digest, and has no ranged read), so each read takes its blob's bytes from the
//!   process's [`ReadBudget`], the one the blob route holds its answers to. A read waits
//!   for its share at most [`TailOptions::wait`], then the tail is left out as `busy`: the
//!   writer never takes memory a person's download needs, and never waits long for it. A
//!   log over [`TAIL_READ_BYTES`] is not read (`too_large`). A read that does not end in
//!   [`TailOptions::read_timeout`] is `unreadable`, so a CAS that stops answering cannot
//!   wedge the pool for good.
//! - **Copies, not pins** (ui-data.md Q21). A stream of at most
//!   [`FLOG_STREAM_BYTES`](kbf_history::records::FLOG_STREAM_BYTES) is kept in the bytes
//!   the tail read already holds and, with the ActionResult, written to the run's
//!   failure-log object; a larger one sets `flog_omitted: too_large` and its tail is
//!   still kept. The object is written first and its index row follows
//!   ([`Event::Flog`], offered only after the PUT returned).
//! - **The run follows its copy.** [`Event::Run`], with the failed-run section, is
//!   offered once the copy is written (or has failed: a failed copy loses the log, never
//!   the run). If the run's invocation was closed meanwhile (the recorder went idle on it
//!   while its tails were read) it is closed again, so a late run does not leave it open.
//! - **A stop waits a little.** [`Tails::quiesce`] lets the writer's last step see the
//!   runs still in the pool.
//!
//! Not redacted: no redaction machinery exists yet (ui-data.md Q8), so the tails and the
//! command rendering are stored as the run produced them, argument values and log bytes
//! alike, and the run page says `redacted: false` beside them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_front::Backend;
use kbf_history::records::{
    CommandRendering, FLOG_STREAM_BYTES, FailedRun, FlogOmitted, RunRecord, Tail, TailOmitted,
    WireDigest,
};
use kbf_history::{Event, EventQueue, FlogObject, FlogParts, FlogRef, InvRef, RunId};
use kbf_objstore::ObjectStoreError;
use kbf_proto::reapi::Command;
use kbf_types::{Digest, DigestFunction};
use prost::Message;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, mpsc};
use tokio::task::JoinHandle;

use super::stats::Stats;
use super::tap::OpenSet;
use crate::http_blobs::{ReadBudget, Refused};

/// The largest log a tail is read from: a larger one is `tail_omitted: "too_large"` and
/// is not read at all (ui-data.md 4.2, `invocations-api.md` 4.7).
pub const TAIL_READ_BYTES: u64 = 64 * 1024 * 1024;

/// The largest Command blob decoded into a rendering; a larger one has no rendering.
pub const COMMAND_READ_BYTES: u64 = 1024 * 1024;

/// The largest ActionResult blob copied; a larger one is not, and the run says
/// `flog_omitted: "too_large"`.
pub const RESULT_COPY_BYTES: u64 = 1024 * 1024;

/// The pool's size and waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TailOptions {
    /// Workers reading and copying (at least 1).
    pub workers: usize,
    /// Failed runs queued for the workers at most (at least 1); one past it has no tails.
    pub queue: usize,
    /// How long a read waits for its share of the read budget before the tail is `busy`.
    pub wait: Duration,
    /// How long one blob read or object write may take before it counts as failed.
    pub read_timeout: Duration,
}

impl Default for TailOptions {
    /// Two workers, a queue of 4096, a 250 ms wait for the budget and a minute for a
    /// read.
    fn default() -> Self {
        Self {
            workers: 2,
            queue: 4096,
            wait: Duration::from_millis(250),
            read_timeout: Duration::from_secs(60),
        }
    }
}

/// A blob read that failed, for the log.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BlobReadError(pub String);

/// Where the tail workers read blobs from: the CAS, whole and verified against their
/// digests.
pub trait BlobReads: Send + Sync {
    /// The blob `digest`, verified.
    ///
    /// # Errors
    /// [`BlobReadError`] when the blob is not held or cannot be read.
    fn read<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, BlobReadError>>;
}

impl BlobReads for Backend {
    fn read<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, BlobReadError>> {
        Box::pin(async move {
            self.read_blob(digest)
                .await
                .map_err(|e| BlobReadError(e.to_string()))
        })
    }
}

/// Where failure-log objects are made and written, with the object store's type erased.
pub(super) trait FlogSink: Send + Sync {
    /// The object of `parts` for `run`: CPU work, run where blocking is allowed.
    fn encode(&self, run: RunId, parts: &FlogParts<'_>) -> FlogObject;

    /// Writes `object`.
    fn put(&self, object: FlogObject) -> BoxFuture<'_, Result<FlogRef, ObjectStoreError>>;
}

/// A failed run waiting for its tails.
#[derive(Debug)]
pub(super) struct Job {
    pub(super) inv: InvRef,
    pub(super) record: Box<RunRecord>,
    pub(super) stream_opened_ms: u64,
}

/// What the workers and the tap share.
struct Shared {
    queue: Arc<EventQueue>,
    blobs: Arc<dyn BlobReads>,
    budget: ReadBudget,
    flog: Arc<dyn FlogSink>,
    stats: Arc<Stats>,
    open: Arc<OpenSet>,
    options: TailOptions,
    /// Jobs queued or being handled.
    inflight: AtomicUsize,
    /// Notified when `inflight` reaches zero.
    idle: Notify,
}

/// What the pool is made over.
pub(super) struct Parts {
    pub(super) queue: Arc<EventQueue>,
    pub(super) blobs: Arc<dyn BlobReads>,
    pub(super) budget: ReadBudget,
    pub(super) flog: Arc<dyn FlogSink>,
    pub(super) stats: Arc<Stats>,
    pub(super) open: Arc<OpenSet>,
}

/// The tap's end of the pool: where a failed run is put.
pub(super) struct Tails {
    jobs: mpsc::Sender<Job>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Tails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tails").finish_non_exhaustive()
    }
}

/// The workers' end of the pool, not yet running.
pub(super) struct Workers {
    jobs: Arc<Mutex<mpsc::Receiver<Job>>>,
    shared: Arc<Shared>,
}

/// A pool over `parts`: the tap's end, and the workers to [`Workers::spawn`].
pub(super) fn pool(parts: Parts, options: TailOptions) -> (Tails, Workers) {
    let options = TailOptions {
        workers: options.workers.max(1),
        queue: options.queue.max(1),
        ..options
    };
    let (jobs, rx) = mpsc::channel(options.queue);
    let shared = Arc::new(Shared {
        queue: parts.queue,
        blobs: parts.blobs,
        budget: parts.budget,
        flog: parts.flog,
        stats: parts.stats,
        open: parts.open,
        options,
        inflight: AtomicUsize::new(0),
        idle: Notify::new(),
    });
    let tails = Tails {
        jobs,
        shared: Arc::clone(&shared),
    };
    let workers = Workers {
        jobs: Arc::new(Mutex::new(rx)),
        shared,
    };
    (tails, workers)
}

impl Tails {
    /// Queues `job` for the workers. A full pool gives it back, counted
    /// (`stage="tail"`), for the caller to offer without tails. Never waits.
    pub(super) fn submit(&self, job: Job) -> Result<(), Job> {
        self.shared.inflight.fetch_add(1, Ordering::SeqCst);
        match self.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job)) => {
                self.shared.finished();
                self.shared.stats.tail_dropped();
                Err(job)
            }
        }
    }

    /// Waits until no job is queued or being handled.
    pub(super) async fn quiesce(&self) {
        loop {
            let idle = self.shared.idle.notified();
            tokio::pin!(idle);
            // Registered before the count is read, so a worker that finishes in between
            // is not missed.
            idle.as_mut().enable();
            if self.shared.inflight.load(Ordering::SeqCst) == 0 {
                return;
            }
            idle.await;
        }
    }
}

impl Workers {
    /// Starts the workers on the runtime.
    pub(super) fn spawn(self) -> Vec<JoinHandle<()>> {
        let n = self.shared.options.workers;
        (0..n)
            .map(|_| tokio::spawn(work(Arc::clone(&self.jobs), Arc::clone(&self.shared))))
            .collect()
    }
}

async fn work(jobs: Arc<Mutex<mpsc::Receiver<Job>>>, shared: Arc<Shared>) {
    loop {
        // One worker waits for the next job holding the lock; the rest wait for the lock.
        let job = jobs.lock().await.recv().await;
        let Some(job) = job else {
            return;
        };
        shared.handle(job).await;
        shared.finished();
    }
}

/// A log read: its tail and, when it is to be copied, its bytes.
struct Read {
    tail: Option<Tail>,
    copy: Option<Keep>,
    /// The log was too large to copy (or to read at all).
    too_large: bool,
}

/// Bytes kept for the failure-log object, and the budget they hold.
struct Keep {
    bytes: Bytes,
    _permit: Option<OwnedSemaphorePermit>,
}

/// What fetching one blob gave.
enum Fetched {
    Bytes(Bytes, OwnedSemaphorePermit),
    /// The budget was not free in time.
    Busy,
    /// The blob could not be read, or not in time.
    Unreadable,
}

impl Shared {
    fn finished(&self) {
        if self.inflight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.idle.notify_waiters();
        }
    }

    /// Reads `d` whole under the budget.
    async fn fetch(&self, d: &WireDigest) -> Fetched {
        let Some(digest) = digest_of(d) else {
            return Fetched::Unreadable;
        };
        let permit = match self.budget.acquire(d.size, self.options.wait).await {
            Ok(permit) => permit,
            Err(Refused::Busy) => return Fetched::Busy,
            Err(Refused::TooLarge) => return Fetched::Unreadable,
        };
        let read = tokio::time::timeout(self.options.read_timeout, self.blobs.read(&digest));
        match read.await {
            Ok(Ok(bytes)) => Fetched::Bytes(bytes, permit),
            Ok(Err(e)) => {
                tracing::debug!(error = %e, %digest, "history: a log could not be read");
                Fetched::Unreadable
            }
            Err(_) => {
                tracing::warn!(%digest, "history: a log read timed out");
                Fetched::Unreadable
            }
        }
    }

    /// The tail of the log `d` names, and its bytes if they are to be copied.
    async fn stream(&self, d: Option<&WireDigest>) -> Read {
        let Some(d) = d else {
            return Read {
                tail: None,
                copy: None,
                too_large: false,
            };
        };
        let omitted = |why: TailOmitted| Tail {
            bytes: Vec::new(),
            total_bytes: d.size,
            omitted: why.into(),
        };
        if d.size > TAIL_READ_BYTES {
            return Read {
                tail: Some(omitted(TailOmitted::TooLarge)),
                copy: None,
                too_large: true,
            };
        }
        if d.size == 0 {
            // An empty log has nothing to read, and no bytes to hold the budget for.
            return Read {
                tail: Some(Tail::of(&[])),
                copy: Some(Keep {
                    bytes: Bytes::new(),
                    _permit: None,
                }),
                too_large: false,
            };
        }
        let (bytes, permit) = match self.fetch(d).await {
            Fetched::Bytes(bytes, permit) => (bytes, permit),
            Fetched::Busy => {
                return Read {
                    tail: Some(omitted(TailOmitted::Busy)),
                    copy: None,
                    too_large: false,
                };
            }
            Fetched::Unreadable => {
                return Read {
                    tail: Some(omitted(TailOmitted::Unreadable)),
                    copy: None,
                    too_large: false,
                };
            }
        };
        let tail = Some(Tail::of(&bytes));
        // A log over the copy bound is let go now: only its tail is kept.
        let too_large = d.size > FLOG_STREAM_BYTES;
        let copy = (!too_large).then_some(Keep {
            bytes,
            _permit: Some(permit),
        });
        Read {
            tail,
            copy,
            too_large,
        }
    }

    /// The ActionResult blob `d` names, if it is to be copied; whether it was too large.
    async fn result(&self, d: Option<&WireDigest>) -> (Option<Keep>, bool) {
        let Some(d) = d else {
            return (None, false);
        };
        if d.size > RESULT_COPY_BYTES {
            return (None, true);
        }
        match self.fetch(d).await {
            Fetched::Bytes(bytes, permit) => (
                Some(Keep {
                    bytes,
                    _permit: Some(permit),
                }),
                false,
            ),
            Fetched::Busy | Fetched::Unreadable => (None, false),
        }
    }

    /// The rendering of the Command `d` names; `None` when there is none or it cannot be
    /// read and decoded.
    async fn command(&self, d: Option<&WireDigest>) -> Option<CommandRendering> {
        let d = d.filter(|d| d.size <= COMMAND_READ_BYTES)?;
        let Fetched::Bytes(bytes, _permit) = self.fetch(d).await else {
            return None;
        };
        let command = Command::decode(bytes).ok()?;
        Some(render(command))
    }

    /// Handles one failed run: reads, copies, then offers the run.
    async fn handle(&self, job: Job) {
        let Job {
            inv,
            record,
            stream_opened_ms,
        } = job;
        let command = self.command(record.command.as_ref()).await;
        let out = self.stream(record.stdout.as_ref()).await;
        let err = self.stream(record.stderr.as_ref()).await;
        let (result, result_too_large) = self.result(record.result.as_ref()).await;
        let too_large = out.too_large || err.too_large || result_too_large;
        let flog_omitted = if too_large {
            FlogOmitted::TooLarge
        } else {
            FlogOmitted::No
        };
        let failed = FailedRun {
            seq: record.seq,
            instance: record.instance.clone(),
            stdout: out.tail,
            stderr: err.tail,
            command,
            flog_omitted: flog_omitted.into(),
        };
        let copies = [out.copy, err.copy, result];
        self.copy(record.run_id(), &record.instance, &copies).await;
        let queued = self.queue.offer(Event::Run {
            inv: inv.clone(),
            record,
            failed: Some(failed),
            stream_opened_ms,
        });
        // The recorder may have called the invocation idle while this run's logs were
        // read: its close is then ahead of the run, which would reopen it. Close it again.
        let closed = inv
            .record_id
            .as_deref()
            .is_some_and(|name| !self.open.contains(name));
        if queued && closed {
            self.queue.offer(Event::Close { inv });
        }
    }

    /// Writes the failure-log object of `copies` (stdout, stderr, result) for `run`, and
    /// offers its row to the writer after the PUT returned. A copy that cannot be written
    /// is counted (`stage="put"`) and given up: the run is offered without it.
    async fn copy(&self, run: RunId, instance: &str, copies: &[Option<Keep>; 3]) {
        let bytes = |at: usize| copies[at].as_ref().map(|c| c.bytes.clone());
        let (out, err, result) = (bytes(0), bytes(1), bytes(2));
        if out.is_none() && err.is_none() && result.is_none() {
            return;
        }
        let sink = Arc::clone(&self.flog);
        let instance = instance.to_owned();
        let encoding = tokio::task::spawn_blocking(move || {
            let parts = FlogParts {
                instance: &instance,
                stdout: out.as_deref(),
                stderr: err.as_deref(),
                result: result.as_deref(),
            };
            sink.encode(run, &parts)
        });
        let Ok(object) = encoding.await else {
            self.stats.flog_put_failed();
            return;
        };
        let put = tokio::time::timeout(self.options.read_timeout, self.flog.put(object));
        match put.await {
            Ok(Ok(written)) => {
                self.queue.offer(Event::Flog(written));
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, %run, "history: a failure log could not be written");
                self.stats.flog_put_failed();
            }
            Err(_) => {
                tracing::warn!(%run, "history: writing a failure log timed out");
                self.stats.flog_put_failed();
            }
        }
    }
}

/// The SHA-256 digest `d` is, if its hash is 32 bytes.
fn digest_of(d: &WireDigest) -> Option<Digest> {
    let hash = <[u8; 32]>::try_from(d.hash.as_slice()).ok()?;
    Some(Digest::new(DigestFunction::Sha256, hash, d.size))
}

/// The rendering of `command`: argv, environment names only, working directory and
/// output paths, bounded as the failed-run section is.
#[allow(deprecated)] // `output_files` and `output_directories`: REAPI before 2.1 sends only these.
fn render(command: Command) -> CommandRendering {
    let Command {
        arguments,
        environment_variables,
        working_directory,
        mut output_paths,
        output_files,
        output_directories,
        ..
    } = command;
    output_paths.extend(output_files);
    output_paths.extend(output_directories);
    let mut rendering = CommandRendering {
        argv: arguments,
        env_names: environment_variables.into_iter().map(|e| e.name).collect(),
        working_directory,
        output_paths,
        truncated: false,
    };
    rendering.bound();
    rendering
}

#[cfg(test)]
#[path = "tails_tests.rs"]
mod tests;
