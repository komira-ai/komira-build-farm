//! The lease manager: starts work on `Start` and only on `Start`, through the runtime
//! that serves the Start's kind, turns each outcome into one `Result`, and fences
//! (kills) running work when contact is lost. A lease is killed through the runtime it
//! runs on.
//!
//! A `LeaseOffer` is the scheduler placing a lease before it commits it; running on an
//! offer could run a lease the scheduler never commits, or run it twice beside the
//! node it is placed on instead. So an offer is acknowledged in the log and nothing
//! runs until the committed `Start` arrives.
//!
//! v0 fences every lease (`SELF_FENCE`); the `RUN_ON` policy for hermetic actions
//! arrives when Start carries a fence policy.
//!
//! A `Cancel` (the server no longer holds the lease here) kills a running lease
//! without waiting: the lease stays running, and listed, until its run has stopped,
//! and then reports its outcome (ABORTED, killed) like any other run. A fence kills
//! every running lease the same way, without waiting: a run whose kill takes long (one
//! in the middle of a slow output upload sees it only when the upload ends) must not
//! hold up the daemon's heartbeats or its next stream. A fenced lease reports, once
//! its run has stopped, as lost to the farm, whatever the run's own outcome.
//!
//! A run whose driver panics reports INTERNAL, naming the panic, once the runtime has
//! been asked to kill what the run left: the panic ends that run's task only
//! (the all-in-one design, komira-build-farm#657, section 7.3), and its lease does
//! not stay running.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use kbf_proto::google::rpc::precondition_failure::Violation;
use kbf_proto::google::rpc::{Code, PreconditionFailure, Status};
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::{self, Lost, MemoryKill, ResourceUsage, Start};
use kbf_types::{LeaseId, LostWhy, Resources};
use prost::Message;
use tokio::sync::mpsc;

use crate::lease_panic::contained;
use crate::phases::LeasePhases;
use crate::runtime::{RuntimeError, Work};
use crate::runtimes::{AnyRuntime, Runtimes};

/// A finished run: its lease, outcome, what the runtime measured of it if it ended
/// without a result ([`crate::Runtime::failed_usage`]), and where its time went.
pub(crate) type Done = (
    LeaseId,
    Result<ActionResult, RuntimeError>,
    Option<ResourceUsage>,
    LeasePhases,
);

/// The leases a daemon is running.
pub(crate) struct Leases {
    runtimes: Runtimes,
    running: BTreeMap<LeaseId, Running>,
    /// Running leases being killed on the server's `Cancel`.
    cancelled: BTreeSet<LeaseId>,
    /// Running leases being killed by a fence.
    fenced: BTreeSet<LeaseId>,
    done: mpsc::UnboundedSender<Done>,
}

/// A running lease: the runtime it runs on. Its run reports through `done`.
struct Running {
    runtime: Arc<dyn AnyRuntime>,
}

impl Leases {
    /// A manager that runs leases through `runtimes`, whose runs report on `done`.
    pub(crate) fn new(runtimes: Runtimes, done: mpsc::UnboundedSender<Done>) -> Self {
        Self {
            runtimes,
            running: BTreeMap::new(),
            cancelled: BTreeSet::new(),
            fenced: BTreeSet::new(),
            done,
        }
    }

    /// The leases running now, oldest first.
    pub(crate) fn running(&self) -> Vec<LeaseId> {
        self.running.keys().copied().collect()
    }

    /// Handles a Start: runs it on the runtime that serves its kind. Returns a Result to
    /// send at once when the lease is refused (no runtime serves its kind, or it names
    /// no action); a started lease reports through `done`. A Start for a lease already
    /// running is a resend and changes nothing.
    pub(crate) fn start(&mut self, start: Start) -> Option<worker::Result> {
        let Some(id) = start.lease_id.map(lease_id) else {
            tracing::warn!("Start without a lease id ignored");
            return None;
        };
        if self.running.contains_key(&id) {
            return None;
        }
        let Some(runtime) = self.runtimes.serving(&start.kind) else {
            return Some(failure(
                id,
                Code::FailedPrecondition,
                format!("no driver here serves lease kind {:?}", start.kind),
            ));
        };
        let runtime = Arc::clone(runtime);
        let Some(work) = work(id, start) else {
            return Some(failure(
                id,
                Code::InvalidArgument,
                "Start has no action digest",
            ));
        };
        let done = self.done.clone();
        let run = Arc::clone(&runtime);
        tokio::spawn(async move {
            let started = Instant::now();
            let outcome = contained(run.as_ref(), id, work).await;
            // An error carries no result, so no usage: the runtime is asked for it. An
            // out-of-memory kill (the lease's own limit, or the node's) is marked.
            let usage = outcome.as_ref().err().and_then(|_| run.failed_usage(id));
            let oom = matches!(
                outcome,
                Err(RuntimeError::OutOfMemory { .. } | RuntimeError::BusyNode(_))
            );
            let phases =
                LeasePhases::new(id, run.driver(), started.elapsed(), outcome.as_ref().ok())
                    .failed_with(usage.as_ref(), oom);
            // The receiver lives as long as the daemon.
            let _ = done.send((id, outcome, usage, phases));
        });
        tracing::info!(lease = %id, driver = runtime.driver(), "lease started");
        self.running.insert(id, Running { runtime });
        None
    }

