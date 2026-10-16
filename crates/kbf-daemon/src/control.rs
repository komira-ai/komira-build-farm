//! The daemon's control plane: how it opens a `kbf.worker.v1` session, the [`ControlPlane`]
//! trait. A daemon whose server is elsewhere opens each over the connection it dialled
//! to one of its `--server`s (the `Session` call, over mutual TLS); a daemon
//! run in its server's own process is given a plane that opens it there, with no
//! listener, TLS or encoding between them (`kbf-all-in-one`'s). Either way the session is
//! the same exchange of messages ([`crate::Daemon`] says which), so nothing a remote daemon
//! sends depends on which plane another daemon uses.

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use kbf_proto::worker::worker_client::WorkerClient;
use kbf_proto::worker::{DaemonMessage, ServerMessage};
use tonic::Status;
use tonic::transport::Channel;

/// What the daemon sends on a session, in order; it ends when the daemon drops its end.
pub type DaemonMessages = UnboundedReceiver<DaemonMessage>;

/// What the server sends on a session, in order: its messages, or the error that ended
/// the stream. The stream ends when the server ends the session.
pub type ServerMessages = BoxStream<'static, Result<ServerMessage, Status>>;

/// Opens the daemon's sessions with a server.
pub trait ControlPlane: Send + Sync {
    /// Opens one session over `outbound`, the daemon's messages (its `Hello` first),
    /// and returns the server's, once the server has taken the session up.
    ///
    /// # Errors
    /// The status the server refused the session with, or why it could not be reached.
    fn session(&self, outbound: DaemonMessages) -> BoxFuture<'_, Result<ServerMessages, Status>>;
}

/// The plane of a daemon whose server is elsewhere: the `Session` call over a
/// connection it dialled.
pub(crate) struct Dialled(pub(crate) WorkerClient<Channel>);

impl ControlPlane for Dialled {
    fn session(&self, outbound: DaemonMessages) -> BoxFuture<'_, Result<ServerMessages, Status>> {
        let mut client = self.0.clone();
        Box::pin(async move {
            let response = client.session(outbound).await?;
            Ok(response.into_inner().boxed())
        })
    }
}
