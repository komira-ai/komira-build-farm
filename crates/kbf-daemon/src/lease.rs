//! The lease manager: starts work on `Start` and only on `Start`, turns each outcome
//! into one `Result`, and fences (kills) running work when contact is lost.
//!
//! A `LeaseOffer` is the scheduler placing a lease before it commits it; running on an
//! offer could run a lease the scheduler never commits, or run it twice beside the
//! node it is placed on instead. So an offer is acknowledged in the log and nothing
//! runs until the committed `Start` arrives.
//!
//! v0 fences every lease (`SELF_FENCE`); the `RUN_ON` policy for hermetic actions
//! arrives when Start carries a fence policy.

use std::collections::BTreeMap;
use std::sync::Arc;

use kbf_proto::google::rpc::{Code, Status};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{self, Start};
use kbf_types::{LeaseId, Resources};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::runtime::{Runtime, RuntimeError, Work};

/// A finished run: its lease and outcome.
pub(crate) type Done = (LeaseId, Result<ActionResult, RuntimeError>);

/// The leases a daemon is running.
pub(crate) struct Leases<R> {
    runtime: Arc<R>,
    running: BTreeMap<LeaseId, JoinHandle<()>>,
    done: mpsc::UnboundedSender<Done>,
}

impl<R: Runtime> Leases<R> {
    /// A manager whose runs report on `done`.
    pub(crate) fn new(runtime: Arc<R>, done: mpsc::UnboundedSender<Done>) -> Self {
        Self {
            runtime,
            running: BTreeMap::new(),
            done,
        }
    }

    /// The leases running now, oldest first.
    pub(crate) fn running(&self) -> Vec<LeaseId> {
        self.running.keys().copied().collect()
    }

    /// Handles a Start. Returns a Result to send at once when the lease is refused; a
    /// started lease reports through `done`. A Start for a lease already running is a
    /// resend and changes nothing.
    pub(crate) fn start(&mut self, start: Start) -> Option<worker::Result> {
        let Some(id) = start.lease_id.map(lease_id) else {
            tracing::warn!("Start without a lease id ignored");
            return None;
        };
        if self.running.contains_key(&id) {
            return None;
        }
        if !self.runtime.serves(&start.kind) {
            return Some(failure(
                id,
                Code::FailedPrecondition,
                format!("no driver here serves lease kind {:?}", start.kind),
            ));
        }
        let Some(work) = work(id, start) else {
            return Some(failure(
                id,
                Code::InvalidArgument,
                "Start has no action digest",
            ));
        };
        let runtime = Arc::clone(&self.runtime);
        let done = self.done.clone();
        let task = tokio::spawn(async move {
            let outcome = runtime.run(work).await;
            // The receiver lives as long as the daemon.
            let _ = done.send((id, outcome));
        });
        tracing::info!(lease = %id, "lease started");
        self.running.insert(id, task);
        None
    }

    /// Turns a finished run into its Result. `None` if the lease was already reported
    /// (it was fenced).
    pub(crate) fn finished(
        &mut self,
        id: LeaseId,
        outcome: Result<ActionResult, RuntimeError>,
    ) -> Option<worker::Result> {
        self.running.remove(&id)?;
        tracing::info!(lease = %id, ok = outcome.is_ok(), "lease finished");
        Some(result_of(id, outcome))
    }

    /// Kills every running lease and returns one ABORTED Result for each, oldest
    /// first. The kills run together, so fencing takes as long as the slowest lease's
    /// kill, not the sum. Returns once every lease's work has stopped.
    pub(crate) async fn fence(&mut self) -> Vec<worker::Result> {
        let fenced = std::mem::take(&mut self.running);
        let runtime = &*self.runtime;
        let kills = fenced.into_iter().map(|(id, task)| async move {
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
fn result_of(
    id: LeaseId,
    outcome: std::result::Result<ActionResult, RuntimeError>,
) -> worker::Result {
    match outcome {
        Ok(action_result) => worker::Result {
            lease_id: Some(proto_lease_id(id)),
            status: Some(Status::default()),
            action_result: Some(action_result),
        },
        Err(RuntimeError::Killed) => failure(id, Code::Aborted, "killed"),
        Err(RuntimeError::Failed(why)) => failure(id, Code::Internal, why),
        Err(RuntimeError::Invalid(why)) => failure(id, Code::InvalidArgument, why),
        Err(RuntimeError::TimedOut) => failure(
            id,
            Code::DeadlineExceeded,
            "the action ran past its timeout",
        ),
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
    }

    impl Runtime for Rendezvous {
        fn driver(&self) -> &'static str {
            "rendezvous"
        }

        fn serves(&self, _kind: &str) -> bool {
            true
        }

        async fn run(&self, _work: Work) -> Result<ActionResult, RuntimeError> {
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
        });
        let (done, _done_rx) = mpsc::unbounded_channel();
        let mut leases = Leases::new(runtime, done);
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

    /// Catches a Start without an action being run anyway.
    #[test]
    fn a_start_without_an_action_is_no_work() {
        assert_eq!(work(id(), Start::default()), None);
    }

    /// Catches an outcome reported under the wrong status: a client error reported as
    /// INTERNAL would be retried as an infrastructure failure, and a timeout reported
    /// as OK would be cached.
    #[test]
    fn each_outcome_maps_to_its_status() {
        let ok = result_of(id(), Ok(ActionResult::default()));
        assert_eq!(code(&ok), Code::Ok as i32);
        assert!(ok.action_result.is_some());
        let cases = [
            (RuntimeError::Killed, Code::Aborted),
            (RuntimeError::Failed("x".to_owned()), Code::Internal),
            (
                RuntimeError::Invalid("tag".to_owned()),
                Code::InvalidArgument,
            ),
            (RuntimeError::TimedOut, Code::DeadlineExceeded),
        ];
        for (error, want) in cases {
            let result = result_of(id(), Err(error));
            assert_eq!(code(&result), want as i32);
            assert!(result.action_result.is_none());
            assert_eq!(result.lease_id, Some(proto_lease_id(id())));
        }
    }
}
