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

use kbf_proto::google::rpc::precondition_failure::Violation;
use kbf_proto::google::rpc::{Code, PreconditionFailure, Status};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{self, Start};
use kbf_types::LeaseId;
use prost::Message;
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
        let Some(action_digest) = start.action_digest else {
            return Some(failure(
                id,
                Code::InvalidArgument,
                "Start has no action digest",
            ));
        };
        let work = Work {
            lease_id: id,
            kind: start.kind,
            action_digest,
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

    /// Kills every running lease and returns one ABORTED Result for each. Returns
    /// once every lease's work has stopped.
    pub(crate) async fn fence(&mut self) -> Vec<worker::Result> {
        let fenced = std::mem::take(&mut self.running);
        let mut results = Vec::with_capacity(fenced.len());
        for (id, task) in fenced {
            self.runtime.kill(id).await;
            // The run reports Killed on `done`; `finished` drops it, since the lease is
            // no longer running. Waiting for the task proves the work has ended.
            let _ = task.await;
            tracing::warn!(lease = %id, "lease fenced: contact with the server lost");
            results.push(failure(
                id,
                Code::Aborted,
                "self-fenced: no acknowledged heartbeat within the fence time",
            ));
        }
        results
    }
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
        },
        Err(RuntimeError::Killed) => failure(id, Code::Aborted, "killed"),
        Err(RuntimeError::Failed(why)) => failure(id, Code::Internal, why),
        Err(RuntimeError::Invalid(why)) => failure(id, Code::InvalidArgument, why),
        Err(RuntimeError::MissingBlob(blob)) => missing(id, &blob),
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
