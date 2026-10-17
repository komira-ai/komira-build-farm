//! ByteStream through the frontend as a raw HTTP/2 pass-through.
//!
//! The service is registered under `google.bytestream.ByteStream` and handed the whole
//! HTTP request. It reads the routing key from the first bytes of the request body
//! (`resource_name` is field 1 of `ReadRequest`, `WriteRequest` and
//! `QueryWriteStatusRequest`), authorizes the call against the instance name in it, and
//! gives the request to a storage node with its headers, `grpc-timeout` and body
//! unchanged, except for the map generation header (`kbf-map-generation`): the client's
//! is dropped and the frontend's own is set when it routed by a map, so a node can tell
//! the frontend's map is old. The response (data and trailers) comes back the same way.
//! No protobuf message past that first field is decoded, and no payload byte is
//! collected, so a blob of any size passes in constant memory and nothing here caps one.
//!
//! **Read** tries the owners of the blob in rank order: a node that fails before its
//! first byte (unreachable, an error status in the headers or as the first frame) is
//! passed over, transparently. When no owner answers, the client is told NOT_FOUND only
//! if every owner said so; otherwise it is told the first failure of an owner that could
//! not say (it may hold the blob). Once a byte has been sent the node is the answer: a
//! failure after it ends the call with that error, and the client resumes with
//! `read_offset`.
//!
//! **STALE_MAP.** A node that holds a newer map answers FAILED_PRECONDITION with its
//! generation; on this raw path that answer goes to the client as it is, with no refetch
//! of the map and no second try (the whole-blob calls, which go through the router, do
//! both). The frontend's map is refreshed by its own timer, so the client's next call is
//! routed by the newer map. A storage node does not judge a call's generation yet; when
//! it does, a Read or QueryWriteStatus here (whose request is held) can be run again once
//! the map is fetched, and a Write (whose body is not) cannot.
//!
//! **Write** goes to the writer alone: the first owner of the blob that the map does not
//! say is down (not always the owner a read asks first). If the blob's writer changes
//! during the upload it ends UNAVAILABLE and the client starts the upload again: the
//! frontend holds no bytes it could send to the new one. Nothing here hashes what passes:
//! a node checks the blob it is sent and the blob it sends.
//!
//! **QueryWriteStatus** asks the writer.
//!
//! **Compression.** A gRPC-compressed message hides the name, so a call whose first
//! message is compressed, or that names a `grpc-encoding` other than `identity`, is
//! INVALID_ARGUMENT.

use std::convert::Infallible;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use kbf_auth::{Authorizers, authorize};
use kbf_storage::service::MAP_GENERATION_HEADER;
use tonic::body::Body;
use tonic::codegen::{Service, http};
use tonic::server::NamedService;
use tonic::{Code, Status};

use super::body::{Guard, Pass, Probe, collect, compressed, name_in};
use super::owners::{Owner, Owners, no_storage};
use super::{Direction, Meter, Method, name};

/// The raw ByteStream service.
#[derive(Clone)]
pub(super) struct RawByteStream {
    pub owners: Owners,
    pub authorizers: Arc<Authorizers>,
    pub meter: Arc<Meter>,
}

impl NamedService for RawByteStream {
    const NAME: &'static str = "google.bytestream.ByteStream";
}

impl Service<http::Request<Body>> for RawByteStream {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { Ok(this.serve(request).await.unwrap_or_else(|s| s.into_http())) })
    }
}

/// Whether a node's failure before its first byte says nothing of the request, so the
/// next owner may answer it. A refusal of the request itself (a bad name, a range past
/// the end, no permission) would be the same everywhere.
const fn next_owner_may_answer(code: Code) -> bool {
    matches!(
        code,
        Code::Unavailable | Code::NotFound | Code::Internal | Code::DataLoss | Code::Unknown
    )
}

fn unreachable_node(owner: &Owner, error: &dyn std::fmt::Display) -> Status {
    Status::unavailable(format!("storage node {}: {error}", owner.id()))
}

impl RawByteStream {
    async fn serve(&self, request: http::Request<Body>) -> Result<http::Response<Body>, Status> {
        let (parts, body) = request.into_parts();
        let method = Method::of_path(parts.uri.path())
            .ok_or_else(|| Status::unimplemented(format!("{} is not served", parts.uri.path())))?;
        let encoding = parts
            .headers
            .get("grpc-encoding")
            .map(http::HeaderValue::as_bytes);
        if encoding.is_some_and(|e| e != b"identity") {
            return Err(compressed("a grpc-encoding other than identity"));
        }
        if method == Method::Write {
            self.write(&parts, body).await
        } else {
            self.unary(&parts, body, method).await
        }
    }

