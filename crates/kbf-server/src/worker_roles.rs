//! Who may call what on the worker listener (`docs/design/architecture-v2.md`, section
//! 9: the role is in the client certificate, and each endpoint takes only the roles
//! expected there). One table, [`WORKER_LISTENER_ROLES`], maps each gRPC method the
//! listener serves to the roles whose certificates may call it; [`WorkerRolesLayer`]
//! applies it to every call before routing, and [`crate::serve::worker_peers`] puts the
//! layer on the listener's server, so every service added to that server is behind it.
//!
//! - The daemon role opens a worker session (`kbf.worker.v1.Worker/Session`, which also
//!   carries the storage map to the daemon) and makes the blob calls: `ByteStream`,
//!   `ContentAddressableStorage` (the chunking calls too) and `kbf.worker.v1.Segments`, and
//!   fetches the release it was offered (`kbf.update.v1.ReleaseService/Fetch`).
//! - The storage role registers and heartbeats (`kbf.map.v1.Membership`); a storage
//!   node gets the map in those answers.
//! - The frontend role may call nothing here.
//!
//! A method not in the table is refused PERMISSION_DENIED, whatever the certificate and
//! in plain text too, so an RPC added to the listener without a row is never served.
//! Under mutual TLS the role is read from the leaf certificate alone (`kbf_peer`: one
//! `urn:kbf:role:<name>` URI subjectAltName): no certificate is UNAUTHENTICATED; no role,
//! two, a name that is no role, or a role the row does not take is PERMISSION_DENIED. In
//! plain text a listed call goes on, as there is no identity to read (for tests and
//! trials on one machine).
//!
//! A node that runs both storage and a daemon holds two certificates and presents one on
//! each connection: its daemon half's connection is a daemon's, its storage half's a
//! storage node's.
//!
//! The services keep their own checks behind this one: the worker stream and the blob
//! calls bind the certificate's node name and read the deny list ([`crate::identity`]),
//! and `Membership` takes the storage role alone again ([`crate::members_rpc`]).

use std::task::{Context, Poll};

use futures::future::{Either, Ready, ready};
use kbf_storage::service::{Admission, PeerRole, role_of};
use tonic::Status;
use tonic::codegen::{Service, http};
use tower_layer::Layer;

const DAEMON: &[PeerRole] = &[PeerRole::Daemon];
const STORAGE: &[PeerRole] = &[PeerRole::Storage];

/// Each gRPC method path the worker listener serves, and the roles that may call it.
pub const WORKER_LISTENER_ROLES: &[(&str, &[PeerRole])] = &[
    ("/kbf.worker.v1.Worker/Session", DAEMON),
    ("/google.bytestream.ByteStream/Read", DAEMON),
    ("/google.bytestream.ByteStream/Write", DAEMON),
    ("/google.bytestream.ByteStream/QueryWriteStatus", DAEMON),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs",
        DAEMON,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs",
        DAEMON,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchReadBlobs",
        DAEMON,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/GetTree",
        DAEMON,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/SplitBlob",
        DAEMON,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/SpliceBlob",
        DAEMON,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/GetChunkMapping",
        DAEMON,
    ),
    (
        "/build.bazel.remote.execution.v2.ContentAddressableStorage/RegisterChunkMapping",
        DAEMON,
    ),
    // The node's update client fetches the release it was offered (the daemon half of a
    // storage-and-daemon host presents its daemon certificate).
    ("/kbf.update.v1.ReleaseService/Fetch", DAEMON),
    ("/kbf.worker.v1.Segments/Begin", DAEMON),
    ("/kbf.worker.v1.Segments/PutRange", DAEMON),
    ("/kbf.worker.v1.Segments/Commit", DAEMON),
    ("/kbf.worker.v1.Segments/Locate", DAEMON),
    ("/kbf.worker.v1.Segments/ReadRange", DAEMON),
    ("/kbf.map.v1.Membership/Register", STORAGE),
    ("/kbf.map.v1.Membership/Heartbeat", STORAGE),
];

/// The roles that may call `path` on the worker listener; `None` if it is not served.
#[must_use]
pub fn roles_for(path: &str) -> Option<&'static [PeerRole]> {
    WORKER_LISTENER_ROLES
        .iter()
        .find(|(served, _)| *served == path)
        .map(|(_, roles)| *roles)
}

/// Admits a call of `path` whose verified chain starts with `leaf`, as the module docs
/// say.
///
/// # Errors
/// PERMISSION_DENIED: `path` is not in [`WORKER_LISTENER_ROLES`]; or, under
/// [`Admission::Certified`], the certificate names no role, several, a name that is no
/// role, or a role the path does not take. UNAUTHENTICATED: under
/// [`Admission::Certified`], no certificate, or one that does not parse.
pub fn admit(admission: Admission, path: &str, leaf: Option<&[u8]>) -> Result<(), Status> {
    let Some(roles) = roles_for(path) else {
        return Err(Status::permission_denied(format!(
            "{path} is not served on the worker listener"
        )));
    };
    if admission == Admission::Open {
        return Ok(());
    }
    let der = leaf.ok_or_else(|| Status::unauthenticated("no client certificate"))?;
    let role = role_of(der)?;
    if roles.contains(&role) {
        return Ok(());
    }
    let takes: Vec<&str> = roles.iter().map(|r| r.name()).collect();
    Err(Status::permission_denied(format!(
        "role {role} may not call {path} on the worker listener; it takes {}",
        takes.join(", ")
    )))
}

/// Applies [`admit`] to every call of the server it is added to (with
/// `Server::layer`), before routing: a refused call is answered with its status and
/// never reaches a service.
#[derive(Clone, Copy, Debug)]
pub struct WorkerRolesLayer {
    admission: Admission,
}

impl WorkerRolesLayer {
    /// The layer for a listener that admits as `admission` says.
    #[must_use]
    pub const fn new(admission: Admission) -> Self {
        Self { admission }
    }
}

impl<S> Layer<S> for WorkerRolesLayer {
    type Service = WorkerRoles<S>;

    fn layer(&self, inner: S) -> WorkerRoles<S> {
        WorkerRoles {
            inner,
            admission: self.admission,
        }
    }
}

/// The service [`WorkerRolesLayer`] wraps around the worker listener's routes.
#[derive(Clone, Debug)]
pub struct WorkerRoles<S> {
    inner: S,
    admission: Admission,
}

impl<S, B, R> Service<http::Request<B>> for WorkerRoles<S>
where
    S: Service<http::Request<B>, Response = http::Response<R>>,
    R: Default,
{
    type Response = http::Response<R>;
    type Error = S::Error;
    type Future = Either<Ready<Result<Self::Response, Self::Error>>, S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let certs = kbf_tls::peer_certs_of(request.extensions());
        let leaf = certs.as_deref().and_then(|chain| chain.first());
        let path = request.uri().path();
        match admit(self.admission, path, leaf.map(AsRef::as_ref)) {
            Ok(()) => Either::Right(self.inner.call(request)),
            Err(refused) => {
                let (code, reason) = (refused.code(), refused.message());
                tracing::warn!(call = path, ?code, reason, "worker listener call refused");
                Either::Left(ready(Ok(refused.into_http())))
            }
        }
    }
}
