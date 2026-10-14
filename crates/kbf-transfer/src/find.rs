//! FindMissing before every upload (`docs/design/worker-cache.md#43-api`, `upload_missing`).
//!
//! An upload asks whether the server holds its blob; the server is the truth, so
//! nothing local answers (`docs/design/worker-cache.md#3-contracts-every-stage-keeps`, 1).
//! The questions of every upload in the process queue here, and one task asks them
//! in calls of at most `--transfer-find-missing-chunk` digests, one call at a time:
//! while a call is out, the questions that arrive meanwhile gather for the next. So
//! the concurrent uploads of a tree's outputs share a few calls, with no wait added
//! to gather them.
//!
//! A caller with a whole set of blobs to store (a lease's outputs) asks about all of
//! them at once instead ([`crate::Cas::missing`]): its digests are cut into calls of at
//! most `--transfer-find-missing-chunk` and every call goes out at once, each within a
//! request permit, so a tree of 15,000 outputs costs 15 calls side by side, not one
//! call per 64 uploads in a row. The digests answered missing are remembered by the
//! lease's [`crate::Scope`] until each is stored, so their uploads do not ask again.
//!
//! FindMissing only saves uploads, so it never fails one: a call that fails after its
//! retries answers "missing" for its digests, and the blobs are uploaded. The first
//! UNIMPLEMENTED (a worker listener that does not serve the CAS service) turns the
//! question off for the life of the process, logged once.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use kbf_proto::reapi::Digest;
use tokio::sync::oneshot;
use tonic::Code;

use crate::cas::label;
use crate::engine::Inner;
use crate::stats::{Counts, add, add_since};

/// The questions waiting for the next FindMissing call.
#[derive(Debug, Default)]
pub(crate) struct Finder {
    state: Mutex<Queue>,
    /// Set by the first UNIMPLEMENTED.
    off: AtomicBool,
    pub calls: AtomicU64,
    pub unavailable: AtomicU64,
}

#[derive(Debug, Default)]
struct Queue {
    asked: Vec<(Digest, oneshot::Sender<bool>)>,
    /// A task is asking them.
    running: bool,
}

impl Finder {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        // Every change is one push, one drain or one flag.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    /// Whether the server holds the blob `digest` names, counted in `counts`. `false`
    /// when it could not be asked.
    pub(crate) async fn present(self: &Arc<Self>, digest: &Digest, counts: &Counts) -> bool {
        if self.finder.off.load(Ordering::SeqCst) {
            return false;
        }
        add(&counts.find_missing_digests, 1);
        let (answer, answered) = oneshot::channel();
        let start = {
            let mut queue = self.finder.lock();
            queue.asked.push((digest.clone(), answer));
            !std::mem::replace(&mut queue.running, true)
        };
        if start {
            tokio::spawn(Arc::clone(self).ask());
        }
        // An asking task that ended without answering (the runtime shutting down)
        // leaves the blob to be uploaded.
        answered.await.unwrap_or(false)
    }

    /// Asks the queued questions, a call at a time, until none is left.
    async fn ask(self: Arc<Self>) {
        loop {
            let asked: Vec<(Digest, oneshot::Sender<bool>)> = {
                let mut queue = self.finder.lock();
                if queue.asked.is_empty() {
                    queue.running = false;
                    return;
                }
                let n = queue.asked.len().min(self.config.find_missing_chunk);
                queue.asked.drain(..n).collect()
            };
            let digests: Vec<Digest> = asked.iter().map(|(d, _)| d.clone()).collect();
            // Questions queued before an UNIMPLEMENTED turned FindMissing off are not
            // sent either.
            let missing = if self.finder.off.load(Ordering::SeqCst) {
                None
            } else {
                self.find_missing(digests).await
            };
            for (digest, answer) in asked {
                let present = missing
                    .as_ref()
                    .is_some_and(|m| !m.contains(&label(&digest)));
                // A caller that stopped waiting needs no answer.
                let _ = answer.send(present);
            }
        }
    }

    /// The digests among `digests` the server does not hold, in the order asked,
    /// counted in `counts`: every chunk of `--transfer-find-missing-chunk` asked at once.
    /// A chunk that could not be asked answers all its digests missing.
    pub(crate) async fn missing(&self, digests: &[Digest], counts: &Counts) -> Vec<Digest> {
        if self.finder.off.load(Ordering::SeqCst) {
            return digests.to_vec();
        }
        add(&counts.find_missing_digests, digests.len() as u64);
        let calls = digests
            .chunks(self.config.find_missing_chunk)
            .map(|chunk| async move {
                let asked = Instant::now();
                let missing = self.find_missing(chunk.to_vec()).await;
                add(&counts.find_missing_calls, 1);
                add_since(&counts.find_missing_us, asked);
                chunk
                    .iter()
                    .filter(|d| missing.as_ref().is_none_or(|m| m.contains(&label(d))))
                    .cloned()
                    .collect::<Vec<Digest>>()
            });
        futures::future::join_all(calls)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// The labels of the digests among `digests` the server does not hold; `None` when
    /// the call could not be made.
    async fn find_missing(&self, digests: Vec<Digest>) -> Option<BTreeSet<String>> {
        let mut attempt = 0;
        loop {
            add(&self.finder.calls, 1);
            let permit = self.request().await;
            let found = self.wire.find_missing(digests.clone()).await;
            drop(permit);
            let status = match found {
                Ok(missing) => return Some(missing.iter().map(label).collect()),
                Err(status) => status,
            };
            if status.code() == Code::Unimplemented {
                // The calls of a whole set go out together, so several can come back
                // UNIMPLEMENTED: the first is counted and logged, and none is asked after.
                if !self.finder.off.swap(true, Ordering::SeqCst) {
                    add(&self.finder.unavailable, 1);
                    tracing::warn!("FindMissing answered UNIMPLEMENTED; every blob is uploaded");
                }
                return None;
            }
            if !self.retry(&status, &mut attempt).await {
                tracing::warn!("FindMissing failed; uploading its blobs: {status}");
                return None;
            }
        }
    }
}
