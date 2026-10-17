//! The build pages over history (`docs/design/ui-data.md` sections 5.5, 6 and 8): an
//! invocation the recorder no longer holds (the server restarted, or the record was
//! removed at the memory bound) is read back from the chunks the history writer put in
//! the object store, and `GET /v1/runs/{run_id}` names one attempt, live or from history.
//!
//! - **Authorized in two phases, all-must-allow.** The invocation's instance names are
//!   read first (one index row, [`kbf_history::Reader::invocation_instances`]) and each is
//!   authorized as a REAPI call's instance name is. The records are then read with an
//!   `allow` that accepts those names only, and the reader checks the header and the
//!   newest chunk's summary against it. An instance added in between answers `503`
//!   ([`Bad::Changed`], as for a live record) and a list that cannot be checked whole (empty
//!   or truncated) answers as if the invocation were not held, to every caller alike.
//! - **One renderer.** The records become the [`kbf_invocation::Invocation`] the live
//!   record is, so every route, filter and page token of the build pages serves them
//!   unchanged ([`restore`] says what history does not keep). A restored record says
//!   `"restored": true`, and, when it has a hole, `"complete": false` and `"gap_from"`.
//!   A page token made for a record the recorder held answers `no_record` once the record is
//!   read back instead: its serial is another one's.
//! - **Bounded.** A read gathers at most [`MAX_RECORDS`] records, and the records restored
//!   are held in a cache of [`RESTORED_BYTES`] ([`cache`]).
//! - **A run is one instance's.** `GET /v1/runs/{id}` reads the run, then authorizes its
//!   instance, and answers only after: nothing is read in between that could change it.

mod cache;
mod cross;
mod query;
mod restore;
mod run;
mod scope;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::response::Response;
use kbf_auth::{AuthenticationMetadata, authorize};
use kbf_history::records::{HitRecord, RunRecord};
use kbf_history::{Allow, ChunkCursor, InvKey, InvocationView, MAX_PAGE_CHUNKS, ReadError};
use kbf_invocation::{Invocation, Restored, WallTime};

use self::cache::{Cache, stamp_of};
use crate::history::HistoryReads;
use crate::http_blobs::{error, refusal};
use crate::http_invocations::{Bad, Draft, Request, answer};
use crate::http_invocations::{CALL, InvocationReaders, bad_answer};

pub(crate) use cross::router as cross_router;
pub(crate) use run::router as run_router;

/// Restored records are held within this many bytes (the 64 MiB of
/// [`Invocation::bytes`]), least recently used out first.
pub const RESTORED_BYTES: usize = 64 << 20;

/// Records (runs and hits) one read gathers at most: past it the record is `rows_capped`
/// and not complete. About 50 MiB restored.
pub const MAX_RECORDS: usize = 50_000;

/// A restored record's serial is this plus a count: no record the recorder made has one
/// (it counts up from 0), so a page token of the one is never read against the other.
const RESTORED_SERIAL: u64 = 1 << 63;

/// The records of one invocation, gathered for [`Durable::restore`].
struct Gathered {
    runs: Vec<RunRecord>,
    hits: Vec<HitRecord>,
    /// There were more records than [`MAX_RECORDS`].
    capped: bool,
}

/// History as the build pages read it.
pub(crate) struct Durable {
    reads: Arc<HistoryReads>,
    restored: Mutex<Cache>,
    serials: AtomicU64,
    /// Records one read gathers at most.
    max_records: usize,
}

