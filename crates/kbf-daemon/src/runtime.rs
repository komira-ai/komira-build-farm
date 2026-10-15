//! Execution: the [`Runtime`] trait the lease manager runs work through (a daemon may
//! have several, one per lease kind: [`crate::Runtimes`]), and [`FakeRuntime`], which
//! runs nothing. [`crate::LocalRuntime`] (tests only) runs
//! actions as plain processes; the container driver (`kbf-driver-container`)
//! implements the trait for farm nodes.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kbf_proto::reapi::{ActionResult, Digest};
use kbf_types::{LeaseId, LeaseKind, LostWhy, Resources};
use tokio::sync::oneshot;

/// One lease's work, as the server's Start describes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Work {
    pub lease_id: LeaseId,
    /// The lease kind (`action`, `whole_machine`).
    pub kind: String,
    pub action_digest: Digest,
    /// What the scheduler booked for the lease here; zero on an axis it did not book.
    pub resources: Resources,
}

/// Why work did not produce an action result.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The work was stopped by [`Runtime::kill`].
    #[error("killed")]
    Killed,
    /// The runtime could not run the work (an infrastructure failure, not the action's).
    #[error("{0}")]
    Failed(String),
    /// The action asks for something no node may run (an image named by tag, an output
    /// path that leaves the working directory, a Command without arguments): the
    /// client's error, not the farm's.
    #[error("invalid action: {0}")]
    Invalid(String),
    /// A blob the action needs (`hash/size`) is not in the CAS: the client's to upload.
    #[error("blob {0} is not in the CAS")]
    MissingBlob(String),
    /// The action ran past its timeout and was stopped. `stdout` and `stderr` are what
    /// it wrote to each before then, stored in the CAS: REAPI's partial result, which
    /// the Result carries with DEADLINE_EXCEEDED. `None` for a log the driver left out
    /// (past the output limit, or not stored).
    #[error("timed out")]
    TimedOut {
        stdout: Option<Digest>,
        stderr: Option<Digest>,
    },
    /// The action's processes together used more memory than the lease allows, and
    /// the runtime (or the kernel, at the lease's own cap) killed them. Reported as
    /// RESOURCE_EXHAUSTED with `MEMORY_KILL_OWN_LIMIT`: the farm's to retry with more
    /// memory, not the client's to fix.
    #[error("out of memory: the action used {used} bytes, past the lease's limit of {limit}")]
    OutOfMemory { used: u64, limit: u64 },
    /// The kernel's OOM killer ended the action although it stayed within its lease's
    /// limit: the node ran short of memory (its cap on all actions, or the host). The
    /// string says which counters showed it. Reported as UNAVAILABLE with
    /// `MEMORY_KILL_NODE_PRESSURE`: the farm's to run again with the same booking.
    #[error("killed on a busy node: {0}")]
    BusyNode(String),
    /// The farm lost the run, and its result with it: the daemon restarted while the
    /// action ran, or the node fenced it. The string says what happened. Reported as
    /// UNAVAILABLE with `Result.lost`: the farm's to run again, preferring another node.
    #[error("{1}")]
    Lost(LostWhy, String),
}

/// How long a lease's whole output upload may take unless the daemon is told otherwise
/// (`--output-upload-deadline-secs`): 30 minutes.
pub const UPLOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Runs `upload`, a lease's whole output upload (its output files, stdout and stderr),
/// for at most `deadline`. Past it, `upload` is dropped, which cancels its calls to the
/// store, and the lease fails with [`RuntimeError::Failed`] naming the upload, the
/// deadline and the time it ran: a store that stops answering ends the lease instead
/// of holding it with no error.
///
/// # Errors
/// What `upload` returns, or the failure past the deadline.
pub async fn upload_within<T>(
    deadline: Duration,
    upload: impl Future<Output = Result<T, RuntimeError>>,
) -> Result<T, RuntimeError> {
    let started = tokio::time::Instant::now();
    tokio::time::timeout(deadline, upload)
        .await
        .unwrap_or_else(|_| {
            Err(RuntimeError::Failed(format!(
                "the output upload did not finish within its deadline of {deadline:?} \
                 (--output-upload-deadline-secs): stopped after {:.1?}",
                started.elapsed()
            )))
        })
}

/// Runs leases. The lease manager never names a driver: it asks the runtime whether it
/// serves a lease kind, and runs the lease through it.
pub trait Runtime: Send + Sync + 'static {
    /// The driver name the node report lists under `drivers`.
    fn driver(&self) -> &'static str;

    /// Whether this runtime runs leases of `kind`.
    fn serves(&self, kind: &str) -> bool;

    /// Runs one lease to completion. An action that runs and exits non-zero is `Ok`.
    fn run(&self, work: Work) -> impl Future<Output = Result<ActionResult, RuntimeError>> + Send;

    /// Stops a running lease. Returns once its work has stopped; the lease's `run` then
    /// ends with [`RuntimeError::Killed`]. Killing a lease that is not running does
    /// nothing.
    fn kill(&self, lease_id: LeaseId) -> impl Future<Output = ()> + Send;
}

