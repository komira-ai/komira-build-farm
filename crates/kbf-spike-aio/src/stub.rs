//! A stub `grpc.health.v1` server: `Check` answers a flag, `List` names the stub, and
//! `Watch` is the silent server-streaming call the spike holds open.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use futures::Stream;
use kbf_proto::grpc::health::v1::health_check_response::ServingStatus;
use kbf_proto::grpc::health::v1::health_server::{Health, HealthServer};
use kbf_proto::grpc::health::v1::{
    HealthCheckRequest, HealthCheckResponse, HealthListRequest, HealthListResponse,
};
use tokio::sync::watch;
use tonic::{Request, Response, Status};

/// What the stub saw, readable from the harness.
pub struct Stub {
    pub id: String,
    pub port: u16,
    /// What `Check` answers.
    pub serving: AtomicBool,
    /// Milliseconds between frames on an open `Watch`; 0 is silent.
    pub progress_ms: AtomicU64,
    /// `Watch` streams open now.
    pub open: AtomicI64,
    /// `Watch` streams dropped by the peer before the stub ended them (RST_STREAM or
    /// a closed connection).
    pub cancelled: AtomicU64,
    /// Milliseconds `Check` waits before answering.
    pub check_delay_ms: AtomicU64,
    /// `Check` calls answered.
    pub checks: AtomicU64,
    /// `List` calls answered.
    pub lists: AtomicU64,
    /// `Watch` calls accepted.
    pub watches: AtomicU64,
    end: watch::Sender<u64>,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Stub {
    /// Starts a stub on `port`.
    pub fn start(id: &str, port: u16) -> Arc<Stub> {
        let (end, _) = watch::channel(0);
        let stub = Arc::new(Stub {
            id: id.to_owned(),
            port,
            serving: AtomicBool::new(true),
            progress_ms: AtomicU64::new(0),
            open: AtomicI64::new(0),
            cancelled: AtomicU64::new(0),
            check_delay_ms: AtomicU64::new(0),
            checks: AtomicU64::new(0),
            lists: AtomicU64::new(0),
            watches: AtomicU64::new(0),
            end,
            task: std::sync::Mutex::new(None),
        });
        let service = HealthServer::new(Svc(Arc::clone(&stub)));
        let addr = ([127, 0, 0, 1], port).into();
        let task = tokio::spawn(async move {
            let served = tonic::transport::Server::builder()
                .max_concurrent_streams(Some(20_000))
                .add_service(service)
                .serve(addr)
                .await;
            if let Err(e) = served {
                eprintln!("stub {addr}: {e}");
            }
        });
        *stub.task.lock().expect("lock") = Some(task);
        stub
    }

    /// Ends every open `Watch` cleanly (OK trailers).
    pub fn end_watches(&self) {
        self.end.send_modify(|n| *n += 1);
    }

    /// Stops the listener (open connections stay until the process exits).
    pub fn stop(&self) {
        if let Some(t) = self.task.lock().expect("lock").take() {
            t.abort();
        }
    }
}

struct Svc(Arc<Stub>);

struct Guard {
    stub: Arc<Stub>,
    ended: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stub.open.fetch_sub(1, Ordering::SeqCst);
        if !self.ended {
            self.stub.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}

type WatchStream = Pin<Box<dyn Stream<Item = Result<HealthCheckResponse, Status>> + Send>>;

fn serving() -> HealthCheckResponse {
    HealthCheckResponse {
        status: ServingStatus::Serving.into(),
    }
}

#[tonic::async_trait]
impl Health for Svc {
    async fn check(
        &self,
        _: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        let delay = self.0.check_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.0.checks.fetch_add(1, Ordering::SeqCst);
        let status = if self.0.serving.load(Ordering::SeqCst) {
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
        self.0.lists.fetch_add(1, Ordering::SeqCst);
        let statuses = HashMap::from([(self.0.id.clone(), serving())]);
        Ok(Response::new(HealthListResponse { statuses }))
    }

    type WatchStream = WatchStream;

    async fn watch(
        &self,
        _: Request<HealthCheckRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let stub = Arc::clone(&self.0);
        stub.watches.fetch_add(1, Ordering::SeqCst);
        stub.open.fetch_add(1, Ordering::SeqCst);
        let mut end = stub.end.subscribe();
        end.mark_unchanged();
        let state = (false, end, Guard { stub, ended: false });
        let stream = futures::stream::unfold(state, |(sent, mut end, mut guard)| async move {
            if !sent {
                return Some((Ok(serving()), (true, end, guard)));
            }
            let every = guard.stub.progress_ms.load(Ordering::SeqCst);
            let tick = async {
                if every == 0 {
                    std::future::pending::<()>().await;
                } else {
                    tokio::time::sleep(Duration::from_millis(every)).await;
                }
            };
            tokio::select! {
                _ = end.changed() => { guard.ended = true; None }
                () = tick => Some((Ok(serving()), (true, end, guard))),
            }
        });
        Ok(Response::new(Box::pin(stream)))
    }
}

/// A TCP server that accepts and never writes: the quiet session of a `tcp_proxy`.
pub struct Silent {
    pub port: u16,
    /// Connections accepted so far.
    pub accepted: AtomicU64,
    /// Connections the peer closed (read returned 0 or an error).
    pub closed_by_peer: AtomicU64,
    stop: watch::Sender<bool>,
}

impl Silent {
    pub fn start(port: u16) -> Arc<Silent> {
        let (stop, _) = watch::channel(false);
        let me = Arc::new(Silent { port, accepted: AtomicU64::new(0), closed_by_peer: AtomicU64::new(0), stop });
        let (me2, mut stop_rx) = (Arc::clone(&me), me.stop.subscribe());
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("bind silent");
        listener.set_nonblocking(true).expect("nonblocking");
        let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_rx.changed() => return, // the listener is dropped; open sessions stay
                    r = listener.accept() => {
                        let Ok((mut conn, _)) = r else { continue };
                        me2.accepted.fetch_add(1, Ordering::SeqCst);
                        let me3 = Arc::clone(&me2);
                        tokio::spawn(async move {
                            use tokio::io::AsyncReadExt;
                            let mut buf = [0u8; 64];
                            loop {
                                match conn.read(&mut buf).await {
                                    Ok(n) if n > 0 => {}
                                    _ => break,
                                }
                            }
                            me3.closed_by_peer.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                }
            }
        });
        me
    }

    /// Stops listening (health checks by connect now fail); accepted sessions stay open.
    pub fn stop_listening(&self) {
        self.stop.send_replace(true);
    }
}
