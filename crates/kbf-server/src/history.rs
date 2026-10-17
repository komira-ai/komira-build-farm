//! Build history on the server (`docs/design/ui-data.md`, section 5; phase U3, the
//! writer): each attempt and cache hit of an invocation a client named is written, off
//! the request path, as records in `kbf-history`'s chunks, with a run id the build page
//! names the run by.
//!
//! - **Off unless asked.** Without `--history-objects` there is no tap, no task and no
//!   queue: the server runs as it did.
//! - **Behind the recorder.** The recorder's task ([`kbf_front::Recording`]) hands each
//!   settled attempt to the [`HistoryTap`], which maps it to a record and offers it to
//!   a bounded queue ([`kbf_history::EventQueue`]) and returns; the writer's own task
//!   ([`over`], [`spawn`]) drains the queue every [`STEP_EVERY`] and does the object
//!   store and state store calls, the latter on the blocking pool. Nothing a request
//!   path does waits for any of it: a `Result` is acknowledged before the recorder is
//!   told of it, and the recorder's channel drops and counts when full
//!   (`kbf_invocation_events_dropped`).
//! - **It drops, and counts, and never waits.** A full queue, a buffer past its bound,
//!   a failed write or a failed index transaction drops records
//!   (`kbf_history_dropped_total{stage}` on `/metrics`); a writer stuck in a call to a
//!   store that never answers fills the queue and then drops, and the calls it was
//!   stuck behind never see it.
//! - **A stop is a flush.** On a graceful stop the writer takes one more step, for at
//!   most five seconds, with every open invocation closed.
//!
//! - **A failed run keeps its logs** (ui-data.md Q21). Given a blob source
//!   ([`History::with_blobs`]), the tap hands a failed run to a small pool of workers
//!   ([`tails`]) instead of the queue: they read its last 16 KiB of stdout and stderr and
//!   its Command, copy its logs and ActionResult into an object of their own, and then
//!   offer the run with those tails. Successful runs are offered at once, as before.
//!
//! Not here: the list and shape index rows.

mod driver;
mod flags;
mod reads;
mod stats;
mod tails;
mod tap;

pub use driver::{History, Options, STEP_EVERY, over};
pub(crate) use driver::{finish, spawn};
pub use flags::{HistoryError, HistoryFlags, with_history};
pub use reads::HistoryReads;
#[cfg(test)]
pub(crate) use reads::Shared;
pub use stats::Stats;
pub use tails::{
    BlobReadError, BlobReads, COMMAND_READ_BYTES, RESULT_COPY_BYTES, TAIL_READ_BYTES, TailOptions,
};
pub use tap::HistoryTap;
#[cfg(test)]
pub(crate) use tap::RunIndex;
pub(crate) use tap::run_record;

use kbf_front::Recording;

use crate::StateStore;

/// Takes the history out of `state`, which the farm then takes whole.
pub(crate) fn take(state: Option<StateStore>) -> (Option<StateStore>, Option<History>) {
    match state {
        Some(mut state) => {
            let history = state.history.take();
            (Some(state), history)
        }
        None => (None, None),
    }
}

/// `recording` with `history`'s tap, if there is a history.
pub(crate) fn install(recording: Recording, history: Option<&History>) -> Recording {
    match history {
        Some(history) => recording.with_tap(history.tap()),
        None => recording,
    }
}
