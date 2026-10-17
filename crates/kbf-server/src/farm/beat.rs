//! A worker's heartbeat as the farm takes it: fed to the scheduler, its leases not held
//! here cancelled, and the callers of an operation it finished answered.

use kbf_proto::worker::{Cancel, ServerMessage, server_message};
use kbf_sched::Event;
use kbf_types::{FarmTime, LeaseId, WorkerId};

use super::{Farm, Settled, State, StreamId, wire_lease};

impl Farm {
    /// Heartbeat `seq` on `stream`. Returns whether it was taken (and is to be
    /// acknowledged): a heartbeat from a replaced stream is dropped. Each lease it
    /// lists that the scheduler no longer holds on `worker` is sent a `Cancel`. A lease
    /// it leaves out that a proven restart lost past its operation's farm reruns
    /// finishes the operation: its callers are answered (nothing is cached).
    pub async fn heartbeat(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        seq: u64,
        running: Vec<LeaseId>,
    ) -> bool {
        let now = self.now();
        let taken = self
            .lock()
            .take_heartbeat(now, worker, stream, seq, running);
        let Some(settled) = taken else {
            return false;
        };
        for settled in settled {
            self.deliver(settled).await;
        }
        true
    }
}

impl State {
    /// [`Farm::heartbeat`] under the lock: `None` for a heartbeat from a replaced
    /// stream, else what its callers are told, for each operation it finished. Not
    /// generic, unlike `Farm::heartbeat`, so every test binary's runs count toward the
    /// same function's coverage.
    fn take_heartbeat(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        stream: StreamId,
        seq: u64,
        running: Vec<LeaseId>,
    ) -> Option<Vec<Settled>> {
        let link = self.links.get_mut(worker).filter(|l| l.stream == stream)?;
        link.newest_beat = link.newest_beat.max(seq);
        let outbound = link.outbound.clone();
        let event = Event::Heartbeat {
            worker: worker.clone(),
            running: running.clone(),
        };
        let answers = self.feed(now, event);
        self.renew_session(now, worker);
        for lease in self.sched.not_held(worker, &running) {
            tracing::info!(%worker, %lease, "a lease not held here is listed: cancelled");
            let cancel = server_message::Message::Cancel(Cancel {
                lease_id: Some(wire_lease(lease)),
            });
            // A stream that has just ended drops it; the next heartbeat listing the
            // lease, on the next stream, sends it again.
            let _ = outbound.send(Ok(ServerMessage {
                message: Some(cancel),
            }));
        }
        Some(answers.iter().map(|a| self.settle(a, None)).collect())
    }
}
