//! The server side of `kbf.worker.v1`: one `Session` stream per daemon.
//!
//! Under mutual TLS a stream without a usable client certificate is refused before its
//! first message is read. The first message must be `Hello`. It is checked (protocol
//! version, node id, that the certificate names that node and neither is denied: see
//! [`identity`](crate::identity) and issue #79; the capacity entries of the node report,
//! and the entries placement matches platforms against: `arch` and the rest that
//! [`caps`] reads), answered with `Welcome`, and registers the node: only this first
//! `Hello` does (issue #25). `Welcome` names this process's term as its lease epoch
//! (issue #137). The registration names the daemon process by the `Hello`'s
//! `instance_id`: leases whose `Start` went to another process are given up only once it
//! has fenced (issue #140). On the stream after that:
//! - a resent `Hello` whose `node_id` is not the stream's node, or that the deny list
//!   now refuses, ends the stream with that error; otherwise it changes the node's
//!   capacity and capabilities and nothing else;
//! - a `Heartbeat` the deny list now refuses ends the stream. Otherwise it is fed to
//!   the scheduler and acknowledged, unless a newer stream of
//!   the same node has registered since: then it is dropped unacknowledged, so a
//!   daemon still talking on the old stream fences on time. A lease it lists that the
//!   scheduler no longer holds on the node is sent a `Cancel` (issue #23);
//! - a `Result` the deny list now refuses ends the stream, so it never reaches the
//!   action cache. Otherwise it is accepted only from the node holding the
//!   operation's current lease, and only if the action it names (if any) is the one
//!   that lease runs, and answered with a `ResultAck`;
//! - a `NodeStatus` the deny list now refuses ends the stream, so it never reaches
//!   the operator API. Otherwise it is kept as the node's newest software status,
//!   unless a newer stream of the node has registered since;
//! - after the server ends a stream, nothing more is read from it;
//! - an `Offer` is not read yet.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, stream};
use kbf_caps::NodeCaps;
use kbf_front::MetaLog;
use kbf_objstore::ObjectStore;
use kbf_proto::worker::{
    Capability, DaemonMessage, HeartbeatAck, Hello, ServerMessage, Welcome, daemon_message,
    server_message, worker_server::Worker,
};
use kbf_sched::DaemonInstance;
use kbf_types::{LeaseId, Resources, WorkerId};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tonic::{Request, Response, Status, Streaming};

use crate::farm::{Farm, Outbound, StreamId};
use crate::identity::{PeerCert, Peers};

/// The `kbf.worker.v1` version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest version it accepts. The protocol promises N-1 and N; version 1 is the
/// first, so there is no N-1 yet.
pub const OLDEST_PROTOCOL_VERSION: u32 = 1;

/// The `Worker` service over a [`Farm`].
#[derive(Debug)]
pub struct WorkerService<M, O> {
    farm: Arc<Farm<M, O>>,
    peers: Peers,
    heartbeat_interval: Duration,
    hello_wait: Duration,
}

impl<M, O> WorkerService<M, O> {
    /// The service over `farm`, knowing its daemons as `peers`. `Welcome` asks daemons
    /// for a heartbeat every `heartbeat_interval`; a stream that sends no `Hello` within
    /// `hello_wait` is ended DEADLINE_EXCEEDED.
    pub const fn new(
        farm: Arc<Farm<M, O>>,
        peers: Peers,
        heartbeat_interval: Duration,
        hello_wait: Duration,
    ) -> Self {
        Self {
            farm,
            peers,
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
        let certs = request.peer_certs();
        let leaf = certs.as_deref().and_then(|chain| chain.first());
        let peer = self.peers.peer(leaf.map(AsRef::as_ref))?;
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
        self.peers.admit(peer.as_ref(), &hello.node_id).await?;
        let capacity = capacity(&hello).map_err(Status::invalid_argument)?;
        let caps = caps(&hello).map_err(Status::invalid_argument)?;
        let worker = WorkerId::new(hello.node_id);
        let instance = DaemonInstance::new(hello.instance_id);
        let welcome = message(server_message::Message::Welcome(Welcome {
            protocol_version: version,
            heartbeat_interval_ms: u64::try_from(self.heartbeat_interval.as_millis())
                .unwrap_or(u64::MAX),
            epoch: self.farm.term(),
        }));
        let (outbound, responses) = mpsc::unbounded_channel();
        let stream =
            self.farm
                .register(&worker, instance, capacity, caps, outbound.clone(), welcome);
        tracing::info!(%worker, ?capacity, "worker registered");
        let session = Session {
            farm: Arc::clone(&self.farm),
            peers: self.peers.clone(),
            peer,
            worker,
            stream,
        };
        tokio::spawn(session.serve(inbound, outbound));
        Ok(Response::new(Box::pin(stream::unfold(
            responses,
            |mut responses| async move { responses.recv().await.map(|m| (m, responses)) },
        ))))
    }
}

/// One stream after its first `Hello`.
struct Session<M, O> {
    farm: Arc<Farm<M, O>>,
    peers: Peers,
    /// The stream's client certificate (`None` in plain text).
    peer: Option<PeerCert>,
    worker: WorkerId,
    stream: StreamId,
}

impl<M: MetaLog, O: ObjectStore> Session<M, O> {
    /// Handles the stream's messages until it ends, or one of them ends it.
    async fn serve(self, mut inbound: Streaming<DaemonMessage>, outbound: Outbound) {
        let worker = &self.worker;
        while let Ok(Some(received)) = inbound.message().await {
            // The response stream outlives this loop unless the stream is gone.
            match self.handle(received.message).await {
                Ok(None) => {}
                Ok(Some(reply)) => {
                    let _ = outbound.send(Ok(message(reply)));
                }
                Err(status) => {
                    tracing::warn!(%worker, %status, "worker stream ended by the server");
                    let _ = outbound.send(Err(status));
                    break;
                }
            }
        }
        // The farm keeps the stream as the worker's until it registers again: `Start`s
        // sent meanwhile are lost, and the scheduler gives their leases up (at once on
        // the next session's first heartbeat, or after G).
        tracing::info!(%worker, "worker stream ended");
    }