impl Durable {
    pub(crate) fn new(reads: Arc<HistoryReads>) -> Self {
        Self {
            reads,
            restored: Mutex::new(Cache::new(RESTORED_BYTES)),
            serials: AtomicU64::new(0),
            max_records: MAX_RECORDS,
        }
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        // A map and counters; a panic elsewhere leaves it whole.
        self.restored.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The route `request` names, read from history, for `caller`: authorized on each of
    /// the invocation's instance names, then read and rendered as a live record is.
    pub(crate) async fn read(
        &self,
        readers: &InvocationReaders,
        caller: &AuthenticationMetadata,
        request: &Request,
        since: WallTime,
        now: WallTime,
    ) -> Result<Draft, Response> {
        let name = request.key.to_string();
        let key = InvKey::from_record_id(&name);
        let gone = || bad_answer(&Bad::Gone, &name, since);
        // Phase 1: the names, each one authorized.
        let Some(mut names) = self.reads.instances(&key).await.map_err(unreadable)? else {
            return Err(gone());
        };
        names.sort();
        names.dedup();
        for instance in &names {
            authorize(&*readers.authorizer, caller, CALL, instance)
                .await
                .map_err(|e| refusal(&e))?;
        }
        // Phase 2: everything read is checked against the names authorized.
        let allowed: BTreeSet<&str> = names.iter().map(String::as_str).collect();
        let allow = |instance: &str| allowed.contains(instance);
        let Some(view) = self
            .reads
            .invocation(&key, &allow)
            .await
            .map_err(unreadable)?
        else {
            return Err(self.refused(&key, &names, &name, since).await);
        };
        // No chunk (a crash before the first was written leaves no row, but a close with
        // nothing to write leaves a header), or another invocation's header under this
        // key's hash: no record.
        if view.newest.is_none() || view.header.record_id != name {
            return Err(gone());
        }
        let stamp = stamp_of(&view.header);
        let held = self.cache().get(&request.key, stamp);
        let inv = match held {
            Some(inv) => inv,
            None => {
                let gathered = self.gather(&key, &allow).await.map_err(unreadable)?;
                let Some(gathered) = gathered else {
                    return Err(self.refused(&key, &names, &name, since).await);
                };
                let inv = Arc::new(self.restore(request, &view, &gathered));
                self.cache()
                    .put(request.key.clone(), stamp, Arc::clone(&inv));
                inv
            }
        };
        answer(&inv, request, (inv.serial(), &names), readers.blobs, now)
            .map_err(|bad| bad_answer(&bad, &name, since))
    }

    /// Every run and hit record of `key` that `allow` accepts, up to [`MAX_RECORDS`],
    /// and whether there were more. `None` when the reader no longer answers for the
    /// invocation (its header lists an instance `allow` does not).
    async fn gather(&self, key: &InvKey, allow: Allow<'_>) -> Result<Option<Gathered>, ReadError> {
        let (mut runs, mut hits) = (Vec::new(), Vec::new());
        let mut after: Option<ChunkCursor> = None;
        loop {
            let page = self
                .reads
                .records(key, after.as_ref(), MAX_PAGE_CHUNKS, allow)
                .await?;
            let Some(page) = page else {
                return Ok(None);
            };
            runs.extend(page.runs);
            hits.extend(page.hits);
            // The last page, or as many records as one read takes.
            let more = page.next;
            let capped = more.is_some() && runs.len() + hits.len() >= self.max_records;
            if more.is_none() || capped {
                return Ok(Some(Gathered { runs, hits, capped }));
            }
            after = more;
        }
    }

    /// The invocation `view` and the records say, as the recorder's record is.
    fn restore(&self, request: &Request, view: &InvocationView, got: &Gathered) -> Invocation {
        let (rows, skipped) = restore::rows(&got.runs, &got.hits);
        let capped = got.capped;
        let h = &view.header;
        let complete = view.complete && skipped == 0 && !capped;
        let ms = |ms: u64| WallTime::from_unix_micros(ms.saturating_mul(1_000));
        Invocation::restored(
            Restored {
                key: request.key.clone(),
                serial: RESTORED_SERIAL | self.serials.fetch_add(1, Ordering::Relaxed),
                instances: h.instances.iter().cloned().collect(),
                first_seen: ms(h.first_seen_ms),
                last_seen: ms(h.last_seen_ms),
                complete,
                gap_from: Some(ms(h.last_chunk_ms)),
                rows_capped: capped,
            },
            rows,
        )
    }

    /// The answer when the reader does not answer for an invocation whose names were
    /// authorized: if its names are others than they were, it changed between the two
    /// phases (`503`, to be sent again); otherwise it is not one a caller may be answered
    /// for, and so is not held.
    async fn refused(
        &self,
        key: &InvKey,
        names: &[String],
        name: &str,
        since: WallTime,
    ) -> Response {
        let now = match self.reads.instances(key).await {
            Ok(now) => now,
            Err(e) => return unreadable(e),
        };
        let changed = now.is_some_and(|mut now| {
            now.sort();
            now.dedup();
            now != names
        });
        let bad = if changed {
            Bad::Changed(
                "the invocation gained an instance name while it was read; send the request \
                 again"
                    .to_owned(),
            )
        } else {
            Bad::Gone
        };
        bad_answer(&bad, name, since)
    }
}

/// The `503` of a read history could not make: the stores failed or hold what is not
/// history. The cause is logged, not sent.
fn unreadable(e: ReadError) -> Response {
    tracing::warn!(error = %e, "build history could not be read");
    error(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "build history cannot be read right now",
    )
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
