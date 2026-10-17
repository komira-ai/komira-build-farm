//! A stand-in for one kbf instance on loopback: a TLS gRPC server with the REAPI
//! `Execution` calls and `grpc.health.v1`, a plain TCP worker port that accepts and never
//! writes (a daemon's session is quiet), and an API port answering `/readyz`. One flag,
//! `ready`, is what the health service and `/readyz` both answer, as kbf's readiness is.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::Stream;
use kbf_deploy::envoy::box_config::Instance as Ports;
use kbf_it::pki::PkiFiles;
use kbf_proto::google::longrunning::Operation;
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_server::{Health, HealthServer};
use kbf_proto::grpc::health::v1::{
    HealthCheckRequest, HealthCheckResponse, HealthListRequest, HealthListResponse,
};
use kbf_proto::reapi::execution_server::{Execution, ExecutionServer};
use kbf_proto::reapi::{ExecuteRequest, WaitExecutionRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

/// How long an `Execute` stream stays silent when nobody releases it.
pub const FOR_EVER: u64 = 3_600_000;

/// What one instance saw, and the knobs the tests turn.
pub struct Instance {
    pub id: &'static str,
    /// What `grpc.health.v1` and `/readyz` answer.
    pub ready: AtomicBool,
    /// Milliseconds a new `Execute` or `WaitExecution` stream stays silent after its
    /// first frame, unless released.
    pub hold_ms: AtomicU64,
    /// Calls accepted.
    pub accepted: AtomicU64,
    /// Streams the peer dropped before the stub ended them (RST_STREAM or a closed
    /// connection).
    pub cut: AtomicU64,
    /// Worker-port connections accepted, and those the peer closed.
    pub sessions: AtomicU64,
    pub sessions_closed: AtomicU64,
    release: watch::Sender<u64>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        for t in self.tasks.lock().expect("lock").drain(..) {
            t.abort();
        }
    }
}

impl Instance {
    /// End every silent stream now, with its final frame.
    pub fn release(&self) {
        self.release.send_modify(|n| *n += 1);
    }

    /// Start the instance's three listeners. Each stream carries at most `max_streams`
    /// per connection, as kbf's `--reapi-max-streams` does.
    pub async fn start(
        id: &'static str,
        pki: &PkiFiles,
        ports: Ports,
        max_streams: u32,
    ) -> Arc<Instance> {
        let (release, _) = watch::channel(0);
        let me = Arc::new(Instance {
            id,
            ready: AtomicBool::new(true),
            hold_ms: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            cut: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
            sessions_closed: AtomicU64::new(0),
            release,
            tasks: Mutex::new(Vec::new()),
        });
        let cert = std::fs::read(&pki.server_cert).expect("certificate");
        let key = std::fs::read(&pki.server_key).expect("key");
        let tls = ServerTlsConfig::new().identity(Identity::from_pem(cert, key));
        let grpc = {
            let me = Arc::clone(&me);
            tokio::spawn(async move {
                let served = Server::builder()
                    .tls_config(tls)
                    .expect("tls")
                    .max_concurrent_streams(Some(max_streams))
                    .add_service(HealthServer::new(Svc(Arc::clone(&me))))
                    .add_service(ExecutionServer::new(Svc(Arc::clone(&me))))
                    .serve(([127, 0, 0, 1], ports.reapi).into())
                    .await;
                panic!("stub {} gRPC listener ended: {served:?}", me.id);
            })
        };
        let worker = tokio::spawn(sessions(Arc::clone(&me), bind(ports.worker)));
        let api = tokio::spawn(readyz(Arc::clone(&me), bind(ports.api)));
        me.tasks.lock().expect("lock").extend([grpc, worker, api]);
        me
    }
}

fn bind(port: u16) -> tokio::net::TcpListener {
    let l = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind a stub port");
    l.set_nonblocking(true).expect("nonblocking");
    tokio::net::TcpListener::from_std(l).expect("listener")
}

/// The worker port: accept, count, read until the peer closes.
async fn sessions(me: Arc<Instance>, listener: tokio::net::TcpListener) {
    loop {
        let Ok((mut conn, _)) = listener.accept().await else {
            continue;
        };
        me.sessions.fetch_add(1, Ordering::SeqCst);
        let me = Arc::clone(&me);
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while matches!(conn.read(&mut buf).await, Ok(n) if n > 0) {}
            me.sessions_closed.fetch_add(1, Ordering::SeqCst);
        });
    }
}

/// The API port: `/readyz`, 200 or 503, one request per connection.
async fn readyz(me: Arc<Instance>, listener: tokio::net::TcpListener) {
    loop {
        let Ok((mut conn, _)) = listener.accept().await else {
            continue;
        };
        let ready = me.ready.load(Ordering::SeqCst);
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf).await;
            let status = if ready {
                "200 OK"
            } else {
                "503 Service Unavailable"
            };
            let reply =
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = conn.write_all(reply.as_bytes()).await;
        });
    }
}

struct Svc(Arc<Instance>);

type Operations = Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send>>;

fn operation(id: &str, done: bool) -> Operation {
    Operation {
        name: id.to_owned(),
        done,
        ..Operation::default()
    }
}

impl Svc {
    /// A stream that sends one frame naming the instance, stays silent for `hold_ms` (or
    /// until released), then sends a final frame and ends.
    fn silent(&self) -> Operations {
        let me = Arc::clone(&self.0);
        me.accepted.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(2);
        tokio::spawn(async move {
            let mut release = me.release.subscribe();
            release.mark_unchanged();
            let hold = Duration::from_millis(me.hold_ms.load(Ordering::SeqCst));
            let sent = tx.send(Ok(operation(me.id, false))).await.is_ok();
            let finished = sent
                && {
                    tokio::select! {
                        () = tokio::time::sleep(hold) => tx.send(Ok(operation(me.id, true))).await.is_ok(),
                        _ = release.changed() => tx.send(Ok(operation(me.id, true))).await.is_ok(),
                        () = tx.closed() => false,
                    }
                };
            if !finished {
                me.cut.fetch_add(1, Ordering::SeqCst);
            }
        });
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|op| (op, rx))
        }))
    }
}

#[tonic::async_trait]
impl Execution for Svc {
    type ExecuteStream = Operations;
    type WaitExecutionStream = Operations;

    async fn execute(&self, _: Request<ExecuteRequest>) -> Result<Response<Operations>, Status> {
        Ok(Response::new(self.silent()))
    }

    async fn wait_execution(
        &self,
        _: Request<WaitExecutionRequest>,
    ) -> Result<Response<Operations>, Status> {
        Ok(Response::new(self.silent()))
    }
}

type Watched = Pin<Box<dyn Stream<Item = Result<HealthCheckResponse, Status>> + Send>>;

#[tonic::async_trait]
impl Health for Svc {
    async fn check(
        &self,
        _: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let status = if self.0.ready.load(Ordering::SeqCst) {
            ServingStatus::Serving
        } else {
            ServingStatus::NotServing
        };
        Ok(Response::new(HealthCheckResponse {
            status: status.into(),
        }))
    }

    async fn list(
        &self,
        _: Request<HealthListRequest>,
    ) -> Result<Response<HealthListResponse>, Status> {
        Err(Status::unimplemented("not used"))
    }

    type WatchStream = Watched;

    async fn watch(&self, _: Request<HealthCheckRequest>) -> Result<Response<Watched>, Status> {
        Err(Status::unimplemented("not used"))
    }
}
