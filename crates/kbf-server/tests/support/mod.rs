//! An in-process single-node server on loopback ports, REAPI clients for it, and a
//! scripted fake daemon speaking `kbf.worker.v1` in plain text.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::future::pending;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedSender, unbounded};
use kbf_front::{Cache, MAX_MESSAGE_BYTES, MemoryMetaLog, MetaLog, MetaLogError};
use kbf_meta::{Applied, Command as MetaCommand, MetaState, Retention};
use kbf_objstore::{Capabilities, KeyPrefix, MemoryStore};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
use kbf_proto::reapi::execution_client::ExecutionClient;
use kbf_proto::reapi::{
    self, Action, ActionResult, BatchUpdateBlobsRequest, Command, Directory, ExecuteRequest,
    ExecuteResponse, FileNode, FindMissingBlobsRequest, GetActionResultRequest, OutputFile,
    Platform, batch_update_blobs_request, platform,
};
use kbf_proto::worker::{
    Capability, DaemonMessage, Heartbeat, Hello, LeaseId, LeaseOffer, ResultAck, ServerMessage,
    Start, daemon_message, server_message, worker_client::WorkerClient,
};
use kbf_server::{Listeners, bind_server};
use prost::Message;
use tokio::sync::Notify;
use tokio::time::timeout;
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Streaming};

/// How long a test waits for something that should happen promptly.
pub const PROMPT: Duration = Duration::from_secs(5);

/// How long a test watches for something that must not happen. Several ticks and
/// heartbeat intervals.
pub const QUIET: Duration = Duration::from_millis(400);

/// How long the test server waits for a stream's `Hello`.
pub const HELLO_WAIT: Duration = Duration::from_millis(300);

/// The heartbeat interval the test server names in `Welcome`.
pub const INTERVAL: Duration = Duration::from_millis(100);