    /// The reply to one message, if any; an error ends the stream with it.
    async fn handle(
        &self,
        received: Option<daemon_message::Message>,
    ) -> Result<Option<server_message::Message>, Status> {
        let (farm, worker, stream) = (&self.farm, &self.worker, self.stream);
        let reply = match received {
            Some(daemon_message::Message::Hello(hello)) => {
                if hello.node_id != worker.as_str() {
                    return Err(Status::permission_denied(format!(
                        "a resent Hello names node {:?}; this stream is {worker}'s",
                        hello.node_id
                    )));
                }
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                match capacity(&hello).and_then(|capacity| Ok((capacity, caps(&hello)?))) {
                    Ok((capacity, caps)) => farm.resize(worker, stream, capacity, caps),
                    Err(e) => tracing::warn!(%worker, error = %e, "a resent Hello ignored"),
                }
                None
            }
            Some(daemon_message::Message::Heartbeat(beat)) => {
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                let running = beat
                    .running
                    .iter()
                    .map(|l| LeaseId::new(l.term, l.seq))
                    .collect();
                farm.heartbeat(worker, stream, beat.seq, running).then_some(
                    server_message::Message::HeartbeatAck(HeartbeatAck { seq: beat.seq }),
                )
            }
            Some(daemon_message::Message::Result(result)) => {
                // A revoked daemon must not write its result into the action cache,
                // even before its next heartbeat.
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                farm.report(worker, result)
                    .await
                    .map(server_message::Message::ResultAck)
            }
            Some(daemon_message::Message::NodeStatus(status)) => {
                // A revoked daemon's status must not reach the operator API either.
                self.peers
                    .admit(self.peer.as_ref(), worker.as_str())
                    .await?;
                // Logged there; the lines are for tests.
                let _ = farm.node_status(worker, stream, status);
                None
            }
            Some(daemon_message::Message::Offer(_)) => None,
            None => {
                tracing::warn!(%worker, "an empty daemon message ignored");
                None
            }
        };
        Ok(reply)
    }
}

fn message(message: server_message::Message) -> ServerMessage {
    ServerMessage {
        message: Some(message),
    }
}

/// What placement may book on a node: its `cpus` and `mem_gib` entries, and its `gpu`
/// entry (0 when absent: a daemon that does not detect GPUs has none to book). v0 books
/// the whole machine; the protected floors (RFC 4.3) are subtracted once they are
/// reported.
///
/// # Errors
/// `cpus` or `mem_gib` is missing; an entry is repeated or not a whole number.
pub fn capacity(hello: &Hello) -> Result<Resources, String> {
    let required = |key| entry(&hello.capabilities, key)?.ok_or_else(|| missing(key));
    let cpus = required("cpus")?;
    let mem_gib = required("mem_gib")?;
    let gpus = entry(&hello.capabilities, "gpu")?.unwrap_or(0);
    Ok(Resources::new(cpus.saturating_mul(1_000), mem_gib.saturating_mul(1 << 30)).with_gpus(gpus))
}

/// What placement matches an action's platform against: the node report read by
/// `kbf_caps::NodeCaps::from_report` (`arch`, `cpu.features`, `os`, labels, ...).
///
/// # Errors
/// `arch` is missing, repeated or unknown; another single-valued entry is repeated; a
/// countable entry is not a whole number.
pub fn caps(hello: &Hello) -> Result<NodeCaps, String> {
    let entries = hello
        .capabilities
        .iter()
        .map(|c| (c.key.as_str(), c.value.as_str()));
    NodeCaps::from_report(entries).map_err(|e| e.to_string())
}

fn missing(key: &str) -> String {
    format!("the node report needs exactly one {key:?} entry")
}

/// The value of the `key` entry, `None` if there is none.
fn entry(capabilities: &[Capability], key: &str) -> Result<Option<u64>, String> {
    let mut values = capabilities.iter().filter(|c| c.key == key);
    let (only, None) = (values.next(), values.next()) else {
        return Err(missing(key));
    };
    only.map(|only| {
        only.value.parse().map_err(|_| {
            format!(
                "node report entry {key}={:?} is not a whole number",
                only.value
            )
        })
    })
    .transpose()
}