    /// Starts killing lease `id` because the server cancelled it, and returns at once.
    /// Returns whether a kill began: not for a lease that is not running or is already
    /// being cancelled. The run reports through `done` once it has stopped.
    pub(crate) fn cancel(&mut self, id: LeaseId) -> bool {
        let Some(running) = self.running.get(&id) else {
            return false;
        };
        if !self.cancelled.insert(id) {
            return false;
        }
        let runtime = Arc::clone(&running.runtime);
        tokio::spawn(async move { runtime.kill(id).await });
        tracing::warn!(lease = %id, "lease cancelled: the server no longer holds it here");
        true
    }

    /// Has every runtime kill lease `id`, which is not running here, and returns at
    /// once: for a runtime that keeps a lease across its own restart (the whole-machine
    /// runtime), the server no longer holding it means it is to be cleaned up. Every
    /// other runtime does nothing ([`crate::Runtime::kill`] of a lease it does not run).
    pub(crate) fn forget(&self, id: LeaseId) {
        tracing::debug!(lease = %id, "a lease not running here is forgotten by every runtime");
        for runtime in self.runtimes.all() {
            let runtime = Arc::clone(runtime);
            tokio::spawn(async move { runtime.kill(id).await });
        }
    }

    /// Turns a finished run into its Result: a fenced (lost) one if the lease was
    /// fenced, whatever the run's outcome. Every run reports once, on `done`, and its
    /// lease is running until then: neither a fence nor a `Cancel` drops it sooner.
    /// Logs where the lease's time went, `phases`, in one INFO line (`lease phases:`).
    pub(crate) fn finished(
        &mut self,
        id: LeaseId,
        outcome: Result<ActionResult, RuntimeError>,
        usage: Option<ResourceUsage>,
        phases: &LeasePhases,
    ) -> worker::Result {
        tracing::info!("lease phases: {phases}");
        self.running.remove(&id);
        // A cancelled run ends ABORTED (killed), or as it ended if it beat the kill;
        // the server refuses either.
        self.cancelled.remove(&id);
        if self.fenced.remove(&id) {
            tracing::warn!(lease = %id, "lease fenced: contact with the server lost");
            let why = "self-fenced: no acknowledged heartbeat within the fence time";
            return result_of(id, Err(RuntimeError::Lost(LostWhy::Fenced, why.to_owned())));
        }
        tracing::info!(lease = %id, ok = outcome.is_ok(), "lease finished");
        if let Err(error) = &outcome {
            tracing::warn!(
                lease = %id,
                kind = %failure_kind(error),
                reason = %one_line(&error.to_string()),
                "lease failed"
            );
        }
        let mut result = result_of(id, outcome);
        if let Some(usage) = usage {
            carry_usage(&mut result, &usage);
        }
        result
    }

    /// Whether a running lease is not being fenced yet.
    pub(crate) fn unfenced(&self) -> bool {
        self.running.keys().any(|id| !self.fenced.contains(id))
    }

    /// Starts killing every running lease not already being fenced, all at once, and
    /// returns them, oldest first. Returns at once: each stays running, and listed,
    /// until its run has stopped, and then reports through `done` as lost to the farm
    /// (fenced, UNAVAILABLE with `LOST_FENCED`), which the server runs again.
    pub(crate) fn fence(&mut self) -> Vec<LeaseId> {
        let mut fenced = Vec::new();
        for (id, running) in &self.running {
            if self.fenced.insert(*id) {
                let (id, runtime) = (*id, Arc::clone(&running.runtime));
                tokio::spawn(async move { runtime.kill(id).await });
                fenced.push(id);
            }
        }
        fenced
    }
}

/// What kind of failure `error` is, for the `lease failed` line: the variant, since a
/// runtime's error says what it was doing (the path or step) in its own words, which the
/// line carries as `reason`. A runtime's error does not say which phase (inputs, run,
/// outputs) it ended in.
const fn failure_kind(error: &RuntimeError) -> &'static str {
    match error {
        RuntimeError::Killed => "killed",
        RuntimeError::Failed(_) => "failed",
        RuntimeError::Invalid(_) => "invalid",
        RuntimeError::MissingBlob(_) => "missing_blob",
        RuntimeError::TimedOut { .. } => "timed_out",
        RuntimeError::OutOfMemory { .. } => "out_of_memory",
        RuntimeError::BusyNode(_) => "busy_node",
        RuntimeError::Lost(..) => "lost",
    }
}

/// The longest `reason` the `lease failed` line carries, in characters.
const REASON_CHARS: usize = 400;