/// A runtime that runs nothing: each lease "succeeds" with exit code 0 and an empty
/// result after a fixed time, unless killed first. It records what it was asked to do,
/// for tests and for bringing up a daemon before a real driver exists.
#[derive(Debug)]
pub struct FakeRuntime {
    run_for: Duration,
    kind: LeaseKind,
    driver: &'static str,
    state: Mutex<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    started: Vec<LeaseId>,
    killed: Vec<LeaseId>,
    stop: BTreeMap<LeaseId, oneshot::Sender<()>>,
}

impl FakeRuntime {
    /// Serves lease kind `action` as driver `fake`; each lease takes `run_for`.
    #[must_use]
    pub fn new(run_for: Duration) -> Self {
        Self {
            run_for,
            kind: LeaseKind::Action,
            driver: "fake",
            state: Mutex::new(FakeState::default()),
        }
    }

    /// This runtime serving lease kind `kind` alone, as driver `driver`: a daemon with
    /// several runtimes ([`crate::Runtimes`]) can be brought up, or tested, with fakes.
    #[must_use]
    pub fn serving(self, kind: LeaseKind, driver: &'static str) -> Self {
        Self {
            kind,
            driver,
            ..self
        }
    }

    /// Leases started, in order.
    #[must_use]
    pub fn started(&self) -> Vec<LeaseId> {
        self.lock().started.clone()
    }

    /// Leases killed while running, in order.
    #[must_use]
    pub fn killed(&self) -> Vec<LeaseId> {
        self.lock().killed.clone()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        // The state stays consistent under a panic elsewhere: every update is one push
        // or one map operation.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Runtime for FakeRuntime {
    fn driver(&self) -> &'static str {
        self.driver
    }

    fn serves(&self, kind: &str) -> bool {
        kind == self.kind.name()
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.lock();
            state.started.push(work.lease_id);
            state.stop.insert(work.lease_id, tx);
        }
        let outcome = tokio::select! {
            () = tokio::time::sleep(self.run_for) => Ok(ActionResult::default()),
            _ = rx => Err(RuntimeError::Killed),
        };
        self.lock().stop.remove(&work.lease_id);
        outcome
    }

    async fn kill(&self, lease_id: LeaseId) {
        let mut state = self.lock();
        if let Some(stop) = state.stop.remove(&lease_id) {
            state.killed.push(lease_id);
            // The receiver is gone only if the run already ended.
            let _ = stop.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches an upload that is not bounded (the lease waits on a store that never
    /// answers) and a failure that does not say it was the upload, its deadline and
    /// how long it ran.
    #[tokio::test]
    async fn an_upload_past_its_deadline_fails_saying_so() {
        let deadline = Duration::from_millis(50);
        let pending = std::future::pending::<Result<(), RuntimeError>>();
        let ended = tokio::time::timeout(Duration::from_secs(10), upload_within(deadline, pending))
            .await
            .expect("the upload ends at its deadline, not never");
        let why = format!("{:?}", ended.expect_err("the upload fails at its deadline"));
        assert!(
            why.starts_with(
                "Failed(\"the output upload did not finish within its deadline of 50ms"
            ),
            "{why}"
        );
        assert!(why.contains("--output-upload-deadline-secs"), "{why}");
        // How long it ran: never less than the deadline, and not "never". An upper bound
        // tighter than that fails on a loaded runner whose timer fires late (61 ms on
        // macOS for a 50 ms deadline).
        let tail = why
            .rsplit("stopped after ")
            .next()
            .expect("a split has an item");
        let ran = tail.trim_end_matches("\")").trim_end_matches("ms");
        let ms: f64 = ran.parse().expect("the elapsed time is in milliseconds");
        assert!((50.0..10_000.0).contains(&ms), "{why}");
    }

    /// Catches an upload that finishes in time being failed or changed.
    #[tokio::test]
    async fn an_upload_within_its_deadline_is_its_own_outcome() {
        let deadline = Duration::from_secs(10);
        assert_eq!(
            upload_within(deadline, async { Ok::<_, RuntimeError>(7) })
                .await
                .expect("ok"),
            7
        );
        let refused = upload_within(deadline, async {
            Err::<(), _>(RuntimeError::Invalid("bad".to_owned()))
        })
        .await
        .expect_err("the upload's own refusal");
        assert_eq!(format!("{refused:?}"), r#"Invalid("bad")"#);
    }
}
