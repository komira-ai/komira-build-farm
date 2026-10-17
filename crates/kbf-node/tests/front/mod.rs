//! A fake server for the daemon binary: a mutual-TLS listener with `kbf.worker.v1`,
//! whose sessions the test drives message by message, and `ByteStream` over a CAS in
//! this process (`kbf-front` over memory) that the daemon reads actions from, as the
//! server's worker listener serves both; and the same CAS in plain text on another
//! port, for the test's own uploads. [`Front::stalling_writes`] serves a CAS whose
//! `Write` on the worker listener takes the upload and never answers.

use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt as _};
use kbf_daemon::cas::Cas as _;
use kbf_proto::google::bytestream::byte_stream_server::{ByteStream, ByteStreamServer};
use kbf_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, WriteRequest, WriteResponse,
};
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::{Action, Command, Digest, Directory};
use kbf_proto::worker::worker_server::{Worker, WorkerServer};
use kbf_proto::worker::{
    DaemonMessage, Hello, LeaseId, ServerMessage, Start, Welcome, daemon_message, server_message,
};
use kbf_transfer::{TransferConfig, Transfers};
use prost::Message as _;
use tokio::sync::mpsc as tokio_mpsc;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Certificate, Endpoint, Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};

/// The server's side of one daemon stream.
pub struct Session {
    from_daemon: mpsc::Receiver<DaemonMessage>,
    to_daemon: tokio_mpsc::UnboundedSender<Result<ServerMessage, Status>>,
}

impl Session {
    /// The next daemon message, or `None` once `within` has passed or the stream ended.
    pub fn next(&self, within: Duration) -> Option<daemon_message::Message> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = self.from_daemon.recv_timeout(left).ok()?;
            if let Some(m) = message.message {
                return Some(m);
            }
        }
    }

    /// Waits for the stream's `Hello`, skipping nothing: it must come first.
    pub fn hello(&self) -> Hello {
        match self.next(Duration::from_secs(30)) {
            Some(daemon_message::Message::Hello(hello)) => hello,
            other => panic!("expected a Hello, got {other:?}"),
        }
    }

    fn send(&self, message: server_message::Message) {
        self.to_daemon
            .send(Ok(ServerMessage {
                message: Some(message),
            }))
            .expect("the stream is open");
    }

    /// Answers the `Hello` with a heartbeat every second and no lease epoch.
    pub fn welcome(&self) {
        self.send(server_message::Message::Welcome(Welcome {
            protocol_version: 1,
            heartbeat_interval_ms: 1_000,
            epoch: 0,
            ..Welcome::default()
        }));
    }

    /// The next `Result` the daemon sends, skipping other messages, or `None` once
    /// `within` has passed or the stream ended.
    pub fn result(&self, within: Duration) -> Option<kbf_proto::worker::Result> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if let daemon_message::Message::Result(result) = self.next(left)? {
                return Some(result);
            }
        }
    }

    /// Starts lease `term.seq` running `action`, valid from the stream's `Hello`.
    pub fn start(&self, term: u64, seq: u64, action: Digest) {
        self.send(server_message::Message::Start(Start {
            lease_id: Some(LeaseId { term, seq }),
            kind: "action".to_owned(),
            action_digest: Some(action),
            ..Start::default()
        }));
    }
}

struct FakeWorker {
    sessions: mpsc::Sender<Session>,
}

#[tonic::async_trait]
impl Worker for FakeWorker {
    type SessionStream = Pin<Box<dyn Stream<Item = Result<ServerMessage, Status>> + Send>>;

    async fn session(
        &self,
        request: Request<Streaming<DaemonMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let (to_daemon, outbound) = tokio_mpsc::unbounded_channel();
        let (seen, from_daemon) = mpsc::channel();
        let mut inbound = request.into_inner();
        tokio::spawn(async move {
            while let Ok(Some(message)) = inbound.message().await {
                if seen.send(message).is_err() {
                    break;
                }
            }
        });
        let _ = self.sessions.send(Session {
            from_daemon,
            to_daemon,
        });
        let stream = futures::stream::unfold(outbound, |mut rx| async move {
            rx.recv().await.map(|m| (m, rx))
        });
        Ok(Response::new(stream.boxed()))
    }
}