    /// Authorizes the call `method` against the instance in `name`, and says which blob
    /// it is about.
    async fn authorized(
        &self,
        parts: &http::request::Parts,
        method: Method,
        named: &str,
    ) -> Result<name::Resource, Status> {
        let resource = name::parse(method, named)?;
        let authorizer = match method {
            Method::Read => &self.authorizers.cas_get,
            Method::Write | Method::QueryWriteStatus => &self.authorizers.cas_put,
        };
        let caller = kbf_auth::metadata_of_http(&parts.extensions);
        authorize(&**authorizer, &caller, method.path(), &resource.instance).await?;
        Ok(resource)
    }

    async fn write(
        &self,
        parts: &http::request::Parts,
        body: Body,
    ) -> Result<http::Response<Body>, Status> {
        let mut up = Pass::new(body, &self.meter, Method::Write, Direction::ToStorage);
        let name = up.peek_name().await?;
        let resource = self.authorized(parts, Method::Write, &name).await?;
        let owner = self.owners.writer(&resource.digest)?;
        let failed = Arc::new(Mutex::new(None));
        let up = up.guarded(Guard {
            owners: self.owners.clone(),
            digest: resource.digest,
            owner: Arc::from(owner.id()),
            failed: Arc::clone(&failed),
        });
        let answer = send(&owner, parts, up).await;
        // The upload ended because its primary changed: that is the answer, whatever the
        // node said of the body that broke.
        let refusal = failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        let (head, down) = answer?.into_parts();
        let down = Pass::new(down, &self.meter, Method::Write, Direction::FromStorage);
        Ok(http::Response::from_parts(head, Body::new(down)))
    }

    /// `Read` and `QueryWriteStatus`: one small request message, replayed to each owner
    /// tried.
    async fn unary(
        &self,
        parts: &http::request::Parts,
        body: Body,
        method: Method,
    ) -> Result<http::Response<Body>, Status> {
        let request = collect(body).await?;
        let name = name_in(&request)?.ok_or_else(|| {
            Status::invalid_argument("the request is shorter than its resource_name")
        })?;
        let resource = self.authorized(parts, method, &name).await?;
        // A read asks every owner in turn; the status of an upload is the writer's alone.
        let owners = if method == Method::Read {
            self.owners.for_digest(&resource.digest)
        } else {
            Arc::from(vec![self.owners.writer(&resource.digest)?])
        };
        // NOT_FOUND is the answer only if every owner said it: an owner that could not say
        // (down, broken) may hold the blob, so the first of those is what the client is
        // told, as the router does for a whole-blob read.
        let (mut failure, mut missed) = (None, None);
        for owner in owners.iter() {
            match self.ask(owner, parts, &request, method).await {
                Ok(response) => return Ok(response),
                Err(status) if next_owner_may_answer(status.code()) => {
                    tracing::warn!(node = owner.id(), %status, "storage node failed before its first byte");
                    if status.code() == Code::NotFound {
                        missed = Some(status);
                    } else {
                        failure.get_or_insert(status);
                    }
                }
                Err(status) => return Err(status),
            }
        }
        Err(failure.or(missed).unwrap_or_else(no_storage))
    }

    /// One owner's answer, once it has failed or sent its first byte.
    async fn ask(
        &self,
        owner: &Owner,
        parts: &http::request::Parts,
        request: &Bytes,
        method: Method,
    ) -> Result<http::Response<Body>, Status> {
        let up = Pass::replay(request.clone(), &self.meter, method, Direction::ToStorage);
        let (head, down) = send(owner, parts, up).await?.into_parts();
        // A status in the headers is a trailers-only answer: the node failed (or, for an
        // empty blob, finished) with nothing to send.
        let failed = Status::from_header_map(&head.headers).filter(|s| s.code() != Code::Ok);
        if let Some(status) = failed {
            return Err(status);
        }
        let mut down = Pass::new(down, &self.meter, method, Direction::FromStorage);
        match down.probe().await {
            Probe::Failed(status) => Err(status),
            Probe::Data | Probe::Done => Ok(http::Response::from_parts(head, Body::new(down))),
        }
    }
}

/// `body` to `owner` as a request like the client's: same method, path, headers.
async fn send(
    owner: &Owner,
    parts: &http::request::Parts,
    body: Pass,
) -> Result<http::Response<tonic::body::Body>, Status> {
    let mut request = http::Request::new(Body::new(body));
    *request.method_mut() = parts.method.clone();
    *request.uri_mut() = parts.uri.clone();
    *request.version_mut() = parts.version;
    *request.headers_mut() = parts.headers.clone();
    // The generation of the map the frontend routed by, never the client's.
    request.headers_mut().remove(MAP_GENERATION_HEADER);
    if owner.generation > 0 {
        request.headers_mut().insert(
            MAP_GENERATION_HEADER,
            http::HeaderValue::from(owner.generation),
        );
    }
    // The channel is a tower Buffer: ready it before calling it.
    let mut channel = owner.channel.clone();
    let response = async {
        poll_fn(|cx| channel.poll_ready(cx)).await?;
        channel.call(request).await
    }
    .await
    .map_err(|e| unreachable_node(owner, &e))?;
    let (head, body) = response.into_parts();
    Ok(http::Response::from_parts(head, Body::new(body)))
}
