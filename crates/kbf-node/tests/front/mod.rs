//! A fake front for the daemon binary: a mutual-TLS `kbf.worker.v1` listener whose
//! sessions the test drives message by message, and a REAPI CAS in this process
//! (`kbf-front` over memory) that the daemon reads actions from.

use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt as _};
use kbf_daemon::cas::{Cas as _, CasClient};
use kbf_proto::reapi::command::EnvironmentVariable;
use kbf_proto::reapi::{Action, Command, Digest, Directory};
use kbf_proto::worker::worker_server::{Worker, WorkerServer};
use kbf_proto::worker::{
    DaemonMessage, LeaseId, ServerMessage, Start, Welcome, daemon_message, server_message,
};
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
    pub fn hello(&self) {
        match self.next(Duration::from_secs(30)) {
            Some(daemon_message::Message::Hello(_)) => {}
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
        }));
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

/// The worker listener and the CAS, served on loopback ports by their own runtime.
pub struct Front {
    pub worker: SocketAddr,
    pub cas: SocketAddr,
    sessions: mpsc::Receiver<Session>,
    runtime: tokio::runtime::Runtime,
}

impl Front {
    /// Serves both, with the server certificate and CA that `tls` wrote into `dir`.
    pub fn start(dir: &Path) -> Self {
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
        let router = Server::builder()
            .tls_config(tls)
            .expect("server TLS")
            .add_service(WorkerServer::new(FakeWorker {
                sessions: sessions_tx,
            }));
        runtime.spawn(router.serve_with_incoming(worker_incoming));
        let cas_incoming = bind();
        let cas = cas_incoming.local_addr().expect("addr");
        let cache = Arc::new(kbf_front::Cache::memory());
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
    /// its digest.
    pub fn sh(&self, script: &str) -> Digest {
        self.runtime.block_on(async {
            let channel = Endpoint::from_shared(format!("http://{}", self.cas))
                .expect("endpoint")
                .connect()
                .await
                .expect("connect to the CAS");
            let cas = CasClient::new(channel);
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