/// `text` on one line (every run of whitespace a single space) and cut at
/// [`REASON_CHARS`], so a multi-line error cannot split the log line.
fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.char_indices().nth(REASON_CHARS) {
        Some((end, _)) => format!("{}...", &joined[..end]),
        None => joined,
    }
}

/// The work a Start describes; `None` if it names no action.
fn work(id: LeaseId, start: Start) -> Option<Work> {
    Some(Work {
        lease_id: id,
        kind: start.kind,
        action_digest: start.action_digest?,
        resources: Resources::new(start.millicpus, start.memory_bytes),
    })
}

/// The Result that reports a finished run.
pub(crate) fn result_of(
    id: LeaseId,
    outcome: Result<ActionResult, RuntimeError>,
) -> worker::Result {
    match outcome {
        Ok(action_result) => worker::Result {
            lease_id: Some(proto_lease_id(id)),
            status: Some(Status::default()),
            action_result: Some(action_result),
            // The daemon fills in the action its Start named.
            action_digest: None,
            memory_kill: worker::MemoryKill::Unspecified as i32,
            lost: worker::Lost::Unspecified as i32,
        },
        Err(RuntimeError::Killed) => failure(id, Code::Aborted, "killed"),
        Err(RuntimeError::Failed(why)) => failure(id, Code::Internal, why),
        Err(RuntimeError::Invalid(why)) => failure(id, Code::InvalidArgument, why),
        Err(RuntimeError::MissingBlob(blob)) => missing(id, &blob),
        Err(RuntimeError::TimedOut { stdout, stderr }) => worker::Result {
            // REAPI's partial result: what the action wrote before it was stopped, and
            // nothing else (no outputs; the server never caches it).
            action_result: (stdout.is_some() || stderr.is_some()).then(|| ActionResult {
                stdout_digest: stdout,
                stderr_digest: stderr,
                ..ActionResult::default()
            }),
            ..failure(
                id,
                Code::DeadlineExceeded,
                "the action ran past its timeout",
            )
        },
        Err(oom @ RuntimeError::OutOfMemory { .. }) => worker::Result {
            memory_kill: MemoryKill::OwnLimit as i32,
            ..failure(id, Code::ResourceExhausted, oom.to_string())
        },
        Err(busy @ RuntimeError::BusyNode(_)) => worker::Result {
            memory_kill: MemoryKill::NodePressure as i32,
            ..failure(id, Code::Unavailable, busy.to_string())
        },
        Err(RuntimeError::Lost(why, message)) => worker::Result {
            lost: match why {
                LostWhy::DaemonRestarted => Lost::DaemonRestarted,
                LostWhy::Fenced => Lost::Fenced,
            } as i32,
            ..failure(id, Code::Unavailable, message)
        },
    }
}

/// Puts `usage` in the metadata of the result's `action_result` (made if the result has
/// none): what a lease that ended without a result still counted. The server reads it
/// from every report, failed ones included, for its `action usage:` line, before it
/// judges the report (a made `action_result` is not a run's: its exit code is not
/// one).
fn carry_usage(result: &mut worker::Result, usage: &ResourceUsage) {
    let metadata = result
        .action_result
        .get_or_insert_with(ActionResult::default)
        .execution_metadata
        .get_or_insert_with(ExecutedActionMetadata::default);
    metadata.auxiliary_metadata.push(usage.to_any());
}

/// FAILED_PRECONDITION with a `MISSING` violation for `blob` (`hash/size`), as REAPI
/// reports an input that is not in the CAS.
fn missing(id: LeaseId, blob: &str) -> worker::Result {
    let detail = PreconditionFailure {
        violations: vec![Violation {
            r#type: "MISSING".to_owned(),
            subject: format!("blobs/{blob}"),
            description: String::new(),
        }],
    };
    worker::Result {
        lease_id: Some(proto_lease_id(id)),
        status: Some(Status {
            code: Code::FailedPrecondition as i32,
            message: format!("blob {blob} is not in the CAS"),
            details: vec![prost_types::Any {
                type_url: "type.googleapis.com/google.rpc.PreconditionFailure".to_owned(),
                value: detail.encode_to_vec(),
            }],
        }),
        action_result: None,
        action_digest: None,
        // A missing input is not a memory kill, nor lost.
        memory_kill: worker::MemoryKill::Unspecified as i32,
        lost: worker::Lost::Unspecified as i32,
    }
}

/// A Result for a lease that did not produce an action result.
pub(crate) fn failure(id: LeaseId, code: Code, message: impl Into<String>) -> worker::Result {
    worker::Result {
        lease_id: Some(proto_lease_id(id)),
        status: Some(Status {
            code: code as i32,
            message: message.into(),
            details: Vec::new(),
        }),
        action_result: None,
        action_digest: None,
        // Not a memory kill, nor lost; `result_of` sets the kind over this for those
        // that are.
        memory_kill: worker::MemoryKill::Unspecified as i32,
        lost: worker::Lost::Unspecified as i32,
    }
}

