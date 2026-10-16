//! [`LocalControlPlane`]: the daemon's control plane ([`kbf_node::ControlPlane`]) as calls
//! into the server half of the same process.
//!
//! Each session the daemon half opens is the server's own `kbf.worker.v1` session
//! ([`LocalWorker::session`]), opened on the server half's async runtime: the same
//! `Hello` checks, registration, heartbeats, `Start`s and results a remote daemon's
//! session on the worker listener has, with no listener, TLS or protobuf encoding
//! between the halves. The session carries no certificate, so it is admitted as on a
//! plain-text worker listener: there is no deny list. What a remote daemon sends is
//! unchanged; this plane sends nothing over the network.
//!
//! A server half that is gone (its runtime shut down) refuses each session UNAVAILABLE,
//! and the daemon half tries again after its reconnect wait, as a dialled daemon does.

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_node::{ControlPlane, DaemonMessages, ServerMessages};
use kbf_server::LocalWorker;
use tokio::runtime::Handle;
use tonic::Status;

/// The daemon's control plane as calls into the server half of this process.
pub struct LocalControlPlane {
    worker: LocalWorker,
    /// The server half's runtime, which every session is opened on.
    server: Handle,
}

impl LocalControlPlane {
    /// The plane over the server's `worker` service, its sessions opened on the server
    /// half's runtime `server`.
    #[must_use]
    pub fn new(worker: LocalWorker, server: Handle) -> Self {
        Self { worker, server }
    }
}

impl ControlPlane for LocalControlPlane {
    fn session(&self, outbound: DaemonMessages) -> BoxFuture<'_, Result<ServerMessages, Status>> {
        let worker = self.worker.clone();
        let opened = self
            .server
            .spawn(async move { worker.session(outbound.map(Ok).boxed()).await });
        Box::pin(async move {
            match opened.await {
                Ok(session) => session,
                Err(gone) => Err(Status::unavailable(format!(
                    "the server half did not answer: {gone}"
                ))),
            }
        })
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