/// The in-memory metadata log, with a gate a test can close on the next query: the
/// query waits there until the test opens it, so the test can act while the server is
/// in the middle of a read.
#[derive(Debug)]
pub struct GateLog {
    inner: MemoryMetaLog,
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl GateLog {
    fn new() -> Self {
        Self {
            inner: MemoryMetaLog::new(Retention::default()),
            armed: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        }
    }

    /// Holds the next query at the gate.
    pub fn close_on_next_query(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    /// Waits until a query is held at the gate.
    pub async fn held(&self) {
        timeout(PROMPT, self.entered.notified())
            .await
            .expect("a query reaches the gate");
    }

    /// Lets the held query through.
    pub fn open(&self) {
        self.release.notify_one();
    }
}

impl MetaLog for GateLog {
    async fn commit(&self, command: MetaCommand) -> Result<Applied, MetaLogError> {
        self.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.query(f).await
    }
}

/// A running server and channels to it.
pub struct Cell {
    pub cache: Arc<Cache<GateLog, MemoryStore>>,
    pub worker_addr: SocketAddr,
    channel: Channel,
}

impl Cell {
    pub async fn start() -> Self {
        let cache = Arc::new(Cache::new(
            GateLog::new(),
            MemoryStore::new(Capabilities::default()),
            KeyPrefix::default(),
        ));
        let listeners = Listeners {
            reapi: SocketAddr::from(([127, 0, 0, 1], 0)),
            worker: SocketAddr::from(([127, 0, 0, 1], 0)),
            worker_tls: None,
            heartbeat_interval: INTERVAL,
            hello_wait: HELLO_WAIT,
            tick: Duration::from_millis(50),
        };
        let bound = bind_server(Arc::clone(&cache), listeners, pending()).expect("bind");
        let reapi = bound.reapi;
        tokio::spawn(async move { bound.serving.await.expect("serve") });
        let channel = Endpoint::from_shared(format!("http://{reapi}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        Self {
            cache,
            worker_addr: bound.worker,
            channel,
        }
    }

    pub fn exec(&self) -> ExecutionClient<Channel> {
        ExecutionClient::new(self.channel.clone()).max_decoding_message_size(MAX_MESSAGE_BYTES)
    }

    pub fn ac(&self) -> ActionCacheClient<Channel> {
        ActionCacheClient::new(self.channel.clone())
    }

    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }

    pub async fn upload(&self, blobs: &[&Blob]) {
        let response = ContentAddressableStorageClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES)
            .batch_update_blobs(BatchUpdateBlobsRequest {
                requests: blobs
                    .iter()
                    .map(|b| batch_update_blobs_request::Request {
                        digest: Some(b.proto.clone()),
                        data: b.data.clone(),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            })
            .await
            .expect("BatchUpdateBlobs")
            .into_inner();
        for r in response.responses {
            assert_eq!(
                r.status.expect("status").code,
                0,
                "upload of {:?}",
                r.digest
            );
        }
    }

    /// Whether the CAS holds `blob`.
    pub async fn holds(&self, blob: &Blob) -> bool {
        ContentAddressableStorageClient::new(self.channel.clone())
            .find_missing_blobs(FindMissingBlobsRequest {
                blob_digests: vec![blob.proto.clone()],
                ..Default::default()
            })
            .await
            .expect("FindMissingBlobs")
            .into_inner()
            .missing_blob_digests
            .is_empty()
    }

    /// Starts an Execute of `action`.
    pub async fn execute(&self, action: &Blob) -> Streaming<Operation> {
        self.execute_with(action, false).await.expect("Execute")
    }

    pub async fn execute_with(
        &self,
        action: &Blob,
        skip_cache_lookup: bool,
    ) -> Result<Streaming<Operation>, tonic::Status> {
        self.exec()
            .execute(ExecuteRequest {
                instance_name: "main".to_owned(),
                action_digest: Some(action.proto.clone()),
                skip_cache_lookup,
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
    }

    /// GetActionResult: the result, or the error code.
    pub async fn cached(&self, action: &Blob) -> Result<ActionResult, Code> {
        self.ac()
            .get_action_result(GetActionResultRequest {
                action_digest: Some(action.proto.clone()),
                ..Default::default()
            })
            .await
            .map(tonic::Response::into_inner)
            .map_err(|s| s.code())
    }

    /// A daemon for `node` with `cpus` CPUs and `mem_gib` GiB, registered.
    pub async fn daemon(&self, node: &str, cpus: u32, mem_gib: u32) -> FakeDaemon {
        FakeDaemon::connect(self.worker_addr, hello(node, cpus, mem_gib))
            .await
            .expect("registered")
    }
}

/// A Hello for `node` reporting `cpus` and `mem_gib`.
pub fn hello(node: &str, cpus: u32, mem_gib: u32) -> Hello {
    Hello {
        protocol_version: 1,
        node_id: node.to_owned(),
        daemon_version: "test".to_owned(),
        capabilities: vec![
            Capability {
                key: "cpus".to_owned(),
                value: cpus.to_string(),
            },
            Capability {
                key: "mem_gib".to_owned(),
                value: mem_gib.to_string(),
            },
        ],
        report_hash: Vec::new(),
    }
}

/// The server's side of a session, as a daemon sees it. Messages a test is not waiting
/// for are kept, in order, for a later `expect`.
pub struct FakeDaemon {
    tx: UnboundedSender<DaemonMessage>,
    inbound: Streaming<ServerMessage>,
    kept: VecDeque<server_message::Message>,
    seq: u64,
}

impl FakeDaemon {
    /// Opens a stream, sends `hello` and waits for Welcome.
    pub async fn connect(addr: SocketAddr, hello: Hello) -> Result<Self, tonic::Status> {
        let mut d = Self::open(addr, daemon_message::Message::Hello(hello)).await?;
        match d.next().await {
            Some(server_message::Message::Welcome(w)) => {
                assert_eq!(w.protocol_version, 1);
                assert_eq!(w.heartbeat_interval_ms, INTERVAL.as_millis() as u64);
                Ok(d)
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    /// Opens a stream whose first message is `first`, and returns once the server has
    /// answered with headers (or refused).
    pub async fn open(
        addr: SocketAddr,
        first: daemon_message::Message,
    ) -> Result<Self, tonic::Status> {
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let (tx, rx) = unbounded();
        tx.unbounded_send(DaemonMessage {
            message: Some(first),
        })
        .expect("queue the first message");
        let inbound = WorkerClient::new(channel).session(rx).await?.into_inner();
        Ok(Self {
            tx,
            inbound,
            kept: VecDeque::new(),
            seq: 0,
        })
    }

    pub fn send(&self, message: daemon_message::Message) {
        self.tx
            .unbounded_send(DaemonMessage {
                message: Some(message),
            })
            .expect("the stream is open");
    }

    pub fn send_empty(&self) {
        self.tx
            .unbounded_send(DaemonMessage { message: None })
            .expect("the stream is open");
    }

    /// Ends the stream from the daemon side.
    pub fn close(self) {}

    async fn next(&mut self) -> Option<server_message::Message> {
        if let Some(m) = self.kept.pop_front() {
            return Some(m);
        }
        let msg = timeout(PROMPT, self.inbound.message())
            .await
            .expect("a server message in time")
            .expect("the stream is healthy")?;
        Some(msg.message.expect("a non-empty server message"))
    }

    /// The first message `pick` accepts, within `within`, keeping the others.
    pub async fn expect_within<T>(
        &mut self,
        within: Duration,
        mut pick: impl FnMut(&server_message::Message) -> Option<T>,
    ) -> Option<T> {
        if let Some(at) = self.kept.iter().position(|m| pick(m).is_some()) {
            let m = self.kept.remove(at).expect("present");
            return pick(&m);
        }
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let msg = timeout(
                deadline - tokio::time::Instant::now(),
                self.inbound.message(),
            )
            .await
            .ok()?
            .expect("the stream is healthy")?;
            let m = msg.message.expect("a non-empty server message");
            if let Some(t) = pick(&m) {
                return Some(t);
            }
            self.kept.push_back(m);
        }
    }

    pub async fn expect<T>(
        &mut self,
        what: &str,
        pick: impl FnMut(&server_message::Message) -> Option<T>,
    ) -> T {
        self.expect_within(PROMPT, pick)
            .await
            .unwrap_or_else(|| panic!("no {what} within {PROMPT:?}"))
    }

    pub async fn offer(&mut self) -> LeaseOffer {
        self.expect("LeaseOffer", |m| match m {
            server_message::Message::LeaseOffer(o) => Some(o.clone()),
            _ => None,
        })
        .await
    }

    /// The next Start. The offer of the same lease, if kept, is dropped with it.
    pub async fn start(&mut self) -> Start {
        let start = self
            .expect("Start", |m| match m {
                server_message::Message::Start(s) => Some(s.clone()),
                _ => None,
            })
            .await;
        self.kept.retain(|m| {
            !matches!(m, server_message::Message::LeaseOffer(o) if o.lease_id == start.lease_id)
        });
        start
    }

    /// Asserts no Offer or Start arrives within [`QUIET`].
    pub async fn no_work(&mut self) {
        let work = self
            .expect_within(QUIET, |m| match m {
                server_message::Message::LeaseOffer(_) | server_message::Message::Start(_) => {
                    Some(m.clone())
                }
                _ => None,
            })
            .await;
        assert_eq!(work, None, "work arrived");
    }

    /// Sends a heartbeat listing `running` and returns whether it was acknowledged
    /// within [`QUIET`].
    pub async fn heartbeat(&mut self, running: &[LeaseId]) -> bool {
        self.seq += 1;
        let seq = self.seq;
        self.send(daemon_message::Message::Heartbeat(Heartbeat {
            seq,
            report_hash: Vec::new(),
            running: running.to_vec(),
        }));
        self.expect_within(QUIET, |m| match m {
            server_message::Message::HeartbeatAck(a) if a.seq == seq => Some(()),
            _ => None,
        })
        .await
        .is_some()
    }

    /// Sends `result` and returns the server's acknowledgement.
    pub async fn report(&mut self, result: kbf_proto::worker::Result) -> ResultAck {
        let lease = result.lease_id;
        self.send(daemon_message::Message::Result(result));
        self.expect("ResultAck", |m| match m {
            server_message::Message::ResultAck(a) if a.lease_id == lease => Some(*a),
            _ => None,
        })
        .await
    }
}

/// An OK Result for `lease` carrying `result`.
pub fn ran(lease: Option<LeaseId>, result: &ActionResult) -> kbf_proto::worker::Result {
    kbf_proto::worker::Result {
        lease_id: lease,
        status: Some(rpc::Status::default()),
        action_result: Some(result.clone()),
    }
}

/// A failed Result for `lease`.
pub fn failed(lease: Option<LeaseId>, code: Code) -> kbf_proto::worker::Result {
    kbf_proto::worker::Result {
        lease_id: lease,
        status: Some(rpc::Status {
            code: code as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        action_result: None,
    }
}

/// A blob: its bytes and its digest in both forms.
#[derive(Clone, Debug)]
pub struct Blob {
    pub data: Vec<u8>,
    pub digest: kbf_types::Digest,
    pub proto: reapi::Digest,
}

impl Blob {
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        let data = data.into();
        let digest = kbf_segments::sha256(&data);
        let proto = kbf_front::digest_to_proto(&digest);
        Self {
            data,
            digest,
            proto,
        }
    }

    pub fn of(message: &impl Message) -> Self {
        Self::new(message.encode_to_vec())
    }
}

/// An action and every blob it needs.
pub struct Job {
    pub action: Blob,
    pub command: Blob,
    pub root: Blob,
    pub input: Blob,
}

impl Job {
    /// A job running `argv`, with one input file, on `platform` properties.
    pub fn new(argv: &str, platform: &[(&str, &str)]) -> Self {
        let input = Blob::new(format!("input of {argv}"));
        let root = Blob::of(&Directory {
            files: vec![FileNode {
                name: "in".to_owned(),
                digest: Some(input.proto.clone()),
                ..Default::default()
            }],
            ..Default::default()
        });
        let command = Blob::of(&Command {
            arguments: vec![argv.to_owned()],
            ..Default::default()
        });
        let action = Blob::of(&Action {
            command_digest: Some(command.proto.clone()),
            input_root_digest: Some(root.proto.clone()),
            platform: Some(Platform {
                properties: platform
                    .iter()
                    .map(|(name, value)| platform::Property {
                        name: (*name).to_owned(),
                        value: (*value).to_owned(),
                    })
                    .collect(),
            }),
            ..Default::default()
        });
        Self {
            action,
            command,
            root,
            input,
        }
    }

    pub fn blobs(&self) -> [&Blob; 4] {
        [&self.action, &self.command, &self.root, &self.input]
    }
}

/// Uploads an output and returns an ActionResult naming it, with `exit_code`.
pub async fn output(cell: &Cell, text: &str, exit_code: i32) -> ActionResult {
    let out = Blob::new(text);
    cell.upload(&[&out]).await;
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(out.proto.clone()),
            ..Default::default()
        }],
        exit_code,
        ..Default::default()
    }
}

/// The ExecuteResponse of a done operation.
pub fn response(op: &Operation) -> ExecuteResponse {
    assert!(op.done, "{op:?} is not done");
    match &op.result {
        Some(operation::Result::Response(any)) => {
            ExecuteResponse::decode(any.value.as_slice()).expect("an ExecuteResponse")
        }
        other => panic!("a done operation without a response: {other:?}"),
    }
}

/// The stage of an operation, from its metadata.
pub fn stage(op: &Operation) -> i32 {
    let any = op.metadata.as_ref().expect("metadata");
    reapi::ExecuteOperationMetadata::decode(any.value.as_slice())
        .expect("ExecuteOperationMetadata")
        .stage
}

/// Reads `ops` until the done operation, which it returns.
pub async fn done(ops: &mut Streaming<Operation>) -> Operation {
    loop {
        let op = timeout(PROMPT, ops.message())
            .await
            .expect("an operation update in time")
            .expect("a healthy stream")
            .expect("the stream ends only after the done operation");
        if op.done {
            return op;
        }
    }
}

/// Whether `ops` produces a done operation within [`QUIET`].
pub async fn done_within_quiet(ops: &mut Streaming<Operation>) -> bool {
    timeout(QUIET, done(ops)).await.is_ok()
}
