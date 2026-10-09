//! What the server adds to an accepted result's `ExecutedActionMetadata` (issue #166):
//! the node that ran it, when it was queued, and the times the daemon left out that
//! the server saw for itself.

use std::time::SystemTime;

use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_types::WorkerId;
use prost_types::Timestamp;

/// The server's own times for one run of an operation, on the wall clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    /// When the operation's first caller submitted it.
    pub(crate) queued: SystemTime,
    /// When the `Start` of the lease that ran it was sent.
    pub(crate) started: SystemTime,
}

impl Stamp {
    /// Writes into `result`'s metadata, which is made if it has none:
    ///
    /// - `worker`: `node`, the node id the server holds the lease under, replacing
    ///   whatever the daemon wrote;
    /// - `queued_timestamp`: when the operation was submitted. Only the server knows
    ///   it; a requeued operation keeps its first submission's time;
    /// - `worker_start_timestamp`, if the daemon left it out: when the `Start` was sent;
    /// - `worker_completed_timestamp`, if the daemon left it out: `received`, when the
    ///   server took the result in.
    ///
    /// The daemon's own timestamps are kept: it saw its run more closely.
    pub(crate) fn apply(self, result: &mut ActionResult, node: &WorkerId, received: SystemTime) {
        let metadata = result
            .execution_metadata
            .get_or_insert_with(ExecutedActionMetadata::default);
        node.as_str().clone_into(&mut metadata.worker);
        metadata.queued_timestamp = Some(Timestamp::from(self.queued));
        metadata
            .worker_start_timestamp
            .get_or_insert_with(|| Timestamp::from(self.started));
        metadata
            .worker_completed_timestamp
            .get_or_insert_with(|| Timestamp::from(received));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn secs(n: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(n)
    }

    fn ts(n: u64) -> Option<Timestamp> {
        Some(Timestamp::from(secs(n)))
    }

    const STAMP: Stamp = Stamp {
        queued: SystemTime::UNIX_EPOCH,
        started: SystemTime::UNIX_EPOCH,
    };

    /// Catches: a result without metadata left without it; `worker` or
    /// `queued_timestamp` not set; and a missing worker start or completion time not
    /// filled from what the server saw.
    #[test]
    fn a_result_without_metadata_gets_the_server_s() {
        let stamp = Stamp {
            queued: secs(10),
            started: secs(12),
        };
        let mut result = ActionResult::default();
        stamp.apply(&mut result, &WorkerId::new("node-a"), secs(20));
        let metadata = result.execution_metadata.expect("metadata");
        assert_eq!(metadata.worker, "node-a");
        assert_eq!(metadata.queued_timestamp, ts(10));
        assert_eq!(metadata.worker_start_timestamp, ts(12));
        assert_eq!(metadata.worker_completed_timestamp, ts(20));
    }

    /// Catches: the daemon's own run times overwritten by the server's coarser ones,
    /// the daemon's other metadata dropped, and a `worker` or `queued_timestamp` the
    /// daemon wrote kept (only the server knows either).
    #[test]
    fn the_daemon_s_times_are_kept_and_worker_and_queued_replaced() {
        let mut result = ActionResult {
            execution_metadata: Some(ExecutedActionMetadata {
                worker: "claimed".to_owned(),
                queued_timestamp: ts(1),
                worker_start_timestamp: ts(3),
                execution_start_timestamp: ts(4),
                worker_completed_timestamp: ts(5),
                ..ExecutedActionMetadata::default()
            }),
            ..ActionResult::default()
        };
        let stamp = Stamp {
            queued: secs(2),
            ..STAMP
        };
        stamp.apply(&mut result, &WorkerId::new("node-b"), secs(9));
        let metadata = result.execution_metadata.expect("metadata");
        assert_eq!(metadata.worker, "node-b");
        assert_eq!(metadata.queued_timestamp, ts(2));
        assert_eq!(metadata.worker_start_timestamp, ts(3));
        assert_eq!(metadata.execution_start_timestamp, ts(4));
        assert_eq!(metadata.worker_completed_timestamp, ts(5));
    }
}