/// `ByteStream` over `inner`, whose `Write`, when `stall` is set, reads the whole
/// upload and then never answers: a store that stopped answering mid-upload.
struct Stalling<S> {
    inner: S,
    stall: bool,
}

#[tonic::async_trait]
impl<S: ByteStream> ByteStream for Stalling<S> {
    type ReadStream = S::ReadStream;

    async fn read(
        &self,
        request: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        self.inner.read(request).await
    }

    async fn write(
        &self,
        request: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        if !self.stall {
            return self.inner.write(request).await;
        }
        let mut upload = request.into_inner();
        while let Ok(Some(_)) = upload.message().await {}
        std::future::pending().await
    }

    async fn query_write_status(
        &self,
        request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        self.inner.query_write_status(request).await
    }
}

/// The worker listener and the CAS, served on loopback ports by their own runtime.
pub struct Front {
    /// Mutual TLS: worker sessions and `ByteStream` (the daemon's `--cas`).
    pub worker: SocketAddr,
    /// Plain text: the same CAS, for the test's uploads.
    pub cas: SocketAddr,
    sessions: mpsc::Receiver<Session>,
    runtime: tokio::runtime::Runtime,
}

impl Front {
    /// Serves both, with the server certificate and CA that `tls` wrote into `dir`.
    pub fn start(dir: &Path) -> Self {
        Self::serve(dir, false)
    }

    /// [`Self::start`], but each `Write` on the worker listener takes the upload and
    /// never answers. The plain-text CAS stores the test's uploads as before.
    pub fn stalling_writes(dir: &Path) -> Self {
        Self::serve(dir, true)
    }

    fn serve(dir: &Path, stall: bool) -> Self {
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).expect("read a TLS file");
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(read("server.pem"), read("server.key")))
            .client_ca_root(Certificate::from_pem(read("ca.pem")));
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _context = runtime.enter();
        let (sessions_tx, sessions) = mpsc::channel();
        let bind = || TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
        let worker_incoming = bind();
        let worker = worker_incoming.local_addr().expect("addr");
        let cache = Arc::new(kbf_front::Cache::memory());
        let router = Server::builder()
            .tls_config(tls)
            .expect("server TLS")
            .add_service(WorkerServer::new(FakeWorker {
                sessions: sessions_tx,
            }))
            .add_service(ByteStreamServer::new(Stalling {
                inner: kbf_front::ByteStreamService::new(Arc::clone(&cache)),
                stall,
            }));
        runtime.spawn(router.serve_with_incoming(worker_incoming));
        let cas_incoming = bind();
        let cas = cas_incoming.local_addr().expect("addr");
        runtime.spawn(
            Server::builder()
                .add_routes(kbf_front::routes(cache))
                .serve_with_incoming(cas_incoming),
        );
        Self {
            worker,
            cas,
            sessions,
            runtime,
        }
    }

    /// The next stream a daemon opens.
    pub fn session(&self, within: Duration) -> Session {
        self.sessions
            .recv_timeout(within)
            .expect("the daemon connects")
    }

    /// Stores an action that runs `/bin/sh -c script` in an empty input root; returns
    /// its digest. Only for the tests of how the daemon ends a user's shell command and
    /// the background job it starts (the shell's `$$` and `$!` name the processes).
    pub fn user_shell(&self, script: &str) -> Digest {
        self.runtime.block_on(async {
            let channel = Endpoint::from_shared(format!("http://{}", self.cas))
                .expect("endpoint")
                .connect()
                .await
                .expect("connect to the CAS");
            let cas = Transfers::over(channel, TransferConfig::default());
            let put = |bytes: Vec<u8>| {
                let cas = &cas;
                async move { cas.put(bytes).await.expect("upload") }
            };
            let root = put(Directory::default().encode_to_vec()).await;
            let command = Command {
                arguments: ["/bin/sh", "-c", script].map(str::to_owned).to_vec(),
                environment_variables: vec![EnvironmentVariable {
                    name: "PATH".to_owned(),
                    value: "/usr/bin:/bin".to_owned(),
                }],
                ..Command::default()
            };
            let command = put(command.encode_to_vec()).await;
            let action = Action {
                command_digest: Some(command),
                input_root_digest: Some(root),
                ..Action::default()
            };
            put(action.encode_to_vec()).await
        })
    }
}
