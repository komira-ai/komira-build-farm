//! Memory kills as the server sees them (failure classes, 6.1): the scheduler's outcome
//! for a daemon `Result` that names one, the attention line for a busy node's kill, and
//! what callers are told once the scheduler finishes an operation with one.
//!
//! The scheduler decides whether a killed run runs again (`kbf_types::MemoryRun`); the
//! server only translates. An operation finished by an own-limit kill was killed at the
//! largest node that could run it: its callers get FAILED_PRECONDITION, the
//! `status.message` saying the action needs more memory than any node offers, and a
//! `google.rpc.ErrorInfo` of reason [`OUT_OF_MEMORY_REASON`]. One finished by a busy
//! node's kills has used its farm reruns: INTERNAL, naming the node.

use kbf_front::{ERROR_DOMAIN, Finished};
use kbf_proto::google::rpc::{self, ErrorInfo};
use kbf_proto::worker::{self, MemoryKill};
use kbf_sched::Gib;
use kbf_types::{Failure, MemoryKill as Kill, MemoryRun, WorkerId};
use prost::Message;
use tonic::Code;

/// The `ErrorInfo.reason` of an action that needs more memory than any node offers.
pub const OUT_OF_MEMORY_REASON: &str = "ACTION_OUT_OF_MEMORY";

/// The scheduler's failure for `result`, if it is a memory kill: its status is not OK
/// and it names which memory ran out. `None` for every other `Result`, which its
/// status code alone decides.
pub(crate) fn killed(result: &worker::Result) -> Option<Failure> {
    let ok = result
        .status
        .as_ref()
        .is_none_or(|s| s.code == Code::Ok as i32);
    if ok {
        return None;
    }
    match MemoryKill::try_from(result.memory_kill) {
        Ok(MemoryKill::OwnLimit) => Some(Failure::OutOfMemory),
        Ok(MemoryKill::NodePressure) => Some(Failure::NodeMemoryPressure),
        Ok(MemoryKill::Unspecified) | Err(_) => None,
    }
}

/// Logs, for operators, a lease `node` killed under memory pressure: its `kills`
/// such kills so far.
pub(crate) fn pressure(node: &WorkerId, kills: u64) {
    tracing::warn!(
        target: "kbf_server::attention",
        %node,
        kills,
        "the node killed a lease for memory while the action was under its own limit \
         (node memory pressure)"
    );
}

/// What the callers of an operation the scheduler finished with `failure`, a memory
/// kill, are told; `runs` are its runs killed for memory, oldest first.
pub(crate) fn finished(failure: Failure, runs: &[MemoryRun]) -> Finished {
    let last = runs.last();
    let node = last.map_or("", |r| r.worker.as_str());
    let booked = Gib(last.map_or(0, |r| r.booked));
    let list: Vec<String> = runs.iter().map(run).collect();
    let runs_text = format!("Runs: {} ({})", runs.len(), list.join(", "));
    if failure == Failure::OutOfMemory {
        let message = format!(
            "kbf: the action needs more memory than any node offers: it passed its memory \
             limit with {booked} booked on {node}, the largest node for its platform. \
             {runs_text}."
        );
        let info = ErrorInfo {
            reason: OUT_OF_MEMORY_REASON.to_owned(),
            domain: ERROR_DOMAIN.to_owned(),
            metadata: [
                ("node".to_owned(), node.to_owned()),
                ("booked_bytes".to_owned(), booked.0.to_string()),
            ]
            .into(),
        };
        let detail = prost_types::Any {
            type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
            value: info.encode_to_vec(),
        };
        return Finished::Failed(rpc::Status {
            code: Code::FailedPrecondition as i32,
            message,
            details: vec![detail],
        });
    }
    let message = format!(
        "kbf farm fault on {node}: the node killed the action for memory while it was under \
         its own limit (node memory pressure), and its reruns are used up. Operator fix: \
         find what else holds memory on the node. {runs_text}."
    );
    Finished::Failed(rpc::Status {
        code: Code::Internal as i32,
        message,
        details: Vec::new(),
    })
}

/// One killed run as the callers read it: `2 GiB on node-a`, with how a busy node's
/// kill ended it.
fn run(run: &MemoryRun) -> String {
    let how = match run.kill {
        Kill::OwnLimit { .. } => "",
        Kill::NodePressure => ", node memory pressure",
    };
    format!("{} on {}{how}", Gib(run.booked), run.worker)
}
