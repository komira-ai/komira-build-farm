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
//! and then reports its outcome (ABORTED, killed) like any other run.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kbf_proto::google::rpc::precondition_failure::Violation;
use kbf_proto::google::rpc::{Code, PreconditionFailure, Status};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{self, MemoryKill, Start};
use kbf_types::{LeaseId, Resources};
use prost::Message;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::runtime::{RuntimeError, Work};
use crate::runtimes::{AnyRuntime, Runtimes};

/// A finished run: its lease and outcome.
pub(crate) type Done = (LeaseId, Result<ActionResult, RuntimeError>);

/// The leases a daemon is running.
pub(crate) struct Leases {
    runtimes: Runtimes,
    running: BTreeMap<LeaseId, Running>,
    /// Running leases being killed on the server's `Cancel`.
    cancelled: BTreeSet<LeaseId>,
    done: mpsc::UnboundedSender<Done>,
}

/// A running lease: its run's task and the runtime it runs on.
struct Running {
    task: JoinHandle<()>,
    runtime: Arc<dyn AnyRuntime>,
}

impl Leases {
    /// A manager that runs leases through `runtimes`, whose runs report on `done`.
    pub(crate) fn new(runtimes: Runtimes, done: mpsc::UnboundedSender<Done>) -> Self {
        Self {
            runtimes,
            running: BTreeMap::new(),
            cancelled: BTreeSet::new(),
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
        let task = tokio::spawn(async move {
            let outcome = run.run(work).await;
            // The receiver lives as long as the daemon.
            let _ = done.send((id, outcome));
        });
        tracing::info!(lease = %id, driver = runtime.driver(), "lease started");
        self.running.insert(id, Running { task, runtime });
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

    /// Turns a finished run into its Result. `None` if the lease was already reported
    /// (it was fenced).
    pub(crate) fn finished(
        &mut self,
        id: LeaseId,
        outcome: Result<ActionResult, RuntimeError>,
    ) -> Option<worker::Result> {
        self.running.remove(&id)?;
        // A cancelled run ends ABORTED (killed), or as it ended if it beat the kill;
        // the server refuses either.
        self.cancelled.remove(&id);
        tracing::info!(lease = %id, ok = outcome.is_ok(), "lease finished");
        Some(result_of(id, outcome))
    }

    /// Kills every running lease and returns one ABORTED Result for each, oldest
    /// first. The kills run together, so fencing takes as long as the slowest lease's
    /// kill, not the sum. Returns once every lease's work has stopped.
    pub(crate) async fn fence(&mut self) -> Vec<worker::Result> {
        let fenced = std::mem::take(&mut self.running);
        self.cancelled.clear();
        let kills = fenced.into_iter().map(|(id, running)| async move {
            let Running { task, runtime } = running;
            runtime.kill(id).await;
            // The run reports Killed on `done`; `finished` drops it, since the lease is
            // no longer running. Waiting for the task proves the work has ended.
            let _ = task.await;
            tracing::warn!(lease = %id, "lease fenced: contact with the server lost");
            failure(
                id,
                Code::Aborted,
                "self-fenced: no acknowledged heartbeat within the fence time",
            )
        });
        futures::future::join_all(kills).await
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
        },
        Err(RuntimeError::Killed) => failure(id, Code::Aborted, "killed"),
        Err(RuntimeError::Failed(why)) => failure(id, Code::Internal, why),
        Err(RuntimeError::Invalid(why)) => failure(id, Code::InvalidArgument, why),
        Err(RuntimeError::MissingBlob(blob)) => missing(id, &blob),
        Err(RuntimeError::TimedOut) => failure(
            id,
            Code::DeadlineExceeded,
            "the action ran past its timeout",
        ),
        Err(oom @ RuntimeError::OutOfMemory { .. }) => worker::Result {
            memory_kill: MemoryKill::OwnLimit as i32,
            ..failure(id, Code::ResourceExhausted, oom.to_string())
        },
        Err(busy @ RuntimeError::BusyNode(_)) => worker::Result {
            memory_kill: MemoryKill::NodePressure as i32,
            ..failure(id, Code::Unavailable, busy.to_string())
        },
    }
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
        // A missing input is not a memory kill.
        memory_kill: worker::MemoryKill::Unspecified as i32,
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
        // Not a memory kill; `result_of` sets the kind over this for the two that are.
        memory_kill: worker::MemoryKill::Unspecified as i32,
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
    /// ends; the fence must end, with one ABORTED Result per lease, oldest first.
    #[tokio::test]
    async fn fence_kills_every_lease_at_once() {
        let runtime = Arc::new(Rendezvous {
            kills: tokio::sync::Barrier::new(2),
            stopped: tokio::sync::Semaphore::new(0),
            runs: std::sync::atomic::AtomicUsize::new(0),
        });
        let (done, _done_rx) = mpsc::unbounded_channel();
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
        let results = tokio::time::timeout(std::time::Duration::from_secs(10), leases.fence())
            .await
            .expect("the fence killed one lease at a time");
        let fenced: Vec<_> = results.iter().map(|r| r.lease_id).collect();
        assert_eq!(fenced, ids.map(|id| Some(proto_lease_id(id))));
        assert!(results.iter().all(|r| code(r) == Code::Aborted as i32));
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
        let (done, _done_rx) = mpsc::unbounded_channel();
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
        let fenced = tokio::time::timeout(std::time::Duration::from_secs(5), leases.fence())
            .await
            .expect("the fence waited for a second run of one lease");
        assert_eq!(fenced.len(), 1);
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
        let (ended, outcome) = done_rx.recv().await.expect("the run ends");
        assert_eq!(ended, id());
        let result = leases.finished(ended, outcome).expect("reported");
        assert_eq!(code(&result), Code::Aborted as i32);
        assert_eq!(runtime.killed(), [id()]);
        assert!(leases.running().is_empty());
        assert!(
            leases.cancelled.is_empty(),
            "a finished lease is still cancelled"
        );
        assert!(!leases.cancel(id()), "a finished lease");
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
    /// is checked against the others.
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
            (RuntimeError::TimedOut, Code::DeadlineExceeded),
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
        ];
        for (error, code) in codes {
            let why = error.to_string();
            // Only the two memory kills say so, each with its own kind.
            let kill = match error {
                RuntimeError::OutOfMemory { .. } => MemoryKill::OwnLimit,
                RuntimeError::BusyNode(_) => MemoryKill::NodePressure,
                _ => MemoryKill::Unspecified,
            };
            let result = result_of(id, Err(error));
            assert_eq!(result.lease_id, Some(proto_lease_id(id)), "{why}");
            assert!(result.action_result.is_none(), "{why}");
            assert_eq!(result.memory_kill(), kill, "{why}");
            assert_eq!(result.status.map(|s| s.code), Some(code as i32), "{why}");
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