pub(crate) fn lease_id(id: worker::LeaseId) -> LeaseId {
    LeaseId::new(id.term, id.seq)
}

pub(crate) fn proto_lease_id(id: LeaseId) -> worker::LeaseId {
    worker::LeaseId {
        term: id.term,
        seq: id.seq,
    }
}

#[cfg(test)]
#[path = "lease_panic_tests.rs"]
mod panic_tests;

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::Digest;

    use super::*;
    use crate::runtime::Runtime;

    fn id() -> LeaseId {
        LeaseId::new(3, 4)
    }

    fn code(result: &worker::Result) -> i32 {
        result.status.as_ref().expect("status").code
    }

    /// Catches a Start whose booking never reaches the runtime: the container driver
    /// sizes the lease's cgroup from `Work::resources`.
    #[test]
    fn work_carries_the_booked_resources() {
        let start = Start {
            lease_id: Some(proto_lease_id(id())),
            kind: "action".to_owned(),
            action_digest: Some(Digest {
                hash: "ab".repeat(32),
                size_bytes: 7,
            }),
            millicpus: 1500,
            memory_bytes: 1 << 30,
            ..Start::default()
        };
        let work = work(id(), start).expect("work");
        assert_eq!(work.resources, Resources::new(1500, 1 << 30));
        assert_eq!(work.kind, "action");
        assert_eq!(work.action_digest.size_bytes, 7);
    }

    /// A runtime whose kills meet: no kill returns until every lease's kill has begun.
    /// Each run ends once a kill has returned.
    struct Rendezvous {
        kills: tokio::sync::Barrier,
        stopped: tokio::sync::Semaphore,
        /// How many runs have begun.
        runs: std::sync::atomic::AtomicUsize,
    }

    impl Runtime for Rendezvous {
        fn driver(&self) -> &'static str {
            "rendezvous"
        }

        fn serves(&self, _kind: &str) -> bool {
            true
        }

        async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let permit = self.stopped.acquire().await.expect("open");
            permit.forget();
            Err(RuntimeError::Killed)
        }

        async fn kill(&self, _lease_id: LeaseId) {
            self.kills.wait().await;
            self.stopped.add_permits(1);
        }
    }

    /// Catches a fence that kills leases one after another: a node with many leases
    /// would take the sum of their kill times (each up to twice the kill grace plus
    /// cleanup) to fence. Here each kill waits for the other, so a serial fence never
    /// ends; the fence must end, with one fenced (lost) Result per lease, oldest first.
    #[tokio::test]
    async fn fence_kills_every_lease_at_once() {
        let runtime = Arc::new(Rendezvous {
            kills: tokio::sync::Barrier::new(2),
            stopped: tokio::sync::Semaphore::new(0),
            runs: std::sync::atomic::AtomicUsize::new(0),
        });
        let (done, mut done_rx) = mpsc::unbounded_channel();
        let mut leases = Leases::new(Runtimes::new(runtime), done);
        let ids = [LeaseId::new(1, 1), LeaseId::new(1, 2)];
        for id in ids {
            let start = Start {
                lease_id: Some(proto_lease_id(id)),
                kind: "action".to_owned(),
                action_digest: Some(Digest::default()),
                ..Start::default()
            };
            assert_eq!(leases.start(start), None);
        }
        assert_eq!(leases.fence(), ids, "every running lease, oldest first");
        assert!(!leases.unfenced());
        assert_eq!(leases.fence(), [], "a lease fenced twice");
        let mut results = Vec::new();
        for _ in ids {
            let (id, outcome, usage, phases) =
                tokio::time::timeout(std::time::Duration::from_secs(10), done_rx.recv())
                    .await
                    .expect("the fence killed one lease at a time")
                    .expect("open");
            results.push(leases.finished(id, outcome, usage, &phases));
        }
        results.sort_by_key(|r| r.lease_id.map(|l| (l.term, l.seq)));
        let fenced: Vec<_> = results.iter().map(|r| r.lease_id).collect();
        assert_eq!(fenced, ids.map(|id| Some(proto_lease_id(id))));
        // Lost to the farm, not killed by a Cancel: the server runs it again.
        assert!(results.iter().all(|r| code(r) == Code::Unavailable as i32));
        assert!(results.iter().all(|r| r.lost() == worker::Lost::Fenced));
        assert!(leases.running().is_empty());
    }

    /// Catches a Start that should change nothing starting work anyway: one without a
    /// lease id, and a resend for a lease already running (a second run of one lease).
    /// Also a Start with no action answered with anything but INVALID_ARGUMENT, or
    /// leaving the lease marked running.
    #[tokio::test]
    async fn starts_that_change_nothing_or_are_refused() {
        let runtime = Arc::new(Rendezvous {
            kills: tokio::sync::Barrier::new(1),
            stopped: tokio::sync::Semaphore::new(0),
            runs: std::sync::atomic::AtomicUsize::new(0),
        });
        // Rendezvous is only a stand-in; its name is not under test.
        assert_eq!(Runtime::driver(&*runtime), "rendezvous");
        let (done, mut done_rx) = mpsc::unbounded_channel();
        let mut leases = Leases::new(Runtimes::new(Arc::clone(&runtime)), done);
        let start = Start {
            lease_id: Some(proto_lease_id(id())),
            kind: "action".to_owned(),
            action_digest: Some(Digest::default()),
            ..Start::default()
        };
        let anonymous = Start {
            lease_id: None,
            ..start.clone()
        };
        assert_eq!(leases.start(anonymous), None);
        assert!(leases.running().is_empty());
        assert_eq!(leases.start(start.clone()), None);
        assert_eq!(leases.start(start), None);
        assert_eq!(leases.running(), [id()]);

        let other = LeaseId::new(3, 5);
        let no_action = Start {
            lease_id: Some(proto_lease_id(other)),
            kind: "action".to_owned(),
            ..Start::default()
        };
        let refused = leases.start(no_action).expect("refused at once");
        assert_eq!(code(&refused), Code::InvalidArgument as i32);
        assert_eq!(leases.running(), [id()]);
        // One kill ends one run. Were the resend a second run, the fence would wait
        // for a run nothing stops (or, if the kill ended that one, `runs` would be 2).
        assert_eq!(leases.fence().len(), 1);
        let (ended, outcome, usage, phases) =
            tokio::time::timeout(std::time::Duration::from_secs(5), done_rx.recv())
                .await
                .expect("the fence waited for a second run of one lease")
                .expect("open");
        leases.finished(ended, outcome, usage, &phases);
        assert_eq!(runtime.runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Catches a `Cancel` that kills a lease twice (a second kill of a run already
    /// stopping), or one that does not kill: the run must end and be reported once,
    /// after which the lease is neither running nor being cancelled, and a later
    /// `Cancel` of it, or of a lease never started, begins nothing.
    #[tokio::test]
    async fn a_cancel_kills_a_running_lease_once() {
        let runtime = Arc::new(crate::FakeRuntime::new(std::time::Duration::from_secs(60)));
        let (done, mut done_rx) = mpsc::unbounded_channel();
        let mut leases = Leases::new(Runtimes::new(Arc::clone(&runtime)), done);
        let start = Start {
            lease_id: Some(proto_lease_id(id())),
            kind: "action".to_owned(),
            action_digest: Some(Digest::default()),
            ..Start::default()
        };
        assert_eq!(leases.start(start), None);
        assert!(!leases.cancel(LeaseId::new(3, 9)), "a lease never started");
        assert!(leases.cancel(id()));
        assert!(!leases.cancel(id()), "a lease already being cancelled");
        let (ended, outcome, usage, phases) = done_rx.recv().await.expect("the run ends");
        assert_eq!(ended, id());
        let result = leases.finished(ended, outcome, usage, &phases);
        assert_eq!(code(&result), Code::Aborted as i32);
        assert_eq!(runtime.killed(), [id()]);
        assert!(leases.running().is_empty());
        assert!(
            leases.cancelled.is_empty(),
            "a finished lease is still cancelled"
        );
        assert!(!leases.cancel(id()), "a finished lease");
    }

    /// Asks every callsite again whether the subscriber just installed wants it.
    /// tracing caches each callsite's interest when it is first reached; a test on
    /// another thread that reaches the callsite while no subscriber (or only that
    /// test's, with a lower filter) is registered leaves it cached as never, and
    /// the capture then misses its line. Call it inside `with_default`.
    fn rebuild_interest() {
        tracing_core::callsite::rebuild_interest_cache();
    }

    /// Bytes logged through [`Captured::writer`].
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Catches a lease's end not logged, logged below INFO (a node's default filter
    /// would drop it), or logged without its fields: the `lease phases:` line is
    /// where an operator reads where a slow lease's time went.
    #[tokio::test]
    async fn a_finished_lease_logs_its_phases_at_info() {
        let runtime = Arc::new(crate::FakeRuntime::new(std::time::Duration::ZERO));
        let (done, mut done_rx) = mpsc::unbounded_channel();
        let mut leases = Leases::new(Runtimes::new(runtime), done);
        let start = Start {
            lease_id: Some(proto_lease_id(id())),
            kind: "action".to_owned(),
            action_digest: Some(Digest::default()),
            ..Start::default()
        };
        assert_eq!(leases.start(start), None);
        let (ended, outcome, usage, phases) = done_rx.recv().await.expect("the run ends");
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            rebuild_interest();
            leases.finished(ended, outcome, usage, &phases);
        });
        // Nothing is buffered; the subscriber never flushes.
        std::io::Write::flush(&mut captured.clone()).expect("flush");
        let log = String::from_utf8(captured.0.lock().expect("log lock").clone()).expect("UTF-8");
        assert!(log.contains("lease phases:"), "{log}");
        let line = log
            .lines()
            .find(|l| l.contains("lease phases:"))
            .expect("the line");
        assert!(line.contains(" INFO "), "{line}");
        assert!(line.ends_with(&format!("lease phases: {phases}")), "{line}");
        assert!(line.contains("lease=3.4 driver=fake ok=true "), "{line}");
    }

    /// A runtime whose every run ends at once with the one error it was given, before
    /// any input is fetched.
    struct Failing {
        error: std::sync::Mutex<Option<RuntimeError>>,
        /// What the runtime reports for the run ([`Runtime::failed_usage`]), once.
        usage: std::sync::Mutex<Option<ResourceUsage>>,
    }

    impl Failing {
        fn with(error: RuntimeError) -> Arc<Self> {
            Arc::new(Self {
                error: std::sync::Mutex::new(Some(error)),
                usage: std::sync::Mutex::new(None),
            })
        }

        /// [`Failing::with`] `error`, a run that measured `usage`.
        fn measured(error: RuntimeError, usage: &ResourceUsage) -> Arc<Self> {
            Arc::new(Self {
                error: std::sync::Mutex::new(Some(error)),
                usage: std::sync::Mutex::new(Some(usage.clone())),
            })
        }
    }

    impl Runtime for Failing {
        fn driver(&self) -> &'static str {
            "failing"
        }

        fn serves(&self, kind: &str) -> bool {
            kind == "action"
        }

        async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
            Err(self
                .error
                .lock()
                .expect("error lock")
                .take()
                .expect("one run per runtime"))
        }

        fn failed_usage(&self, _lease_id: LeaseId) -> Option<ResourceUsage> {
            self.usage.lock().expect("usage lock").take()
        }

        async fn kill(&self, _lease_id: LeaseId) {}
    }

    /// Runs one lease on `runtime` to its end (fenced first if `fence`) and returns
    /// its Result and everything logged while it was finished.
    async fn finish_logged<R: Runtime>(runtime: Arc<R>, fence: bool) -> (worker::Result, String) {
        let (done, mut done_rx) = mpsc::unbounded_channel();
        let mut leases = Leases::new(Runtimes::new(runtime), done);
        let start = Start {
            lease_id: Some(proto_lease_id(id())),
            kind: "action".to_owned(),
            action_digest: Some(Digest::default()),
            ..Start::default()
        };
        assert_eq!(leases.start(start), None);
        if fence {
            assert_eq!(leases.fence().len(), 1);
        }
        let (ended, outcome, usage, phases) = done_rx.recv().await.expect("the run ends");
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, || {
            rebuild_interest();
            leases.finished(ended, outcome, usage, &phases)
        });
        let log = String::from_utf8(captured.0.lock().expect("log lock").clone()).expect("UTF-8");
        (result, log)
    }

    /// The WARN lines of `log`.
    fn warns(log: &str) -> Vec<&str> {
        log.lines().filter(|l| l.contains(" WARN ")).collect()
    }

    /// Catches (the old code logged nothing about why) a lease that fails before its
    /// inputs finish ending with no WARN naming it and the reason: the `lease phases:`
    /// line then reads `inputs_ms=- run_ms=- outputs_ms=-` and nothing says why. Also
    /// catches the reason split over two log lines, and the `lease finished` INFO line
    /// changed or dropped (tools parse it).
    #[tokio::test]
    async fn a_failed_lease_logs_why_at_warn() {
        let error =
            RuntimeError::Failed("input tree: the blob store is unreachable\nsecond line".into());
        let (result, log) = finish_logged(Failing::with(error), false).await;
        assert_eq!(code(&result), Code::Internal as i32);
        let warns = warns(&log);
        assert_eq!(warns.len(), 1, "one WARN line expected: {log}");
        let warn = warns[0];
        assert!(warn.contains("lease=3.4"), "{warn}");
        assert!(warn.contains("kind=failed"), "{warn}");
        assert!(
            warn.contains("reason=input tree: the blob store is unreachable second line"),
            "{warn}"
        );
        let finished = log
            .lines()
            .find(|l| l.ends_with("lease finished lease=3.4 ok=false"));
        assert!(finished.is_some_and(|l| l.contains(" INFO ")), "{log}");
        assert!(log.contains("inputs_ms=- run_ms=- outputs_ms=-"), "{log}");
    }

    /// Catches a `kind` that is wrong or shared by two variants, so a log search by
    /// kind finds the wrong leases: each variant's WARN must carry its own.
    #[tokio::test]
    async fn each_failure_logs_its_own_kind() {
        let errors = [
            ("killed", RuntimeError::Killed),
            ("invalid", RuntimeError::Invalid("no arguments".into())),
            ("missing_blob", RuntimeError::MissingBlob("ab/3".into())),
            (
                "timed_out",
                RuntimeError::TimedOut {
                    stdout: None,
                    stderr: None,
                },
            ),
            (
                "out_of_memory",
                RuntimeError::OutOfMemory { used: 9, limit: 8 },
            ),
            ("busy_node", RuntimeError::BusyNode("psi".into())),
            (
                "lost",
                RuntimeError::Lost(LostWhy::DaemonRestarted, "restarted".into()),
            ),
        ];
        for (kind, error) in errors {
            let (_, log) = finish_logged(Failing::with(error), false).await;
            let warns = warns(&log);
            assert_eq!(warns.len(), 1, "{kind}: one WARN line expected: {log}");
            let warn = warns[0];
            assert!(warn.contains("lease failed"), "{kind}: {warn}");
            assert!(warn.contains(&format!("kind={kind} ")), "{kind}: {warn}");
        }
    }

    /// Catches a successful lease logging a WARN (a node's alerts on WARN would fire
    /// for every lease), and a fenced lease logging the `lease failed` line as well as
    /// its own `lease fenced` WARN, which already says why (the server could not be
    /// reached); its run's outcome is not the reason.
    #[tokio::test]
    async fn only_a_failed_lease_logs_a_failure() {
        let runtime = Arc::new(crate::FakeRuntime::new(std::time::Duration::ZERO));
        let (_, log) = finish_logged(runtime, false).await;
        assert_eq!(warns(&log), Vec::<&str>::new(), "{log}");

        let (result, log) = finish_logged(Failing::with(RuntimeError::Killed), true).await;
        assert_eq!(code(&result), Code::Unavailable as i32);
        let warns = warns(&log);
        assert_eq!(warns.len(), 1, "one WARN line expected: {log}");
        let warn = warns[0];
        assert!(warn.contains("lease fenced"), "{warn}");
        assert!(warn.contains("lease=3.4"), "{warn}");
        assert!(!log.contains("lease failed"), "{log}");
    }

    /// Catches a lease that ends without a result losing what its runtime counted:
    /// the memory ladder needs the peak of an out-of-memory kill and the CPU of a
    /// timed-out or killed action, and a result built only from the error carries
    /// neither. Each error must put the usage in its Result's metadata and its
    /// counters on the `lease phases:` line; only a memory kill (the lease's own
    /// limit, or the node's) ends the line with `oom=1`. On the code before this change
    /// the runtime is never asked, so the Result has no `action_result` for an
    /// out-of-memory kill and the line says `mem_peak=-`.
    #[tokio::test]
    async fn a_lease_that_failed_still_reports_what_it_counted() {
        let usage = ResourceUsage {
            cpu_user_micros: 700,
            wall_micros: 2_000_000,
            peak_memory_bytes: 4096,
            cgroup: Some(worker::CgroupUsage {
                cpu_usage_micros: Some(1_000_000),
                memory_peak_bytes: Some(4096),
                ..worker::CgroupUsage::default()
            }),
            ..ResourceUsage::default()
        };
        let errors = [
            (
                RuntimeError::OutOfMemory {
                    used: 4096,
                    limit: 4096,
                },
                true,
            ),
            (RuntimeError::BusyNode("psi".into()), true),
            (
                RuntimeError::TimedOut {
                    stdout: None,
                    stderr: None,
                },
                false,
            ),
            (RuntimeError::Killed, false),
        ];
        for (error, oom) in errors {
            let name = failure_kind(&error);
            let runtime = Failing::measured(error, &usage);
            let (result, log) = finish_logged(runtime, false).await;
            let carried = result
                .action_result
                .as_ref()
                .and_then(ResourceUsage::in_result);
            assert_eq!(carried.as_ref(), Some(&usage), "{name}: {result:?}");
            let line = log
                .lines()
                .find(|l| l.contains("lease phases:"))
                .expect("a phases line");
            assert!(line.contains(" cpu_usage_us=1000000 "), "{name}: {line}");
            assert!(line.contains(" mem_peak=4096 "), "{name}: {line}");
            assert!(line.contains("parallelism=0.50"), "{name}: {line}");
            assert_eq!(line.ends_with(" oom=1"), oom, "{name}: {line}");
        }
    }

    /// Catches a failed lease whose runtime measured nothing getting a made-up result
    /// or counters: no usage in the Result, and `-` on the line, with no `oom` key.
    #[tokio::test]
    async fn a_failed_lease_without_a_measure_carries_none() {
        let (result, log) = finish_logged(Failing::with(RuntimeError::Killed), false).await;
        assert_eq!(result.action_result, None);
        let line = log
            .lines()
            .find(|l| l.contains("lease phases:"))
            .expect("line");
        assert!(line.ends_with("numa_spread=-"), "{line}");
    }

    /// Catches a reason that is cut mid-character, left unbounded, or left with
    /// newlines.
    #[test]
    fn a_reason_is_one_bounded_line() {
        assert_eq!(one_line("a\n  b\tc"), "a b c");
        let long = "é".repeat(REASON_CHARS + 50);
        let cut = one_line(&long);
        assert_eq!(cut.chars().count(), REASON_CHARS + 3);
        assert!(cut.ends_with("..."));
    }

    /// Catches a Start without an action being run anyway.
    #[test]
    fn a_start_without_an_action_is_no_work() {
        assert_eq!(work(id(), Start::default()), None);
    }

    /// Catches an outcome reported with the wrong status: a run killed while its lease
    /// still runs (no fence took it; the runtime stopped it) as anything but ABORTED,
    /// the farm's failure as the client's, or the reverse (a client error reported as
    /// INTERNAL would be retried as an infrastructure failure), and a timeout reported
    /// as OK, which would be cached; and the two memory kills not told apart (a busy
    /// node's kill read as the action's own would grow its booking for nothing), or
    /// any other outcome marked as a memory kill. Every arm in one test, so each code
    /// is checked against the others. Also catches (issue #45) a timeout's partial
    /// stdout or stderr dropped, swapped or sent without the other, and a result sent
    /// with a timeout that has no log to carry. Also (issue #22) a lost run reported
    /// without `lost`, or with the other reason, and any other outcome marked lost.
    #[test]
    fn each_outcome_has_its_status() {
        let id = LeaseId::new(3, 4);
        let ok = result_of(
            id,
            Ok(ActionResult {
                exit_code: 2,
                ..ActionResult::default()
            }),
        );
        assert_eq!(ok.lease_id, Some(proto_lease_id(id)));
        assert_eq!(ok.status, Some(Status::default()));
        assert_eq!(ok.memory_kill(), MemoryKill::Unspecified);
        assert_eq!(ok.action_result.map(|r| r.exit_code), Some(2));
        let codes = [
            (RuntimeError::Killed, Code::Aborted),
            (RuntimeError::Failed("disk".to_owned()), Code::Internal),
            (
                RuntimeError::Invalid("argv".to_owned()),
                Code::InvalidArgument,
            ),
            (
                RuntimeError::MissingBlob("ab/1".to_owned()),
                Code::FailedPrecondition,
            ),
            (
                RuntimeError::TimedOut {
                    stdout: None,
                    stderr: None,
                },
                Code::DeadlineExceeded,
            ),
            (
                RuntimeError::OutOfMemory {
                    used: 3 << 30,
                    limit: 2 << 30,
                },
                Code::ResourceExhausted,
            ),
            (
                RuntimeError::BusyNode("actions/ ran short".to_owned()),
                Code::Unavailable,
            ),
            (
                RuntimeError::Lost(LostWhy::DaemonRestarted, "restarted".to_owned()),
                Code::Unavailable,
            ),
            (
                RuntimeError::Lost(LostWhy::Fenced, "fenced".to_owned()),
                Code::Unavailable,
            ),
        ];
        for (error, code) in codes {
            let why = error.to_string();
            // Only the two memory kills say so, each with its own kind.
            let kill = match error {
                RuntimeError::OutOfMemory { .. } => MemoryKill::OwnLimit,
                RuntimeError::BusyNode(_) => MemoryKill::NodePressure,
                _ => MemoryKill::Unspecified,
            };
            let lost = match error {
                RuntimeError::Lost(LostWhy::DaemonRestarted, _) => worker::Lost::DaemonRestarted,
                RuntimeError::Lost(LostWhy::Fenced, _) => worker::Lost::Fenced,
                _ => worker::Lost::Unspecified,
            };
            let result = result_of(id, Err(error));
            assert_eq!(result.lease_id, Some(proto_lease_id(id)), "{why}");
            assert!(result.action_result.is_none(), "{why}");
            assert_eq!(result.memory_kill(), kill, "{why}");
            assert_eq!(result.lost(), lost, "{why}");
            if lost != worker::Lost::Unspecified {
                let message = result.status.as_ref().map(|s| s.message.as_str());
                assert_eq!(message, Some(why.as_str()), "the lost run's own words");
            }
            assert_eq!(result.status.map(|s| s.code), Some(code as i32), "{why}");
        }

        // A timeout carries the logs the action wrote before it was stopped, each in
        // its own field, and nothing else.
        let digest = |n: i64| Digest {
            hash: format!("{n:064x}"),
            size_bytes: n,
        };
        for (stdout, stderr) in [
            (Some(digest(1)), Some(digest(2))),
            (Some(digest(1)), None),
            (None, Some(digest(2))),
        ] {
            let timed_out = RuntimeError::TimedOut {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            };
            let result = result_of(id, Err(timed_out));
            let code = result.status.map(|s| s.code);
            assert_eq!(code, Some(Code::DeadlineExceeded as i32));
            let partial = ActionResult {
                stdout_digest: stdout,
                stderr_digest: stderr,
                ..ActionResult::default()
            };
            assert_eq!(result.action_result, Some(partial));
        }

        // The OOM status says how much was used and what the limit was.
        let oom = RuntimeError::OutOfMemory { used: 7, limit: 5 };
        let message = result_of(id, Err(oom)).status.map(|s| s.message);
        assert_eq!(
            message.as_deref(),
            Some("out of memory: the action used 7 bytes, past the lease's limit of 5")
        );
    }
}
