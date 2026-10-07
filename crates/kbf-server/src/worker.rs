//! The server side of `kbf.worker.v1`: one `Session` stream per daemon.
//!
//! The first message must be `Hello`. It is checked (protocol version, node id, the
//! capacity entries of the node report), answered with `Welcome`, and registers the
//! node: only this first `Hello` does (issue #25). On the stream after that:
//! - a resent `Hello` changes the node's capacity and nothing else;
//! - a `Heartbeat` is fed to the scheduler and acknowledged, unless a newer stream of
//!   the same node has registered since: then it is dropped unacknowledged, so a
//!   daemon still talking on the old stream fences on time. A lease it lists that the
//!   scheduler no longer holds on the node is sent a `Cancel` (issue #23);
//! - a `Result` is accepted only from the node holding the operation's current lease,
//!   and answered with a `ResultAck`;
//! - an `Offer` is not read yet.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, stream};
use kbf_front::MetaLog;
use kbf_objstore::ObjectStore;
use kbf_proto::worker::{
    Capability, DaemonMessage, HeartbeatAck, Hello, ServerMessage, Welcome, daemon_message,
    server_message, worker_server::Worker,
};
use kbf_types::{LeaseId, Resources, WorkerId};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tonic::{Request, Response, Status, Streaming};

use crate::farm::{Farm, Outbound, StreamId};

/// The `kbf.worker.v1` version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest version it accepts. The protocol promises N-1 and N; version 1 is the
/// first, so there is no N-1 yet.
pub const OLDEST_PROTOCOL_VERSION: u32 = 1;

/// The `Worker` service over a [`Farm`].
#[derive(Debug)]
pub struct WorkerService<M, O> {
    farm: Arc<Farm<M, O>>,
    heartbeat_interval: Duration,
    hello_wait: Duration,
}

impl<M, O> WorkerService<M, O> {
    /// The service over `farm`. `Welcome` asks daemons for a heartbeat every
    /// `heartbeat_interval`; a stream that sends no `Hello` within `hello_wait` is
    /// ended DEADLINE_EXCEEDED.
    pub const fn new(
        farm: Arc<Farm<M, O>>,
        heartbeat_interval: Duration,
        hello_wait: Duration,
    ) -> Self {
        Self {
            farm,
            heartbeat_interval,
            hello_wait,
        }
    }
}

/// The server's half of a session.
pub type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

#[tonic::async_trait]
impl<M, O> Worker for WorkerService<M, O>
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    type SessionStream = SessionStream;

    async fn session(
        &self,
        request: Request<Streaming<DaemonMessage>>,
    ) -> Result<Response<SessionStream>, Status> {
        let mut inbound = request.into_inner();
        let first = timeout(self.hello_wait, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no Hello within the wait"))??;
        let Some(daemon_message::Message::Hello(hello)) = first.and_then(|m| m.message) else {
            return Err(Status::invalid_argument(
                "the first message of a session must be Hello",
            ));
        };
        let version = hello.protocol_version;
        if !(OLDEST_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version) {
            return Err(Status::failed_precondition(format!(
                "protocol version {version} is not accepted; this server speaks \
                 {OLDEST_PROTOCOL_VERSION} to {PROTOCOL_VERSION}"
            )));
        }
        if hello.node_id.is_empty() {
            return Err(Status::invalid_argument("Hello has no node_id"));
        }
        let capacity = capacity(&hello).map_err(Status::invalid_argument)?;
        let worker = WorkerId::new(hello.node_id);
        let welcome = message(server_message::Message::Welcome(Welcome {
            protocol_version: version,
            heartbeat_interval_ms: u64::try_from(self.heartbeat_interval.as_millis())
                .unwrap_or(u64::MAX),
        }));
        let (outbound, responses) = mpsc::unbounded_channel();
        let stream = self
            .farm
            .register(&worker, capacity, outbound.clone(), welcome);
        tracing::info!(%worker, ?capacity, "worker registered");
        let farm = Arc::clone(&self.farm);
        tokio::spawn(serve(farm, worker, stream, inbound, outbound));
        Ok(Response::new(Box::pin(stream::unfold(
            responses,
            |mut responses| async move { responses.recv().await.map(|m| (m, responses)) },
        ))))
    }
}

/// Handles one stream's messages after its first `Hello`, until it ends.
async fn serve<M: MetaLog, O: ObjectStore>(
    farm: Arc<Farm<M, O>>,
    worker: WorkerId,
    stream: StreamId,
    mut inbound: Streaming<DaemonMessage>,
    outbound: Outbound,
) {
    loop {
        let Ok(Some(received)) = inbound.message().await else {
            break;
        };
        let reply = match received.message {
            Some(daemon_message::Message::Hello(hello)) => match capacity(&hello) {
                Ok(capacity) => {
                    farm.resize(&worker, stream, capacity);
                    None
                }
                Err(e) => {
                    tracing::warn!(%worker, error = %e, "a resent Hello ignored");
                    None
                }
            },
            Some(daemon_message::Message::Heartbeat(beat)) => {
                let running = beat
                    .running
                    .iter()
                    .map(|l| LeaseId::new(l.term, l.seq))
                    .collect();
                farm.heartbeat(&worker, stream, beat.seq, running)
                    .then_some(server_message::Message::HeartbeatAck(HeartbeatAck {
                        seq: beat.seq,
                    }))
            }
            Some(daemon_message::Message::Result(result)) => farm
                .report(&worker, result)
                .await
                .map(server_message::Message::ResultAck),
            Some(daemon_message::Message::Offer(_)) => None,
            None => {
                tracing::warn!(%worker, "an empty daemon message ignored");
                None
            }
        };
        if let Some(reply) = reply {
            // The response stream outlives this loop unless the stream is gone.
            let _ = outbound.send(Ok(message(reply)));
        }
    }
    // The farm keeps the stream as the worker's until it registers again: `Start`s
    // sent meanwhile are lost, and the scheduler gives their leases up (at once on the
    // next session's first heartbeat, or after G).
    tracing::info!(%worker, "worker stream ended");
}

fn message(message: server_message::Message) -> ServerMessage {
    ServerMessage {
        message: Some(message),
    }
}

/// What placement may book on a node: its `cpus` and `mem_gib` entries. v0 books the
/// whole machine; the protected floors (RFC 4.3) are subtracted once they are reported.
///
/// # Errors
/// An entry is missing, repeated or not a whole number.
pub fn capacity(hello: &Hello) -> Result<Resources, String> {
    let cpus = entry(&hello.capabilities, "cpus")?;
    let mem_gib = entry(&hello.capabilities, "mem_gib")?;
    Ok(Resources::new(
        cpus.saturating_mul(1_000),
        mem_gib.saturating_mul(1 << 30),
    ))
}

fn entry(capabilities: &[Capability], key: &str) -> Result<u64, String> {
    let mut values = capabilities.iter().filter(|c| c.key == key);
    let (Some(only), None) = (values.next(), values.next()) else {
        return Err(format!("the node report needs exactly one {key:?} entry"));
    };
    only.value.parse().map_err(|_| {
        format!(
            "node report entry {key}={:?} is not a whole number",
            only.value
        )
    })
}
